//! Spending the outputs of non-default contracts, such as VHTLCs, through their
//! [`ContractSpec`] handler.
//!
//! The wallet's own send and settle flows only ever spend default, boarding and delegate
//! contracts through their forfeit leaf. A contract that isn't safe to pick automatically (a VHTLC
//! is escrow, not the wallet's money) is spent here instead, and only when the caller names its
//! outpoints. The contract's handler picks the leaf from what the spender knows: which keys it can
//! sign for, the preimage, and the time.

use crate::batch::BatchOutputType;
use crate::error::ErrorContext;
use crate::intent_fee;
use crate::intent_fee::UneconomicInputs;
use crate::utils::timeout_op;
use crate::utils::unix_now;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::ContractSpendPlan;
use crate::Error;
use ark_core::contract::ContractSpec;
use ark_core::contract::ContractState;
use ark_core::contract::PathContext;
use ark_core::contract::SpendPathKind;
use ark_core::contract::StoredContract;
use ark_core::intent;
use ark_core::introspector::packet::add_packet_to_psbt;
use ark_core::introspector::packet::IntrospectorEntry;
use ark_core::introspector::packet::Packet;
use ark_core::introspector::prev_tx::set_prev_ark_tx;
use ark_core::script::extract_checksig_pubkeys;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::OffchainTransactions;
use ark_core::send::SendReceiver;
use ark_core::send::VtxoInput;
use ark_core::server::GetVtxosRequest;
use ark_core::server::VirtualTxOutPoint;
use ark_core::unilateral_exit::create_contract_exit_transaction;
use ark_core::unilateral_exit::ContractExitInput;
use ark_core::ArkAddress;
use ark_core::VTXO_CONDITION_KEY;
use ark_introspector_client::IntrospectorClient;
use bitcoin::absolute;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::relative;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use bitcoin::Sequence;
use bitcoin::Transaction;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use rand::CryptoRng;
use rand::Rng;
use std::collections::HashMap;

/// Signs for contract keys the wallet does not hold, such as a VHTLC counterparty's.
///
/// Only used for offchain spends; a batch spend signs with the wallet's keys alone.
pub trait ExternalSigner: Send + Sync {
    /// The keys this signer can sign for.
    fn public_keys(&self) -> Vec<XOnlyPublicKey>;

    /// A BIP-340 signature over `msg` with the key behind `pk`.
    fn sign(
        &self,
        pk: &XOnlyPublicKey,
        msg: &secp256k1::Message,
    ) -> Result<schnorr::Signature, Error>;
}

/// What the spender brings to a contract spend, on top of the wallet's keys.
#[derive(Default, Clone, Copy)]
pub struct ContractSpendOptions<'a> {
    /// The preimage for preimage-gated leaves, such as a VHTLC claim.
    pub preimage: Option<[u8; 32]>,
    /// The leaf to spend through. `None` lets the contract pick one.
    pub path: Option<&'a SpendPathKind>,
    /// Signs for the contract keys the wallet does not hold. Offchain spends only.
    pub cosigner: Option<&'a dyn ExternalSigner>,
    /// The emulator, needed to spend through a covenant leaf. Offchain spends only.
    pub introspector: Option<&'a IntrospectorClient>,
    /// The chain tip height, so a height-typed CLTV that has not matured is refused locally.
    pub block_height: Option<u32>,
}

struct ContractInput {
    vtxo: VirtualTxOutPoint,
    stored: StoredContract,
    plan: ContractSpendPlan,
}

impl<B, W> Client<B, W>
where
    B: Blockchain,
    W: OnchainWallet,
{
    /// Track a contract, so that its outputs can be spent with [`Self::send_contract_vtxos`] and
    /// [`Self::settle_contract_vtxos`].
    ///
    /// Inserting a contract that is already tracked returns the stored one.
    pub fn add_contract<T: ContractSpec>(&self, contract: T) -> Result<StoredContract, Error> {
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let mut manager = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?;
        manager.insert_or_get(contract, ContractState::Active, None)
    }

    /// Spend the named outputs of tracked contracts offchain, draining them to `to_address`.
    ///
    /// Each output is spent through the leaf its contract's handler picks from `options` and
    /// the keys the wallet holds, or through [`ContractSpendOptions::path`]. A covenant leaf is
    /// submitted through [`ContractSpendOptions::introspector`]; it pins where the funds go, so
    /// `to_address` must be that destination, and only one output can be spent at a time.
    ///
    /// Swept outputs can't be spent offchain: use [`Self::settle_contract_vtxos`].
    pub async fn send_contract_vtxos(
        &self,
        outpoints: &[OutPoint],
        to_address: ArkAddress,
        options: ContractSpendOptions<'_>,
    ) -> Result<Txid, Error> {
        let server_info = self.server_info().await?;
        let inputs = self
            .resolve_contract_inputs(outpoints, &options)
            .await
            .context("failed to resolve contract inputs")?;

        if let Some(swept) = inputs.iter().find(|input| input.vtxo.is_swept) {
            return Err(Error::ad_hoc(format!(
                "VTXO {} is swept and can only be settled in a batch",
                swept.vtxo.outpoint
            )));
        }

        let covenant = inputs
            .iter()
            .filter_map(|input| input.plan.selection.arkade_script.clone())
            .collect::<Vec<_>>();
        if !covenant.is_empty() && inputs.len() > 1 {
            return Err(Error::ad_hoc(
                "a covenant leaf can only be spent on its own, one VTXO per transaction",
            ));
        }

        let vtxo_inputs = inputs
            .iter()
            .map(|input| {
                VtxoInput::new_with_spend_selection(
                    input.plan.selection.clone(),
                    input.plan.tapscripts.clone(),
                    input.vtxo.script.clone(),
                    input.vtxo.amount,
                    input.vtxo.outpoint,
                    input.vtxo.assets.clone(),
                )
            })
            .collect::<Vec<_>>();

        let amount = inputs
            .iter()
            .fold(Amount::ZERO, |acc, input| acc + input.vtxo.amount);
        let receivers = [SendReceiver::bitcoin(to_address, amount)];

        let OffchainTransactions {
            mut ark_tx,
            mut checkpoint_txs,
        } = build_offchain_transactions(&receivers, &to_address, &vtxo_inputs, &server_info)
            .map_err(Error::from)
            .context("failed to build offchain transactions")?;

        // Keyed by leaf script: outputs of the same contract share a leaf, and so its witness.
        let mut extra_witnesses = HashMap::<ScriptBuf, Vec<Vec<u8>>>::new();
        for input in &inputs {
            let selection = &input.plan.selection;
            if selection.extra_witness.is_empty() {
                continue;
            }
            match extra_witnesses.get(&selection.path.script) {
                Some(existing) if *existing != selection.extra_witness => {
                    return Err(Error::ad_hoc(
                        "two inputs spend the same leaf with different condition witnesses",
                    ));
                }
                Some(_) => {}
                None => {
                    extra_witnesses.insert(
                        selection.path.script.clone(),
                        selection.extra_witness.clone(),
                    );
                }
            }
        }
        let secp = Secp256k1::new();
        let sign_fn = |psbt_input: &mut psbt::Input, msg: secp256k1::Message| {
            self.sign_contract_input(psbt_input, msg, &extra_witnesses, options.cosigner, &secp)
        };

        let ark_txid = if let Some(arkade_script) = covenant.into_iter().next() {
            let introspector = options.introspector.ok_or_else(|| {
                Error::ad_hoc("spending a covenant leaf needs the emulator (introspector)")
            })?;

            // The emulator evaluates the arkade script against the Ark tx, and needs the Ark tx
            // that created the coin it spends.
            let funding_txid = inputs[0].vtxo.outpoint.txid;
            let funding_tx = timeout_op(
                self.inner.timeout,
                self.network_client()
                    .get_virtual_txs(vec![funding_txid.to_string()], None, None),
            )
            .await
            .context("failed to fetch the Ark tx that created the VTXO")??
            .txs
            .into_iter()
            .map(|psbt| psbt.unsigned_tx)
            .find(|tx| tx.compute_txid() == funding_txid)
            .ok_or_else(|| Error::ad_hoc(format!("Ark tx {funding_txid} not found")))?;

            set_prev_ark_tx(&mut ark_tx, 0, &funding_tx).map_err(Error::from)?;
            let packet = Packet::new(vec![IntrospectorEntry {
                vin: 0,
                script: arkade_script,
                witness: bitcoin::Witness::default(),
            }])
            .map_err(|e| Error::ad_hoc(format!("invalid introspector packet: {e}")))?;
            add_packet_to_psbt(&mut ark_tx, &packet)
                .map_err(|e| Error::ad_hoc(format!("failed to attach introspector packet: {e}")))?;

            sign_ark_transaction(sign_fn, &mut ark_tx, 0)
                .map_err(Error::from)
                .context("failed to sign Ark transaction")?;
            for checkpoint in checkpoint_txs.iter_mut() {
                sign_checkpoint_transaction(sign_fn, checkpoint)
                    .map_err(Error::from)
                    .context("failed to sign checkpoint transaction")?;
            }

            let response = timeout_op(
                self.inner.timeout,
                introspector.submit_tx(&ark_tx, &checkpoint_txs),
            )
            .await
            .context("failed to submit to the emulator")?
            .map_err(|e| Error::ad_hoc(format!("emulator refused the transaction: {e}")))?;

            response.signed_ark_tx.unsigned_tx.compute_txid()
        } else {
            for i in 0..vtxo_inputs.len() {
                sign_ark_transaction(sign_fn, &mut ark_tx, i)
                    .map_err(Error::from)
                    .context("failed to sign Ark transaction")?;
            }
            let ark_txid = ark_tx.unsigned_tx.compute_txid();

            let response = timeout_op(
                self.inner.timeout,
                self.network_client()
                    .submit_offchain_transaction_request(ark_tx, checkpoint_txs),
            )
            .await
            .context("failed to submit offchain transaction")?
            .map_err(Error::ark_server)
            .context("failed to submit offchain transaction")?;

            let mut signed_checkpoint_txs = response.signed_checkpoint_txs;
            for checkpoint in signed_checkpoint_txs.iter_mut() {
                sign_checkpoint_transaction(sign_fn, checkpoint)
                    .map_err(Error::from)
                    .context("failed to sign checkpoint transaction")?;
            }

            timeout_op(
                self.inner.timeout,
                self.network_client()
                    .finalize_offchain_transaction(ark_txid, signed_checkpoint_txs),
            )
            .await
            .context("failed to finalize offchain transaction")?
            .map_err(Error::ark_server)
            .context("failed to finalize offchain transaction")?;

            ark_txid
        };

        tracing::info!(%ark_txid, ?outpoints, "Spent contract VTXOs offchain");

        Ok(ark_txid)
    }

    /// Settle the named outputs of tracked contracts in the next batch, draining them to
    /// `to_address`.
    ///
    /// This is the only way to spend a swept output. Leaves are picked as in
    /// [`Self::send_contract_vtxos`], but only the wallet's own keys sign: a leaf that needs
    /// another signature, or a covenant leaf, is refused.
    pub async fn settle_contract_vtxos<R>(
        &self,
        rng: &mut R,
        outpoints: &[OutPoint],
        to_address: ArkAddress,
        options: ContractSpendOptions<'_>,
    ) -> Result<Txid, Error>
    where
        R: Rng + CryptoRng,
    {
        if options.cosigner.is_some() || options.introspector.is_some() {
            return Err(Error::ad_hoc(
                "a batch spend signs with the wallet's keys only: no co-signer or emulator",
            ));
        }

        let server_info = self.server_info().await?;
        let inputs = self
            .resolve_contract_inputs(outpoints, &options)
            .await
            .context("failed to resolve contract inputs")?;

        if inputs
            .iter()
            .any(|input| input.plan.selection.arkade_script.is_some())
        {
            return Err(Error::ad_hoc(
                "a covenant leaf can only be spent offchain, through the emulator",
            ));
        }

        let intent_inputs = inputs
            .into_iter()
            .map(|input| {
                let fee_params = intent_fee::offchain_input_fee_params(&input.vtxo);
                let intent_input = intent::Input::new_with_spend_selection(
                    input.vtxo.outpoint,
                    // Not final, so that a CLTV leaf's locktime applies to the intent proof.
                    Sequence::ENABLE_LOCKTIME_NO_RBF,
                    TxOut {
                        value: input.vtxo.amount,
                        script_pubkey: input.stored.script_pubkey.clone(),
                    },
                    input.plan.tapscripts,
                    input.plan.selection,
                    false,
                    input.vtxo.is_swept,
                    input.vtxo.assets,
                );
                (intent_input, fee_params)
            })
            .collect::<Vec<_>>();

        // The operator's intent fees come out of the settled output.
        let mut priced = self.price_inputs(Vec::new(), intent_inputs, None)?;
        priced.retain_economic(UneconomicInputs::Reject)?;
        let to_amount =
            self.settled_output_amount(priced.net_total(), &to_address, server_info.dust, None)?;
        let (_, intent_inputs) = priced.into_inputs();

        let commitment_txid = self
            .join_next_batch(
                rng,
                &server_info,
                Vec::new(),
                intent_inputs,
                BatchOutputType::Board {
                    to_address,
                    to_amount,
                },
            )
            .await
            .context("failed to join batch")?;

        tracing::info!(%commitment_txid, ?outpoints, "Settled contract VTXOs");

        Ok(commitment_txid)
    }

    async fn resolve_contract_inputs(
        &self,
        outpoints: &[OutPoint],
        options: &ContractSpendOptions<'_>,
    ) -> Result<Vec<ContractInput>, Error> {
        check_outpoints(outpoints)?;

        let vtxos = self
            .fetch_all_vtxos(GetVtxosRequest::new_for_outpoints(outpoints))
            .await
            .context("failed to fetch VTXOs")?;

        let now = u64::try_from(unix_now()?)
            .map_err(|_| Error::ad_hoc("system time is before the Unix epoch"))?;
        let cosigner_keys = options
            .cosigner
            .map(ExternalSigner::public_keys)
            .unwrap_or_default();
        let can_sign =
            |key: &XOnlyPublicKey| cosigner_keys.contains(key) || self.keypair_by_pk(key).is_ok();

        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let manager = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?;

        outpoints
            .iter()
            .map(|outpoint| {
                let vtxo = vtxos
                    .iter()
                    .find(|vtxo| vtxo.outpoint == *outpoint)
                    .cloned()
                    .ok_or_else(|| Error::ad_hoc(format!("VTXO {outpoint} not found")))?;
                if vtxo.is_spent {
                    return Err(Error::ad_hoc(format!("VTXO {outpoint} is already spent")));
                }
                if vtxo.is_unrolled {
                    return Err(Error::ad_hoc(format!(
                        "VTXO {outpoint} is unrolled: spend it on-chain with \
                         `send_contract_on_chain`"
                    )));
                }
                reject_assets(&vtxo)?;

                let stored = manager.get_by_script(&vtxo.script)?.ok_or_else(|| {
                    Error::ad_hoc(format!(
                        "VTXO {outpoint} does not belong to a tracked contract"
                    ))
                })?;

                let mut path_ctx = PathContext::collaborative(now);
                path_ctx.block_height = options.block_height;
                path_ctx.preimage = options.preimage;
                path_ctx.path = options.path.cloned();

                let plan = manager
                    .contract_spend(&stored, &can_sign, path_ctx)
                    .with_context(|| format!("cannot spend VTXO {outpoint}"))?;

                Ok(ContractInput { vtxo, stored, plan })
            })
            .collect()
    }

    /// Build the unilateral exit branches that put the named outputs of tracked contracts
    /// on-chain.
    ///
    /// Broadcast each branch with [`Self::broadcast_next_unilateral_exit_node`]. Once the outputs
    /// are confirmed and their exit leaf has matured, drain them with
    /// [`Self::send_contract_on_chain`].
    pub async fn build_contract_exit_trees(
        &self,
        outpoints: &[OutPoint],
    ) -> Result<Vec<Vec<Transaction>>, Error> {
        check_outpoints(outpoints)?;

        let vtxos = self
            .fetch_all_vtxos(GetVtxosRequest::new_for_outpoints(outpoints))
            .await
            .context("failed to fetch VTXOs")?;

        let vtxos = outpoints
            .iter()
            .map(|outpoint| {
                let vtxo = vtxos
                    .iter()
                    .find(|vtxo| vtxo.outpoint == *outpoint)
                    .cloned()
                    .ok_or_else(|| Error::ad_hoc(format!("VTXO {outpoint} not found")))?;
                if vtxo.is_spent {
                    return Err(Error::ad_hoc(format!("VTXO {outpoint} is already spent")));
                }
                reject_assets(&vtxo)?;
                if self.tracked_contract(&vtxo.script)?.is_none() {
                    return Err(Error::ad_hoc(format!(
                        "VTXO {outpoint} does not belong to a tracked contract"
                    )));
                }
                Ok(vtxo)
            })
            .collect::<Result<Vec<_>, Error>>()?;

        self.build_exit_branches(&vtxos).await
    }

    /// Drain unrolled outputs of tracked contracts to `to_address` on-chain, paying `fee`.
    ///
    /// Each output is spent through the unilateral leaf its contract's handler picks, or through
    /// [`ContractSpendOptions::path`]. The outputs must already be confirmed on-chain (see
    /// [`Self::build_contract_exit_trees`]) and the leaf's CSV must have matured. Unlike a batch
    /// spend, a co-signer can sign here, such as the receiver in a VHTLC's `unilateral_refund`.
    ///
    /// A time-based CSV or CLTV is checked against the wall clock, but the chain measures both in
    /// median time past, which lags behind; the node can still refuse the spend for a while after
    /// this check passes. A height-based CLTV is only checked when
    /// [`ContractSpendOptions::block_height`] is set; without it, an immature one is left for the
    /// node to refuse.
    pub async fn send_contract_on_chain(
        &self,
        outpoints: &[OutPoint],
        to_address: Address,
        fee: Amount,
        options: ContractSpendOptions<'_>,
    ) -> Result<Txid, Error> {
        let (tx, _) = self
            .create_contract_on_chain_transaction_inner(outpoints, to_address, fee, options)
            .await?;

        let txid = tx.compute_txid();
        tracing::info!(%txid, "Broadcasting transaction draining contract outputs on-chain");

        timeout_op(self.inner.timeout, self.blockchain().broadcast(&tx))
            .await
            .with_context(|| format!("failed to broadcast transaction {txid}"))??;

        Ok(txid)
    }

    /// Build the transaction [`Self::send_contract_on_chain`] would broadcast, with its
    /// prevouts.
    #[cfg(feature = "test-utils")]
    pub async fn create_contract_on_chain_transaction(
        &self,
        outpoints: &[OutPoint],
        to_address: Address,
        fee: Amount,
        options: ContractSpendOptions<'_>,
    ) -> Result<(Transaction, Vec<TxOut>), Error> {
        self.create_contract_on_chain_transaction_inner(outpoints, to_address, fee, options)
            .await
    }

    async fn create_contract_on_chain_transaction_inner(
        &self,
        outpoints: &[OutPoint],
        to_address: Address,
        fee: Amount,
        options: ContractSpendOptions<'_>,
    ) -> Result<(Transaction, Vec<TxOut>), Error> {
        check_outpoints(outpoints)?;
        if options.introspector.is_some() {
            return Err(Error::ad_hoc(
                "an on-chain spend can't go through the emulator",
            ));
        }

        let network = self.server_info().await?.network;
        let now = u64::try_from(unix_now()?)
            .map_err(|_| Error::ad_hoc("system time is before the Unix epoch"))?;

        // The server still knows unrolled VTXOs; we only ask it whether they carry assets. This is
        // best effort: an exit must not depend on the server, so an output it no longer reports is
        // not refused here.
        let vtxos = self
            .fetch_all_vtxos(GetVtxosRequest::new_for_outpoints(outpoints))
            .await
            .context("failed to fetch VTXOs")?;
        for vtxo in &vtxos {
            reject_assets(vtxo)?;
        }

        let cosigner_keys = options
            .cosigner
            .map(ExternalSigner::public_keys)
            .unwrap_or_default();
        let can_sign =
            |key: &XOnlyPublicKey| cosigner_keys.contains(key) || self.keypair_by_pk(key).is_ok();

        let mut inputs = Vec::new();
        for outpoint in outpoints {
            let tx = timeout_op(
                self.inner.timeout,
                self.blockchain().find_tx(&outpoint.txid),
            )
            .await??
            .ok_or_else(|| {
                Error::ad_hoc(format!(
                    "VTXO {outpoint} is not on-chain yet: broadcast its exit tree first"
                ))
            })?;
            let witness_utxo = usize::try_from(outpoint.vout)
                .ok()
                .and_then(|vout| tx.output.get(vout))
                .cloned()
                .ok_or_else(|| Error::ad_hoc(format!("no output {outpoint}")))?;

            let stored = self
                .tracked_contract(&witness_utxo.script_pubkey)?
                .ok_or_else(|| {
                    Error::ad_hoc(format!(
                        "VTXO {outpoint} does not belong to a tracked contract"
                    ))
                })?;

            let mut path_ctx = PathContext::unilateral(now);
            path_ctx.block_height = options.block_height;
            path_ctx.preimage = options.preimage;
            path_ctx.path = options.path.cloned();

            let plan = {
                let state = self
                    .state
                    .read()
                    .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
                let manager = state
                    .contract_manager
                    .lock()
                    .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?;
                manager
                    .contract_spend(&stored, &can_sign, path_ctx)
                    .with_context(|| format!("cannot spend VTXO {outpoint}"))?
            };
            if plan.selection.arkade_script.is_some() {
                return Err(Error::ad_hoc(format!(
                    "VTXO {outpoint}: a covenant leaf can't be spent on-chain"
                )));
            }

            let address = Address::from_script(&witness_utxo.script_pubkey, network)
                .map_err(Error::ad_hoc)?;
            let utxo = timeout_op(
                self.inner.timeout,
                self.blockchain().find_outpoints(&address),
            )
            .await??
            .into_iter()
            .find(|utxo| utxo.outpoint == *outpoint)
            .ok_or_else(|| Error::ad_hoc(format!("VTXO {outpoint} is not on-chain yet")))?;
            if utxo.is_spent {
                return Err(Error::ad_hoc(format!("VTXO {outpoint} is already spent")));
            }
            let confirmed_at = utxo
                .confirmation_blocktime
                .ok_or_else(|| Error::ad_hoc(format!("VTXO {outpoint} is not confirmed yet")))?;

            if let Some(csv) = plan
                .selection
                .sequence
                .and_then(|sequence| sequence.to_relative_lock_time())
            {
                match csv {
                    relative::LockTime::Blocks(blocks) => {
                        if utxo.confirmations < u64::from(blocks.value()) {
                            return Err(Error::ad_hoc(format!(
                                "VTXO {outpoint}: the exit leaf opens after {} confirmations, \
                                 it has {}",
                                blocks.value(),
                                utxo.confirmations
                            )));
                        }
                    }
                    relative::LockTime::Time(time) => {
                        let opens_at = confirmed_at + u64::from(time.value()) * 512;
                        if now <= opens_at {
                            return Err(Error::ad_hoc(format!(
                                "VTXO {outpoint}: the exit leaf opens at Unix time {opens_at}"
                            )));
                        }
                    }
                }
            }

            if let Some(locktime) = plan
                .selection
                .locktime
                .filter(|locktime| *locktime != absolute::LockTime::ZERO)
            {
                let mature = match locktime {
                    absolute::LockTime::Seconds(time) => {
                        Some(now > u64::from(time.to_consensus_u32()))
                    }
                    absolute::LockTime::Blocks(height) => options
                        .block_height
                        .map(|tip| tip >= height.to_consensus_u32()),
                };
                if mature == Some(false) {
                    return Err(Error::ad_hoc(format!(
                        "VTXO {outpoint}: the leaf's locktime {locktime} has not passed yet"
                    )));
                }
            }

            inputs.push(ContractExitInput::new(
                *outpoint,
                witness_utxo,
                plan.selection,
            ));
        }

        let no_extra_witness = HashMap::new();
        let secp = Secp256k1::new();
        let tx = create_contract_exit_transaction(to_address, fee, &inputs, |input, msg| {
            self.sign_contract_input(input, msg, &no_extra_witness, options.cosigner, &secp)
        })
        .map_err(Error::from)?;

        let prevouts = inputs
            .iter()
            .map(ContractExitInput::previous_output)
            .collect();

        Ok((tx, prevouts))
    }

    fn tracked_contract(&self, script: &ScriptBuf) -> Result<Option<StoredContract>, Error> {
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let manager = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?;
        manager.get_by_script(script)
    }

    /// Sign a contract input with the wallet's keys and the co-signer's, after attaching the
    /// leaf's condition witness (such as a preimage).
    fn sign_contract_input(
        &self,
        input: &mut psbt::Input,
        msg: secp256k1::Message,
        extra_witnesses: &HashMap<ScriptBuf, Vec<Vec<u8>>>,
        cosigner: Option<&dyn ExternalSigner>,
        secp: &Secp256k1<secp256k1::All>,
    ) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
        let script = input
            .witness_script
            .clone()
            .ok_or_else(|| ark_core::Error::ad_hoc("missing witness script for psbt::Input"))?;

        if let Some(extra_witness) = extra_witnesses.get(&script) {
            input.unknown.insert(
                psbt::raw::Key {
                    type_value: 222,
                    key: VTXO_CONDITION_KEY.to_vec(),
                },
                intent::encode_witness(extra_witness),
            );
        }

        let cosigner_keys = cosigner.map(|c| c.public_keys()).unwrap_or_default();
        let mut sigs = Vec::new();
        for pk in extract_checksig_pubkeys(&script) {
            if let Ok(keypair) = self.keypair_by_pk(&pk) {
                sigs.push((secp.sign_schnorr_no_aux_rand(&msg, &keypair), pk));
            } else if let Some(cosigner) = cosigner.filter(|_| cosigner_keys.contains(&pk)) {
                let sig = cosigner
                    .sign(&pk, &msg)
                    .map_err(|e| ark_core::Error::ad_hoc(format!("co-signer failed: {e}")))?;
                sigs.push((sig, pk));
            }
        }

        Ok(sigs)
    }
}

/// Contract spends pay out plain bitcoin, so an output carrying assets would lose them.
fn reject_assets(vtxo: &VirtualTxOutPoint) -> Result<(), Error> {
    if vtxo.assets.is_empty() {
        Ok(())
    } else {
        Err(Error::ad_hoc(format!(
            "VTXO {} carries assets, which contract spends don't support yet",
            vtxo.outpoint
        )))
    }
}

fn check_outpoints(outpoints: &[OutPoint]) -> Result<(), Error> {
    if outpoints.is_empty() {
        return Err(Error::ad_hoc("no outpoints to spend"));
    }
    let mut seen = std::collections::HashSet::new();
    if let Some(duplicate) = outpoints.iter().find(|outpoint| !seen.insert(**outpoint)) {
        return Err(Error::ad_hoc(format!("duplicate outpoint {duplicate}")));
    }
    Ok(())
}

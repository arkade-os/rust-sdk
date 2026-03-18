use crate::error::ErrorContext;
use crate::swap_storage::SwapStorage;
use crate::utils::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::coin_select::select_vtxos;
use ark_core::intent;
use ark_core::script::extract_checksig_pubkeys;
use ark_core::send;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::OffchainTransactions;
use ark_core::server::PendingTx;
use ark_core::ArkAddress;
use ark_core::ErrorContext as _;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;

impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: crate::KeyProvider,
{
    // ── High-level send (submit + finalize) ────────────────────────────

    /// Spend confirmed and pre-confirmed VTXOs in an Ark transaction sending the given `amount` to
    /// the given `address`.
    ///
    /// The Ark transaction is built in collaboration with the Ark server. The outputs of said
    /// transaction will be pre-confirmed VTXOs.
    ///
    /// Coin selection is performed automatically to choose which VTXOs to spend.
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the generated Ark transaction.
    pub async fn send_vtxo(&self, address: ArkAddress, amount: Amount) -> Result<Txid, Error> {
        let vtxo_inputs = self.coin_select_vtxo_inputs(amount).await?;
        let pending_tx = self
            .submit_offchain_send(vtxo_inputs, address, amount)
            .await?;
        let ark_txid = pending_tx.ark_txid;
        self.sign_and_finalize_pending_tx(pending_tx).await?;
        Ok(ark_txid)
    }

    /// Spend specific VTXOs in an Ark transaction sending the given `amount` to the given
    /// `address`.
    ///
    /// The Ark transaction is built in collaboration with the Ark server. The outputs of said
    /// transaction will be pre-confirmed VTXOs.
    ///
    /// Unlike [`Self::send_vtxo`], this method allows the caller to specify exactly which VTXOs
    /// to spend by providing their outpoints. This is useful for applications that want to have
    /// full control over VTXO selection.
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the generated Ark transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected VTXOs don't have enough value to cover the requested
    /// amount.
    pub async fn send_vtxo_selection(
        &self,
        vtxo_outpoints: &[OutPoint],
        address: ArkAddress,
        amount: Amount,
    ) -> Result<Txid, Error> {
        let (vtxo_inputs, total_amount) =
            self.select_vtxo_inputs_with_total(vtxo_outpoints).await?;

        if total_amount < amount {
            return Err(Error::coin_select(format!(
                "insufficient VTXO amount: {} < {}",
                total_amount, amount
            )));
        }

        let pending_tx = self
            .submit_offchain_send(vtxo_inputs, address, amount)
            .await?;
        let ark_txid = pending_tx.ark_txid;
        self.sign_and_finalize_pending_tx(pending_tx).await?;
        Ok(ark_txid)
    }

    // ── Submit-only (no finalize) ──────────────────────────────────────

    /// Submit an offchain transaction sending `amount` to `address` without finalizing.
    ///
    /// Coin selection is performed automatically. The transaction stays pending on the server
    /// until [`Self::finalize_pending_offchain_tx`] or
    /// [`Self::continue_pending_offchain_txs`] completes it.
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the submitted Ark transaction.
    pub async fn submit_vtxo_send(
        &self,
        address: ArkAddress,
        amount: Amount,
    ) -> Result<Txid, Error> {
        let vtxo_inputs = self.coin_select_vtxo_inputs(amount).await?;
        let pending_tx = self
            .submit_offchain_send(vtxo_inputs, address, amount)
            .await?;
        Ok(pending_tx.ark_txid)
    }

    /// Build, sign and submit an offchain transaction to the server without finalizing.
    ///
    /// This is primarily useful for testing pending transaction recovery flows.
    ///
    /// Returns the Ark txid. The transaction will remain in a pending state on the server
    /// until [`Self::finalize_pending_offchain_tx`] or
    /// [`Self::continue_pending_offchain_txs`] completes it.
    #[cfg(feature = "test-utils")]
    pub async fn submit_offchain_tx(
        &self,
        vtxo_inputs: Vec<send::VtxoInput>,
        address: ArkAddress,
        amount: Amount,
    ) -> Result<Txid, Error> {
        let pending_tx = self
            .submit_offchain_send(vtxo_inputs, address, amount)
            .await?;
        Ok(pending_tx.ark_txid)
    }

    // ── Finalize pending ───────────────────────────────────────────────

    /// Finalize a specific pending offchain transaction.
    ///
    /// Fetches the pending transaction identified by `ark_txid` from the server, signs the
    /// checkpoint transactions, and finalizes.
    ///
    /// This is useful when you need fine-grained control over which pending transaction to
    /// finalize (e.g. when a database tracks individual pending funding attempts).
    ///
    /// # Errors
    ///
    /// Returns an error if no pending transaction with the given `ark_txid` is found, or if
    /// signing / finalization fails.
    pub async fn finalize_pending_offchain_tx(&self, ark_txid: Txid) -> Result<(), Error> {
        let pending_txs = self.fetch_pending_offchain_txs().await?;

        let pending_tx = pending_txs
            .into_iter()
            .find(|tx| tx.ark_txid == ark_txid)
            .ok_or_else(|| {
                Error::ad_hoc(format!(
                    "no pending transaction found for ark txid {ark_txid}"
                ))
            })?;

        self.sign_and_finalize_pending_tx(pending_tx).await
    }

    /// Resume and finalize any pending (submitted but not finalized) offchain transactions.
    ///
    /// This handles the case where `send_vtxo` successfully submitted the transaction to the
    /// server but failed before finalizing (e.g. due to a crash or network error). The server
    /// holds the submitted-but-not-finalized transaction in a pending state. This method
    /// retrieves it, signs the checkpoint transactions, and finalizes.
    ///
    /// # Returns
    ///
    /// The [`Txid`]s of the finalized Ark transactions, or an empty vec if there were no
    /// pending transactions.
    pub async fn continue_pending_offchain_txs(&self) -> Result<Vec<Txid>, Error> {
        let pending_txs = self.fetch_pending_offchain_txs().await?;

        if pending_txs.is_empty() {
            return Ok(vec![]);
        }

        let mut finalized_txids = Vec::new();

        for pending_tx in pending_txs {
            let ark_txid = pending_tx.ark_txid;
            self.sign_and_finalize_pending_tx(pending_tx).await?;
            finalized_txids.push(ark_txid);
        }

        Ok(finalized_txids)
    }

    /// List pending (submitted but not finalized) offchain transactions.
    ///
    /// This retrieves any transactions that were submitted to the server but not yet finalized
    /// (e.g. due to a crash or network error between submit and finalize).
    ///
    /// # Returns
    ///
    /// The pending transactions, or an empty vec if there are none.
    pub async fn list_pending_offchain_txs(&self) -> Result<Vec<PendingTx>, Error> {
        self.fetch_pending_offchain_txs().await
    }

    // ── Private helpers ────────────────────────────────────────────────

    /// Create a signing closure that signs with any known keypair.
    fn make_sign_fn(
        &self,
    ) -> impl FnMut(
        &mut psbt::Input,
        secp256k1::Message,
    ) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error>
           + '_ {
        |input, msg| {
            let script = input
                .witness_script
                .as_ref()
                .ok_or_else(|| ark_core::Error::ad_hoc("Missing witness script for psbt::Input"))?;
            let pks = extract_checksig_pubkeys(script);
            let secp = Secp256k1::new();
            let mut sigs = vec![];
            for pk in pks {
                if let Ok(keypair) = self.keypair_by_pk(&pk) {
                    let sig = secp.sign_schnorr_no_aux_rand(&msg, &keypair);
                    sigs.push((sig, keypair.x_only_public_key().0));
                }
            }
            Ok(sigs)
        }
    }

    /// Run automatic coin selection and build [`send::VtxoInput`]s.
    async fn coin_select_vtxo_inputs(&self, amount: Amount) -> Result<Vec<send::VtxoInput>, Error> {
        let (vtxo_list, script_pubkey_to_vtxo_map) = self
            .list_vtxos()
            .await
            .context("failed to get spendable VTXOs")?;

        let spendable = vtxo_list
            .spendable_offchain()
            .map(|vtxo| ark_core::coin_select::VirtualTxOutPoint {
                outpoint: vtxo.outpoint,
                script_pubkey: vtxo.script.clone(),
                expire_at: vtxo.expires_at,
                amount: vtxo.amount,
            })
            .collect::<Vec<_>>();

        let selected = select_vtxos(spendable, amount, self.server_info().dust, true)
            .map_err(Error::from)
            .context("failed to select coins")?;

        self.build_vtxo_inputs(selected, &script_pubkey_to_vtxo_map)
    }

    /// Filter VTXOs by outpoints and build [`send::VtxoInput`]s, returning the total amount.
    async fn select_vtxo_inputs_with_total(
        &self,
        vtxo_outpoints: &[OutPoint],
    ) -> Result<(Vec<send::VtxoInput>, Amount), Error> {
        let (vtxo_list, script_pubkey_to_vtxo_map) =
            self.list_vtxos().await.context("failed to get VTXO list")?;

        let selected: Vec<_> = vtxo_list
            .spendable_offchain()
            .filter(|vtxo| vtxo_outpoints.contains(&vtxo.outpoint))
            .map(|vtxo| ark_core::coin_select::VirtualTxOutPoint {
                outpoint: vtxo.outpoint,
                script_pubkey: vtxo.script.clone(),
                expire_at: vtxo.expires_at,
                amount: vtxo.amount,
            })
            .collect();

        if selected.is_empty() {
            return Err(Error::ad_hoc("no matching VTXO outpoints found"));
        }

        let total = selected.iter().fold(Amount::ZERO, |acc, v| acc + v.amount);
        let inputs = self.build_vtxo_inputs(selected, &script_pubkey_to_vtxo_map)?;
        Ok((inputs, total))
    }

    /// Convert selected [`VirtualTxOutPoint`]s into [`send::VtxoInput`]s.
    fn build_vtxo_inputs(
        &self,
        selected: Vec<ark_core::coin_select::VirtualTxOutPoint>,
        script_pubkey_to_vtxo_map: &std::collections::HashMap<bitcoin::ScriptBuf, ark_core::Vtxo>,
    ) -> Result<Vec<send::VtxoInput>, Error> {
        selected
            .into_iter()
            .map(|vtp| {
                let vtxo = script_pubkey_to_vtxo_map
                    .get(&vtp.script_pubkey)
                    .ok_or_else(|| {
                        ark_core::Error::ad_hoc(format!(
                            "missing VTXO for script pubkey: {}",
                            vtp.script_pubkey
                        ))
                    })?;

                let (forfeit_script, control_block) = vtxo
                    .forfeit_spend_info()
                    .context("failed to get forfeit spend info")?;

                Ok(send::VtxoInput::new(
                    forfeit_script,
                    None,
                    control_block,
                    vtxo.tapscripts(),
                    vtxo.script_pubkey(),
                    vtp.amount,
                    vtp.outpoint,
                ))
            })
            .collect()
    }

    /// Build, sign the Ark transaction, and submit to the server *without* finalizing.
    ///
    /// Returns the pending transaction payload from the server. The change-address key is marked
    /// as used.
    async fn submit_offchain_send(
        &self,
        vtxo_inputs: Vec<send::VtxoInput>,
        address: ArkAddress,
        amount: Amount,
    ) -> Result<PendingTx, Error> {
        let (change_address, change_address_vtxo) = self.get_offchain_address()?;

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &[(&address, amount)],
            Some(&change_address),
            &vtxo_inputs,
            &self.server_info(),
        )
        .map_err(Error::from)
        .context("failed to build offchain transactions")?;

        // Sign the Ark transaction (one signature per checkpoint input).
        for i in 0..checkpoint_txs.len() {
            sign_ark_transaction(self.make_sign_fn(), &mut ark_tx, i)?;
        }

        let res = self
            .network_client()
            .submit_offchain_transaction_request(ark_tx, checkpoint_txs)
            .await
            .map_err(Error::ark_server)
            .context("failed to submit offchain transaction request")?;

        let pending_tx = PendingTx {
            ark_txid: res.signed_ark_tx.unsigned_tx.compute_txid(),
            signed_ark_tx: res.signed_ark_tx,
            signed_checkpoint_txs: res.signed_checkpoint_txs,
        };

        // Mark the change-address key as used so future sends pick a new one.
        let used_pk = change_address_vtxo.owner_pk();
        if let Err(err) = self.inner.key_provider.mark_as_used(&used_pk) {
            tracing::warn!(
                "Failed updating keypair cache for used change address: {:?}",
                err
            );
        }

        Ok(pending_tx)
    }

    /// Sign checkpoint transactions from a [`PendingTx`] and finalize.
    async fn sign_and_finalize_pending_tx(&self, pending_tx: PendingTx) -> Result<(), Error> {
        let ark_txid = pending_tx.ark_txid;
        let mut signed_checkpoint_txs = pending_tx.signed_checkpoint_txs;

        // Build a map from checkpoint txid → ark tx input index so we can
        // restore witness scripts that the server may have stripped.
        let ark_input_idx_by_cp_txid: std::collections::HashMap<_, _> = pending_tx
            .signed_ark_tx
            .unsigned_tx
            .input
            .iter()
            .enumerate()
            .map(|(i, inp)| (inp.previous_output.txid, i))
            .collect();

        for checkpoint_psbt in signed_checkpoint_txs.iter_mut() {
            if checkpoint_psbt.inputs[0].witness_script.is_none() {
                let checkpoint_txid = checkpoint_psbt.unsigned_tx.compute_txid();
                let idx = ark_input_idx_by_cp_txid
                    .get(&checkpoint_txid)
                    .ok_or_else(|| {
                        Error::ad_hoc(format!(
                            "checkpoint txid {checkpoint_txid} not found in ark tx inputs \
                             for pending tx {ark_txid}"
                        ))
                    })?;

                let ws = pending_tx
                    .signed_ark_tx
                    .inputs
                    .get(*idx)
                    .and_then(|input| input.witness_script.clone())
                    .ok_or_else(|| {
                        Error::ad_hoc(format!(
                            "missing witness script on ark tx input {idx} \
                             for pending tx {ark_txid}"
                        ))
                    })?;

                checkpoint_psbt.inputs[0].witness_script = Some(ws);
            }

            sign_checkpoint_transaction(self.make_sign_fn(), checkpoint_psbt)?;
        }

        self.finalize_offchain_tx(ark_txid, signed_checkpoint_txs)
            .await
    }

    /// Finalize an offchain transaction.
    async fn finalize_offchain_tx(
        &self,
        ark_txid: Txid,
        signed_checkpoint_txs: Vec<bitcoin::Psbt>,
    ) -> Result<(), Error> {
        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, signed_checkpoint_txs),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize offchain transaction")
        .map(|_| ())
    }

    /// Fetch pending offchain transactions from the server.
    async fn fetch_pending_offchain_txs(&self) -> Result<Vec<PendingTx>, Error> {
        const MAX_INPUTS_PER_INTENT: usize = 20;

        let ark_addresses = self.get_offchain_addresses()?;

        let script_pubkey_to_vtxo_map: std::collections::HashMap<_, _> = ark_addresses
            .iter()
            .map(|(a, v)| (a.to_p2tr_script_pubkey(), v.clone()))
            .collect();

        // Use pending_only filter to only fetch VTXOs that are spent but not
        // finalized. This is much cheaper than fetching all VTXOs when there
        // are no pending transactions (common case).
        let addresses = ark_addresses.iter().map(|(a, _)| *a);
        let request = ark_core::server::GetVtxosRequest::new_for_addresses(addresses)
            .pending_only()
            .map_err(Error::from)?;

        let vtxos = self
            .fetch_all_vtxos(request)
            .await
            .context("failed to fetch pending VTXOs")?;

        tracing::debug!(num_pending_vtxos = vtxos.len(), "Fetched pending VTXOs");

        if vtxos.is_empty() {
            return Ok(vec![]);
        }

        let secp = Secp256k1::new();
        let mut all_pending_txs = Vec::new();
        let mut seen_ark_txids = std::collections::HashSet::new();

        // Batch inputs to avoid oversized intents.
        for (batch_idx, batch) in vtxos.chunks(MAX_INPUTS_PER_INTENT).enumerate() {
            let mut vtxo_inputs = Vec::new();
            for virtual_tx_outpoint in batch {
                let vtxo = match script_pubkey_to_vtxo_map.get(&virtual_tx_outpoint.script) {
                    Some(v) => v,
                    None => {
                        tracing::warn!(
                            outpoint = %virtual_tx_outpoint.outpoint,
                            script = %virtual_tx_outpoint.script,
                            "Skipping VTXO with unknown script"
                        );
                        continue;
                    }
                };
                let spend_info = vtxo
                    .forfeit_spend_info()
                    .context("failed to get forfeit spend info")?;

                vtxo_inputs.push(intent::Input::new(
                    virtual_tx_outpoint.outpoint,
                    vtxo.exit_delay(),
                    None,
                    TxOut {
                        value: virtual_tx_outpoint.amount,
                        script_pubkey: vtxo.script_pubkey(),
                    },
                    vtxo.tapscripts(),
                    spend_info,
                    false,
                    virtual_tx_outpoint.is_swept,
                ));
            }

            if vtxo_inputs.is_empty() {
                continue;
            }

            tracing::debug!(
                batch = batch_idx,
                num_inputs = vtxo_inputs.len(),
                "Querying server for pending txs"
            );

            // expire_at = 0: server does not enforce expiry for get-pending-tx intents.
            let message = intent::IntentMessage::GetPendingTx { expire_at: 0 };

            let sign_for_vtxo_fn = |input: &mut psbt::Input,
                                    msg: secp256k1::Message|
             -> Result<
                Vec<(schnorr::Signature, XOnlyPublicKey)>,
                ark_core::Error,
            > {
                match &input.witness_script {
                    None => Err(ark_core::Error::ad_hoc(
                        "Missing witness script in psbt::Input when signing get-pending-tx intent",
                    )),
                    Some(script) => {
                        let pks = extract_checksig_pubkeys(script);
                        let mut res = vec![];
                        for pk in &pks {
                            if let Ok(keypair) = self.keypair_by_pk(pk) {
                                let sig = secp.sign_schnorr_no_aux_rand(&msg, &keypair);
                                res.push((sig, keypair.x_only_public_key().0));
                            }
                        }
                        Ok(res)
                    }
                }
            };

            let sign_for_onchain_fn =
                |_: &mut psbt::Input,
                 _: secp256k1::Message|
                 -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
                    Err(ark_core::Error::ad_hoc(
                        "unexpected onchain input in get-pending-tx intent",
                    ))
                };

            let get_pending_intent = intent::make_intent(
                sign_for_vtxo_fn,
                sign_for_onchain_fn,
                vtxo_inputs,
                vec![],
                message,
            )?;

            let pending_txs = self
                .network_client()
                .get_pending_tx(get_pending_intent)
                .await
                .map_err(Error::ark_server)
                .context("failed to get pending transactions")?;

            tracing::debug!(
                batch = batch_idx,
                num_pending_txs = pending_txs.len(),
                "Server response for batch"
            );

            for tx in pending_txs {
                if seen_ark_txids.insert(tx.ark_txid) {
                    tracing::info!(
                        ark_txid = %tx.ark_txid,
                        "Found pending transaction"
                    );
                    all_pending_txs.push(tx);
                }
            }
        }

        tracing::info!(
            num_pending_txs = all_pending_txs.len(),
            "Total pending transactions found"
        );

        Ok(all_pending_txs)
    }
}

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
        let (vtxo_list, script_pubkey_to_vtxo_map) = self
            .list_vtxos()
            .await
            .context("failed to get spendable VTXOs")?;

        // Run coin selection algorithm on candidate spendable VTXOs.
        let spendable_virtual_tx_outpoints = vtxo_list
            .spendable_offchain()
            .map(|vtxo| ark_core::coin_select::VirtualTxOutPoint {
                outpoint: vtxo.outpoint,
                script_pubkey: vtxo.script.clone(),
                expire_at: vtxo.expires_at,
                amount: vtxo.amount,
            })
            .collect::<Vec<_>>();

        let selected_coins = select_vtxos(
            spendable_virtual_tx_outpoints,
            amount,
            self.server_info.dust,
            true,
        )
        .map_err(Error::from)
        .context("failed to select coins")?;

        let vtxo_inputs = selected_coins
            .into_iter()
            .map(|virtual_tx_outpoint| {
                let vtxo = script_pubkey_to_vtxo_map
                    .get(&virtual_tx_outpoint.script_pubkey)
                    .ok_or_else(|| {
                        ark_core::Error::ad_hoc(format!(
                            "missing VTXO for script pubkey: {}",
                            virtual_tx_outpoint.script_pubkey
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
                    virtual_tx_outpoint.amount,
                    virtual_tx_outpoint.outpoint,
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        self.build_and_sign_offchain_tx(vtxo_inputs, address, amount)
            .await
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
    /// # Arguments
    ///
    /// * `vtxo_outpoints` - The specific VTXO outpoints to spend
    /// * `address` - The destination Ark address
    /// * `amount` - The amount to send
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
        let (vtxo_list, script_pubkey_to_vtxo_map) =
            self.list_vtxos().await.context("failed to get VTXO list")?;

        // Get all spendable VTXOs for reference
        let all_spendable = vtxo_list
            .spendable_offchain()
            .map(|vtxo| ark_core::coin_select::VirtualTxOutPoint {
                outpoint: vtxo.outpoint,
                script_pubkey: vtxo.script.clone(),
                expire_at: vtxo.expires_at,
                amount: vtxo.amount,
            })
            .collect::<Vec<_>>();

        // Filter to only the specified outpoints
        let selected_coins: Vec<_> = all_spendable
            .into_iter()
            .filter(|vtxo| vtxo_outpoints.contains(&vtxo.outpoint))
            .collect();

        if selected_coins.is_empty() {
            return Err(Error::ad_hoc("no matching VTXO outpoints found"));
        }

        // Check that total amount is sufficient
        let total_amount = selected_coins
            .iter()
            .fold(Amount::ZERO, |acc, vtxo| acc + vtxo.amount);

        if total_amount < amount {
            return Err(Error::coin_select(format!(
                "insufficient VTXO amount: {} < {}",
                total_amount, amount
            )));
        }

        // Build VTXO inputs from selected coins
        let vtxo_inputs = selected_coins
            .into_iter()
            .map(|virtual_tx_outpoint| {
                let vtxo = script_pubkey_to_vtxo_map
                    .get(&virtual_tx_outpoint.script_pubkey)
                    .ok_or_else(|| {
                        ark_core::Error::ad_hoc(format!(
                            "missing VTXO for script pubkey: {}",
                            virtual_tx_outpoint.script_pubkey
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
                    virtual_tx_outpoint.amount,
                    virtual_tx_outpoint.outpoint,
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        self.build_and_sign_offchain_tx(vtxo_inputs, address, amount)
            .await
    }

    /// Build, sign and submit an offchain transaction to the server without finalizing.
    ///
    /// This is primarily useful for testing pending transaction recovery flows.
    ///
    /// Returns the Ark txid. The transaction will remain in a pending state on the server
    /// until [`Self::continue_pending_offchain_txs`] or a manual finalize call completes it.
    #[cfg(feature = "test-utils")]
    pub async fn submit_offchain_tx(
        &self,
        vtxo_inputs: Vec<send::VtxoInput>,
        address: ArkAddress,
        amount: Amount,
    ) -> Result<Txid, Error> {
        let (change_address, _) = self.get_offchain_address()?;

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &[(&address, amount)],
            Some(&change_address),
            &vtxo_inputs,
            &self.server_info,
        )
        .map_err(Error::from)
        .context("failed to build offchain transactions")?;

        for i in 0..checkpoint_txs.len() {
            let sign_fn = |input: &mut psbt::Input,
                           msg: secp256k1::Message|
             -> Result<
                Vec<(schnorr::Signature, XOnlyPublicKey)>,
                ark_core::Error,
            > {
                match &input.witness_script {
                    None => Err(ark_core::Error::ad_hoc(
                        "Missing witness script for psbt::Input when signing ark transaction",
                    )),
                    Some(script) => {
                        let mut res = vec![];
                        let pks = extract_checksig_pubkeys(script);
                        for pk in pks {
                            if let Ok(keypair) = self.keypair_by_pk(&pk) {
                                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &keypair);
                                let pk = keypair.x_only_public_key().0;
                                res.push((sig, pk))
                            }
                        }
                        Ok(res)
                    }
                }
            };

            sign_ark_transaction(sign_fn, &mut ark_tx, i)?;
        }

        let ark_txid = ark_tx.unsigned_tx.compute_txid();

        self.network_client()
            .submit_offchain_transaction_request(ark_tx, checkpoint_txs)
            .await
            .map_err(Error::ark_server)
            .context("failed to submit offchain transaction request")?;

        Ok(ark_txid)
    }

    /// Build and sign an Ark transaction with the given VTXO inputs.
    ///
    /// This is a shared helper used by both [`Self::send_vtxo`] and [`Self::send_vtxo_selection`].
    async fn build_and_sign_offchain_tx(
        &self,
        vtxo_inputs: Vec<send::VtxoInput>,
        address: ArkAddress,
        amount: Amount,
    ) -> Result<Txid, Error> {
        let (change_address, change_address_vtxo) = self.get_offchain_address()?;

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &[(&address, amount)],
            Some(&change_address),
            &vtxo_inputs,
            &self.server_info,
        )
        .map_err(Error::from)
        .context("failed to build offchain transactions")?;

        for i in 0..checkpoint_txs.len() {
            let sign_fn = |input: &mut psbt::Input,
                           msg: secp256k1::Message|
             -> Result<
                Vec<(schnorr::Signature, XOnlyPublicKey)>,
                ark_core::Error,
            > {
                match &input.witness_script {
                    None => Err(ark_core::Error::ad_hoc(
                        "Missing witness script for psbt::Input when signing ark transaction",
                    )),
                    Some(script) => {
                        let mut res = vec![];
                        let pks = extract_checksig_pubkeys(script);
                        for pk in pks {
                            if let Ok(keypair) = self.keypair_by_pk(&pk) {
                                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &keypair);
                                let pk = keypair.x_only_public_key().0;
                                res.push((sig, pk))
                            }
                        }
                        Ok(res)
                    }
                }
            };

            sign_ark_transaction(sign_fn, &mut ark_tx, i)?;
        }

        let ark_txid = ark_tx.unsigned_tx.compute_txid();

        let mut res = self
            .network_client()
            .submit_offchain_transaction_request(ark_tx, checkpoint_txs.clone())
            .await
            .map_err(Error::ark_server)
            .context("failed to submit offchain transaction request")?;

        // Build a map from checkpoint txid → witness_script from the client's
        // original checkpoints. The server may return signed checkpoints in a
        // different order, so we match by txid rather than assuming index order.
        let client_checkpoint_ws: std::collections::HashMap<_, _> = checkpoint_txs
            .iter()
            .map(|cp| {
                let txid = cp.unsigned_tx.compute_txid();
                let ws = cp.inputs[0].witness_script.clone();
                (txid, ws)
            })
            .collect();

        for checkpoint_psbt in res.signed_checkpoint_txs.iter_mut() {
            let sign_fn = |input: &mut psbt::Input,
                           msg: secp256k1::Message|
             -> Result<
                Vec<(schnorr::Signature, XOnlyPublicKey)>,
                ark_core::Error,
            > {
                match &input.witness_script {
                    None => Err(ark_core::Error::ad_hoc(
                        "Missing witness script for psbt::Input signing checkpoint tx",
                    )),
                    Some(script) => {
                        let mut res = vec![];
                        let pks = extract_checksig_pubkeys(script);
                        for pk in pks {
                            if let Ok(keypair) = self.keypair_by_pk(&pk) {
                                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &keypair);
                                let pk = keypair.x_only_public_key().0;
                                res.push((sig, pk));
                            }
                        }
                        Ok(res)
                    }
                }
            };

            let cp_txid = checkpoint_psbt.unsigned_tx.compute_txid();
            if let Some(ws) = client_checkpoint_ws.get(&cp_txid).cloned().flatten() {
                checkpoint_psbt.inputs[0].witness_script = Some(ws);
            }

            sign_checkpoint_transaction(sign_fn, checkpoint_psbt)?;
        }

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, res.signed_checkpoint_txs),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize offchain transaction")?;

        let used_pk = change_address_vtxo.owner_pk();
        if let Err(err) = self.inner.key_provider.mark_as_used(&used_pk) {
            tracing::warn!(
                "Failed updating keypair cache for used change address: {:?} ",
                err
            );
        }

        Ok(ark_txid)
    }

    /// List pending (submitted but not finalized) offchain transactions.
    ///
    /// This retrieves any transactions that were submitted to the server but not yet finalized
    /// (e.g. due to a crash or network error between submit and finalize).
    ///
    /// # Returns
    ///
    /// The pending transactions, or an empty vec if there are none.
    pub async fn list_pending_offchain_txs(
        &self,
    ) -> Result<Vec<ark_core::server::PendingTx>, Error> {
        self.fetch_pending_offchain_txs().await
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
            let mut signed_checkpoint_txs = pending_tx.signed_checkpoint_txs;

            // Build a map from checkpoint txid → ark tx input index, since the
            // server may return checkpoint txs in a different order than the ark
            // tx inputs reference them.
            let ark_tx_input_index_by_checkpoint_txid: std::collections::HashMap<_, _> = pending_tx
                .signed_ark_tx
                .unsigned_tx
                .input
                .iter()
                .enumerate()
                .map(|(i, inp)| (inp.previous_output.txid, i))
                .collect();

            for checkpoint_psbt in signed_checkpoint_txs.iter_mut() {
                if checkpoint_psbt.inputs[0].witness_script.is_none() {
                    // Server stripped the witness_script — restore it from
                    // the ark tx input that spends this checkpoint.
                    let checkpoint_txid = checkpoint_psbt.unsigned_tx.compute_txid();
                    let ark_input_idx = ark_tx_input_index_by_checkpoint_txid
                        .get(&checkpoint_txid)
                        .ok_or_else(|| {
                            Error::ad_hoc(format!(
                                "checkpoint txid {checkpoint_txid} not found in ark tx inputs for pending tx {ark_txid}"
                            ))
                        })?;

                    let witness_script = pending_tx
                        .signed_ark_tx
                        .inputs
                        .get(*ark_input_idx)
                        .and_then(|input| input.witness_script.clone())
                        .ok_or_else(|| {
                            Error::ad_hoc(format!(
                                "missing witness script on ark tx input {ark_input_idx} for pending tx {ark_txid}"
                            ))
                        })?;

                    checkpoint_psbt.inputs[0].witness_script = Some(witness_script);
                }

                let sign_fn = |input: &mut psbt::Input,
                               msg: secp256k1::Message|
                 -> Result<
                    Vec<(schnorr::Signature, XOnlyPublicKey)>,
                    ark_core::Error,
                > {
                    match &input.witness_script {
                        None => Err(ark_core::Error::ad_hoc(
                            "Missing witness script for psbt::Input signing checkpoint tx",
                        )),
                        Some(script) => {
                            let mut res = vec![];
                            let pks = extract_checksig_pubkeys(script);
                            for pk in pks {
                                if let Ok(keypair) = self.keypair_by_pk(&pk) {
                                    let sig =
                                        Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &keypair);
                                    let pk = keypair.x_only_public_key().0;
                                    res.push((sig, pk));
                                }
                            }
                            Ok(res)
                        }
                    }
                };

                sign_checkpoint_transaction(sign_fn, checkpoint_psbt)?;
            }

            timeout_op(
                self.inner.timeout,
                self.network_client()
                    .finalize_offchain_transaction(ark_txid, signed_checkpoint_txs),
            )
            .await?
            .map_err(Error::ark_server)
            .context("failed to finalize pending offchain transaction")?;

            finalized_txids.push(ark_txid);
        }

        Ok(finalized_txids)
    }

    /// Fetch pending offchain transactions from the server.
    ///
    /// Shared helper used by both [`Self::list_pending_offchain_txs`] and
    /// [`Self::continue_pending_offchain_txs`].
    async fn fetch_pending_offchain_txs(&self) -> Result<Vec<ark_core::server::PendingTx>, Error> {
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

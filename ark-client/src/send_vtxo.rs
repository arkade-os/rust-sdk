use crate::error::ErrorContext;
use crate::swap_storage::SwapStorage;
use crate::utils::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::coin_select::select_vtxos;
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
        let (vtxo_list, script_pubkey_to_vtxo_map) = self
            .list_vtxos()
            .await
            .context("failed to get spendable VTXOs")?;

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

        for (index, checkpoint_psbt) in res.signed_checkpoint_txs.iter_mut().enumerate() {
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

            // TODO: Maybe it's better to add the signature from the server-signed checkpoint PSBT
            // instead.
            checkpoint_psbt.inputs[0].witness_script =
                checkpoint_txs[index].inputs[0].witness_script.clone();

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
}

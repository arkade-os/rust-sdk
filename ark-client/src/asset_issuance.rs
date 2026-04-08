use crate::error::ErrorContext;
use crate::swap_storage::SwapStorage;
use crate::utils::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::asset::AssetId;
use ark_core::asset::ControlAssetConfig;
use ark_core::coin_select::select_vtxos;
use ark_core::coin_select::select_vtxos_for_asset;
use ark_core::script::extract_checksig_pubkeys;
use ark_core::send;
use ark_core::send::build_asset_reissuance_transactions;
use ark_core::send::build_self_asset_issuance_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::AssetReissuanceTransactions;
use ark_core::send::SelfAssetIssuanceTransactions;
use ark_core::ErrorContext as _;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Amount;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use std::collections::HashMap;
use std::collections::HashSet;

/// Result of an asset issuance.
#[derive(Debug, Clone)]
pub struct IssueAssetResult {
    /// The Ark transaction ID.
    pub ark_txid: Txid,
    /// The issued asset IDs. If a new control asset was created, it is first.
    pub asset_ids: Vec<AssetId>,
}

impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: crate::KeyProvider,
{
    /// Issue a new asset.
    ///
    /// Creates a fresh asset with the given `amount`. The asset is sent to the caller's own
    /// address. If `control_asset` is provided, the asset can be reissued in the future.
    ///
    /// # Arguments
    ///
    /// * `amount` - The number of asset units to issue
    /// * `control_asset` - Optional control asset configuration for reissuance
    /// * `metadata` - Optional key-value metadata for the asset
    ///
    /// # Returns
    ///
    /// An [`IssueAssetResult`] containing the Ark txid and the new asset IDs.
    pub async fn issue_asset(
        &self,
        amount: u64,
        control_asset_config: Option<ControlAssetConfig>,
        metadata: Option<Vec<(String, String)>>,
    ) -> Result<IssueAssetResult, Error> {
        if amount == 0 {
            return Err(Error::ad_hoc("asset amount must be > 0"));
        }

        let (own_address, _) = self.get_offchain_address()?;

        // We need a dust-amount VTXO to carry the issued asset.
        let send_amount = self.server_info.dust;

        // Coin select for the BTC needed.
        let (vtxo_list, script_pubkey_to_vtxo_map) =
            self.list_vtxos().await.context("failed to list VTXOs")?;

        let spendable = vtxo_list
            .spendable_offchain()
            .map(|vtxo| ark_core::coin_select::VirtualTxOutPoint {
                outpoint: vtxo.outpoint,
                script_pubkey: vtxo.script.clone(),
                expire_at: vtxo.expires_at,
                amount: vtxo.amount,
                assets: vtxo.assets.clone(),
            })
            .collect::<Vec<_>>();

        let selected_coins = select_vtxos(spendable, send_amount, self.server_info.dust, true)
            .map_err(Error::from)
            .context("failed to select coins for asset issuance")?;

        let issuance_inputs = selected_coins
            .iter()
            .map(|vto| {
                let vtxo = script_pubkey_to_vtxo_map
                    .get(&vto.script_pubkey)
                    .ok_or_else(|| {
                        ark_core::Error::ad_hoc(format!(
                            "missing VTXO for script pubkey: {}",
                            vto.script_pubkey
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
                    vto.amount,
                    vto.outpoint,
                    vto.assets.clone(),
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        let (change_address, change_address_vtxo) = self.get_offchain_address()?;

        let SelfAssetIssuanceTransactions {
            mut ark_tx,
            checkpoint_txs,
            asset_ids,
        } = build_self_asset_issuance_transactions(
            &own_address,
            &change_address,
            &issuance_inputs,
            &self.server_info,
            amount,
            control_asset_config.clone(),
            metadata.clone(),
        )
        .map_err(Error::from)
        .context("failed to build asset issuance transactions")?;

        // Sign the ark transaction inputs.
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

        // Submit to server.
        let mut res = timeout_op(
            self.inner.timeout,
            self.network_client()
                .submit_offchain_transaction_request(ark_tx.clone(), checkpoint_txs.clone()),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to submit asset issuance transaction")?;

        // Sign server-returned checkpoint transactions.
        let client_checkpoint_ws: HashMap<_, _> = checkpoint_txs
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

        self.finalize_with_retry(ark_txid, res.signed_checkpoint_txs)
            .await?;

        // Mark key as used.
        let used_pk = change_address_vtxo.owner_pk();
        if let Err(err) = self.inner.key_provider.mark_as_used(&used_pk) {
            tracing::warn!(
                "Failed updating keypair cache for used change address: {:?}",
                err
            );
        }

        Ok(IssueAssetResult {
            ark_txid,
            asset_ids,
        })
    }

    /// Reissue additional units of an existing asset.
    ///
    /// The asset must have been created with a control asset. The control asset
    /// is spent as input and sent back to the caller, while the new asset units
    /// are minted.
    ///
    /// # Arguments
    ///
    /// * `asset_id` - The ID of the asset to reissue
    /// * `amount` - The number of additional asset units to mint
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the generated Ark transaction.
    pub async fn reissue_asset(&self, asset_id: AssetId, amount: u64) -> Result<Txid, Error> {
        if amount == 0 {
            return Err(Error::ad_hoc("reissue amount must be > 0"));
        }

        // 1. Look up the control asset ID for this asset.
        let asset_info = self
            .get_asset(asset_id)
            .await
            .context("failed to get asset info")?;

        let control_asset_id = match asset_info.control_asset_id {
            Some(control_asset_id) => control_asset_id,
            None => {
                return Err(Error::ad_hoc(format!(
                    "Asset {} can't be reissued, no control asset",
                    asset_id
                )));
            }
        };

        // TODO: Steps 2 and 3 are hard to follow.

        // 2. Select VTXOs holding the control asset.
        let (vtxo_list, script_pubkey_to_vtxo_map) =
            self.list_vtxos().await.context("failed to list VTXOs")?;

        let spendable = vtxo_list
            .spendable_offchain()
            .map(|vtxo| ark_core::coin_select::VirtualTxOutPoint {
                outpoint: vtxo.outpoint,
                script_pubkey: vtxo.script.clone(),
                expire_at: vtxo.expires_at,
                amount: vtxo.amount,
                assets: vtxo.assets.clone(),
            })
            .collect::<Vec<_>>();

        let mut selected_outpoints: HashSet<bitcoin::OutPoint> = HashSet::new();
        let mut all_selected = Vec::new();

        // Select the control asset VTXO (amount = 1).
        let (control_coins, _control_change) =
            select_vtxos_for_asset(spendable.clone(), 1, control_asset_id)
                .map_err(Error::from)
                .context("failed to select control asset for reissuance")?;

        let mut btc_provided = Amount::ZERO;
        for coin in &control_coins {
            if selected_outpoints.insert(coin.outpoint) {
                btc_provided += coin.amount;
                all_selected.push(coin.clone());
            }
        }

        // 3. Reissuance is self-directed, so one dust receiver output is enough.
        let (self_address, _) = self
            .get_offchain_addresses()?
            .into_iter()
            .next()
            .ok_or_else(|| ark_core::Error::ad_hoc("no offchain address available"))?;

        let btc_shortfall = self
            .server_info
            .dust
            .checked_sub(btc_provided)
            .unwrap_or(Amount::ZERO);
        if btc_shortfall > Amount::ZERO {
            let available: Vec<_> = spendable
                .iter()
                .filter(|v| !selected_outpoints.contains(&v.outpoint))
                .cloned()
                .collect();

            let btc_coins = select_vtxos(available, btc_shortfall, self.server_info.dust, true)
                .map_err(Error::from)
                .context("failed to select BTC coins for reissuance")?;

            for coin in &btc_coins {
                if selected_outpoints.insert(coin.outpoint) {
                    btc_provided += coin.amount;
                    all_selected.push(coin.clone());
                }
            }
        }

        // 4. Build VTXO inputs.
        let reissuance_inputs = all_selected
            .iter()
            .map(|vto| {
                let vtxo = script_pubkey_to_vtxo_map
                    .get(&vto.script_pubkey)
                    .ok_or_else(|| {
                        ark_core::Error::ad_hoc(format!(
                            "missing VTXO for script pubkey: {}",
                            vto.script_pubkey
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
                    vto.amount,
                    vto.outpoint,
                    vto.assets.clone(),
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        // 5. Build offchain transaction.
        let (change_address, change_address_vtxo) = self.get_offchain_address()?;

        let AssetReissuanceTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_asset_reissuance_transactions(
            &self_address,
            &change_address,
            &reissuance_inputs,
            &self.server_info,
            asset_id,
            control_asset_id,
            amount,
        )
        .map_err(Error::from)
        .context("failed to build asset reissuance transactions")?;

        // 6. Sign, submit, finalize.
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

        let mut res = timeout_op(
            self.inner.timeout,
            self.network_client()
                .submit_offchain_transaction_request(ark_tx, checkpoint_txs.clone()),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to submit reissuance transaction")?;

        let client_checkpoint_ws: HashMap<_, _> = checkpoint_txs
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

        self.finalize_with_retry(ark_txid, res.signed_checkpoint_txs)
            .await?;

        let used_pk = change_address_vtxo.owner_pk();
        if let Err(err) = self.inner.key_provider.mark_as_used(&used_pk) {
            tracing::warn!(
                "Failed updating keypair cache for used change address: {:?}",
                err
            );
        }

        Ok(ark_txid)
    }
}

use crate::error::ErrorContext;
use crate::send_vtxo::parse_asset_id_hex;
use crate::swap_storage::SwapStorage;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::asset_packet;
use ark_core::asset_packet::add_asset_packet_to_psbt;
use ark_core::coin_select::select_vtxos;
use ark_core::coin_select::select_vtxos_for_asset;
use ark_core::script::extract_checksig_pubkeys;
use ark_core::send;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::OffchainTransactions;
use ark_core::server::Asset;
use ark_core::server::ControlAsset;
use ark_core::ErrorContext as _;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Amount;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use std::collections::HashMap;

/// Result of an asset issuance.
#[derive(Debug, Clone)]
pub struct IssueAssetResult {
    /// The Ark transaction ID.
    pub ark_txid: Txid,
    /// The issued asset IDs. If a new control asset was created, it is first.
    pub asset_ids: Vec<asset_packet::AssetId>,
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
        control_asset: Option<ControlAsset>,
        metadata: Option<Vec<(String, String)>>,
    ) -> Result<IssueAssetResult, Error> {
        if amount == 0 {
            return Err(Error::ad_hoc("asset amount must be > 0"));
        }

        let (own_address, change_address_vtxo) = self.get_offchain_address()?;

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

        let vtxo_inputs = selected_coins
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
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        // Build the base offchain transaction: send dust to self.
        let (change_address, _) = self.get_offchain_address()?;

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &[(&own_address, send_amount)],
            Some(&change_address),
            &vtxo_inputs,
            &self.server_info,
        )
        .map_err(Error::from)
        .context("failed to build offchain transactions")?;

        // Build the asset packet.
        let mut groups = Vec::new();

        // Preserve any assets carried by the funding VTXOs.
        let num_psbt_outputs = ark_tx.unsigned_tx.output.len();
        let has_change_output = num_psbt_outputs > 2; // receiver + optional change + anchor
        let existing_asset_output_index = if has_change_output {
            (num_psbt_outputs - 2) as u16
        } else {
            0
        };
        let mut existing_asset_groups: HashMap<String, asset_packet::AssetGroup> = HashMap::new();
        for (input_index, coin) in selected_coins.iter().enumerate() {
            for asset in &coin.assets {
                let group = existing_asset_groups
                    .entry(asset.asset_id.clone())
                    .or_insert_with(|| asset_packet::AssetGroup {
                        asset_id: None,
                        control_asset: None,
                        metadata: None,
                        inputs: Vec::new(),
                        outputs: vec![asset_packet::AssetOutput {
                            output_index: existing_asset_output_index,
                            amount: 0,
                        }],
                    });

                if group.asset_id.is_none() {
                    group.asset_id = Some(parse_asset_id_hex(&asset.asset_id)?);
                }

                group.inputs.push(asset_packet::AssetInput {
                    input_index: input_index as u16,
                    amount: asset.amount,
                });
                group.outputs[0].amount += asset.amount;
            }
        }
        // If creating a new control asset, it goes first.
        if let Some(ControlAsset::New { amount: ctrl_amt }) = &control_asset {
            groups.push(asset_packet::AssetGroup {
                asset_id: None, // Fresh mint
                control_asset: None,
                metadata: metadata.clone(),
                inputs: vec![],
                outputs: vec![asset_packet::AssetOutput {
                    output_index: 0,
                    amount: *ctrl_amt,
                }],
            });
        }

        // Build the control asset reference for the issued asset group.
        let control_ref = match &control_asset {
            Some(ControlAsset::New { .. }) => {
                // Reference the control asset group we just created (index 0).
                Some(asset_packet::AssetRef::ByGroup(0))
            }
            Some(ControlAsset::Existing { id }) => {
                // Parse the existing asset ID.
                let asset_id = parse_asset_id_string(id)?;
                Some(asset_packet::AssetRef::ById(asset_id))
            }
            None => None,
        };

        // The issued asset group.
        groups.push(asset_packet::AssetGroup {
            asset_id: None, // Fresh mint
            control_asset: control_ref,
            metadata,
            inputs: vec![],
            outputs: vec![asset_packet::AssetOutput {
                output_index: 0,
                amount,
            }],
        });

        groups.extend(existing_asset_groups.into_values());

        let packet = asset_packet::Packet { groups };

        // Add the asset packet OP_RETURN to the PSBT (before the anchor output).
        add_asset_packet_to_psbt(&mut ark_tx, &packet);

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
        let mut res = self
            .network_client()
            .submit_offchain_transaction_request(ark_tx.clone(), checkpoint_txs.clone())
            .await
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

        // Derive asset IDs from the PSBT txid.
        let psbt_txid = ark_tx.unsigned_tx.compute_txid();
        let mut asset_ids = Vec::new();
        let mut gidx: u16 = 0;

        if matches!(control_asset, Some(ControlAsset::New { .. })) {
            asset_ids.push(asset_packet::AssetId {
                txid: psbt_txid,
                group_index: gidx,
            });
            gidx += 1;
        }

        asset_ids.push(asset_packet::AssetId {
            txid: psbt_txid,
            group_index: gidx,
        });

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
    /// * `asset_id` - The hex-encoded asset ID to reissue
    /// * `amount` - The number of additional asset units to mint
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the generated Ark transaction.
    pub async fn reissue_asset(&self, asset_id: &str, amount: u64) -> Result<Txid, Error> {
        if amount == 0 {
            return Err(Error::ad_hoc("reissue amount must be > 0"));
        }

        // 1. Look up the control asset ID for this asset.
        let asset_info = self
            .get_asset(asset_id)
            .await
            .context("failed to get asset info")?;

        if asset_info.control_asset_id.is_empty() {
            return Err(Error::ad_hoc(format!(
                "{} can't be reissued, no control asset",
                asset_id
            )));
        }

        let control_asset_id = &asset_info.control_asset_id;

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

        let mut selected_outpoints: std::collections::HashSet<bitcoin::OutPoint> =
            std::collections::HashSet::new();
        let mut all_selected = Vec::new();
        let mut asset_changes: HashMap<String, u64> = HashMap::new();

        // Select the control asset VTXO (amount = 1).
        let (control_coins, control_change) =
            select_vtxos_for_asset(spendable.clone(), 1, control_asset_id)
                .map_err(Error::from)
                .context("failed to select control asset for reissuance")?;

        let mut btc_provided = Amount::ZERO;
        for coin in &control_coins {
            if selected_outpoints.insert(coin.outpoint) {
                btc_provided += coin.amount;
                for a in &coin.assets {
                    if a.asset_id != *control_asset_id {
                        *asset_changes.entry(a.asset_id.clone()).or_insert(0) += a.amount;
                    }
                }
                all_selected.push(coin.clone());
            }
        }
        if control_change > 0 {
            asset_changes.insert(control_asset_id.to_string(), control_change);
        }

        // 3. We need dust for the receiver output (reissued asset) + dust for the control asset
        //    output back to self.
        let (self_address, _) = self
            .get_offchain_addresses()?
            .into_iter()
            .next()
            .ok_or_else(|| ark_core::Error::ad_hoc("no offchain address available"))?;

        // Two dust outputs: one for the reissued asset, one for the control asset back to self.
        // Plus a change output if there are other asset changes.
        let mut btc_needed = self.server_info.dust * 2;
        if !asset_changes.is_empty() {
            btc_needed += self.server_info.dust;
        }

        let btc_shortfall = btc_needed.checked_sub(btc_provided).unwrap_or(Amount::ZERO);
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
                    for a in &coin.assets {
                        *asset_changes.entry(a.asset_id.clone()).or_insert(0) += a.amount;
                    }
                    all_selected.push(coin.clone());
                }
            }
        }

        // 4. Build VTXO inputs.
        let vtxo_inputs = all_selected
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
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        // 5. Build offchain transaction.
        // Like the Go SDK, create a single receiver sending the control asset to self.
        let (change_address, change_address_vtxo) = self.get_offchain_address()?;

        let receivers = vec![crate::Receiver {
            address: self_address,
            amount: self.server_info.dust,
            assets: vec![Asset {
                asset_id: control_asset_id.to_string(),
                amount: 1,
            }],
        }];

        let outputs: Vec<(&ark_core::ArkAddress, Amount)> =
            receivers.iter().map(|r| (&r.address, r.amount)).collect();

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &outputs,
            Some(&change_address),
            &vtxo_inputs,
            &self.server_info,
        )
        .map_err(Error::from)
        .context("failed to build offchain transactions")?;

        // 6. Build the asset packet using the same approach as send_assets.
        // This creates groups for all asset transfers (control asset + any other assets).
        let mut asset_inputs_map: HashMap<u16, Vec<Asset>> = HashMap::new();
        for (idx, coin) in all_selected.iter().enumerate() {
            if !coin.assets.is_empty() {
                asset_inputs_map.insert(idx as u16, coin.assets.clone());
            }
        }

        let num_psbt_outputs = ark_tx.unsigned_tx.output.len();
        let has_change_output = num_psbt_outputs > receivers.len() + 1;
        let change_output_index = if has_change_output {
            num_psbt_outputs - 2
        } else {
            0
        };
        let change_assets: Vec<Asset> = if has_change_output {
            asset_changes
                .into_iter()
                .map(|(asset_id, amount)| Asset { asset_id, amount })
                .collect()
        } else {
            Vec::new()
        };

        let packet = crate::send_vtxo::create_asset_packet(
            &asset_inputs_map,
            &receivers,
            &change_assets,
            change_output_index,
        )?;

        // Now add the reissue output: find or create a group for the reissued asset.
        let mut packet = packet.unwrap_or_else(|| asset_packet::Packet { groups: Vec::new() });

        let reissue_asset_id = parse_asset_id_hex(asset_id)?;
        let reissue_output = asset_packet::AssetOutput {
            output_index: 0, // reissued asset goes to the first receiver output
            amount,
        };

        // Check if a group for this asset already exists (e.g. from existing balance on the VTXO).
        let existing_group = packet.groups.iter_mut().find(|g| {
            g.asset_id
                .as_ref()
                .map(|id| *id == reissue_asset_id)
                .unwrap_or(false)
        });

        if let Some(group) = existing_group {
            // Append reissue output to the existing group.
            group.outputs.push(reissue_output);
        } else {
            // Create a new group for the reissued asset (no inputs, no control_asset ref).
            packet.groups.push(asset_packet::AssetGroup {
                asset_id: Some(reissue_asset_id),
                control_asset: None,
                metadata: None,
                inputs: vec![],
                outputs: vec![reissue_output],
            });
        }

        add_asset_packet_to_psbt(&mut ark_tx, &packet);

        // 7. Sign, submit, finalize.
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

/// Parse an asset ID string in the format "txid:gidx" into an [`asset_packet::AssetId`].
fn parse_asset_id_string(s: &str) -> Result<asset_packet::AssetId, Error> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        return Err(Error::ad_hoc(format!(
            "invalid asset ID format '{}', expected 'txid:gidx'",
            s
        )));
    }

    let txid: Txid = parts[0]
        .parse()
        .map_err(|e| Error::ad_hoc(format!("invalid txid in asset ID: {}", e)))?;
    let group_index: u16 = parts[1]
        .parse()
        .map_err(|e| Error::ad_hoc(format!("invalid group index in asset ID: {}", e)))?;

    Ok(asset_packet::AssetId { txid, group_index })
}

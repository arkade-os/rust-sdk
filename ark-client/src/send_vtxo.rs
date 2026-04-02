use crate::error::ErrorContext;
use crate::swap_storage::SwapStorage;
use crate::utils::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::asset;
use ark_core::asset::AssetId;
use ark_core::coin_select::select_vtxos;
use ark_core::coin_select::select_vtxos_for_asset;
use ark_core::intent;
use ark_core::script::extract_checksig_pubkeys;
use ark_core::send;
use ark_core::send::build_offchain_transactions;
use ark_core::send::build_send_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::AssetBearingVtxoInput;
use ark_core::send::OffchainTransactions;
use ark_core::send::SendReceiver;
use ark_core::server::Asset;
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
use std::collections::HashMap;
use std::collections::HashSet;
use std::time::Duration;

impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: crate::KeyProvider,
{
    // ── High-level send (submit + finalize) ────────────────────────────

    /// Send BTC and optional assets offchain to one or more receivers.
    ///
    /// Each receiver specifies a BTC amount (at least dust) and may also request one or more
    /// assets. Coin selection handles both BTC-only and asset-bearing VTXOs. An asset packet is
    /// attached only when the transfer actually involves carried or requested assets.
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the generated Ark transaction.
    pub async fn send(&self, receivers: Vec<SendReceiver>) -> Result<Txid, Error> {
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
                assets: vtxo.assets.clone(),
            })
            .collect::<Vec<_>>();

        // Track which VTXOs are already selected (by outpoint) to avoid double-spending.
        let mut selected_outpoints = HashSet::new();
        let mut all_selected: Vec<ark_core::coin_select::VirtualTxOutPoint> = Vec::new();

        // Per-asset change amounts that may satisfy later receivers.
        let mut asset_changes: HashMap<AssetId, u64> = HashMap::new();

        // Track BTC needed and BTC already provided by asset-selected VTXOs.
        let mut btc_needed = Amount::ZERO;
        let mut btc_provided = Amount::ZERO;

        // 1. Asset coin selection: for each receiver's assets, select VTXOs holding them.
        for receiver in &receivers {
            btc_needed += receiver.amount;

            for asset in &receiver.assets {
                let mut amount_to_select = asset.amount;

                // Use existing asset change if available.
                if let Some(existing_change) = asset_changes.get_mut(&asset.asset_id) {
                    if amount_to_select <= *existing_change {
                        *existing_change -= amount_to_select;
                        if *existing_change == 0 {
                            asset_changes.remove(&asset.asset_id);
                        }
                        continue;
                    }
                    amount_to_select -= *existing_change;
                    asset_changes.remove(&asset.asset_id);
                }

                // Filter to not-yet-selected VTXOs.
                let available: Vec<_> = spendable
                    .iter()
                    .filter(|v| !selected_outpoints.contains(&v.outpoint))
                    .cloned()
                    .collect();

                let (asset_coins, asset_change) =
                    select_vtxos_for_asset(available, amount_to_select, asset.asset_id)
                        .map_err(Error::from)
                        .context("failed to select coins for asset transfer")?;

                for coin in &asset_coins {
                    if selected_outpoints.insert(coin.outpoint) {
                        btc_provided += coin.amount;

                        for a in &coin.assets {
                            if a.asset_id != asset.asset_id {
                                *asset_changes.entry(a.asset_id).or_insert(0) += a.amount;
                            }
                        }

                        all_selected.push(coin.clone());
                    }
                }

                if asset_change > 0 {
                    *asset_changes.entry(asset.asset_id).or_insert(0) += asset_change;
                }
            }
        }

        // 2. BTC coin selection for any remaining BTC needed.
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
                .context("failed to select BTC coins for asset transfer")?;

            for coin in &btc_coins {
                if selected_outpoints.insert(coin.outpoint) {
                    for a in &coin.assets {
                        *asset_changes.entry(a.asset_id).or_insert(0) += a.amount;
                    }
                    all_selected.push(coin.clone());
                }
            }
        }

        let (change_address, change_address_vtxo) = self.get_offchain_address()?;
        let vtxo_inputs =
            self.build_vtxo_inputs(all_selected.clone(), &script_pubkey_to_vtxo_map)?;
        let asset_inputs = vtxo_inputs
            .into_iter()
            .zip(all_selected.into_iter())
            .map(|(input, coin)| AssetBearingVtxoInput {
                input,
                assets: coin.assets,
            })
            .collect::<Vec<_>>();

        let send::SendTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_send_transactions(
            &receivers,
            &change_address,
            &asset_inputs,
            &self.server_info,
        )
        .map_err(Error::from)
        .context("failed to build offchain send transactions")?;

        for i in 0..checkpoint_txs.len() {
            sign_ark_transaction(self.make_sign_fn(), &mut ark_tx, i)?;
        }

        let ark_txid = ark_tx.unsigned_tx.compute_txid();

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

        self.sign_and_finalize_pending_tx(pending_tx).await?;

        let used_pk = change_address_vtxo.owner_pk();
        if let Err(err) = self.inner.key_provider.mark_as_used(&used_pk) {
            tracing::warn!(
                "Failed updating keypair cache for used change address: {:?}",
                err
            );
        }

        Ok(ark_txid)
    }

    /// Spend specific VTXOs in an Ark transaction sending BTC and optional assets to the given
    /// receivers.
    ///
    /// Unlike [`Self::send`], this method allows the caller to specify exactly which VTXOs to
    /// spend by providing their outpoints. This is useful for applications that want to have full
    /// control over VTXO selection.
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the generated Ark transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the selected VTXOs don't have enough BTC value to cover the requested
    /// receiver amounts.
    pub async fn send_selection(
        &self,
        vtxo_outpoints: &[OutPoint],
        receivers: Vec<SendReceiver>,
    ) -> Result<Txid, Error> {
        let (selected_coins, vtxo_inputs, total_amount) =
            self.select_vtxo_inputs_with_total(vtxo_outpoints).await?;
        let total_requested_amount = receivers.iter().fold(Amount::ZERO, |acc, r| acc + r.amount);

        if total_amount < total_requested_amount {
            return Err(Error::coin_select(format!(
                "insufficient VTXO amount: {} < {}",
                total_amount, total_requested_amount
            )));
        }

        let asset_inputs = selected_coins
            .into_iter()
            .zip(vtxo_inputs.into_iter())
            .map(|(coin, input)| AssetBearingVtxoInput {
                input,
                assets: coin.assets,
            })
            .collect::<Vec<_>>();
        let pending_tx = self.submit_asset_send(asset_inputs, receivers).await?;
        let ark_txid = pending_tx.ark_txid;
        self.sign_and_finalize_pending_tx(pending_tx).await?;
        Ok(ark_txid)
    }

    /// Spend confirmed and pre-confirmed VTXOs in an Ark transaction sending the given `amount` to
    /// the given `address`.
    ///
    /// The Ark transaction is built in collaboration with the Ark server. The outputs of said
    /// transaction will be pre-confirmed VTXOs.
    ///
    /// Coin selection is performed automatically to choose which VTXOs to spend.
    ///
    /// If the selected coins carry assets, an asset packet is built to route those assets to the
    /// change output so they are not lost.
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the generated Ark transaction.
    pub async fn send_vtxo(&self, address: ArkAddress, amount: Amount) -> Result<Txid, Error> {
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
                assets: vtxo.assets.clone(),
            })
            .collect::<Vec<_>>();

        let selected_coins = select_vtxos(spendable, amount, self.server_info.dust, true)
            .map_err(Error::from)
            .context("failed to select coins")?;

        let asset_inputs =
            self.build_asset_bearing_vtxo_inputs(selected_coins, &script_pubkey_to_vtxo_map)?;
        let receivers = vec![SendReceiver {
            address,
            amount,
            assets: Vec::new(),
        }];
        let pending_tx = self.submit_asset_send(asset_inputs, receivers).await?;
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
        let (selected_coins, vtxo_inputs, total_amount) =
            self.select_vtxo_inputs_with_total(vtxo_outpoints).await?;

        if total_amount < amount {
            return Err(Error::coin_select(format!(
                "insufficient VTXO amount: {} < {}",
                total_amount, amount
            )));
        }

        let asset_inputs = selected_coins
            .into_iter()
            .zip(vtxo_inputs.into_iter())
            .map(|(coin, input)| AssetBearingVtxoInput {
                input,
                assets: coin.assets,
            })
            .collect::<Vec<_>>();
        let receivers = vec![SendReceiver {
            address,
            amount,
            assets: Vec::new(),
        }];

        let pending_tx = self.submit_asset_send(asset_inputs, receivers).await?;
        let ark_txid = pending_tx.ark_txid;
        self.sign_and_finalize_pending_tx(pending_tx).await?;
        Ok(ark_txid)
    }

    /// Send assets offchain to one or more receivers.
    ///
    /// Each receiver specifies a BTC amount (at least dust) and a list of assets.
    /// Coin selection handles both BTC and asset VTXOs. An asset packet is built
    /// and attached to the PSBT.
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the generated Ark transaction.
    pub async fn send_assets(&self, receivers: Vec<SendReceiver>) -> Result<Txid, Error> {
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
                assets: vtxo.assets.clone(),
            })
            .collect::<Vec<_>>();

        // Track which VTXOs are already selected (by outpoint) to avoid double-spending.
        let mut selected_outpoints = HashSet::new();
        let mut all_selected: Vec<ark_core::coin_select::VirtualTxOutPoint> = Vec::new();

        // Per-asset change amounts that may satisfy later receivers.
        let mut asset_changes: HashMap<AssetId, u64> = HashMap::new();

        // Track BTC needed and BTC already provided by asset-selected VTXOs.
        let mut btc_needed = Amount::ZERO;
        let mut btc_provided = Amount::ZERO;

        // 1. Asset coin selection: for each receiver's assets, select VTXOs holding them.
        for receiver in &receivers {
            btc_needed += receiver.amount;

            for asset in &receiver.assets {
                let mut amount_to_select = asset.amount;

                // Use existing asset change if available.
                if let Some(existing_change) = asset_changes.get_mut(&asset.asset_id) {
                    if amount_to_select <= *existing_change {
                        *existing_change -= amount_to_select;
                        if *existing_change == 0 {
                            asset_changes.remove(&asset.asset_id);
                        }
                        continue;
                    }
                    amount_to_select -= *existing_change;
                    asset_changes.remove(&asset.asset_id);
                }

                // Filter to not-yet-selected VTXOs.
                let available: Vec<_> = spendable
                    .iter()
                    .filter(|v| !selected_outpoints.contains(&v.outpoint))
                    .cloned()
                    .collect();

                let (asset_coins, asset_change) =
                    select_vtxos_for_asset(available, amount_to_select, asset.asset_id)
                        .map_err(Error::from)
                        .context("failed to select coins for asset transfer")?;

                for coin in &asset_coins {
                    if selected_outpoints.insert(coin.outpoint) {
                        // Track BTC provided by this asset VTXO.
                        btc_provided += coin.amount;

                        // Collect change for other assets in this coin.
                        for a in &coin.assets {
                            if a.asset_id != asset.asset_id {
                                *asset_changes.entry(a.asset_id).or_insert(0) += a.amount;
                            }
                        }

                        all_selected.push(coin.clone());
                    }
                }

                if asset_change > 0 {
                    *asset_changes.entry(asset.asset_id).or_insert(0) += asset_change;
                }
            }
        }

        // 2. BTC coin selection for any remaining BTC needed.
        // If there are asset changes, we need a change output to carry them,
        // which requires at least dust sats.
        if !asset_changes.is_empty() {
            btc_needed += self.server_info.dust;
        }

        // Only select more BTC if asset-selected VTXOs don't already cover the need.
        let btc_shortfall = btc_needed.checked_sub(btc_provided).unwrap_or(Amount::ZERO);

        if btc_shortfall > Amount::ZERO {
            let available: Vec<_> = spendable
                .iter()
                .filter(|v| !selected_outpoints.contains(&v.outpoint))
                .cloned()
                .collect();

            let btc_coins = select_vtxos(available, btc_shortfall, self.server_info.dust, true)
                .map_err(Error::from)
                .context("failed to select BTC coins for asset transfer")?;

            for coin in &btc_coins {
                if selected_outpoints.insert(coin.outpoint) {
                    // Collect asset change from BTC-selected coins.
                    for a in &coin.assets {
                        *asset_changes.entry(a.asset_id).or_insert(0) += a.amount;
                    }
                    all_selected.push(coin.clone());
                }
            }
        }

        // 3. Build the unsigned asset-send transactions.
        let (change_address, change_address_vtxo) = self.get_offchain_address()?;

        let vtxo_inputs =
            self.build_vtxo_inputs(all_selected.clone(), &script_pubkey_to_vtxo_map)?;
        let asset_inputs = vtxo_inputs
            .into_iter()
            .zip(all_selected.into_iter())
            .map(|(input, coin)| AssetBearingVtxoInput {
                input,
                assets: coin.assets,
            })
            .collect::<Vec<_>>();
        let receivers = receivers
            .into_iter()
            .map(|receiver| SendReceiver {
                address: receiver.address,
                amount: receiver.amount,
                assets: receiver.assets,
            })
            .collect::<Vec<_>>();

        let send::SendTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_send_transactions(
            &receivers,
            &change_address,
            &asset_inputs,
            &self.server_info,
        )
        .map_err(Error::from)
        .context("failed to build offchain asset-send transactions")?;

        // 6. Sign, submit, finalize.
        for i in 0..checkpoint_txs.len() {
            sign_ark_transaction(self.make_sign_fn(), &mut ark_tx, i)?;
        }

        let ark_txid = ark_tx.unsigned_tx.compute_txid();

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

        self.sign_and_finalize_pending_tx(pending_tx).await?;

        let used_pk = change_address_vtxo.owner_pk();
        if let Err(err) = self.inner.key_provider.mark_as_used(&used_pk) {
            tracing::warn!(
                "Failed updating keypair cache for used change address: {:?}",
                err
            );
        }

        Ok(ark_txid)
    }

    /// Burn a specific amount of an asset.
    ///
    /// The burned asset amount is represented in the asset packet as inputs with
    /// no corresponding outputs. Any remaining asset change is routed to a change
    /// output.
    ///
    /// # Returns
    ///
    /// The [`Txid`] of the generated Ark transaction.
    pub async fn burn_asset(&self, asset_id: AssetId, amount: u64) -> Result<Txid, Error> {
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
                assets: vtxo.assets.clone(),
            })
            .collect::<Vec<_>>();

        // 1. Select VTXOs holding the asset to burn.
        let (asset_coins, asset_change) =
            select_vtxos_for_asset(spendable.clone(), amount, asset_id)
                .map_err(Error::from)
                .context("failed to select coins for asset burn")?;

        let mut selected_outpoints: HashSet<OutPoint> =
            asset_coins.iter().map(|c| c.outpoint).collect();
        let mut all_selected = asset_coins.clone();

        // Collect asset changes from selected coins (other assets on same VTXOs).
        let mut asset_changes: HashMap<AssetId, u64> = HashMap::new();
        if asset_change > 0 {
            asset_changes.insert(asset_id, asset_change);
        }
        for coin in &asset_coins {
            for a in &coin.assets {
                if a.asset_id != asset_id {
                    *asset_changes.entry(a.asset_id).or_insert(0) += a.amount;
                }
            }
        }

        // 2. We send dust to our own address as the receiver output.
        let (self_address, _) = self
            .get_offchain_addresses()?
            .into_iter()
            .next()
            .ok_or_else(|| ark_core::Error::ad_hoc("no offchain address available"))?;

        let btc_provided: Amount = all_selected.iter().map(|c| c.amount).sum();
        let mut btc_needed = self.server_info.dust; // receiver output

        // If there are asset changes, we need a change output to carry them.
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
                .context("failed to select BTC coins for asset burn")?;

            for coin in &btc_coins {
                if selected_outpoints.insert(coin.outpoint) {
                    for a in &coin.assets {
                        *asset_changes.entry(a.asset_id).or_insert(0) += a.amount;
                    }
                    all_selected.push(coin.clone());
                }
            }
        }

        // 3. Build VTXO inputs.
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

        // 4. Build offchain transaction. The receiver is self with dust amount.
        let (change_address, change_address_vtxo) = self.get_offchain_address()?;

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &[(&self_address, self.server_info.dust)],
            Some(&change_address),
            &vtxo_inputs,
            &self.server_info,
        )
        .map_err(Error::from)
        .context("failed to build offchain transactions")?;

        // 5. Build the asset packet.
        // Inputs: all assets from selected coins.
        // Outputs: only asset *changes* on the change output -- the burned amount has NO output.
        let mut asset_inputs: HashMap<u16, Vec<Asset>> = HashMap::new();
        for (idx, coin) in all_selected.iter().enumerate() {
            if !coin.assets.is_empty() {
                asset_inputs.insert(idx as u16, coin.assets.clone());
            }
        }

        let num_psbt_outputs = ark_tx.unsigned_tx.output.len();
        let has_change_output = num_psbt_outputs > 2; // more than [receiver, anchor]
        let change_output_index = if has_change_output {
            num_psbt_outputs - 2
        } else {
            0
        };

        // The receiver gets NO assets (they're burned).
        // Only asset changes go to the change output.
        let change_assets: Vec<Asset> = if has_change_output {
            asset_changes
                .into_iter()
                .map(|(asset_id, amount)| Asset { asset_id, amount })
                .collect()
        } else {
            Vec::new()
        };

        let empty_receivers = vec![SendReceiver {
            address: self_address,
            amount: self.server_info.dust,
            assets: Vec::new(), // no assets to receiver = burn
        }];
        let packet = create_asset_packet(
            &asset_inputs,
            &empty_receivers,
            &change_assets,
            change_output_index,
        )?;

        if let Some(packet) = packet {
            asset::packet::add_asset_packet_to_psbt(&mut ark_tx, &packet);
        }

        // 6. Sign, submit, finalize.
        for i in 0..checkpoint_txs.len() {
            sign_ark_transaction(self.make_sign_fn(), &mut ark_tx, i)?;
        }

        let ark_txid = ark_tx.unsigned_tx.compute_txid();

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

        self.sign_and_finalize_pending_tx(pending_tx).await?;

        let used_pk = change_address_vtxo.owner_pk();
        if let Err(err) = self.inner.key_provider.mark_as_used(&used_pk) {
            tracing::warn!(
                "Failed updating keypair cache for used change address: {:?}",
                err
            );
        }

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
        let asset_inputs = vtxo_inputs
            .into_iter()
            .map(|input| AssetBearingVtxoInput {
                input,
                assets: Vec::new(),
            })
            .collect::<Vec<_>>();
        let receivers = vec![SendReceiver {
            address,
            amount,
            assets: Vec::new(),
        }];
        let pending_tx = self.submit_asset_send(asset_inputs, receivers).await?;
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
        let asset_inputs = vtxo_inputs
            .into_iter()
            .map(|input| AssetBearingVtxoInput {
                input,
                assets: Vec::new(),
            })
            .collect::<Vec<_>>();
        let receivers = vec![SendReceiver {
            address,
            amount,
            assets: Vec::new(),
        }];
        let pending_tx = self.submit_asset_send(asset_inputs, receivers).await?;
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
                assets: vtxo.assets.clone(),
            })
            .collect::<Vec<_>>();

        let selected = select_vtxos(spendable, amount, self.server_info.dust, true)
            .map_err(Error::from)
            .context("failed to select coins")?;

        self.build_vtxo_inputs(selected, &script_pubkey_to_vtxo_map)
    }

    /// Filter VTXOs by outpoints and build [`send::VtxoInput`]s, returning the selected coins
    /// and total amount.
    async fn select_vtxo_inputs_with_total(
        &self,
        vtxo_outpoints: &[OutPoint],
    ) -> Result<
        (
            Vec<ark_core::coin_select::VirtualTxOutPoint>,
            Vec<send::VtxoInput>,
            Amount,
        ),
        Error,
    > {
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
                assets: vtxo.assets.clone(),
            })
            .collect();

        if selected.is_empty() {
            return Err(Error::ad_hoc("no matching VTXO outpoints found"));
        }

        let total = selected.iter().fold(Amount::ZERO, |acc, v| acc + v.amount);
        let inputs = self.build_vtxo_inputs(selected.clone(), &script_pubkey_to_vtxo_map)?;
        Ok((selected, inputs, total))
    }

    /// Convert selected [`VirtualTxOutPoint`]s into [`send::VtxoInput`]s.
    fn build_vtxo_inputs(
        &self,
        selected: Vec<ark_core::coin_select::VirtualTxOutPoint>,
        script_pubkey_to_vtxo_map: &HashMap<bitcoin::ScriptBuf, ark_core::Vtxo>,
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

    /// Convert selected [`VirtualTxOutPoint`]s into [`AssetBearingVtxoInput`]s.
    fn build_asset_bearing_vtxo_inputs(
        &self,
        selected: Vec<ark_core::coin_select::VirtualTxOutPoint>,
        script_pubkey_to_vtxo_map: &HashMap<bitcoin::ScriptBuf, ark_core::Vtxo>,
    ) -> Result<Vec<AssetBearingVtxoInput>, Error> {
        let vtxo_inputs = self.build_vtxo_inputs(selected.clone(), script_pubkey_to_vtxo_map)?;
        Ok(vtxo_inputs
            .into_iter()
            .zip(selected.into_iter())
            .map(|(input, coin)| AssetBearingVtxoInput {
                input,
                assets: coin.assets,
            })
            .collect())
    }

    /// Sign and submit a prebuilt offchain transaction to the server without finalizing.
    ///
    /// Returns the pending transaction payload from the server. The change-address key is marked
    /// as used.
    async fn submit_built_offchain_send(
        &self,
        mut ark_tx: bitcoin::Psbt,
        checkpoint_txs: Vec<bitcoin::Psbt>,
        used_pk: XOnlyPublicKey,
    ) -> Result<PendingTx, Error> {
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

        if let Err(err) = self.inner.key_provider.mark_as_used(&used_pk) {
            tracing::warn!(
                "Failed updating keypair cache for used change address: {:?}",
                err
            );
        }

        Ok(pending_tx)
    }

    /// Build, sign the Ark transaction, and submit to the server *without* finalizing.
    async fn submit_asset_send(
        &self,
        inputs: Vec<AssetBearingVtxoInput>,
        receivers: Vec<SendReceiver>,
    ) -> Result<PendingTx, Error> {
        let (change_address, change_address_vtxo) = self.get_offchain_address()?;
        let send::SendTransactions {
            ark_tx,
            checkpoint_txs,
        } = build_send_transactions(&receivers, &change_address, &inputs, &self.server_info)
            .map_err(Error::from)
            .context("failed to build offchain asset-send transactions")?;

        self.submit_built_offchain_send(ark_tx, checkpoint_txs, change_address_vtxo.owner_pk())
            .await
    }

    /// Sign checkpoint transactions from a [`PendingTx`] and finalize.
    async fn sign_and_finalize_pending_tx(&self, pending_tx: PendingTx) -> Result<(), Error> {
        let ark_txid = pending_tx.ark_txid;
        let mut signed_checkpoint_txs = pending_tx.signed_checkpoint_txs;

        // Build a map from checkpoint txid -> ark tx input index so we can
        // restore witness scripts that the server may have stripped.
        let ark_input_idx_by_cp_txid: HashMap<_, _> = pending_tx
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

    /// Finalize an offchain transaction, retrying on transient failures.
    ///
    /// After submit succeeds but before finalize completes, a network error
    /// would leave the transaction in a pending state. Retrying here avoids
    /// that without needing full recovery via [`Self::continue_pending_offchain_txs`].
    pub(crate) async fn finalize_with_retry(
        &self,
        ark_txid: Txid,
        signed_checkpoint_txs: Vec<bitcoin::Psbt>,
    ) -> Result<(), Error> {
        const MAX_RETRIES: usize = 3;

        let mut last_err = None;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_millis(500 * (1 << (attempt - 1)));
                tracing::warn!(
                    %ark_txid,
                    attempt,
                    ?delay,
                    "Retrying finalize after transient failure"
                );
                crate::utils::sleep(delay).await;
            }

            match timeout_op(
                self.inner.timeout,
                self.network_client()
                    .finalize_offchain_transaction(ark_txid, signed_checkpoint_txs.clone()),
            )
            .await
            {
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(e)) => {
                    last_err = Some(Error::ark_server(e));
                }
                Err(e) => {
                    last_err = Some(e);
                }
            }
        }

        Err(last_err
            .expect("at least one attempt was made")
            .context("failed to finalize offchain transaction after retries"))
    }

    /// Fetch pending offchain transactions from the server.
    async fn fetch_pending_offchain_txs(&self) -> Result<Vec<PendingTx>, Error> {
        const MAX_INPUTS_PER_INTENT: usize = 20;

        let ark_addresses = self.get_offchain_addresses()?;

        let script_pubkey_to_vtxo_map: HashMap<_, _> = ark_addresses
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
        let mut seen_ark_txids = HashSet::new();

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

/// Build an asset packet for a transfer (not issuance).
///
/// Groups transfers by asset ID, mapping input indices to their asset amounts
/// and output/receiver indices to the requested asset amounts. Returns `None`
/// if there are no assets to transfer.
pub fn create_asset_packet(
    asset_inputs: &HashMap<u16, Vec<Asset>>,
    receivers: &[SendReceiver],
    change_assets: &[Asset],
    change_output_index: usize,
) -> Result<Option<asset::packet::Packet>, Error> {
    // Collect all transfers grouped by asset ID.
    struct AssetTransfer {
        inputs: Vec<asset::packet::AssetInput>,
        outputs: Vec<asset::packet::AssetOutput>,
    }

    let mut transfers: HashMap<AssetId, AssetTransfer> = HashMap::new();

    // Map inputs.
    for (input_index, assets) in asset_inputs {
        for asset in assets {
            let transfer = transfers
                .entry(asset.asset_id)
                .or_insert_with(|| AssetTransfer {
                    inputs: Vec::new(),
                    outputs: Vec::new(),
                });
            transfer.inputs.push(asset::packet::AssetInput {
                input_index: *input_index,
                amount: asset.amount,
            });
        }
    }

    // Map receiver outputs.
    for (receiver_index, receiver) in receivers.iter().enumerate() {
        for asset in &receiver.assets {
            let transfer = transfers.get_mut(&asset.asset_id).ok_or_else(|| {
                Error::ad_hoc(format!(
                    "receiver references asset {} that is not present in selected inputs",
                    asset.asset_id
                ))
            })?;
            transfer.outputs.push(asset::packet::AssetOutput {
                output_index: receiver_index as u16,
                amount: asset.amount,
            });
        }
    }

    // Map change outputs.
    for asset in change_assets {
        if let Some(transfer) = transfers.get_mut(&asset.asset_id) {
            transfer.outputs.push(asset::packet::AssetOutput {
                output_index: change_output_index as u16,
                amount: asset.amount,
            });
        }
    }

    if transfers.is_empty() {
        return Ok(None);
    }

    let groups: Vec<asset::packet::AssetGroup> = transfers
        .into_iter()
        .map(|(asset_id, transfer)| {
            Ok(asset::packet::AssetGroup {
                asset_id: Some(asset_id),
                control_asset: None,
                metadata: None,
                inputs: transfer.inputs,
                outputs: transfer.outputs,
            })
        })
        .collect::<Result<Vec<_>, Error>>()?;

    Ok(Some(asset::packet::Packet { groups }))
}

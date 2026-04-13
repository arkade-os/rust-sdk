use crate::error::ErrorContext;
use crate::swap_storage::SwapStorage;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::asset::AssetId;
use ark_core::asset::ControlAssetConfig;
use ark_core::coin_select::select_vtxos;
use ark_core::coin_select::select_vtxos_for_asset;
use ark_core::coin_select::VirtualTxOutPoint;
use ark_core::send::build_asset_burn_transactions;
use ark_core::send::build_asset_reissuance_transactions;
use ark_core::send::build_self_asset_issuance_transactions;
use ark_core::send::AssetReissuanceTransactions;
use ark_core::send::SelfAssetIssuanceTransactions;
use bitcoin::Amount;
use bitcoin::ScriptBuf;
use bitcoin::Txid;
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
        let (spendable, script_pubkey_to_vtxo_map) = self.spendable_virtual_vtxos().await?;

        let selected_coins = select_vtxos(
            spendable,
            self.server_info.dust,
            self.server_info.dust,
            true,
        )
        .map_err(Error::from)
        .context("failed to select coins for asset issuance")?;

        let issuance_inputs = self.build_vtxo_inputs(selected_coins, &script_pubkey_to_vtxo_map)?;
        let (change_address, change_address_vtxo) = self.get_offchain_address()?;

        let SelfAssetIssuanceTransactions {
            ark_tx,
            checkpoint_txs,
            asset_ids,
        } = build_self_asset_issuance_transactions(
            &own_address,
            &change_address,
            &issuance_inputs,
            &self.server_info,
            amount,
            control_asset_config,
            metadata,
        )
        .map_err(Error::from)
        .context("failed to build asset issuance transactions")?;

        let pending_tx = self
            .submit_built_offchain_send(ark_tx, checkpoint_txs, change_address_vtxo.owner_pk())
            .await
            .context("failed to submit asset issuance transaction")?;

        let ark_txid = pending_tx.ark_txid;
        self.sign_and_finalize_pending_tx(pending_tx)
            .await
            .context("failed to finalize asset issuance transaction")?;

        Ok(IssueAssetResult {
            ark_txid,
            asset_ids,
        })
    }

    /// Reissue additional units of an existing asset.
    ///
    /// The asset must have been created with a control asset. The control asset is spent as input
    /// and sent back to the caller, while the new asset units are minted.
    pub async fn reissue_asset(&self, asset_id: AssetId, amount: u64) -> Result<Txid, Error> {
        if amount == 0 {
            return Err(Error::ad_hoc("reissue amount must be > 0"));
        }

        let asset_info = self
            .get_asset(asset_id)
            .await
            .context("failed to get asset info")?;

        let control_asset_id = asset_info.control_asset_id.ok_or_else(|| {
            Error::ad_hoc(format!(
                "Asset {} can't be reissued, no control asset",
                asset_id
            ))
        })?;

        let (spendable, script_pubkey_to_vtxo_map) = self.spendable_virtual_vtxos().await?;

        let (control_coins, _control_change) =
            select_vtxos_for_asset(&spendable, 1, control_asset_id)
                .map_err(Error::from)
                .context("failed to select control asset for reissuance")?;

        let mut selected_outpoints: HashSet<_> =
            control_coins.iter().map(|coin| coin.outpoint).collect();
        let mut selected = control_coins;
        let btc_provided: Amount = selected.iter().map(|coin| coin.amount).sum();
        let btc_shortfall = self
            .server_info
            .dust
            .checked_sub(btc_provided)
            .unwrap_or(Amount::ZERO);

        if btc_shortfall > Amount::ZERO {
            let available: Vec<_> = spendable
                .iter()
                .filter(|coin| !selected_outpoints.contains(&coin.outpoint))
                .cloned()
                .collect();

            let btc_coins = select_vtxos(available, btc_shortfall, self.server_info.dust, true)
                .map_err(Error::from)
                .context("failed to select BTC coins for reissuance")?;

            for coin in btc_coins {
                if selected_outpoints.insert(coin.outpoint) {
                    selected.push(coin);
                }
            }
        }

        let reissuance_inputs = self.build_vtxo_inputs(selected, &script_pubkey_to_vtxo_map)?;
        let (self_address, _) = self.get_offchain_address()?;
        let (change_address, change_address_vtxo) = self.get_offchain_address()?;

        let AssetReissuanceTransactions {
            ark_tx,
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

        let pending_tx = self
            .submit_built_offchain_send(ark_tx, checkpoint_txs, change_address_vtxo.owner_pk())
            .await
            .context("failed to submit reissuance transaction")?;

        let ark_txid = pending_tx.ark_txid;
        self.sign_and_finalize_pending_tx(pending_tx)
            .await
            .context("failed to finalize reissuance transaction")?;

        Ok(ark_txid)
    }

    /// Burn a specific amount of an asset.
    pub async fn burn_asset(&self, asset_id: AssetId, amount: u64) -> Result<Txid, Error> {
        if amount == 0 {
            return Err(Error::ad_hoc("burn amount must be > 0"));
        }

        let (spendable, script_pubkey_to_vtxo_map) = self.spendable_virtual_vtxos().await?;

        let (asset_coins, asset_change) = select_vtxos_for_asset(&spendable, amount, asset_id)
            .map_err(Error::from)
            .context("failed to select coins for asset burn")?;

        let mut selected_outpoints: HashSet<_> =
            asset_coins.iter().map(|coin| coin.outpoint).collect();
        let mut selected = asset_coins;

        let mut carries_asset_change = asset_change > 0;
        for coin in &selected {
            if coin.assets.iter().any(|asset| asset.asset_id != asset_id) {
                carries_asset_change = true;
                break;
            }
        }

        let btc_provided: Amount = selected.iter().map(|coin| coin.amount).sum();
        let mut btc_needed = self.server_info.dust;
        if carries_asset_change {
            btc_needed += self.server_info.dust;
        }

        let btc_shortfall = btc_needed.checked_sub(btc_provided).unwrap_or(Amount::ZERO);
        if btc_shortfall > Amount::ZERO {
            let available: Vec<_> = spendable
                .iter()
                .filter(|coin| !selected_outpoints.contains(&coin.outpoint))
                .cloned()
                .collect();

            let btc_coins = select_vtxos(available, btc_shortfall, self.server_info.dust, true)
                .map_err(Error::from)
                .context("failed to select BTC coins for asset burn")?;

            for coin in btc_coins {
                if selected_outpoints.insert(coin.outpoint) {
                    selected.push(coin);
                }
            }
        }

        let burn_inputs = self.build_vtxo_inputs(selected, &script_pubkey_to_vtxo_map)?;
        let (own_address, _) = self.get_offchain_address()?;
        let (change_address, change_address_vtxo) = self.get_offchain_address()?;

        let offchain = build_asset_burn_transactions(
            &own_address,
            &change_address,
            &burn_inputs,
            &self.server_info,
            asset_id,
            amount,
        )
        .map_err(Error::from)
        .context("failed to build asset burn transactions")?;

        let pending_tx = self
            .submit_built_offchain_send(
                offchain.ark_tx,
                offchain.checkpoint_txs,
                change_address_vtxo.owner_pk(),
            )
            .await
            .context("failed to submit asset burn transaction")?;

        let ark_txid = pending_tx.ark_txid;
        self.sign_and_finalize_pending_tx(pending_tx)
            .await
            .context("failed to finalize asset burn transaction")?;

        Ok(ark_txid)
    }

    async fn spendable_virtual_vtxos(
        &self,
    ) -> Result<(Vec<VirtualTxOutPoint>, HashMap<ScriptBuf, ark_core::Vtxo>), Error> {
        let (vtxo_list, script_pubkey_to_vtxo_map) =
            self.list_vtxos().await.context("failed to list VTXOs")?;

        let spendable = vtxo_list
            .spendable_offchain()
            .map(|vtxo| VirtualTxOutPoint {
                outpoint: vtxo.outpoint,
                script_pubkey: vtxo.script.clone(),
                expire_at: vtxo.expires_at,
                amount: vtxo.amount,
                assets: vtxo.assets.clone(),
            })
            .collect();

        Ok((spendable, script_pubkey_to_vtxo_map))
    }
}

use crate::asset::AssetId;
use crate::server::Asset;
use crate::Error;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;

#[derive(Clone, Debug)]
pub struct VirtualTxOutPoint {
    pub outpoint: OutPoint,
    pub script_pubkey: ScriptBuf,
    pub expire_at: i64,
    pub amount: Amount,
    pub assets: Vec<Asset>,
}

/// Select VTXOs to be used as inputs in Ark transactions.
pub fn select_vtxos(
    mut virtual_tx_outpoints: Vec<VirtualTxOutPoint>,
    amount: Amount,
    dust: Amount,
    sort_by_expiration_time: bool,
) -> Result<Vec<VirtualTxOutPoint>, Error> {
    let mut selected = Vec::new();
    let mut not_selected = Vec::new();
    let mut selected_amount = Amount::ZERO;

    if sort_by_expiration_time {
        // Sort vtxos by expiration (older first)
        virtual_tx_outpoints.sort_by(|a, b| a.expire_at.cmp(&b.expire_at));
    }

    // Process VTXOs
    for virtual_tx_outpoint in virtual_tx_outpoints {
        if selected_amount >= amount {
            not_selected.push(virtual_tx_outpoint);
        } else {
            selected.push(virtual_tx_outpoint.clone());
            selected_amount += virtual_tx_outpoint.amount;
        }
    }

    if selected_amount < amount {
        return Err(Error::coin_select(format!(
            "insufficient funds: selected = {selected_amount}, needed = {amount}"
        )));
    }

    // Try to avoid generating dust.
    let change_amount = selected_amount - amount;
    if let Some(vtxo) = not_selected.first() {
        if change_amount < dust {
            selected.push(vtxo.clone());
        }
    }

    Ok(selected)
}

/// Select VTXOs that hold a specific asset, accumulating until `amount` is reached.
///
/// Returns the selected VTXOs and the asset change amount.
pub fn select_vtxos_for_asset(
    mut virtual_tx_outpoints: Vec<VirtualTxOutPoint>,
    amount: u64,
    asset_id: AssetId,
) -> Result<(Vec<VirtualTxOutPoint>, u64), Error> {
    // Filter to only VTXOs containing this asset.
    let mut candidates: Vec<VirtualTxOutPoint> = virtual_tx_outpoints
        .drain(..)
        .filter(|v| v.assets.iter().any(|a| a.asset_id == asset_id))
        .collect();

    // Sort by expiration (older first).
    candidates.sort_by(|a, b| a.expire_at.cmp(&b.expire_at));

    let mut selected = Vec::new();
    let mut selected_amount: u64 = 0;

    for vtxo in candidates {
        if selected_amount >= amount {
            break;
        }
        let asset_amt = vtxo
            .assets
            .iter()
            .find(|a| a.asset_id == asset_id)
            .map(|a| a.amount)
            .unwrap_or(0);
        selected_amount += asset_amt;
        selected.push(vtxo);
    }

    if selected_amount < amount {
        return Err(Error::coin_select(format!(
            "insufficient asset funds for {asset_id}: selected = {selected_amount}, needed = {amount}"
        )));
    }

    let change = selected_amount - amount;
    Ok((selected, change))
}

// Tests for the coin selection function
#[cfg(test)]
mod tests {
    use super::*;

    fn vtxo(expire_at: i64, amount: Amount) -> VirtualTxOutPoint {
        VirtualTxOutPoint {
            outpoint: OutPoint::default(),
            script_pubkey: ScriptBuf::new(),
            expire_at,
            amount,
            assets: Vec::new(),
        }
    }

    #[test]
    fn test_basic_coin_selection() {
        let vtxos = vec![vtxo(123456789, Amount::from_sat(3000))];

        let result = select_vtxos(vtxos, Amount::from_sat(2500), Amount::from_sat(100), true);
        assert!(result.is_ok());

        let selected = result.unwrap();
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn test_insufficient_funds() {
        let vtxos = vec![vtxo(123456789, Amount::from_sat(100))];

        let result = select_vtxos(vtxos, Amount::from_sat(1000), Amount::from_sat(50), true);
        assert!(result.is_err());
    }
}

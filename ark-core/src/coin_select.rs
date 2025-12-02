use crate::Error;
use bitcoin::Amount;
use bitcoin::OutPoint;

#[derive(Clone, Debug)]
pub struct VirtualTxOutPoint {
    pub outpoint: OutPoint,
    pub expire_at: i64,
    pub amount: Amount,
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
    if change_amount < dust
        && let Some(vtxo) = not_selected.first()
    {
        selected.push(vtxo.clone());
    }

    Ok(selected)
}

// Tests for the coin selection function
#[cfg(test)]
mod tests {
    use super::*;

    fn vtxo(expire_at: i64, amount: Amount) -> VirtualTxOutPoint {
        VirtualTxOutPoint {
            expire_at,
            amount,
            outpoint: OutPoint::default(),
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

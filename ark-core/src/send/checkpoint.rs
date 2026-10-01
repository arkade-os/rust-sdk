use crate::Error;
use bitcoin::Psbt;
use std::collections::HashMap;

/// Bind the checkpoint transactions returned by the server to the ones the client built, before
/// signing them.
///
/// Every returned checkpoint must have the unsigned transaction of exactly one expected
/// checkpoint, and every expected checkpoint must be returned. Matching is by txid, not by
/// position, and the result keeps the server's order.
///
/// Each returned entry is the expected PSBT carrying the server's signatures: prevouts, spend
/// leaves and witness scripts are taken exclusively from `expected`, never from the server's
/// response, so the client only ever signs what it built.
pub fn bind_checkpoint_transactions(
    expected: &[Psbt],
    returned: Vec<Psbt>,
) -> Result<Vec<Psbt>, Error> {
    if returned.len() != expected.len() {
        return Err(Error::transaction(format!(
            "server returned {} checkpoint transactions, expected {}",
            returned.len(),
            expected.len()
        )));
    }

    let mut by_txid = expected
        .iter()
        .map(|psbt| (psbt.unsigned_tx.compute_txid(), psbt))
        .collect::<HashMap<_, _>>();
    if by_txid.len() != expected.len() {
        return Err(Error::transaction(
            "duplicate expected checkpoint transaction",
        ));
    }

    returned
        .into_iter()
        .enumerate()
        .map(|(i, returned)| {
            let txid = returned.unsigned_tx.compute_txid();
            // Removing on match keeps a duplicated server txid from satisfying two entries.
            let expected = by_txid.remove(&txid).ok_or_else(|| {
                Error::transaction(format!(
                    "checkpoint transaction {i} ({txid}) does not match any expected checkpoint"
                ))
            })?;

            let mut bound = expected.clone();
            for (bound_input, returned_input) in bound.inputs.iter_mut().zip(returned.inputs) {
                for (key, signature) in returned_input.tap_script_sigs {
                    bound_input.tap_script_sigs.entry(key).or_insert(signature);
                }
            }

            Ok(bound)
        })
        .collect()
}

#[cfg(test)]
mod tests;

use super::build_checkpoint_psbt;
use super::VtxoInput;
use crate::Error;
use bitcoin::Psbt;
use bitcoin::ScriptBuf;
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

/// Bind the checkpoint transactions of a pending Ark transaction by rebuilding them from the
/// client's own VTXOs.
///
/// Used when resuming a transaction submitted earlier, for which the originally built
/// checkpoints are no longer available. Each returned checkpoint must spend one of `inputs` and
/// be exactly the checkpoint the client would build for it with one of `exit_scripts`. Pass
/// [`crate::server::Info::checkpoint_exit_scripts`], so that transactions submitted before a
/// server signer rotation still match.
///
/// See [`bind_checkpoint_transactions`] for what the returned PSBTs contain.
pub fn bind_pending_checkpoint_transactions(
    inputs: &[VtxoInput],
    exit_scripts: &[ScriptBuf],
    returned: Vec<Psbt>,
) -> Result<Vec<Psbt>, Error> {
    let by_outpoint = inputs
        .iter()
        .map(|input| (input.outpoint(), input))
        .collect::<HashMap<_, _>>();

    let expected = returned
        .iter()
        .enumerate()
        .map(|(i, checkpoint)| {
            let [spent] = checkpoint.unsigned_tx.input.as_slice() else {
                return Err(Error::transaction(format!(
                    "checkpoint transaction {i} must spend exactly one input"
                )));
            };
            let outpoint = spent.previous_output;
            let input = by_outpoint.get(&outpoint).ok_or_else(|| {
                Error::transaction(format!(
                    "checkpoint transaction {i} spends {outpoint}, which is not one of our VTXOs"
                ))
            })?;

            let txid = checkpoint.unsigned_tx.compute_txid();
            for exit_script in exit_scripts {
                let (rebuilt, _) = build_checkpoint_psbt(input, exit_script.clone())?;
                if rebuilt.unsigned_tx.compute_txid() == txid {
                    return Ok(rebuilt);
                }
            }

            Err(Error::transaction(format!(
                "checkpoint transaction {i} ({txid}) differs from the checkpoint built for {outpoint}"
            )))
        })
        .collect::<Result<Vec<_>, _>>()?;

    bind_checkpoint_transactions(&expected, returned)
}

#[cfg(test)]
mod tests;

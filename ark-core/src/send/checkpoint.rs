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

/// Whether every submitted checkpoint signature was retained by the server.
/// Call on the raw response, before binding (which restores local signatures).
pub fn checkpoint_signatures_retained(expected: &[Psbt], returned: &[Psbt]) -> bool {
    !expected.is_empty()
        && expected.len() == returned.len()
        && expected.iter().all(|tx| {
            returned
                .iter()
                .find(|other| other.unsigned_tx == tx.unsigned_tx)
                .is_some_and(|other| {
                    !tx.inputs.is_empty()
                        && tx.inputs.len() == other.inputs.len()
                        && tx
                            .inputs
                            .iter()
                            .zip(&other.inputs)
                            .all(|(input, returned)| {
                                !input.tap_script_sigs.is_empty()
                                    && input.tap_script_sigs.iter().all(|(key, sig)| {
                                        returned.tap_script_sigs.get(key) == Some(sig)
                                    })
                            })
                })
        })
}

#[cfg(test)]
mod oneshot_tests {
    use super::*;

    #[test]
    fn detects_retention_before_binding_restores_local_signatures() {
        use bitcoin::absolute;
        use bitcoin::hashes::Hash;
        use bitcoin::secp256k1::Keypair;
        use bitcoin::secp256k1::Message;
        use bitcoin::secp256k1::Secp256k1;
        use bitcoin::secp256k1::SecretKey;
        use bitcoin::transaction;
        use bitcoin::Transaction;
        use bitcoin::TxIn;
        let mut signed = Psbt::from_unsigned_tx(Transaction {
            version: transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![TxIn::default()],
            output: vec![],
        })
        .unwrap();
        let secp = Secp256k1::new();
        let key = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[1; 32]).unwrap());
        let signature = bitcoin::taproot::Signature {
            signature: secp.sign_schnorr_no_aux_rand(&Message::from_digest([2; 32]), &key),
            sighash_type: bitcoin::TapSighashType::Default,
        };
        signed.inputs[0].tap_script_sigs.insert(
            (key.x_only_public_key().0, bitcoin::TapLeafHash::all_zeros()),
            signature,
        );
        assert!(checkpoint_signatures_retained(
            std::slice::from_ref(&signed),
            std::slice::from_ref(&signed)
        ));
        let mut dropped = signed.clone();
        dropped.inputs[0].tap_script_sigs.clear();
        assert!(!checkpoint_signatures_retained(
            std::slice::from_ref(&signed),
            std::slice::from_ref(&dropped)
        ));
        let bound =
            bind_checkpoint_transactions(std::slice::from_ref(&signed), vec![dropped]).unwrap();
        assert_eq!(
            bound[0].inputs[0].tap_script_sigs,
            signed.inputs[0].tap_script_sigs
        );
        let mut second = signed.clone();
        second.unsigned_tx.input[0].previous_output.vout = 1;
        let expected = vec![signed.clone(), second.clone()];
        assert!(checkpoint_signatures_retained(
            &expected,
            &[second.clone(), signed.clone()]
        ));
        second.inputs[0].tap_script_sigs.clear();
        assert!(!checkpoint_signatures_retained(
            &expected,
            &[second, signed.clone()]
        ));
        let mut altered = signed.clone();
        altered.inputs[0]
            .tap_script_sigs
            .values_mut()
            .next()
            .unwrap()
            .sighash_type = bitcoin::TapSighashType::All;
        assert!(!checkpoint_signatures_retained(
            std::slice::from_ref(&signed),
            &[altered]
        ));
        assert!(!checkpoint_signatures_retained(&[], &[]));
        assert!(!checkpoint_signatures_retained(&[signed], &[]));
    }
}

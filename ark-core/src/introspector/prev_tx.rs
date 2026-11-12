//! Previous-transaction PSBT fields the emulator needs to resolve an input's prevout.
//!
//! Since emulator v0.0.7, every input of an Ark transaction submitted to the emulator must carry
//! the raw transaction that created the coin it ultimately spends. Ark input `i` spends checkpoint
//! `i`, which spends the coin, so the field holds the coin's creating Ark transaction, not the
//! checkpoint. Without it the emulator rejects the submission with `missing prevout tx for input
//! N`.

use crate::Error;
use bitcoin::consensus::serialize;
use bitcoin::psbt;
use bitcoin::Psbt;
use bitcoin::Transaction;
use bitcoin::Witness;

/// PSBT key type of the Ark-specific `unknown` fields.
const ARK_PSBT_FIELD_KEY_TYPE: u8 = 222;

/// The byte value corresponds to the string "prevarktx".
pub const PREV_ARK_TX_KEY: [u8; 9] = *b"prevarktx";

/// Attach `prev_tx`, the transaction that created the coin spent through input `input_index`, to
/// that input of `psbt`.
///
/// The value is the serialized transaction without witnesses, which is what the emulator hashes
/// to match it against the coin's outpoint. An existing value is replaced, since the emulator
/// rejects an input carrying the field twice.
pub fn set_prev_ark_tx(
    psbt: &mut Psbt,
    input_index: usize,
    prev_tx: &Transaction,
) -> Result<(), Error> {
    let input_count = psbt.inputs.len();
    let input = psbt.inputs.get_mut(input_index).ok_or_else(|| {
        Error::ad_hoc(format!(
            "cannot set prev ark tx on input {input_index}: PSBT has {input_count} inputs"
        ))
    })?;

    let mut prev_tx = prev_tx.clone();
    for txin in prev_tx.input.iter_mut() {
        txin.witness = Witness::default();
    }

    input.unknown.insert(
        psbt::raw::Key {
            type_value: ARK_PSBT_FIELD_KEY_TYPE,
            key: PREV_ARK_TX_KEY.to_vec(),
        },
        serialize(&prev_tx),
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::absolute::LockTime;
    use bitcoin::consensus::deserialize;
    use bitcoin::transaction::Version;
    use bitcoin::Amount;
    use bitcoin::OutPoint;
    use bitcoin::ScriptBuf;
    use bitcoin::Sequence;
    use bitcoin::TxIn;
    use bitcoin::TxOut;

    fn tx_with_witness() -> Transaction {
        Transaction {
            version: Version(3),
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: OutPoint::null(),
                script_sig: ScriptBuf::new(),
                sequence: Sequence::MAX,
                witness: Witness::from_slice(&[vec![1u8; 64]]),
            }],
            output: vec![TxOut {
                value: Amount::from_sat(1_000),
                script_pubkey: ScriptBuf::new(),
            }],
        }
    }

    fn key() -> psbt::raw::Key {
        psbt::raw::Key {
            type_value: ARK_PSBT_FIELD_KEY_TYPE,
            key: PREV_ARK_TX_KEY.to_vec(),
        }
    }

    #[test]
    fn stores_the_tx_without_witnesses() {
        let prev_tx = tx_with_witness();
        let mut psbt = Psbt::from_unsigned_tx(Transaction {
            input: vec![TxIn::default()],
            ..tx_with_witness()
        })
        .unwrap();

        set_prev_ark_tx(&mut psbt, 0, &prev_tx).unwrap();

        let stored: Transaction = deserialize(&psbt.inputs[0].unknown[&key()]).unwrap();
        assert!(stored.input.iter().all(|txin| txin.witness.is_empty()));
        assert_eq!(stored.compute_txid(), prev_tx.compute_txid());
    }

    #[test]
    fn replaces_an_existing_value() {
        let mut psbt = Psbt::from_unsigned_tx(Transaction {
            input: vec![TxIn::default()],
            ..tx_with_witness()
        })
        .unwrap();
        psbt.inputs[0].unknown.insert(key(), vec![0xff]);

        set_prev_ark_tx(&mut psbt, 0, &tx_with_witness()).unwrap();

        assert_eq!(psbt.inputs[0].unknown.len(), 1);
        assert_ne!(psbt.inputs[0].unknown[&key()], vec![0xff]);
    }

    #[test]
    fn rejects_an_out_of_range_input() {
        let mut psbt = Psbt::from_unsigned_tx(Transaction {
            input: vec![TxIn::default()],
            ..tx_with_witness()
        })
        .unwrap();

        assert!(set_prev_ark_tx(&mut psbt, 1, &tx_with_witness()).is_err());
    }
}

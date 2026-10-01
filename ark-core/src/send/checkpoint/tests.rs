use super::*;
use bitcoin::absolute::LockTime;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::schnorr;
use bitcoin::secp256k1::Keypair;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::secp256k1::SecretKey;
use bitcoin::taproot::LeafVersion;
use bitcoin::taproot::Signature;
use bitcoin::transaction::Version;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use bitcoin::TapLeafHash;
use bitcoin::TapSighashType;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;

fn pk(n: u8) -> XOnlyPublicKey {
    Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
        .x_only_public_key()
        .0
}

fn checkpoint(n: u8) -> Psbt {
    let mut psbt = Psbt::from_unsigned_tx(Transaction {
        version: Version::non_standard(3),
        lock_time: LockTime::ZERO,
        input: vec![TxIn {
            previous_output: OutPoint::new(Txid::from_byte_array([n; 32]), 0),
            ..TxIn::default()
        }],
        output: vec![TxOut {
            value: Amount::from_sat(1000),
            script_pubkey: ScriptBuf::new(),
        }],
    })
    .unwrap();
    psbt.inputs[0].witness_utxo = Some(TxOut {
        value: Amount::from_sat(1000),
        script_pubkey: ScriptBuf::from_bytes(vec![n]),
    });
    psbt.inputs[0].witness_script = Some(ScriptBuf::from_bytes(vec![n, n]));
    psbt
}

fn leaf() -> TapLeafHash {
    TapLeafHash::from_script(&ScriptBuf::new(), LeafVersion::TapScript)
}

fn signature(n: u8) -> Signature {
    Signature {
        signature: schnorr::Signature::from_slice(&[n; 64]).unwrap(),
        sighash_type: TapSighashType::Default,
    }
}

fn server_signed(mut psbt: Psbt) -> Psbt {
    psbt.inputs[0]
        .tap_script_sigs
        .insert((pk(2), leaf()), signature(2));
    psbt
}

fn fails(expected: &[Psbt], returned: Vec<Psbt>, message: &str) {
    let error = bind_checkpoint_transactions(expected, returned)
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains(message), "{error}");
}

#[test]
fn binds_by_txid_in_server_order_keeping_only_server_signatures() {
    let expected = vec![checkpoint(1), checkpoint(2)];
    let mut tampered = server_signed(checkpoint(2));
    tampered.inputs[0].witness_utxo = None;
    tampered.inputs[0].witness_script = Some(ScriptBuf::from_bytes(vec![0xff]));
    tampered.inputs[0].tap_scripts.clear();

    let bound =
        bind_checkpoint_transactions(&expected, vec![tampered, server_signed(checkpoint(1))])
            .unwrap();

    assert_eq!(
        bound,
        vec![server_signed(checkpoint(2)), server_signed(checkpoint(1))]
    );
}

#[test]
fn keeps_signatures_already_present_in_expected_checkpoint() {
    let mut expected = checkpoint(1);
    expected.inputs[0]
        .tap_script_sigs
        .insert((pk(1), leaf()), signature(1));
    let mut returned = server_signed(checkpoint(1));
    returned.inputs[0]
        .tap_script_sigs
        .insert((pk(1), leaf()), signature(3));

    let bound = bind_checkpoint_transactions(&[expected], vec![returned]).unwrap();

    let sigs = &bound[0].inputs[0].tap_script_sigs;
    assert_eq!(sigs[&(pk(1), leaf())], signature(1));
    assert_eq!(sigs[&(pk(2), leaf())], signature(2));
}

#[test]
fn rejects_missing_extra_or_changed_checkpoints() {
    let expected = vec![checkpoint(1), checkpoint(2)];
    fails(
        &expected,
        vec![checkpoint(1)],
        "returned 1 checkpoint transactions, expected 2",
    );
    fails(
        &expected,
        vec![checkpoint(1), checkpoint(2), checkpoint(3)],
        "returned 3 checkpoint transactions, expected 2",
    );
    fails(
        &expected,
        vec![checkpoint(1), checkpoint(3)],
        "does not match any expected",
    );

    let mut changed = checkpoint(2);
    changed.unsigned_tx.output[0].value = Amount::from_sat(999);
    fails(
        &expected,
        vec![checkpoint(1), changed],
        "does not match any expected",
    );
}

#[test]
fn rejects_duplicated_checkpoints() {
    fails(
        &[checkpoint(1), checkpoint(2)],
        vec![checkpoint(1), checkpoint(1)],
        "does not match any expected",
    );
    fails(
        &[checkpoint(1), checkpoint(1)],
        vec![checkpoint(1), checkpoint(1)],
        "duplicate expected checkpoint",
    );
}

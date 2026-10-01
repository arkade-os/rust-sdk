use super::*;
use crate::script::multisig_3_of_3_script;
use crate::script::multisig_script;
use crate::send::sign_ark_transaction;
use crate::vhtlc::VhtlcOptions;
use bitcoin::absolute::LockTime;
use bitcoin::hashes::ripemd160;
use bitcoin::secp256k1::Keypair;
use bitcoin::secp256k1::SecretKey;
use bitcoin::taproot::Signature;
use bitcoin::taproot::TaprootBuilder;
use bitcoin::transaction::Version;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use bitcoin::Sequence;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::TxOut;
use bitcoin::Txid;

fn key(n: u8) -> Keypair {
    Keypair::from_secret_key(&Secp256k1::new(), &SecretKey::from_slice(&[n; 32]).unwrap())
}

fn pk(n: u8) -> XOnlyPublicKey {
    key(n).x_only_public_key().0
}

fn unsigned(scripts: &[ScriptBuf]) -> Psbt {
    let mut psbt = Psbt::from_unsigned_tx(Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: scripts
            .iter()
            .enumerate()
            .map(|(i, _)| TxIn {
                previous_output: OutPoint::new(Txid::from_byte_array([i as u8; 32]), 0),
                ..TxIn::default()
            })
            .collect(),
        output: vec![TxOut {
            value: Amount::from_sat(1000),
            script_pubkey: ScriptBuf::new(),
        }],
    })
    .unwrap();
    for (input, script) in psbt.inputs.iter_mut().zip(scripts) {
        let spend_info = TaprootBuilder::new()
            .add_leaf(0, script.clone())
            .unwrap()
            .finalize(&Secp256k1::new(), pk(9))
            .unwrap();
        input.witness_utxo = Some(TxOut {
            value: Amount::from_sat(2000),
            script_pubkey: ScriptBuf::new_p2tr_tweaked(spend_info.output_key()),
        });
        input.tap_scripts.insert(
            spend_info
                .control_block(&(script.clone(), LeafVersion::TapScript))
                .unwrap(),
            (script.clone(), LeafVersion::TapScript),
        );
    }
    psbt
}

fn sign(psbt: &mut Psbt, signer: u8, sighash_type: TapSighashType) {
    let prevouts = psbt
        .inputs
        .iter()
        .map(|i| i.witness_utxo.clone().unwrap())
        .collect::<Vec<_>>();
    for (i, input) in psbt.inputs.iter_mut().enumerate() {
        let (_, (script, version)) = input.tap_scripts.first_key_value().unwrap();
        let leaf = TapLeafHash::from_script(script, *version);
        let hash = SighashCache::new(&psbt.unsigned_tx)
            .taproot_script_spend_signature_hash(i, &Prevouts::All(&prevouts), leaf, sighash_type)
            .unwrap();
        let signature = Secp256k1::new()
            .sign_schnorr_no_aux_rand(&Message::from_digest(hash.to_byte_array()), &key(signer));
        input.tap_script_sigs.insert(
            (pk(signer), leaf),
            Signature {
                signature,
                sighash_type,
            },
        );
    }
}

fn fixture() -> (Psbt, Psbt) {
    // Different server keys per input also covers spending pre-rotation contracts.
    let mut submitted = unsigned(&[multisig_script(pk(1), pk(2)), multisig_script(pk(1), pk(3))]);
    for i in 0..submitted.inputs.len() {
        sign_ark_transaction(
            |_, message| {
                Ok(vec![(
                    Secp256k1::new().sign_schnorr_no_aux_rand(&message, &key(1)),
                    pk(1),
                )])
            },
            &mut submitted,
            i,
        )
        .unwrap();
    }
    let mut signed = submitted.clone();
    sign(&mut signed, 2, TapSighashType::Default);
    sign(&mut signed, 3, TapSighashType::Default);
    (submitted, signed)
}

fn fails(submitted: &Psbt, signed: &Psbt, message: &str) {
    let error = verify_signed_ark_transaction(submitted, signed)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(message),
        "expected {message:?}, got {error:?}"
    );
}

#[test]
fn verifies_every_input_using_original_metadata_and_historical_keys() {
    let (submitted, mut signed) = fixture();
    for input in &mut signed.inputs {
        input.tap_scripts.clear();
        input.witness_utxo = None;
    }
    verify_signed_ark_transaction(&submitted, &signed).unwrap();
}

#[test]
fn supports_escrow_and_all_collaborative_vhtlc_paths() {
    let options = VhtlcOptions {
        sender: pk(1),
        receiver: pk(2),
        server: pk(3),
        preimage_hash: ripemd160::Hash::from_byte_array([42; 20]),
        refund_locktime: 500,
        unilateral_claim_delay: Sequence::from_height(144),
        unilateral_refund_delay: Sequence::from_height(144),
        unilateral_refund_without_receiver_delay: Sequence::from_height(144),
    };
    let submitted = unsigned(&[
        multisig_3_of_3_script(pk(1), pk(2), pk(3)),
        options.claim_script(),
        options.refund_script(),
        options.refund_without_receiver_script(),
    ]);
    let mut signed = submitted.clone();
    for signer in 1..=3 {
        sign(&mut signed, signer, TapSighashType::All);
    }
    verify_signed_ark_transaction(&submitted, &signed).unwrap();
}

#[test]
fn rejects_changed_transaction() {
    let (submitted, mut signed) = fixture();
    signed.unsigned_tx.output[0].value = Amount::from_sat(999);
    fails(&submitted, &signed, "different Ark transaction");
}

#[test]
fn matching_txid_does_not_accept_missing_or_wrong_signatures() {
    let (submitted, signed) = fixture();
    let signature_key = *signed.inputs[1]
        .tap_script_sigs
        .keys()
        .find(|(key, _)| *key == pk(3))
        .unwrap();
    let mut missing = signed.clone();
    missing.inputs[1].tap_script_sigs.remove(&signature_key);
    assert_eq!(
        submitted.unsigned_tx.compute_txid(),
        missing.unsigned_tx.compute_txid()
    );
    fails(&submitted, &missing, "missing signature for input 1");
    let mut invalid = signed.clone();
    let wrong = signed.inputs[0]
        .tap_script_sigs
        .iter()
        .find(|((key, _), _)| *key == pk(3))
        .unwrap()
        .1;
    invalid.inputs[1]
        .tap_script_sigs
        .insert(signature_key, *wrong);
    fails(&submitted, &invalid, "invalid signature for input 1");
    let mut wrong_leaf = signed;
    let sig = wrong_leaf.inputs[1]
        .tap_script_sigs
        .remove(&signature_key)
        .unwrap();
    wrong_leaf.inputs[1]
        .tap_script_sigs
        .insert((pk(3), TapLeafHash::all_zeros()), sig);
    fails(&submitted, &wrong_leaf, "missing signature for input 1");
}

#[test]
fn rejects_signatures_over_server_supplied_prevouts() {
    let (submitted, mut signed) = fixture();
    signed.inputs[1].witness_utxo.as_mut().unwrap().value = Amount::from_sat(9999);
    for signer in 1..=3 {
        sign(&mut signed, signer, TapSighashType::Default);
    }
    fails(&submitted, &signed, "invalid signature for input 0");
}

#[test]
fn rejects_valid_signatures_that_do_not_commit_to_every_input_and_output() {
    let (submitted, signed) = fixture();
    for sighash_type in [TapSighashType::None, TapSighashType::AllPlusAnyoneCanPay] {
        let mut weak = signed.clone();
        sign(&mut weak, 3, sighash_type);
        fails(
            &submitted,
            &weak,
            "signature must commit to all inputs and outputs for input 1",
        );
    }
}

#[test]
fn rejects_malformed_submitted_metadata_without_panicking() {
    let (submitted, signed) = fixture();
    let mut missing = submitted.clone();
    missing.inputs[1].witness_utxo = None;
    fails(&missing, &signed, "missing submitted prevout for input 1");
    let mut wrong_leaf = submitted.clone();
    wrong_leaf.inputs[1].tap_scripts = submitted.inputs[0].tap_scripts.clone();
    fails(
        &wrong_leaf,
        &signed,
        "spend leaf does not match prevout for input 1",
    );
    let mut missing_leaf = submitted.clone();
    missing_leaf.inputs[1].tap_scripts.clear();
    fails(
        &missing_leaf,
        &signed,
        "expected one submitted spend leaf for input 1",
    );
    let mut truncated = signed.clone();
    truncated.inputs.pop();
    fails(
        &submitted,
        &truncated,
        "invalid Ark transaction input count",
    );
    let mut non_taproot = submitted;
    non_taproot.inputs[0]
        .witness_utxo
        .as_mut()
        .unwrap()
        .script_pubkey = ScriptBuf::new();
    fails(&non_taproot, &signed, "non-Taproot prevout for input 0");
}

#[test]
fn rejects_unsupported_signature_scripts() {
    for opcode in [OP_CHECKSIGADD, OP_CODESEPARATOR] {
        let script = ScriptBuf::builder()
            .push_opcode(opcode)
            .push_x_only_key(&pk(1))
            .push_opcode(OP_CHECKSIG)
            .into_script();
        let submitted = unsigned(&[script]);
        fails(&submitted, &submitted, "unsupported signature opcode");
    }
    let submitted = unsigned(&[ScriptBuf::builder().push_opcode(OP_CHECKSIG).into_script()]);
    fails(
        &submitted,
        &submitted,
        "expected literal CHECKSIG public keys",
    );
}

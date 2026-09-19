use super::*;
use ark_core::script::multisig_script;
use ark_core::send::sign_ark_transaction;
use bitcoin::absolute::LockTime;
use bitcoin::secp256k1::Keypair;
use bitcoin::secp256k1::Secp256k1;
use bitcoin::secp256k1::SecretKey;
use bitcoin::taproot::LeafVersion;
use bitcoin::taproot::TaprootBuilder;
use bitcoin::transaction::Version;
use bitcoin::Amount;
use bitcoin::ScriptBuf;
use bitcoin::Transaction;
use bitcoin::TxIn;
use bitcoin::TxOut;

fn fixture() -> (Psbt, Psbt) {
    let secp = Secp256k1::new();
    let party = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[1; 32]).unwrap());
    let server = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[2; 32]).unwrap());
    let script = multisig_script(party.x_only_public_key().0, server.x_only_public_key().0);
    let info = TaprootBuilder::new()
        .add_leaf(0, script.clone())
        .unwrap()
        .finalize(&secp, party.x_only_public_key().0)
        .unwrap();
    let mut submitted = Psbt::from_unsigned_tx(Transaction {
        version: Version::TWO,
        lock_time: LockTime::ZERO,
        input: vec![TxIn::default()],
        output: vec![TxOut {
            value: Amount::from_sat(1000),
            script_pubkey: ScriptBuf::new(),
        }],
    })
    .unwrap();
    submitted.inputs[0].witness_utxo = Some(TxOut {
        value: Amount::from_sat(2000),
        script_pubkey: ScriptBuf::new_p2tr_tweaked(info.output_key()),
    });
    submitted.inputs[0].tap_scripts.insert(
        info.control_block(&(script.clone(), LeafVersion::TapScript))
            .unwrap(),
        (script, LeafVersion::TapScript),
    );
    sign_ark_transaction(
        |_, msg| {
            Ok(vec![(
                secp.sign_schnorr_no_aux_rand(&msg, &party),
                party.x_only_public_key().0,
            )])
        },
        &mut submitted,
        0,
    )
    .unwrap();
    let mut signed = submitted.clone();
    sign_ark_transaction(
        |_, msg| {
            Ok(vec![(
                secp.sign_schnorr_no_aux_rand(&msg, &server),
                server.x_only_public_key().0,
            )])
        },
        &mut signed,
        0,
    )
    .unwrap();
    (submitted, signed)
}

fn encoded(psbt: &Psbt) -> String {
    base64::engine::general_purpose::STANDARD.encode(psbt.serialize())
}

fn response(ark_tx: &Psbt) -> models::SubmitTxResponse {
    serde_json::from_value(serde_json::json!({
        "arkTxid": ark_tx.unsigned_tx.compute_txid().to_string(),
        "finalArkTx": encoded(ark_tx),
        "signedCheckpointTxs": [],
    }))
    .unwrap()
}

#[test]
fn submit_response_returns_verified_transaction_and_checkpoint_payloads() {
    let (submitted, signed) = fixture();
    let mut response = response(&signed);
    response.signed_checkpoint_txs = Some(vec![encoded(&submitted)]);
    let result = Client::decode_submit_response(&submitted, response).unwrap();
    assert_eq!(result.signed_ark_tx, signed);
    assert_eq!(result.signed_checkpoint_txs, vec![submitted]);
}

#[test]
fn submit_response_rejects_changed_transaction() {
    let (submitted, mut signed) = fixture();
    signed.unsigned_tx.output[0].value = Amount::from_sat(999);
    let error = Client::decode_submit_response(&submitted, response(&signed))
        .err()
        .unwrap();
    assert!(error
        .source()
        .unwrap()
        .to_string()
        .contains("different Ark transaction"));
}

#[test]
fn matching_txid_does_not_accept_missing_or_invalid_server_signature() {
    let (submitted, mut signed) = fixture();
    let error = Client::decode_submit_response(&submitted, response(&submitted))
        .err()
        .unwrap();
    assert!(error
        .source()
        .unwrap()
        .to_string()
        .contains("missing signature for input 0"));

    // Replace the server signature with the party signature, keeping the same txid.
    let (&party_key, &party_signature) = submitted.inputs[0]
        .tap_script_sigs
        .first_key_value()
        .unwrap();
    for (key, signature) in &mut signed.inputs[0].tap_script_sigs {
        if *key != party_key {
            *signature = party_signature;
        }
    }
    assert_eq!(
        submitted.unsigned_tx.compute_txid(),
        signed.unsigned_tx.compute_txid()
    );
    let error = Client::decode_submit_response(&submitted, response(&signed))
        .err()
        .unwrap();
    assert!(error
        .source()
        .unwrap()
        .to_string()
        .contains("invalid signature for input 0"));
}

#[test]
fn submit_response_rejects_missing_or_malformed_ark_transaction() {
    let (submitted, signed) = fixture();
    for final_ark_tx in [
        None,
        Some("not base64!".to_owned()),
        Some("AA==".to_owned()),
    ] {
        let mut response = response(&signed);
        response.final_ark_tx = final_ark_tx;
        assert!(Client::decode_submit_response(&submitted, response).is_err());
    }
}

#[test]
fn submit_response_still_requires_decodable_checkpoints() {
    let (submitted, signed) = fixture();
    let mut response = response(&signed);
    response.signed_checkpoint_txs = None;
    let error = Client::decode_submit_response(&submitted, response.clone())
        .err()
        .unwrap();
    assert!(error
        .source()
        .unwrap()
        .to_string()
        .contains("Signed checkpoint tx not received"));
    response.signed_checkpoint_txs = Some(vec!["AA==".to_owned()]);
    assert!(Client::decode_submit_response(&submitted, response).is_err());
}

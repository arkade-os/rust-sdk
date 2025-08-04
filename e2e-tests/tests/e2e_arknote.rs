#![allow(clippy::unwrap_used)]

use ark_core::ArkNote;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::thread_rng;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn e2e_arknote_redemption() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let alice_offchain_address = alice.get_offchain_address().unwrap().0;

    tracing::info!(
        ?alice_offchain_address,
        "Created Alice's wallet for ArkNote redemption test"
    );

    let fund_amount = Amount::from_sat(1000);
    let preimage = [42u8; 32];
    let arknote = ArkNote::new(preimage, fund_amount);

    tracing::info!(
        arknote_string = %arknote.to_encoded_string(),
        value = %fund_amount,
        "Created ArkNote for redemption test"
    );

    let parsed_note = ArkNote::from_string(&arknote.to_encoded_string()).unwrap();
    assert_eq!(parsed_note.value(), fund_amount);
    assert_eq!(parsed_note.preimage(), &preimage);
    tracing::info!("Successfully parsed ArkNote from string");

    assert_eq!(arknote.value(), fund_amount);
    assert_eq!(arknote.vout(), 0);
    assert!(arknote.status().confirmed);
    assert!(arknote.extra_witness().is_some());

    // Verify tap tree is not empty
    let tap_tree = arknote.tap_tree();
    assert!(!tap_tree.is_empty());

    // Verify outpoint creation
    let outpoint = arknote.outpoint();
    assert_eq!(outpoint.vout, 0);

    // Verify TxOut creation
    let tx_out = arknote.to_tx_out();
    assert_eq!(tx_out.value, fund_amount);

    // Redeem the ArkNote using the new redeem_note method
    let mut rng = thread_rng();
    let txid_opt = alice.redeem_note(&mut rng, arknote).await.unwrap();

    assert!(
        txid_opt.is_some(),
        "Expected a transaction ID from ArkNote redemption"
    );
    let txid = txid_opt.unwrap();

    tracing::info!(
        %txid,
        "Successfully redeemed ArkNote"
    );

    // Verify the balance has been updated
    let balance = alice.offchain_balance().await.unwrap();
    tracing::info!(
        confirmed_balance = %balance.confirmed(),
        pending_balance = %balance.pending(),
        "Balance after ArkNote redemption"
    );

    tracing::info!("ArkNote redemption test completed successfully");
}

#[tokio::test]
#[ignore]
pub async fn e2e_multiple_arknotes_redemption() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();

    // Create a test wallet
    let (alice, _) = set_up_client("alice_multi".to_string(), nigiri.clone(), secp.clone()).await;
    let alice_offchain_address = alice.get_offchain_address().unwrap().0;

    tracing::info!(
        ?alice_offchain_address,
        "Created Alice's wallet for multiple ArkNotes redemption test"
    );

    // Create multiple test ArkNotes
    let note1 = ArkNote::new([1u8; 32], Amount::from_sat(500));
    let note2 = ArkNote::new([2u8; 32], Amount::from_sat(750));
    let note3 = ArkNote::new([3u8; 32], Amount::from_sat(250));

    let total_amount = note1.value() + note2.value() + note3.value();
    let notes = vec![note1, note2, note3];

    tracing::info!(
        note_count = notes.len(),
        total_amount = %total_amount,
        "Created multiple ArkNotes for redemption test"
    );

    // Redeem multiple ArkNotes using the redeem_notes method
    let mut rng = thread_rng();
    let txid_opt = alice.redeem_notes(&mut rng, notes).await.unwrap();

    assert!(
        txid_opt.is_some(),
        "Expected a transaction ID from multiple ArkNotes redemption"
    );
    let txid = txid_opt.unwrap();

    tracing::info!(
        %txid,
        %total_amount,
        "Successfully redeemed multiple ArkNotes"
    );

    // Verify the balance has been updated
    let balance = alice.offchain_balance().await.unwrap();
    tracing::info!(
        confirmed_balance = %balance.confirmed(),
        pending_balance = %balance.pending(),
        "Balance after multiple ArkNotes redemption"
    );

    tracing::info!("Multiple ArkNotes redemption test completed successfully");
}

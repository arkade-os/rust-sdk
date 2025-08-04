#![allow(clippy::unwrap_used)]

use ark_core::ArkNote;
use ark_grpc::Client as GrpcClient;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::thread_rng;
use std::sync::Arc;

mod common;

async fn create_grpc_client() -> GrpcClient {
    let mut client = GrpcClient::new("http://localhost:7070".to_string());
    client.connect().await.unwrap();
    client
}

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

    // Create ArkNote using gRPC API

    let note = alice.create_arknote(fund_amount).await.unwrap();

    tracing::info!(
        arknote_string = %note.to_encoded_string(),
        value = %fund_amount,
        "Created ArkNote using gRPC API for redemption test"
    );

    let parsed_note = ArkNote::from_string(&note.to_encoded_string()).unwrap();
    assert_eq!(parsed_note.value(), fund_amount);
    assert!(note.status().confirmed);
    assert!(note.extra_witness().is_some());

    // Verify tap tree is not empty
    let tap_tree = note.tap_tree();
    assert!(!tap_tree.is_empty());

    // Verify outpoint creation
    let outpoint = note.outpoint();
    assert_eq!(outpoint.vout, 0);

    // Verify TxOut creation
    let tx_out = note.to_tx_out();
    assert_eq!(tx_out.value, fund_amount);

    // Redeem the ArkNote using the new redeem_note method
    let mut rng = thread_rng();
    let txid_opt = alice.redeem_note(&mut rng, note).await.unwrap();

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

    // Create multiple test ArkNotes using gRPC API
    let grpc_client = create_grpc_client().await;

    let note_strings1 = grpc_client.create_arknote(500, 1).await.unwrap();
    let note_strings2 = grpc_client.create_arknote(750, 1).await.unwrap();
    let note_strings3 = grpc_client.create_arknote(250, 1).await.unwrap();

    let note1 = ArkNote::from_string(&note_strings1[0]).unwrap();
    let note2 = ArkNote::from_string(&note_strings2[0]).unwrap();
    let note3 = ArkNote::from_string(&note_strings3[0]).unwrap();

    let total_amount = note1.value() + note2.value() + note3.value();
    let notes = vec![note1, note2, note3];

    tracing::info!(
        note_count = notes.len(),
        total_amount = %total_amount,
        "Created multiple ArkNotes using gRPC API for redemption test"
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

#[tokio::test]
#[ignore]
pub async fn e2e_grpc_create_arknote() {
    init_tracing();

    tracing::info!("Starting gRPC ArkNote creation test");

    // Test creating a single note
    let grpc_client = create_grpc_client().await;
    let amount = 1000u32;
    let quantity = 1u32;

    let notes = grpc_client.create_arknote(amount, quantity).await.unwrap();

    assert_eq!(
        notes.len(),
        quantity as usize,
        "Should create the requested number of notes"
    );

    // Verify the note can be parsed
    let arknote = ArkNote::from_string(&notes[0]).unwrap();
    assert_eq!(arknote.value(), Amount::from_sat(amount as u64));

    tracing::info!(
        note_count = notes.len(),
        amount = %amount,
        "Successfully created single ArkNote via gRPC"
    );

    // Test creating multiple notes at once
    let quantity = 3u32;
    let notes = grpc_client.create_arknote(amount, quantity).await.unwrap();

    assert_eq!(
        notes.len(),
        quantity as usize,
        "Should create the requested number of notes"
    );

    // Verify all notes can be parsed and have correct values
    for (i, note_string) in notes.iter().enumerate() {
        let arknote = ArkNote::from_string(note_string).unwrap();
        assert_eq!(arknote.value(), Amount::from_sat(amount as u64));
        tracing::debug!(note_index = i, "Successfully parsed note {}", i);
    }

    tracing::info!(
        note_count = notes.len(),
        amount = %amount,
        "Successfully created multiple ArkNotes via gRPC"
    );

    tracing::info!("gRPC ArkNote creation test completed successfully");
}

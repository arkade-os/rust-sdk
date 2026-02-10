#![allow(clippy::unwrap_used)]

use ark_grpc::test_utils::create_notes;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::wait_until_balance;
use common::Nigiri;
use rand::thread_rng;
use std::sync::Arc;

mod common;

/// Test ArkNote settlement scenarios:
/// 1. Settle a single note
/// 2. Settle multiple notes at once
/// 3. Settle notes together with boarding outputs
#[tokio::test]
#[ignore]
pub async fn settle_arknote() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    // --- Part 1: Settle a single ArkNote ---
    tracing::info!("=== Part 1: Settle single ArkNote ===");

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    let note_amount = Amount::from_sat(100_000);
    let notes = create_notes(note_amount.to_sat() as u32, 1).await.unwrap();
    assert_eq!(notes.len(), 1);

    tracing::info!(
        note = %notes[0].to_encoded_string(),
        value = %notes[0].value(),
        "Created single ArkNote"
    );

    // Alice should start with zero balance
    let alice_balance = alice.offchain_balance().await.unwrap();
    assert_eq!(alice_balance.confirmed(), Amount::ZERO);

    alice
        .settle_with_notes(&mut rng, notes)
        .await
        .expect("failed to settle single note");

    wait_until_balance!(&alice, confirmed: note_amount);
    tracing::info!("Single ArkNote settled successfully");

    // --- Part 2: Settle multiple ArkNotes ---
    tracing::info!("=== Part 2: Settle multiple ArkNotes ===");

    let (bob, _) = set_up_client("bob".to_string(), nigiri.clone(), secp.clone()).await;

    let multi_note_amount = Amount::from_sat(50_000);
    let notes = create_notes(multi_note_amount.to_sat() as u32, 3)
        .await
        .unwrap();
    assert_eq!(notes.len(), 3);

    let total_multi = multi_note_amount * 3;

    for (i, note) in notes.iter().enumerate() {
        tracing::info!(
            index = i,
            note = %note.to_encoded_string(),
            "Created ArkNote"
        );
    }

    bob.settle_with_notes(&mut rng, notes)
        .await
        .expect("failed to settle multiple notes");

    wait_until_balance!(&bob, confirmed: total_multi);
    tracing::info!(total = %total_multi, "Multiple ArkNotes settled successfully");

    // --- Part 3: Settle ArkNote with boarding output ---
    tracing::info!("=== Part 3: Settle ArkNote with boarding output ===");

    let (charlie, _) = set_up_client("charlie".to_string(), nigiri.clone(), secp).await;

    // Fund Charlie with a boarding output
    let boarding_amount = Amount::from_sat(200_000);
    nigiri
        .faucet_fund(&charlie.get_boarding_address().unwrap(), boarding_amount)
        .await;

    // Create a note
    let note_amount = Amount::from_sat(100_000);
    let notes = create_notes(note_amount.to_sat() as u32, 1).await.unwrap();

    tracing::info!(
        note = %notes[0].to_encoded_string(),
        note_value = %note_amount,
        boarding_value = %boarding_amount,
        "Settling ArkNote together with boarding output"
    );

    charlie
        .settle_with_notes(&mut rng, notes)
        .await
        .expect("failed to settle with notes and boarding");

    let expected_total = boarding_amount + note_amount;
    wait_until_balance!(&charlie, confirmed: expected_total);

    tracing::info!(
        total = %expected_total,
        "ArkNote + boarding output settled successfully"
    );
}

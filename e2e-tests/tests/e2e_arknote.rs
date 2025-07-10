#![allow(clippy::unwrap_used)]

use ark_core::ArkNote;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn e2e_arknote_redemption() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();

    // Create a test wallet
    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let alice_offchain_address = alice.get_offchain_address().unwrap().0;

    tracing::info!(
        ?alice_offchain_address,
        "Created Alice's wallet for ArkNote redemption test"
    );

    // Create a test ArkNote
    let fund_amount = Amount::from_sat(1000);
    let arknote = alice.make_arknote(fund_amount).await.unwrap();

    tracing::info!(
        arknote_string = %arknote.to_string(),
        value = %fund_amount,
        "Created ArkNote for redemption test"
    );

    // Test parsing the note back from string
    let parsed_note = ArkNote::from_string(&arknote.to_string()).unwrap();
    assert_eq!(parsed_note.value(), fund_amount);

    tracing::info!("Successfully parsed ArkNote from string");

    // Verify ExtendedCoin implementation
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

    let result = alice.redeem_notes(&[arknote]).await;
    assert!(
        result.is_ok(),
        "Failed to join Ark round with ArkNote: {:?}",
        result.err()
    );

    tracing::info!("ArkNote redemption test completed successfully");
}

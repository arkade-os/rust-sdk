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
    
    // Assert that the balance has increased by the redeemed amount
    assert!(
        balance.confirmed() >= fund_amount || balance.pending() >= fund_amount,
        "Expected balance to increase by at least {} sats after redemption, but got confirmed: {} and pending: {}",
        fund_amount,
        balance.confirmed(),
        balance.pending()
    );

    tracing::info!("ArkNote redemption test completed successfully");
}

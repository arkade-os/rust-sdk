#![allow(clippy::unwrap_used)]

use ark_client::RoundOutputType;
use ark_core::round;
use ark_core::ArkNote;
use ark_core::Vtxo;
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

    // FIXME: how to crate the VtxoInput for ArkNote?
    let vtxo = Vtxo::new_default(
        &secp,
        alice.server_info.pk.into(),
        // FIXME: what is the owner public key? for sure using the server pk is wrong
        arknote.vtxo_script().x_only_public_key(),
        alice.server_info.unilateral_exit_delay,
        alice.server_info.network,
    );
    assert!(
        vtxo.is_ok(),
        "Failed to create Vtxo from ArkNote: {:?}",
        vtxo.err()
    );
    let vtxo = vtxo.unwrap();
    let vtxo_arknote = round::VtxoInput::new(vtxo, arknote.value(), arknote.outpoint(), true);
    let inputs = vec![vtxo_arknote];

    // Create the outpoint to the offchain address
    let arkaddr = alice.get_offchain_address();
    assert!(
        arkaddr.is_ok(),
        "Failed to get offchain address: {:?}",
        arkaddr.err()
    );
    let arkaddr = arkaddr.unwrap().0;

    let rng = &mut rand::thread_rng();
    let result = alice
        .join_next_ark_round(
            rng,
            vec![],
            inputs,
            RoundOutputType::Board {
                to_address: arkaddr.clone(),
                to_amount: arknote.value(),
            },
        )
        .await;
    assert!(
        result.is_ok(),
        "Failed to join Ark round with ArkNote: {:?}",
        result.err()
    );

    tracing::info!("ArkNote redemption test completed successfully");
}

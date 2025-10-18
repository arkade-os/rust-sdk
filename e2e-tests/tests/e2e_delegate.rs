#![allow(clippy::unwrap_used)]

use bitcoin::key::Keypair;
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
pub async fn e2e_delegate() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    // Set up Alice and Bob
    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let (bob, _) = set_up_client("bob".to_string(), nigiri.clone(), secp.clone()).await;

    // Generate Bob's delegate cosigner keypair (ephemeral)
    let bob_delegate_cosigner_kp = Keypair::new(&secp, &mut rng);
    let bob_delegate_cosigner_pk = bob_delegate_cosigner_kp.public_key();

    tracing::info!("Step 1: Fund Alice's boarding output");
    let alice_boarding_address = alice.get_boarding_address().unwrap();
    let alice_fund_amount = Amount::ONE_BTC;

    let alice_boarding_outpoint = nigiri
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;

    tracing::info!(?alice_boarding_outpoint, "Funded Alice's boarding output");

    tracing::info!("Step 2: Alice settles to get a VTXO");
    alice.settle(&mut rng, false).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let vtxos_before = alice.list_vtxos(false).await.unwrap();

    tracing::info!(
        ?alice_offchain_balance,
        vtxos = ?vtxos_before,
        "Alice settled - has confirmed VTXO"
    );

    assert_eq!(alice_offchain_balance.confirmed(), alice_fund_amount);
    assert_eq!(alice_offchain_balance.pending(), Amount::ZERO);

    // Wait for the server's timelock before the VTXO can be used for intent registration
    tracing::info!("Waiting for VTXO timelock (3 seconds)...");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    tracing::info!("Step 3: Alice generates a delegate for Bob");
    let delegate = alice
        .generate_delegate(bob_delegate_cosigner_pk, false)
        .await
        .unwrap();

    tracing::info!(
        delegate_cosigner_pk = %bob_delegate_cosigner_pk,
        vtxo_inputs_count = delegate.vtxo_inputs.len(),
        onchain_inputs_count = delegate.onchain_inputs.len(),
        partial_forfeit_txs_count = delegate.partial_forfeit_txs.len(),
        "Alice generated delegate"
    );

    // Verify delegate was created correctly
    assert_eq!(delegate.delegate_cosigner_pk, bob_delegate_cosigner_pk);
    assert!(
        !delegate.vtxo_inputs.is_empty(),
        "Should have at least one VTXO input"
    );

    // NOTE: The boarding output might still appear if the blockchain hasn't fully marked it as
    // spent For now, we'll just verify we have inputs to delegate
    tracing::info!(
        "Delegate has {} VTXO inputs and {} onchain inputs",
        delegate.vtxo_inputs.len(),
        delegate.onchain_inputs.len()
    );

    tracing::info!("Step 4: Bob settles using Alice's delegate");
    let commitment_txid = bob
        .settle_delegate(&mut rng, delegate, bob_delegate_cosigner_kp)
        .await
        .unwrap();

    tracing::info!(
        %commitment_txid,
        "Bob successfully settled Alice's VTXO using delegation"
    );

    // Wait longer for settlement to complete and new VTXO to appear
    tracing::info!("Waiting for new VTXO to appear...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    tracing::info!("Step 5: Verify Alice's VTXO has been settled");
    let alice_offchain_balance_after = alice.offchain_balance().await.unwrap();

    let vtxos_after = alice.list_vtxos(false).await.unwrap();

    tracing::info!(
        ?alice_offchain_balance_after,
        vtxos = ?vtxos_after,
        "Alice's balance after delegated settlement"
    );

    // Verify that the delegation settlement succeeded
    // The VTXO amount should either be in pending or confirmed depending on timing
    let total_balance =
        alice_offchain_balance_after.confirmed() + alice_offchain_balance_after.pending();
    assert_eq!(
        total_balance, alice_fund_amount,
        "Alice should still have her total funds (either as pending or confirmed VTXO)"
    );

    let pre_settlement_outpoint = vtxos_before.spendable[0].0[0].outpoint;
    let settled_outpoint = vtxos_after.spent[0].0[0].outpoint;

    assert_eq!(
        pre_settlement_outpoint, settled_outpoint,
        "original VTXO should be spent"
    );

    let old_vtxo_settlement_txid = vtxos_after.spent[0].0[0].settled_by.unwrap();
    let new_vtxo_commitment_txid = vtxos_after.spendable[0].0[0].commitment_txids[0];

    assert_eq!(
        old_vtxo_settlement_txid, new_vtxo_commitment_txid,
        "VTXO should be settled"
    );

    tracing::info!("Delegation test completed successfully!");
}

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

    let alice_boarding_address = alice.get_boarding_address().unwrap();
    let alice_fund_amount = Amount::ONE_BTC;

    let alice_boarding_outpoint = nigiri
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;

    tracing::debug!(?alice_boarding_outpoint, "Funded Alice's boarding output");

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let (vtxos_before, _) = alice.list_vtxos().await.unwrap();

    tracing::info!(
        ?alice_offchain_balance,
        vtxos = ?vtxos_before,
        "Alice got confirmed VTXO"
    );

    assert_eq!(alice_offchain_balance.confirmed(), alice_fund_amount);
    assert_eq!(alice_offchain_balance.pre_confirmed(), Amount::ZERO);

    // TODO: Not sure why we have to wait longer here.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let mut delegate = alice
        .generate_delegate(bob_delegate_cosigner_pk)
        .await
        .unwrap();

    alice
        .sign_delegate_psbts(&mut delegate.intent.proof, &mut delegate.forfeit_psbts)
        .unwrap();

    tracing::info!(
        delegate_cosigner_pk = %bob_delegate_cosigner_pk,
        partial_forfeit_txs_count = delegate.forfeit_psbts.len(),
        "Alice generated delegate"
    );

    let commitment_txid = bob
        .settle_delegate(&mut rng, delegate, bob_delegate_cosigner_kp)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_offchain_balance_after = alice.offchain_balance().await.unwrap();

    let (vtxos_after, _) = alice.list_vtxos().await.unwrap();

    tracing::info!(
        %commitment_txid,
        ?alice_offchain_balance_after,
        vtxos = ?vtxos_after,
        "Bob successfully settled Alice's VTXO using delegate system"
    );

    assert_eq!(alice_offchain_balance_after.confirmed(), alice_fund_amount);

    let pre_settlement_outpoint = vtxos_before.all_unspent().next().unwrap().outpoint;
    let settled_outpoint = vtxos_after.spent().next().unwrap();

    assert_eq!(
        pre_settlement_outpoint, settled_outpoint.outpoint,
        "original VTXO should be spent"
    );

    let old_vtxo_settlement_txid = settled_outpoint.settled_by.unwrap();
    let new_vtxo_commitment_txid = vtxos_after.all_unspent().next().unwrap().commitment_txids[0];

    assert_eq!(
        old_vtxo_settlement_txid, new_vtxo_commitment_txid,
        "VTXO should be settled"
    );
}

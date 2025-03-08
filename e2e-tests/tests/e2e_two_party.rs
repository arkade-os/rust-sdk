#![allow(clippy::unwrap_used)]

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
pub async fn e2e() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let alice = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let bob = set_up_client("bob".to_string(), nigiri.clone(), secp).await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();
    let alice_boarding_address = alice.get_boarding_address().unwrap();

    tracing::info!(
        ?alice_boarding_address,
        ?alice_offchain_balance,
        ?bob_offchain_balance,
        "Funding Alice's boarding output"
    );

    assert_eq!(alice_offchain_balance.total(), Amount::ZERO);
    assert_eq!(bob_offchain_balance.total(), Amount::ZERO);

    let alice_fund_amount = Amount::ONE_BTC;

    let alice_boarding_outpoint = nigiri
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();

    tracing::info!(
        ?alice_boarding_outpoint,
        ?alice_offchain_balance,
        ?bob_offchain_balance,
        "Funded Alice's boarding output"
    );

    assert_eq!(alice_offchain_balance.total(), Amount::ZERO);
    assert_eq!(bob_offchain_balance.total(), Amount::ZERO);

    alice.board(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();

    tracing::info!(
        ?alice_offchain_balance,
        ?bob_offchain_balance,
        "Lifted Alice's VTXO"
    );

    assert_eq!(alice_offchain_balance.confirmed(), alice_fund_amount);
    assert_eq!(alice_offchain_balance.pending(), Amount::ZERO);
    assert_eq!(bob_offchain_balance.total(), Amount::ZERO);

    let send_to_bob_vtxo_amount = Amount::from_sat(100_000);
    let (bob_offchain_address, _) = bob.get_offchain_address().unwrap();

    tracing::info!(
        %send_to_bob_vtxo_amount,
        ?bob_offchain_address,
        ?alice_offchain_balance,
        ?bob_offchain_balance,
        "Sending VTXO from Alice to Bob"
    );

    let redeem_tx = alice
        .send_vtxo(bob_offchain_address, send_to_bob_vtxo_amount)
        .await
        .unwrap();

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();

    tracing::info!(
        ?alice_offchain_balance,
        ?bob_offchain_balance,
        redeem_txid = %redeem_tx.unsigned_tx.compute_txid(),
        "Sent VTXO from Alice to Bob"
    );

    let redeem_tx_fee = redeem_tx.fee().unwrap();

    assert_eq!(alice_offchain_balance.confirmed(), Amount::ZERO);
    assert_eq!(
        alice_offchain_balance.pending(),
        alice_fund_amount - send_to_bob_vtxo_amount - redeem_tx_fee
    );
    assert_eq!(bob_offchain_balance.confirmed(), Amount::ZERO);
    assert_eq!(bob_offchain_balance.pending(), send_to_bob_vtxo_amount);

    bob.board(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();

    tracing::info!(
        ?alice_offchain_balance,
        ?bob_offchain_balance,
        "Lifted Bob's VTXO"
    );

    assert_eq!(alice_offchain_balance.confirmed(), Amount::ZERO);
    assert_eq!(
        alice_offchain_balance.pending(),
        alice_fund_amount - send_to_bob_vtxo_amount - redeem_tx_fee
    );
    assert_eq!(bob_offchain_balance.confirmed(), send_to_bob_vtxo_amount);
    assert_eq!(bob_offchain_balance.pending(), Amount::ZERO);

    alice.board(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();

    tracing::info!(
        ?alice_offchain_balance,
        ?bob_offchain_balance,
        "Lifted Alice's change VTXO"
    );

    assert_eq!(
        alice_offchain_balance.confirmed(),
        alice_fund_amount - send_to_bob_vtxo_amount - redeem_tx_fee
    );
    assert_eq!(alice_offchain_balance.pending(), Amount::ZERO);
    assert_eq!(bob_offchain_balance.confirmed(), send_to_bob_vtxo_amount);
    assert_eq!(bob_offchain_balance.pending(), Amount::ZERO);
}

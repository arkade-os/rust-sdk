#![allow(clippy::unwrap_used)]

use crate::common::wait_until_balance;
use bitcoin::Amount;
use bitcoin::key::Secp256k1;
use common::Nigiri;
use common::init_tracing;
use common::set_up_client;
use rand::SeedableRng;
use rand::rngs::StdRng;
use std::sync::Arc;
use std::time::Duration;
use tokio::try_join;

mod common;

#[tokio::test]
#[ignore]
pub async fn concurrent_boarding() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let (bob, _) = set_up_client("bob".to_string(), nigiri.clone(), secp.clone()).await;
    let (claire, _) = set_up_client("claire".to_string(), nigiri.clone(), secp.clone()).await;

    let alice_boarding_address = alice.get_boarding_address().unwrap();
    let bob_boarding_address = bob.get_boarding_address().unwrap();
    let claire_boarding_address = claire.get_boarding_address().unwrap();

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();
    let claire_offchain_balance = claire.offchain_balance().await.unwrap();

    assert_eq!(alice_offchain_balance.total(), Amount::ZERO);
    assert_eq!(bob_offchain_balance.total(), Amount::ZERO);
    assert_eq!(claire_offchain_balance.total(), Amount::ZERO);

    let alice_fund_amount = Amount::from_sat(200_000_000);
    let bob_fund_amount = Amount::ONE_BTC;
    let claire_fund_amount = Amount::from_sat(50_000_000);

    nigiri
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;
    nigiri
        .faucet_fund(&bob_boarding_address, bob_fund_amount)
        .await;
    nigiri
        .faucet_fund(&claire_boarding_address, claire_fund_amount)
        .await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();
    let claire_offchain_balance = claire.offchain_balance().await.unwrap();

    assert_eq!(alice_offchain_balance.total(), Amount::ZERO);
    assert_eq!(bob_offchain_balance.total(), Amount::ZERO);
    assert_eq!(claire_offchain_balance.total(), Amount::ZERO);

    let alice_task = tokio::spawn({
        async move {
            let mut rng = StdRng::from_entropy();
            alice.settle(&mut rng, false).await.unwrap();
            alice
        }
    });

    let bob_task = tokio::spawn(async move {
        let mut rng = StdRng::from_entropy();
        bob.settle(&mut rng, false).await.unwrap();
        bob
    });

    let claire_task = tokio::spawn(async move {
        let mut rng = StdRng::from_entropy();
        claire.settle(&mut rng, false).await.unwrap();
        claire
    });

    // Three parties joining a batch concurrently.
    let (alice, bob, claire) = try_join!(alice_task, bob_task, claire_task).unwrap();

    wait_until_balance(&alice, alice_fund_amount, Amount::ZERO)
        .await
        .unwrap();
    wait_until_balance(&bob, bob_fund_amount, Amount::ZERO)
        .await
        .unwrap();
    wait_until_balance(&claire, claire_fund_amount, Amount::ZERO)
        .await
        .unwrap();

    let (alice_offchain_address, _) = alice.get_offchain_address().unwrap();
    let (bob_offchain_address, _) = bob.get_offchain_address().unwrap();
    let (claire_offchain_address, _) = claire.get_offchain_address().unwrap();

    let alice_to_bob_send_amount = Amount::from_sat(100_000);
    let bob_to_claire_send_amount = Amount::from_sat(50_000);
    let claire_to_alice_send_amount = Amount::from_sat(10_000);

    alice
        .send_vtxo(bob_offchain_address, alice_to_bob_send_amount)
        .await
        .unwrap();

    // FIXME: We should not need to sleep here. We were running into an error when finalising the
    // offchain transaction: the virtual TXID could not be found in the DB.
    tokio::time::sleep(Duration::from_secs(5)).await;

    bob.send_vtxo(claire_offchain_address, bob_to_claire_send_amount)
        .await
        .unwrap();

    // FIXME: We should not need to sleep here. We were running into an error when finalising the
    // offchain transaction: the virtual TXID could not be found in the DB.
    tokio::time::sleep(Duration::from_secs(5)).await;

    claire
        .send_vtxo(alice_offchain_address, claire_to_alice_send_amount)
        .await
        .unwrap();

    wait_until_balance(
        &alice,
        Amount::ZERO,
        alice_fund_amount - alice_to_bob_send_amount + claire_to_alice_send_amount,
    )
    .await
    .unwrap();

    // Checking Bob and Claire's balance is inconsistent because of coin selection.

    let alice_task = tokio::spawn({
        async move {
            let mut rng = StdRng::from_entropy();
            alice.settle(&mut rng, false).await.unwrap();
            alice
        }
    });

    let bob_task = tokio::spawn(async move {
        let mut rng = StdRng::from_entropy();
        bob.settle(&mut rng, false).await.unwrap();
        bob
    });

    let claire_task = tokio::spawn(async move {
        let mut rng = StdRng::from_entropy();
        claire.settle(&mut rng, false).await.unwrap();
        claire
    });

    // Three parties joining a batch concurrently.
    let (alice, bob, claire) = try_join!(alice_task, bob_task, claire_task).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    wait_until_balance(
        &alice,
        alice_fund_amount - alice_to_bob_send_amount + claire_to_alice_send_amount,
        Amount::ZERO,
    )
    .await
    .unwrap();
    wait_until_balance(
        &bob,
        bob_fund_amount - bob_to_claire_send_amount + alice_to_bob_send_amount,
        Amount::ZERO,
    )
    .await
    .unwrap();
    wait_until_balance(
        &claire,
        claire_fund_amount - claire_to_alice_send_amount + bob_to_claire_send_amount,
        Amount::ZERO,
    )
    .await
    .unwrap();
}

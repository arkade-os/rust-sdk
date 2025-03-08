#![allow(clippy::unwrap_used)]

use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::rngs::StdRng;
use rand::SeedableRng;
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

    let alice = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let bob = set_up_client("bob".to_string(), nigiri.clone(), secp.clone()).await;
    let claire = set_up_client("claire".to_string(), nigiri.clone(), secp.clone()).await;

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
            alice.board(&mut rng).await.unwrap();
            alice
        }
    });

    let bob_task = tokio::spawn(async move {
        let mut rng = StdRng::from_entropy();
        bob.board(&mut rng).await.unwrap();
        bob
    });

    let claire_task = tokio::spawn(async move {
        let mut rng = StdRng::from_entropy();
        claire.board(&mut rng).await.unwrap();
        claire
    });

    // Three parties joining a round concurrently.
    let (alice, bob, claire) = try_join!(alice_task, bob_task, claire_task).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();
    let claire_offchain_balance = claire.offchain_balance().await.unwrap();

    assert_eq!(alice_offchain_balance.confirmed(), alice_fund_amount);
    assert_eq!(alice_offchain_balance.pending(), Amount::ZERO);
    assert_eq!(bob_offchain_balance.confirmed(), bob_fund_amount);
    assert_eq!(bob_offchain_balance.pending(), Amount::ZERO);
    assert_eq!(claire_offchain_balance.confirmed(), claire_fund_amount);
    assert_eq!(claire_offchain_balance.pending(), Amount::ZERO);

    let (alice_offchain_address, _) = alice.get_offchain_address().unwrap();
    let (bob_offchain_address, _) = bob.get_offchain_address().unwrap();
    let (claire_offchain_address, _) = claire.get_offchain_address().unwrap();

    let alice_to_bob_send_amount = Amount::from_sat(100_000);
    let bob_to_claire_send_amount = Amount::from_sat(50_000);
    let claire_to_alice_send_amount = Amount::from_sat(10_000);

    let alice_to_bob_redeem_tx = alice
        .send_vtxo(bob_offchain_address, alice_to_bob_send_amount)
        .await
        .unwrap();
    let alice_to_bob_redeem_tx_fee = alice_to_bob_redeem_tx.fee().unwrap();
    let bob_to_claire_redeem_tx = bob
        .send_vtxo(claire_offchain_address, bob_to_claire_send_amount)
        .await
        .unwrap();
    let bob_to_claire_redeem_tx_fee = bob_to_claire_redeem_tx.fee().unwrap();
    let claire_to_alice_redeem_tx = claire
        .send_vtxo(alice_offchain_address, claire_to_alice_send_amount)
        .await
        .unwrap();
    let claire_to_alice_redeem_tx_fee = claire_to_alice_redeem_tx.fee().unwrap();

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();
    let claire_offchain_balance = claire.offchain_balance().await.unwrap();

    assert_eq!(alice_offchain_balance.confirmed(), Amount::ZERO);
    assert_eq!(
        alice_offchain_balance.pending(),
        alice_fund_amount - alice_to_bob_send_amount - alice_to_bob_redeem_tx_fee
            + claire_to_alice_send_amount
    );
    assert_eq!(bob_offchain_balance.confirmed(), Amount::ZERO);
    assert_eq!(
        bob_offchain_balance.pending(),
        bob_fund_amount - bob_to_claire_send_amount - bob_to_claire_redeem_tx_fee
            + alice_to_bob_send_amount
    );
    assert_eq!(claire_offchain_balance.confirmed(), Amount::ZERO);
    assert_eq!(
        claire_offchain_balance.pending(),
        claire_fund_amount - claire_to_alice_send_amount - claire_to_alice_redeem_tx_fee
            + bob_to_claire_send_amount
    );

    let alice_task = tokio::spawn({
        async move {
            let mut rng = StdRng::from_entropy();
            alice.board(&mut rng).await.unwrap();
            alice
        }
    });

    let bob_task = tokio::spawn(async move {
        let mut rng = StdRng::from_entropy();
        bob.board(&mut rng).await.unwrap();
        bob
    });

    let claire_task = tokio::spawn(async move {
        let mut rng = StdRng::from_entropy();
        claire.board(&mut rng).await.unwrap();
        claire
    });

    // Three parties joining a round concurrently.
    let (alice, bob, claire) = try_join!(alice_task, bob_task, claire_task).unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();
    let claire_offchain_balance = claire.offchain_balance().await.unwrap();

    assert_eq!(
        alice_offchain_balance.confirmed(),
        alice_fund_amount - alice_to_bob_send_amount - alice_to_bob_redeem_tx_fee
            + claire_to_alice_send_amount
    );
    assert_eq!(alice_offchain_balance.pending(), Amount::ZERO);
    assert_eq!(
        bob_offchain_balance.confirmed(),
        bob_fund_amount - bob_to_claire_send_amount - bob_to_claire_redeem_tx_fee
            + alice_to_bob_send_amount
    );
    assert_eq!(bob_offchain_balance.pending(), Amount::ZERO);
    assert_eq!(
        claire_offchain_balance.confirmed(),
        claire_fund_amount - claire_to_alice_send_amount - claire_to_alice_redeem_tx_fee
            + bob_to_claire_send_amount
    );
    assert_eq!(claire_offchain_balance.pending(), Amount::ZERO);
}

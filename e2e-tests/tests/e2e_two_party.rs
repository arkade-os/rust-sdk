#![allow(clippy::unwrap_used)]

use crate::common::wait_until_balance;
use bitcoin::Amount;
use bitcoin::key::Secp256k1;
use common::Nigiri;
use common::init_tracing;
use common::set_up_client;
use rand::thread_rng;
use std::str::FromStr;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn e2e() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let (bob, _) = set_up_client("bob".to_string(), nigiri.clone(), secp).await;

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

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();

    tracing::info!(
        ?alice_offchain_balance,
        ?bob_offchain_balance,
        "Lifted Alice's VTXO"
    );

    assert_eq!(alice_offchain_balance.confirmed(), alice_fund_amount);
    assert_eq!(alice_offchain_balance.pre_confirmed(), Amount::ZERO);
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

    let virtual_txid = alice
        .send_vtxo(bob_offchain_address, send_to_bob_vtxo_amount)
        .await
        .unwrap();

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let bob_offchain_balance = bob.offchain_balance().await.unwrap();

    tracing::info!(
        ?alice_offchain_balance,
        ?bob_offchain_balance,
        virtual_txid = %virtual_txid,
        "Sent VTXO from Alice to Bob"
    );

    wait_until_balance!(
        &alice,
        confirmed: Amount::ZERO,
        pre_confirmed: alice_fund_amount - send_to_bob_vtxo_amount,
    );
    wait_until_balance!(&bob, confirmed: Amount::ZERO, pre_confirmed: send_to_bob_vtxo_amount);

    bob.settle(&mut rng).await.unwrap();
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
        alice_offchain_balance.pre_confirmed(),
        alice_fund_amount - send_to_bob_vtxo_amount
    );
    assert_eq!(bob_offchain_balance.confirmed(), send_to_bob_vtxo_amount);
    assert_eq!(bob_offchain_balance.pre_confirmed(), Amount::ZERO);

    alice.settle(&mut rng).await.unwrap();
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
        alice_fund_amount - send_to_bob_vtxo_amount
    );
    assert_eq!(alice_offchain_balance.pre_confirmed(), Amount::ZERO);
    assert_eq!(bob_offchain_balance.confirmed(), send_to_bob_vtxo_amount);
    assert_eq!(bob_offchain_balance.pre_confirmed(), Amount::ZERO);

    let address = bitcoin::Address::from_str(
        "bcrt1puq2gdfn97qd0ep0m335gc7r7uh0hpyhjhnmy90tklyywkdpcdd9sfag5y0",
    )
    .unwrap();

    let txid = bob
        .collaborative_redeem(
            &mut rng,
            address.assume_checked(),
            send_to_bob_vtxo_amount / 2,
        )
        .await
        .unwrap();

    let bob_offchain_balance = bob.offchain_balance().await.unwrap();

    assert_eq!(
        bob_offchain_balance.confirmed(),
        send_to_bob_vtxo_amount / 2
    );

    tracing::info!(?txid, "Collaboratively redeemed from Bob");
}

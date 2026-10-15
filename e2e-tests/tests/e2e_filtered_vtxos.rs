#![allow(clippy::unwrap_used)]

use crate::common::wait_until_balance;
use ark_client::GetVtxosRequestFilter;
use ark_core::send::SendReceiver;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Regtest;
use rand::thread_rng;
use std::collections::HashSet;
use std::sync::Arc;

mod common;

/// The balance and the unspent VTXOs of a wallet with spent history are the same whether the
/// server filters out spent VTXOs or the client does.
#[tokio::test]
#[ignore]
pub async fn filtered_vtxo_listing_matches_unfiltered() {
    init_tracing();
    let regtest = Arc::new(Regtest::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let (alice, _) = set_up_client("alice".to_string(), regtest.clone(), secp.clone()).await;
    let (bob, _) = set_up_client("bob".to_string(), regtest.clone(), secp).await;

    let fund_amount = Amount::ONE_BTC;
    let boarding_address = alice.get_boarding_address().await.unwrap();
    regtest.faucet_fund(&boarding_address, fund_amount).await;
    alice.settle(&mut rng).await.unwrap();
    wait_until_balance!(&alice, confirmed: fund_amount);

    // Alice spends her VTXO and receives again, so her history holds a spent VTXO.
    let to_bob = Amount::from_sat(100_000);
    let (bob_address, _) = bob.get_offchain_address().await.unwrap();
    alice
        .send(vec![SendReceiver::bitcoin(bob_address, to_bob)])
        .await
        .unwrap();
    wait_until_balance!(&bob, pre_confirmed: to_bob);

    let to_alice = Amount::from_sat(40_000);
    let (alice_address, _) = alice.get_offchain_address().await.unwrap();
    bob.send(vec![SendReceiver::bitcoin(alice_address, to_alice)])
        .await
        .unwrap();
    wait_until_balance!(
        &alice,
        confirmed: Amount::ZERO,
        pre_confirmed: fund_amount - to_bob + to_alice,
    );

    let all = alice.list_vtxos().await.unwrap();
    let renewable = alice
        .list_vtxos_filtered(GetVtxosRequestFilter::Renewable)
        .await
        .unwrap();

    assert!(
        all.spent().next().is_some(),
        "Alice's history should hold a spent VTXO"
    );
    assert!(
        renewable
            .all()
            .all(|entry| !entry.vtxo().is_spent && !entry.vtxo().is_unrolled),
        "the renewable list should hold no spent or unrolled VTXO"
    );

    let unspent = all
        .all_unspent()
        .map(|entry| entry.vtxo().outpoint)
        .collect::<HashSet<_>>();
    let renewable_outpoints = renewable
        .all()
        .map(|entry| entry.vtxo().outpoint)
        .collect::<HashSet<_>>();
    assert_eq!(renewable_outpoints, unspent);

    // The balance as computed before from the unfiltered list.
    let server_info = alice.server_info().await.unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let spendable = all
        .spendable_offchain_at(&server_info, now)
        .map(|entry| entry.vtxo().outpoint)
        .collect::<HashSet<_>>();
    let sum = |amounts: Vec<Amount>| amounts.into_iter().fold(Amount::ZERO, |acc, a| acc + a);
    let pre_confirmed = sum(all
        .pre_confirmed()
        .filter(|entry| spendable.contains(&entry.vtxo().outpoint))
        .map(|entry| entry.vtxo().amount)
        .collect());
    let confirmed = sum(all
        .confirmed()
        .filter(|entry| spendable.contains(&entry.vtxo().outpoint))
        .map(|entry| entry.vtxo().amount)
        .collect());
    let recoverable = sum(all.recoverable().map(|entry| entry.vtxo().amount).collect());
    let pending_recovery = sum(all
        .pending_recovery_due_to_signer_at(&server_info, now)
        .map(|entry| entry.vtxo().amount)
        .collect());

    let balance = alice.offchain_balance().await.unwrap();
    assert_eq!(balance.pre_confirmed(), pre_confirmed);
    assert_eq!(balance.confirmed(), confirmed);
    assert_eq!(balance.recoverable(), recoverable);
    assert_eq!(balance.pending_recovery(), pending_recovery);
    assert_eq!(balance.pre_confirmed(), fund_amount - to_bob + to_alice);
}

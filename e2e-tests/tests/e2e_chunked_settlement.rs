#![allow(clippy::unwrap_used)]

use crate::common::wait_until_balance;
use ark_bdk_wallet::Wallet;
use ark_client::Client;
use ark_client::InMemorySwapStorage;
use ark_core::send::SendReceiver;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::set_up_client_with_max_proof_weight;
use common::Regtest;
use rand::rngs::StdRng;
use rand::SeedableRng;
use std::sync::Arc;
use std::time::Duration;

mod common;

/// A settlement whose intent proof would exceed the proof weight limit must be split across
/// multiple sequential batches instead of being rejected by the server with `TX_TOO_LARGE`.
///
/// Bob's client is configured with a proof weight cap far below the server's `max_tx_weight`, so
/// a handful of VTXOs is enough to force chunking. Against a default server, the equivalent
/// scenario is a wallet with a few hundred VTXOs.
#[tokio::test]
#[ignore]
pub async fn settlement_is_chunked_when_proof_weight_exceeds_limit() {
    init_tracing();
    let regtest = Arc::new(Regtest::new());

    let secp = Secp256k1::new();

    let (alice, _) = set_up_client("alice".to_string(), regtest.clone(), secp.clone()).await;

    // Low enough that ~4-6 VTXO inputs fill a chunk, but comfortably above the weight of a
    // single-input proof (~1,100 WU).
    let max_proof_weight = 3_000;
    let (bob, _) = set_up_client_with_max_proof_weight(
        "bob".to_string(),
        regtest.clone(),
        secp.clone(),
        max_proof_weight,
    )
    .await;

    let alice_boarding_address = alice.get_boarding_address().await.unwrap();
    let alice_fund_amount = Amount::ONE_BTC;

    regtest
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;

    let mut rng = StdRng::from_entropy();

    alice.settle(&mut rng).await.unwrap();
    wait_until_balance!(&alice, confirmed: alice_fund_amount, pre_confirmed: Amount::ZERO);

    // Fragment Bob's wallet into many small VTXOs.
    let (bob_offchain_address, _) = bob.get_offchain_address().await.unwrap();

    let n_payments = 8;
    let payment_amount = Amount::from_sat(100_000);

    for i in 0..n_payments {
        alice
            .send(vec![SendReceiver::bitcoin(
                bob_offchain_address,
                payment_amount,
            )])
            .await
            .unwrap();

        // Wait for the payment to land before sending the next one: sending again while the
        // previous offchain transaction is still finalizing fails with "virtual TXID could not
        // be found in the DB".
        wait_until_pre_confirmed_vtxo_count(&bob, i as usize + 1).await;
    }

    let bob_total = payment_amount * n_payments;
    wait_until_balance!(&bob, pre_confirmed: bob_total);

    let bob_vtxos = bob.list_vtxos().await.unwrap();
    assert_eq!(bob_vtxos.pre_confirmed().count(), n_payments as usize);

    // Settling all of Bob's VTXOs at once would exceed the configured proof weight limit, so the
    // client must split the settlement across multiple batches.
    let commitment_txid = bob.settle_all(&mut rng).await.unwrap();
    assert!(commitment_txid.is_some());

    wait_until_balance!(&bob, confirmed: bob_total, pre_confirmed: Amount::ZERO);

    // Each chunk settles into its own confirmed VTXO, so more than one confirmed VTXO proves the
    // settlement was actually chunked.
    let bob_vtxos = bob.list_vtxos().await.unwrap();
    let n_confirmed = bob_vtxos.confirmed().count();
    assert!(
        n_confirmed > 1,
        "expected settlement to be split into multiple batches, got {n_confirmed} confirmed VTXO(s)"
    );

    let confirmed_total = bob_vtxos
        .confirmed()
        .fold(Amount::ZERO, |acc, vtxo| acc + vtxo.vtxo().amount);
    assert_eq!(confirmed_total, bob_total);
}

/// Wait until the client's pre-confirmed VTXO count reaches `count`.
async fn wait_until_pre_confirmed_vtxo_count(
    client: &Client<Regtest, Wallet, InMemorySwapStorage>,
    count: usize,
) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let vtxos = client.list_vtxos().await.unwrap();
            if vtxos.pre_confirmed().count() >= count {
                return;
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} pre-confirmed VTXOs"));
}

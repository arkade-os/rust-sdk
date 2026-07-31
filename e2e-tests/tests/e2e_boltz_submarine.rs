#![allow(clippy::unwrap_used)]

use crate::common::create_lnd_invoice;
use crate::common::create_lnd_invoice_with_expiry;
use crate::common::wait_until_balance;
use ark_client::BoltzVhtlcWatcherConfig;
use ark_client::SwapStatus;
use ark_core::send::SendReceiver;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Regtest;
use rand::thread_rng;
use std::sync::Arc;
use std::time::Duration;

mod common;

#[tokio::test]
#[ignore]
pub async fn submarine_swap() {
    // Requires the arkade-regtest Boltz profile.

    init_tracing();
    let regtest = Arc::new(Regtest::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let invoice_amount = Amount::from_sat(2_000);
    let invoice = create_lnd_invoice(invoice_amount).await;

    let (alice, _) = set_up_client("alice".to_string(), regtest.clone(), secp.clone()).await;

    let alice_fund_amount = Amount::ONE_BTC;

    regtest
        .faucet_fund(
            &alice.get_boarding_address().await.unwrap(),
            alice_fund_amount,
        )
        .await;

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    wait_until_balance!(&alice, confirmed: alice_fund_amount, pre_confirmed: Amount::ZERO);

    let res = alice.pay_ln_invoice(invoice).await.unwrap();

    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: alice_fund_amount - res.amount);
}

#[tokio::test]
#[ignore]
pub async fn submarine_swap_auto_refunds_with_vhtlc_watcher() {
    // Requires the arkade-regtest Boltz profile.

    init_tracing();
    let regtest = Arc::new(Regtest::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let (alice, _) = set_up_client(
        "alice-submarine-vhtlc-watcher".to_string(),
        regtest.clone(),
        secp,
    )
    .await;
    let alice = Arc::new(alice);

    let alice_fund_amount = Amount::ONE_BTC;

    regtest
        .faucet_fund(
            &alice.get_boarding_address().await.unwrap(),
            alice_fund_amount,
        )
        .await;

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    wait_until_balance!(&alice, confirmed: alice_fund_amount, pre_confirmed: Amount::ZERO);

    let invoice_amount = Amount::from_sat(2_000);
    let invoice = create_lnd_invoice_with_expiry(invoice_amount, Some(10)).await;
    let swap = alice.prepare_ln_invoice_payment(invoice).await.unwrap();
    tracing::info!(
        swap_id = swap.id,
        vhtlc_address = %swap.vhtlc_address,
        amount = %swap.amount,
        "Prepared submarine swap for VHTLC watcher auto-refund"
    );

    // Let the short-lived invoice expire before funding the VHTLC so Boltz cannot pay it and the
    // swap becomes cooperatively refundable.
    tokio::time::sleep(Duration::from_secs(12)).await;

    alice
        .send(vec![SendReceiver::bitcoin(swap.vhtlc_address, swap.amount)])
        .await
        .unwrap();

    wait_until_balance!(
        &alice,
        confirmed: Amount::ZERO,
        pre_confirmed: alice_fund_amount - swap.amount,
    );

    let refundable_status = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let status = alice.get_swap_status(&swap.id).await.unwrap().status;
            tracing::info!(
                swap_id = swap.id,
                ?status,
                "Waiting for refundable submarine status"
            );

            if is_submarine_refundable_status(&status) {
                return status;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .expect("timed out waiting for submarine swap to become refundable");

    tracing::info!(
        swap_id = swap.id,
        ?refundable_status,
        "Submarine swap is refundable; starting VHTLC watcher"
    );

    let _watcher = alice.start_boltz_vhtlc_watcher_with_config(BoltzVhtlcWatcherConfig {
        refresh_interval: Duration::from_secs(1),
    });

    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: alice_fund_amount);
}

fn is_submarine_refundable_status(status: &SwapStatus) -> bool {
    matches!(
        status,
        SwapStatus::InvoiceFailedToPay
            | SwapStatus::TransactionLockupFailed
            | SwapStatus::SwapExpired
    )
}

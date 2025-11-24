#![allow(clippy::unwrap_used)]

use crate::common::wait_until_balance;
use ark_client::SwapAmount;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use std::sync::Arc;

mod common;

// TODO: Expand this test to call Lightning APIs directly.

#[tokio::test]
#[ignore]
pub async fn reverse_swap() {
    // This test requires even more setup than regular e2e tests, as well as manual intervention
    // (for now).
    //
    // Follow the steps in
    // https://github.com/ArkLabsHQ/fulmine/blob/6a4cd0b38a29732d03721b925f220b4f3717f379/docs/swaps.regtest.md
    // to setup the environment including Boltz.

    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    let invoice_amount = SwapAmount::invoice(Amount::from_sat(1_000));
    let res = alice.get_ln_invoice(invoice_amount, None).await.unwrap();

    tracing::info!(invoice = %res.invoice, swap_id = res.swap_id, "Generated Boltz reverse swap invoice");

    // Manual intervention.
    tracing::info!("Pay the invoice using a Lightning wallet: {}", res.invoice);

    alice.wait_for_vhtlc(&res.swap_id).await.unwrap();

    tracing::info!(swap_id = res.swap_id, "Lightning invoice paid");

    wait_until_balance(&alice, Amount::ZERO, res.amount)
        .await
        .unwrap();
}

#![allow(clippy::unwrap_used)]

use crate::common::wait_until_balance;
use ark_client::lightning_invoice::Bolt11Invoice;
use bitcoin::Amount;
use bitcoin::key::Secp256k1;
use common::Nigiri;
use common::init_tracing;
use common::set_up_client;
use rand::thread_rng;
use std::sync::Arc;

mod common;

// TODO: Expand this test to call Lightning APIs directly.

// Modify this constant _before_ running the test.
const INVOICE: &str = "";

#[tokio::test]
#[ignore]
pub async fn submarine_swap() {
    // This test requires even more setup than regular e2e tests, as well as manual intervention
    // (for now).
    //
    // Follow the steps in
    // https://github.com/ArkLabsHQ/fulmine/blob/6a4cd0b38a29732d03721b925f220b4f3717f379/docs/swaps.regtest.md
    // to setup the environment including Boltz.

    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let invoice: Bolt11Invoice = INVOICE.parse().expect("valid BOLT11 invoice");

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    let alice_fund_amount = Amount::ONE_BTC;

    nigiri
        .faucet_fund(&alice.get_boarding_address().unwrap(), alice_fund_amount)
        .await;

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    wait_until_balance!(&alice, confirmed: alice_fund_amount, pre_confirmed: Amount::ZERO);

    let res = alice.pay_ln_invoice(invoice).await.unwrap();

    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: alice_fund_amount - res.amount);
}

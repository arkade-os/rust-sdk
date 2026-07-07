#![allow(clippy::unwrap_used)]

use crate::common::create_lnd_invoice;
use crate::common::wait_until_balance;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Regtest;
use rand::thread_rng;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn submarine_swap() {
    // Requires the Boltz regtest environment. See scripts/boltz-setup.sh.

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
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    wait_until_balance!(&alice, confirmed: alice_fund_amount, pre_confirmed: Amount::ZERO);

    let res = alice.pay_ln_invoice(invoice).await.unwrap();

    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: alice_fund_amount - res.amount);
}

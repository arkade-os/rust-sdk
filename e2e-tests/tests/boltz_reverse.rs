#![allow(clippy::unwrap_used)]

use crate::common::start_lnd_payment;
use crate::common::wait_for_lnd_payment;
use crate::common::wait_until_balance;
use ark_client::SwapAmount;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Regtest;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn reverse_swap() {
    // Requires the Boltz regtest environment. See scripts/boltz-setup.sh.

    init_tracing();
    let regtest = Arc::new(Regtest::new());

    let secp = Secp256k1::new();

    let (alice, _) = set_up_client("alice".to_string(), regtest.clone(), secp.clone()).await;

    let invoice_amount = SwapAmount::invoice(Amount::from_sat(1_000));
    let res = alice
        .get_ln_invoice(invoice_amount, None, None)
        .await
        .unwrap();

    tracing::info!(invoice = %res.invoice, swap_id = res.swap_id, "Generated Boltz reverse swap invoice");

    let payment = start_lnd_payment(&res.invoice.to_string());

    alice.wait_for_vhtlc(&res.swap_id).await.unwrap();
    wait_for_lnd_payment(payment).await;

    tracing::info!(swap_id = res.swap_id, "Lightning invoice paid");

    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: res.amount);
}

#![allow(clippy::unwrap_used)]

use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn reverse_swap() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    let alice_fund_amount = Amount::ONE_BTC;

    nigiri
        .faucet_fund(&alice.get_boarding_address().unwrap(), alice_fund_amount)
        .await;

    let invoice = alice
        .get_ln_invoice(Amount::from_sat(10_000))
        .await
        .unwrap();

    tracing::info!(?invoice, "Generated Boltz reverse swap invoice");
}

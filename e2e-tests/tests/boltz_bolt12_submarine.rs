#![allow(clippy::unwrap_used)]

use crate::common::wait_until_balance;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::thread_rng;
use std::env;
use std::sync::Arc;

mod common;

// This test exercises the fully implemented BOLT12 submarine flow:
// BOLT12 offer -> Boltz bolt12_fetch invoice -> submarine swap -> Ark VHTLC funding.
//
// Generate a local CLN offer with:
// docker exec cln lightning-cli --network=regtest offer any "ark-rs BOLT12 e2e"
//
// Then pass the returned `bolt12` value via `BOLT12_OFFER` when running this test.
const BOLT12_OFFER_ENV: &str = "BOLT12_OFFER";

// Must be above Boltz's minimum. Used when the offer does not specify a fixed amount.
const AMOUNT_SATS: u64 = 2_000;

#[tokio::test]
#[ignore]
pub async fn bolt12_submarine_swap() {
    // This test requires the Boltz/Fulmine regtest environment in addition to the regular e2e
    // setup. See `.pi/skills/boltz-regtest/SKILL.md` for local setup instructions.
    //
    // The offer must be reachable by Boltz's BOLT12 node. In the local setup, generate it from
    // the Nigiri CLN node and pass the returned `bolt12` value via BOLT12_OFFER.

    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let offer = env::var(BOLT12_OFFER_ENV)
        .unwrap_or_else(|_| panic!("set {BOLT12_OFFER_ENV} before running this test"));

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    let alice_fund_amount = Amount::ONE_BTC;

    nigiri
        .faucet_fund(&alice.get_boarding_address().unwrap(), alice_fund_amount)
        .await;

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    wait_until_balance!(&alice, confirmed: alice_fund_amount, pre_confirmed: Amount::ZERO);

    let res = alice
        .pay_bolt12_offer(&offer, Some(AMOUNT_SATS))
        .await
        .unwrap();

    assert!(
        res.invoice.starts_with("lni"),
        "Boltz should resolve the offer into a BOLT12 invoice"
    );

    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: alice_fund_amount - res.amount);
}

#![allow(clippy::unwrap_used)]

//! Batch flows against an operator that charges intent fees.
//!
//! The rest of the suite runs with all intent fees at zero (see `.env.regtest`), so it cannot tell
//! whether the client pays them. This test sets non-zero fee programs through arkd's admin API,
//! checks that every batch flow is accepted and leaves exactly the expected balance, and restores
//! the previous fees when it is done.
//!
//! It changes the fees for the whole operator, so it must not run concurrently with other tests
//! against the same stack. CI runs every test binary on its own stack.

use crate::common::wait_until_balance;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Network;
use common::init_tracing;
use common::set_up_client;
use common::Regtest;
use rand::thread_rng;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;

mod common;

/// The admin API of the regtest arkd.
const ARKD_ADMIN_URL: &str = "http://localhost:7071";

/// Fee programs, as CEL expressions evaluated by arkd and by the client.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IntentFees {
    offchain_input_fee: String,
    onchain_input_fee: String,
    offchain_output_fee: String,
    onchain_output_fee: String,
}

#[derive(Deserialize)]
struct GetIntentFeesResponse {
    fees: IntentFees,
}

#[derive(Serialize)]
struct UpdateIntentFeesRequest<'a> {
    fees: &'a IntentFees,
}

fn get_intent_fees() -> IntentFees {
    let response = minreq::get(format!("{ARKD_ADMIN_URL}/v1/admin/intentFees"))
        .send()
        .unwrap();
    assert_eq!(response.status_code, 200, "{}", response.as_str().unwrap());
    response.json::<GetIntentFeesResponse>().unwrap().fees
}

fn set_intent_fees(fees: &IntentFees) {
    try_set_intent_fees(fees).unwrap();
}

fn try_set_intent_fees(fees: &IntentFees) -> Result<(), String> {
    let response = minreq::post(format!("{ARKD_ADMIN_URL}/v1/admin/intentFees"))
        .with_json(&UpdateIntentFeesRequest { fees })
        .map_err(|e| e.to_string())?
        .send()
        .map_err(|e| e.to_string())?;
    if response.status_code != 200 {
        return Err(format!(
            "status {}: {}",
            response.status_code,
            response.as_str().unwrap_or_default()
        ));
    }

    let response = minreq::get(format!("{ARKD_ADMIN_URL}/v1/admin/intentFees"))
        .send()
        .map_err(|e| e.to_string())?;
    let applied = response
        .json::<GetIntentFeesResponse>()
        .map_err(|e| e.to_string())?
        .fees;
    if applied.offchain_input_fee != fees.offchain_input_fee
        || applied.onchain_input_fee != fees.onchain_input_fee
        || applied.offchain_output_fee != fees.offchain_output_fee
        || applied.onchain_output_fee != fees.onchain_output_fee
    {
        return Err("the operator did not apply the fee programs".to_string());
    }

    Ok(())
}

/// Restores the operator's fees when dropped, even if the test panics.
struct RestoreFees(IntentFees);

impl Drop for RestoreFees {
    #[allow(clippy::print_stderr)]
    fn drop(&mut self) {
        if let Err(e) = try_set_intent_fees(&self.0) {
            // `eprintln!`: tracing may already be torn down. A failed restore leaves the operator
            // charging the test's fees, which breaks every test that runs after this one.
            eprintln!("FAILED TO RESTORE THE OPERATOR'S INTENT FEES: {e}");
            if !std::thread::panicking() {
                panic!("failed to restore the operator's intent fees: {e}");
            }
        }
    }
}

fn fees(
    offchain_input: &str,
    onchain_input: &str,
    offchain_output: &str,
    onchain_output: &str,
) -> IntentFees {
    IntentFees {
        offchain_input_fee: offchain_input.to_string(),
        onchain_input_fee: onchain_input.to_string(),
        offchain_output_fee: offchain_output.to_string(),
        onchain_output_fee: onchain_output.to_string(),
    }
}

const OFFCHAIN_INPUT_FEE: Amount = Amount::from_sat(100);
const ONCHAIN_INPUT_FEE: Amount = Amount::from_sat(200);
const OFFCHAIN_OUTPUT_FEE: Amount = Amount::from_sat(50);
const ONCHAIN_OUTPUT_FEE: Amount = Amount::from_sat(300);

#[tokio::test]
#[ignore]
pub async fn batch_flows_pay_intent_fees() {
    init_tracing();
    let _restore = RestoreFees(get_intent_fees());

    // Flat fees, so every expected balance below is exact.
    set_intent_fees(&fees(
        &format!("{}.0", OFFCHAIN_INPUT_FEE.to_sat()),
        &format!("{}.0", ONCHAIN_INPUT_FEE.to_sat()),
        &format!("{}.0", OFFCHAIN_OUTPUT_FEE.to_sat()),
        &format!("{}.0", ONCHAIN_OUTPUT_FEE.to_sat()),
    ));

    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    // Set up after the fees changed, so the client's cached server info carries them.
    let (alice, _) = set_up_client("alice".to_string(), regtest.clone(), secp.clone()).await;

    // Boarding: one onchain input, one offchain output.
    let fund_amount = Amount::from_sat(100_000);
    regtest
        .faucet_fund(&alice.get_boarding_address().await.unwrap(), fund_amount)
        .await;

    alice.settle(&mut rng).await.unwrap().unwrap();

    let boarded = fund_amount - ONCHAIN_INPUT_FEE - OFFCHAIN_OUTPUT_FEE;
    wait_until_balance!(&alice, confirmed: boarded, pre_confirmed: Amount::ZERO);

    // Renewal: one offchain input, one offchain output.
    let vtxo = alice
        .list_vtxos()
        .await
        .unwrap()
        .spendable_offchain()
        .next()
        .unwrap()
        .vtxo()
        .outpoint;
    alice.settle_vtxos(&mut rng, &[vtxo], &[]).await.unwrap();

    let renewed = boarded - OFFCHAIN_INPUT_FEE - OFFCHAIN_OUTPUT_FEE;
    wait_until_balance!(&alice, confirmed: renewed, pre_confirmed: Amount::ZERO);

    // Collaborative redeem: one offchain input, the onchain output and an offchain change output.
    // The fees come out of the change, never out of the amount sent.
    let redeem_amount = Amount::from_sat(50_000);
    let redeem_address = Address::p2tr(
        &secp,
        Keypair::new(&secp, &mut rng).x_only_public_key().0,
        None,
        Network::Regtest,
    );
    alice
        .collaborative_redeem(&mut rng, redeem_address.clone(), redeem_amount)
        .await
        .unwrap();

    let change =
        renewed - redeem_amount - OFFCHAIN_INPUT_FEE - ONCHAIN_OUTPUT_FEE - OFFCHAIN_OUTPUT_FEE;
    wait_until_balance!(&alice, confirmed: change, pre_confirmed: Amount::ZERO);

    regtest.mine(1).await;
    let received = wait_for_onchain_output(&regtest, &redeem_address).await;
    assert_eq!(received, redeem_amount);

    // A percentage fee that does not divide evenly: the client must round its share up, or arkd
    // rejects the intent.
    set_intent_fees(&fees(
        "amount * 0.001",
        &format!("{}.0", ONCHAIN_INPUT_FEE.to_sat()),
        &format!("{}.0", OFFCHAIN_OUTPUT_FEE.to_sat()),
        &format!("{}.0", ONCHAIN_OUTPUT_FEE.to_sat()),
    ));
    alice.refresh_server_info().await.unwrap();

    let vtxo = alice
        .list_vtxos()
        .await
        .unwrap()
        .spendable_offchain()
        .next()
        .unwrap()
        .vtxo()
        .outpoint;
    alice.settle_vtxos(&mut rng, &[vtxo], &[]).await.unwrap();

    let percentage_fee = Amount::from_sat((change.to_sat() as f64 * 0.001).ceil() as u64);
    let renewed = change - percentage_fee - OFFCHAIN_OUTPUT_FEE;
    wait_until_balance!(&alice, confirmed: renewed, pre_confirmed: Amount::ZERO);

    // Offboard everything: the client picks the amount, so no change is left behind.
    let offboard_address = Address::p2tr(
        &secp,
        Keypair::new(&secp, &mut rng).x_only_public_key().0,
        None,
        Network::Regtest,
    );
    let (_, sent) = alice
        .offboard_all(&mut rng, offboard_address.clone())
        .await
        .unwrap();

    let percentage_fee = Amount::from_sat((renewed.to_sat() as f64 * 0.001).ceil() as u64);
    assert_eq!(sent, renewed - percentage_fee - ONCHAIN_OUTPUT_FEE);
    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: Amount::ZERO);

    regtest.mine(1).await;
    let received = wait_for_onchain_output(&regtest, &offboard_address).await;
    assert_eq!(received, sent);
}

async fn wait_for_onchain_output(regtest: &Regtest, address: &Address) -> Amount {
    for _ in 0..30 {
        if let Some(utxo) = regtest.find_outpoints_blocking(address).into_iter().next() {
            return utxo.amount;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    panic!("no output to {address} found via esplora");
}

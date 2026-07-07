#![allow(clippy::unwrap_used)]

use ark_core::coin_select::select_vtxos;
use ark_core::contract::SpendPathKind;
use ark_core::send::VtxoInput;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::wait_until_balance;
use common::Regtest;
use rand::thread_rng;
use std::sync::Arc;

mod common;

/// Test that a specific pending offchain transaction can be recovered by txid.
///
/// Simulates a crash between submit and finalize by using `submit_offchain_tx` (which does
/// not call finalize), then recovering only that transaction with `finalize_pending_offchain_tx`.
#[tokio::test]
#[ignore]
pub async fn e2e_finalize_pending_tx() {
    init_tracing();
    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let (alice, _) = set_up_client("alice".to_string(), regtest.clone(), secp.clone()).await;
    let (bob, _) = set_up_client("bob".to_string(), regtest.clone(), secp).await;

    // Fund Alice via boarding.
    let alice_fund_amount = Amount::ONE_BTC;
    let alice_boarding_address = alice.get_boarding_address().await.unwrap();
    regtest
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    wait_until_balance!(&alice, confirmed: alice_fund_amount);
    tracing::info!("Alice has confirmed VTXO");

    // Build VTXO inputs for the offchain tx.
    let send_amount = Amount::from_sat(100_000);
    let (bob_address, _) = bob.get_offchain_address().await.unwrap();

    let vtxo_list = alice.list_vtxos().await.unwrap();

    let spendable = vtxo_list
        .spendable_offchain()
        .map(|entry| ark_core::coin_select::VirtualTxOutPoint {
            outpoint: entry.vtxo.outpoint,
            script_pubkey: entry.vtxo.script.clone(),
            expire_at: entry.vtxo.expires_at,
            amount: entry.vtxo.amount,
            assets: entry.vtxo.assets.clone(),
        })
        .collect::<Vec<_>>();

    let selected = select_vtxos(
        spendable,
        send_amount,
        alice.server_info().await.unwrap().dust,
        true,
    )
    .unwrap();

    let vtxo_inputs: Vec<VtxoInput> = selected
        .into_iter()
        .map(|coin| {
            let entry = vtxo_list
                .all()
                .find(|entry| entry.vtxo.outpoint == coin.outpoint)
                .unwrap();
            let spend_selection = entry.spend_selection(SpendPathKind::Forfeit).unwrap();
            VtxoInput::new_with_spend_selection(
                spend_selection,
                entry.tapscripts(),
                entry.script_pubkey(),
                coin.amount,
                coin.outpoint,
                coin.assets,
            )
        })
        .collect();

    // Submit the offchain tx but do NOT finalize (simulating a crash).
    let ark_txid = alice
        .submit_offchain_tx(vtxo_inputs, bob_address, send_amount)
        .await
        .unwrap();

    tracing::info!(%ark_txid, "Submitted offchain tx without finalizing (simulating crash)");

    // Small delay for server to process the submission.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    let pending = alice.list_pending_offchain_txs().await.unwrap();
    assert!(
        pending.iter().any(|tx| tx.ark_txid == ark_txid),
        "should find pending tx {ark_txid} before finalizing"
    );

    // Recover only the requested pending transaction.
    alice.finalize_pending_offchain_tx(ark_txid).await.unwrap();

    // Verify there is nothing left for the broad recovery API to finalize.
    let finalized = alice.continue_pending_offchain_txs().await.unwrap();
    assert!(
        finalized.is_empty(),
        "specific finalize should leave no pending txs, got {finalized:?}"
    );

    // Verify balances.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    wait_until_balance!(
        &alice,
        pre_confirmed: alice_fund_amount - send_amount,
    );
    wait_until_balance!(
        &bob,
        pre_confirmed: send_amount,
    );

    tracing::info!("Balances verified after specific pending tx recovery");
}

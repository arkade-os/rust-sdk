#![allow(clippy::unwrap_used)]

use ark_core::coin_select::select_vtxos;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::wait_until_balance;
use common::Nigiri;
use rand::thread_rng;
use std::sync::Arc;

mod common;

/// Test that a pending (submitted but not finalized) offchain transaction can be recovered.
///
/// Simulates a crash between submit and finalize by using `submit_offchain_tx` (which does
/// not call finalize), then recovering with `continue_pending_offchain_txs`.
#[tokio::test]
#[ignore]
pub async fn e2e_continue_pending_tx() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let (bob, _) = set_up_client("bob".to_string(), nigiri.clone(), secp).await;

    // Fund Alice via boarding.
    let alice_fund_amount = Amount::ONE_BTC;
    let alice_boarding_address = alice.get_boarding_address().unwrap();
    nigiri
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    wait_until_balance!(&alice, confirmed: alice_fund_amount);
    tracing::info!("Alice has confirmed VTXO");

    // Build VTXO inputs for the offchain tx.
    let send_amount = Amount::from_sat(100_000);
    let (bob_address, _) = bob.get_offchain_address().unwrap();

    let (vtxo_list, script_pubkey_to_vtxo_map) = alice.list_vtxos().await.unwrap();

    let spendable = vtxo_list
        .spendable_offchain()
        .map(|vtxo| ark_core::coin_select::VirtualTxOutPoint {
            outpoint: vtxo.outpoint,
            script_pubkey: vtxo.script.clone(),
            expire_at: vtxo.expires_at,
            amount: vtxo.amount,
        })
        .collect::<Vec<_>>();

    let selected = select_vtxos(spendable, send_amount, alice.server_info.dust, true).unwrap();

    let vtxo_inputs: Vec<ark_core::send::VtxoInput> = selected
        .into_iter()
        .map(|coin| {
            let vtxo = script_pubkey_to_vtxo_map.get(&coin.script_pubkey).unwrap();
            let (forfeit_script, control_block) = vtxo.forfeit_spend_info().unwrap();
            ark_core::send::VtxoInput::new(
                forfeit_script,
                None,
                control_block,
                vtxo.tapscripts(),
                vtxo.script_pubkey(),
                coin.amount,
                coin.outpoint,
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

    // Recover by continuing the pending transaction.
    let finalized = alice.continue_pending_offchain_txs().await.unwrap();

    assert!(!finalized.is_empty(), "should have finalized pending tx(s)");
    assert!(finalized.contains(&ark_txid));
    tracing::info!(?finalized, "Recovered pending transactions");

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

    tracing::info!("Balances verified after recovery");
}

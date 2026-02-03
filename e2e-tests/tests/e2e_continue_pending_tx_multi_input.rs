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

/// Test that a pending offchain transaction with MULTIPLE inputs can be recovered.
///
/// This is a regression test for the case where a pending transaction consumes
/// more than one VTXO as inputs. The server may require all inputs to be present
/// in the GetPendingTx intent to return the pending transaction.
///
/// Flow:
/// 1. Fund Alice 1 BTC → settle → 1 confirmed VTXO
/// 2. Alice sends 0.4 BTC to Bob → Alice has ~0.6 BTC change, Bob has 0.4 BTC
/// 3. Bob sends 0.2 BTC back to Alice → Alice has 2 VTXOs (~0.6 + 0.2)
/// 4. Submit offchain tx using both VTXOs (no finalize)
/// 5. Recover with continue_pending_offchain_txs
/// 6. Verify balances
#[tokio::test]
#[ignore]
pub async fn e2e_continue_pending_tx_multi_input() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let (bob, _) = set_up_client("bob".to_string(), nigiri.clone(), secp.clone()).await;
    let (carol, _) = set_up_client("carol".to_string(), nigiri.clone(), secp).await;

    // Step 1: Fund Alice via boarding and settle.
    let alice_fund_amount = Amount::ONE_BTC;
    let alice_boarding_address = alice.get_boarding_address().unwrap();
    nigiri
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    wait_until_balance!(&alice, confirmed: alice_fund_amount);
    tracing::info!("Alice has 1 confirmed VTXO");

    // Step 2: Alice sends to Bob → Alice gets change VTXO.
    let send_to_bob = Amount::from_sat(40_000_000); // 0.4 BTC
    let (bob_address, _) = bob.get_offchain_address().unwrap();
    alice.send_vtxo(bob_address, send_to_bob).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_change = alice_fund_amount - send_to_bob;
    wait_until_balance!(&alice, pre_confirmed: alice_change);
    wait_until_balance!(&bob, pre_confirmed: send_to_bob);
    tracing::info!(%alice_change, "Alice has 1 change VTXO, Bob has 0.4 BTC");

    // Step 3: Bob sends back to Alice → Alice now has 2 VTXOs.
    let send_back = Amount::from_sat(20_000_000); // 0.2 BTC
    let (alice_address, _) = alice.get_offchain_address().unwrap();
    bob.send_vtxo(alice_address, send_back).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_total = alice_change + send_back;
    wait_until_balance!(&alice, pre_confirmed: alice_total);
    tracing::info!(%alice_total, "Alice has 2 VTXOs");

    // Verify Alice has exactly 2 spendable VTXOs.
    let (vtxo_list, script_pubkey_to_vtxo_map) = alice.list_vtxos().await.unwrap();
    let spendable: Vec<_> = vtxo_list.spendable_offchain().collect();
    assert_eq!(
        spendable.len(),
        2,
        "Alice should have exactly 2 spendable VTXOs, got {}",
        spendable.len()
    );
    tracing::info!(
        vtxo_1 = %spendable[0].outpoint,
        vtxo_1_amount = %spendable[0].amount,
        vtxo_2 = %spendable[1].outpoint,
        vtxo_2_amount = %spendable[1].amount,
        "Alice has 2 spendable VTXOs"
    );

    // Step 4: Send an amount that requires BOTH VTXOs.
    // Alice has ~0.6 BTC + 0.2 BTC. Send 0.7 BTC to carol (needs both).
    let send_amount = Amount::from_sat(70_000_000); // 0.7 BTC
    let (carol_address, _) = carol.get_offchain_address().unwrap();

    let spendable_coins = vtxo_list
        .spendable_offchain()
        .map(|vtxo| ark_core::coin_select::VirtualTxOutPoint {
            outpoint: vtxo.outpoint,
            script_pubkey: vtxo.script.clone(),
            expire_at: vtxo.expires_at,
            amount: vtxo.amount,
        })
        .collect::<Vec<_>>();

    let selected =
        select_vtxos(spendable_coins, send_amount, alice.server_info.dust, true).unwrap();
    assert_eq!(
        selected.len(),
        2,
        "Coin selection should pick both VTXOs, got {}",
        selected.len()
    );

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

    tracing::info!(
        num_inputs = vtxo_inputs.len(),
        %send_amount,
        "Submitting multi-input offchain tx WITHOUT finalizing"
    );

    // Submit but do NOT finalize (simulating a crash).
    let ark_txid = alice
        .submit_offchain_tx(vtxo_inputs, carol_address, send_amount)
        .await
        .unwrap();

    tracing::info!(%ark_txid, "Submitted multi-input offchain tx");

    // Small delay for server to process.
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    // Step 5: Verify list_pending_offchain_txs finds the pending tx.
    let pending = alice.list_pending_offchain_txs().await.unwrap();
    assert!(
        !pending.is_empty(),
        "should find pending tx(s) via list_pending_offchain_txs"
    );
    tracing::info!(num_pending = pending.len(), "Found pending transactions");

    // Recover by continuing the pending transaction.
    let finalized = alice.continue_pending_offchain_txs().await.unwrap();

    assert!(!finalized.is_empty(), "should have finalized pending tx(s)");
    assert!(
        finalized.contains(&ark_txid),
        "finalized list should contain our ark_txid {ark_txid}"
    );
    tracing::info!(?finalized, "Recovered pending multi-input transaction");

    // Step 6: Verify balances.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let expected_alice_change = alice_total - send_amount;
    wait_until_balance!(
        &alice,
        pre_confirmed: expected_alice_change,
    );
    wait_until_balance!(
        &carol,
        pre_confirmed: send_amount,
    );

    tracing::info!(
        alice_remaining = %expected_alice_change,
        carol_received = %send_amount,
        "Balances verified after multi-input recovery"
    );
}

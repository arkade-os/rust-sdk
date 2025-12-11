#![allow(clippy::unwrap_used)]

use crate::common::wait_until_balance;
use ark_client::wallet::OnchainWallet;
use bitcoin::Amount;
use bitcoin::address::NetworkUnchecked;
use bitcoin::key::Secp256k1;
use common::Nigiri;
use common::init_tracing;
use common::set_up_client;
use rand::thread_rng;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

mod common;

#[tokio::test]
#[ignore]
pub async fn send_onchain_vtxo_and_boarding_output() {
    init_tracing();

    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let (alice, alice_wallet) =
        set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    let offchain_balance = alice.offchain_balance().await.unwrap();

    assert_eq!(offchain_balance.total(), Amount::ZERO);

    let alice_boarding_address = alice.get_boarding_address().unwrap();

    let fund_amount = Amount::ONE_BTC;

    nigiri
        .faucet_fund(&alice_boarding_address, fund_amount)
        .await;

    // We give Alice two extra UTXOs to be able to bump the two transactions she needs to broadcast
    // to commit her VTXO (and the change VTXO too!) to the blockchain.
    let alice_onchain_address = alice.get_onchain_address().unwrap();
    nigiri
        .faucet_fund(&alice_onchain_address, Amount::from_sat(100_000))
        .await;
    nigiri
        .faucet_fund(&alice_onchain_address, Amount::from_sat(100_000))
        .await;

    let offchain_balance = alice.offchain_balance().await.unwrap();

    assert_eq!(offchain_balance.total(), Amount::ZERO);

    alice.settle(&mut rng).await.unwrap();
    wait_until_balance!(&alice, confirmed: fund_amount, pre_confirmed: Amount::ZERO);

    // Ensure that the commitment TX is mined.
    nigiri.mine(1).await;
    alice_wallet.sync().await.unwrap();

    let (alice_offchain_address, _) = alice.get_offchain_address().unwrap();

    alice
        .send_vtxo(alice_offchain_address, Amount::from_sat(100_000))
        .await
        .unwrap();

    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: fund_amount);

    let unilateral_exit_trees = alice.build_unilateral_exit_trees().await.unwrap();

    // Mine blocks regularly to ensure that any transaction published by the Ark server actually
    // gets confirmed.
    tokio::spawn({
        let nigiri = nigiri.clone();
        let alice_wallet = alice_wallet.clone();
        async move {
            loop {
                nigiri.mine(1).await;
                alice_wallet.sync().await.unwrap();

                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });

    for (i, unilateral_exit_tree) in unilateral_exit_trees.iter().enumerate() {
        while let Some(txid) = alice
            .broadcast_next_unilateral_exit_node(unilateral_exit_tree)
            .await
            .expect("to broadcast unilateral exit node")
        {
            tracing::info!(i, %txid, "Broadcast virtual transaction");

            // The transaction needs a confirmation so that we can bump the P2A output for the next
            // transaction in the tree.
            nigiri.mine(1).await;
            alice_wallet.sync().await.unwrap();
        }

        tracing::debug!(i, "Finished with unilateral exit tree");
    }

    // Get one confirmation on the VTXO.
    nigiri.mine(1).await;

    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: Amount::ZERO);

    let alice_boarding_address = alice.get_boarding_address().unwrap();
    nigiri
        .faucet_fund(&alice_boarding_address, Amount::ONE_BTC)
        .await;

    let offchain_balance = alice.offchain_balance().await.unwrap();

    assert_eq!(offchain_balance.confirmed(), Amount::ZERO);
    assert_eq!(offchain_balance.pre_confirmed(), Amount::ZERO);

    // To be able to spend a VTXO it needs to have been confirmed for at least
    // `unilateral_exit_delay` seconds.
    //
    // And to be able to spend a boarding output it needs to have been confirmed for at least
    // `boarding_exit_delay` seconds.
    //
    // We take the larger value of the two here.
    let boarding_exit_delay = alice.boarding_exit_delay_seconds();
    let unilateral_vtxo_exit_delay = alice.unilateral_vtxo_exit_delay_seconds();
    let blocktime_offset = boarding_exit_delay.max(unilateral_vtxo_exit_delay);

    nigiri.set_outpoint_blocktime_offset(blocktime_offset);

    let (tx, prevouts) = alice
        .create_send_on_chain_transaction(
            bitcoin::Address::<NetworkUnchecked>::from_str(
                "bcrt1q8df4sx3hz63tq44ve3q6tr4qz0q30usk5sntpt",
            )
            .unwrap()
            .assume_checked(),
            Amount::from_btc(1.4).unwrap(),
        )
        .await
        .unwrap();

    // 1 boarding output and 2 VTXOs.
    assert_eq!(tx.input.len(), 3);
    assert_eq!(prevouts.len(), 3);

    for (i, prevout) in prevouts.iter().enumerate() {
        let script_pubkey = prevout.script_pubkey.clone();
        let amount = prevout.value;
        let spent_outputs = prevouts
            .iter()
            .map(|o| bitcoinconsensus::Utxo {
                script_pubkey: o.script_pubkey.as_bytes().as_ptr(),
                script_pubkey_len: o.script_pubkey.len() as u32,
                value: o.value.to_sat() as i64,
            })
            .collect::<Vec<_>>();

        bitcoinconsensus::verify(
            script_pubkey.as_bytes(),
            amount.to_sat(),
            bitcoin::consensus::serialize(&tx).as_slice(),
            Some(&spent_outputs),
            i,
        )
        .expect("valid input");
    }
}

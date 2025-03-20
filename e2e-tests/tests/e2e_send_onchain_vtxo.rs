#![allow(clippy::unwrap_used)]

use crate::common::InMemoryDb;
use ark_bdk_wallet::Wallet;
use bitcoin::address::NetworkUnchecked;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
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

    let alice = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    let offchain_balance = alice.offchain_balance().await.unwrap();

    assert_eq!(offchain_balance.total(), Amount::ZERO);

    let alice_boarding_address = alice.get_boarding_address().unwrap();

    let fund_amount = Amount::ONE_BTC;

    nigiri
        .faucet_fund(&alice_boarding_address, fund_amount)
        .await;

    let offchain_balance = alice.offchain_balance().await.unwrap();

    assert_eq!(offchain_balance.total(), Amount::ZERO);

    alice.board(&mut rng).await.unwrap();
    wait_until_balance(&alice, fund_amount, Amount::ZERO).await;

    alice.commit_vtxos_on_chain().await.unwrap();
    wait_until_balance(&alice, Amount::ZERO, Amount::ZERO).await;

    // Get one confirmation on the VTXO.
    nigiri.mine(1).await;

    let alice_boarding_address = alice.get_boarding_address().unwrap();
    nigiri
        .faucet_fund(&alice_boarding_address, Amount::ONE_BTC)
        .await;

    let offchain_balance = alice.offchain_balance().await.unwrap();

    assert_eq!(offchain_balance.confirmed(), Amount::ZERO);
    assert_eq!(offchain_balance.pending(), Amount::ZERO);

    // To be able to spend a VTXO or a boarding output it needs to have been confirmed for at least
    // 1024 seconds.
    nigiri.set_outpoint_blocktime_offset(1024);

    let (tx, prevouts) = alice
        .create_send_on_chain_transaction(
            bitcoin::Address::<NetworkUnchecked>::from_str(
                "bcrt1q8df4sx3hz63tq44ve3q6tr4qz0q30usk5sntpt",
            )
            .unwrap()
            .assume_checked(),
            Amount::from_btc(0.7).unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(tx.input.len(), 2);
    assert_eq!(prevouts.len(), 2);

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

async fn wait_until_balance(
    client: &ark_client::Client<Nigiri, Wallet<InMemoryDb>>,
    confirmed_target: Amount,
    pending_target: Amount,
) {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let offchain_balance = client.offchain_balance().await.unwrap();

            tracing::debug!(
                ?offchain_balance,
                %confirmed_target,
                %pending_target,
                "Waiting for balance to match targets"
            );

            if offchain_balance.confirmed() == confirmed_target
                && offchain_balance.pending() == pending_target
            {
                return;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap();
}

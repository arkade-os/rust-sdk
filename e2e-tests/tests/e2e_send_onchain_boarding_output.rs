#![allow(clippy::unwrap_used)]

use bitcoin::address::NetworkUnchecked;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use std::str::FromStr;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn send_onchain_boarding_output() {
    init_tracing();

    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    // To be able to spend a boarding output it needs to have been confirmed for at least
    // `boarding_exit_delay` seconds.
    let boarding_exit_delay = alice.boarding_exit_delay_seconds();

    nigiri.set_outpoint_blocktime_offset(boarding_exit_delay);

    let alice_boarding_address = alice.get_boarding_address().unwrap();

    nigiri
        .faucet_fund(&alice_boarding_address, Amount::ONE_BTC)
        .await;

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

    assert_eq!(tx.input.len(), 1);
    assert_eq!(prevouts.len(), 1);

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

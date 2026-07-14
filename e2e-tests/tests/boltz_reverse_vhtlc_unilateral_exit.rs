#![allow(clippy::unwrap_used)]

use crate::common::format_command_output;
use crate::common::start_lnd_payment;
use crate::common::wait_for_lnd_payment;
use crate::common::wait_until_balance;
use ark_client::AnchorSpendDeps;
use ark_client::SwapAmount;
use bitcoin::address::NetworkUnchecked;
use bitcoin::key::Secp256k1;
use bitcoin::relative;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Regtest;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

mod common;

#[tokio::test]
#[ignore]
pub async fn reverse_swap_claim_with_vhtlc_ancestor_can_exit_unilaterally() {
    // Requires the Boltz regtest environment. See scripts/boltz-setup.sh.
    init_tracing();

    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();

    let (alice, alice_wallet) =
        set_up_client("alice".to_string(), regtest.clone(), secp.clone()).await;

    // The unilateral exit transactions are fee-bumped through Alice's on-chain wallet.
    let alice_onchain_address = alice_wallet.get_onchain_address().unwrap();
    for _ in 0..5 {
        regtest
            .faucet_fund(&alice_onchain_address, Amount::from_sat(100_000))
            .await;
    }

    let invoice_amount = SwapAmount::invoice(Amount::from_sat(10_000));
    let reverse_swap = alice
        .get_ln_invoice(invoice_amount, None, None)
        .await
        .unwrap();

    tracing::info!(
        invoice = %reverse_swap.invoice,
        swap_id = reverse_swap.swap_id,
        "Generated Boltz reverse swap invoice"
    );

    let mut payment = start_lnd_payment(&reverse_swap.invoice.to_string());

    let claim = tokio::select! {
        res = alice.wait_for_vhtlc(&reverse_swap.swap_id) => res.unwrap(),
        payment_res = &mut payment => {
            let output = payment_res
                .expect("lncli payinvoice task panicked")
                .expect("failed to wait for lncli payinvoice");
            panic!(
                "lncli payinvoice exited before the VHTLC was claimed: {}",
                format_command_output(&output)
            );
        }
        () = tokio::time::sleep(Duration::from_secs(120)) => {
            payment.abort();
            panic!("timed out waiting for Boltz to fund and claim the VHTLC");
        }
    };
    wait_for_lnd_payment(payment).await;

    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: claim.claim_amount);

    let unilateral_exit_trees = alice.build_unilateral_exit_trees().await.unwrap();
    assert!(
        !unilateral_exit_trees.is_empty(),
        "expected a unilateral exit tree for the VTXO claimed from the VHTLC"
    );

    // Mine blocks regularly to ensure any transaction published by the Ark server confirms.
    tokio::spawn({
        let regtest = regtest.clone();
        let alice_wallet = alice_wallet.clone();
        async move {
            loop {
                regtest.mine(1).await;
                alice_wallet.sync().await.unwrap();

                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });

    let bump_deps = AnchorSpendDeps {
        change_address: Box::new({
            let wallet = alice_wallet.clone();
            move || wallet.get_onchain_address()
        }),
        select_coins: Box::new({
            let wallet = alice_wallet.clone();
            move |amount| wallet.select_coins(amount)
        }),
        sign: Box::new({
            let wallet = alice_wallet.clone();
            move |psbt| wallet.sign(psbt)
        }),
    };

    for (i, unilateral_exit_tree) in unilateral_exit_trees.iter().enumerate() {
        while let Some(txid) = alice
            .broadcast_next_unilateral_exit_node(unilateral_exit_tree, &bump_deps)
            .await
            .expect("to broadcast unilateral exit node")
        {
            tracing::info!(i, %txid, "Broadcast virtual transaction");

            // Each transaction needs a confirmation so the next transaction in the tree can use
            // the P2A fee-bump output.
            regtest.mine(1).await;
            alice_wallet.sync().await.unwrap();
        }

        tracing::debug!(i, "Finished with unilateral exit tree");
    }

    // Confirm the exited VTXO itself.
    regtest.mine(1).await;
    alice_wallet.sync().await.unwrap();

    wait_until_balance!(&alice, confirmed: Amount::ZERO, pre_confirmed: Amount::ZERO);

    let mut max_block_height_offset = 0;
    let mut max_blocktime_offset = 0;

    match alice
        .server_info()
        .await
        .unwrap()
        .unilateral_exit_delay
        .to_relative_lock_time()
        .expect("unilateral VTXO exit delay should be relative")
    {
        relative::LockTime::Blocks(height) => {
            max_block_height_offset = height.value();
        }
        relative::LockTime::Time(time) => {
            max_blocktime_offset = time.value() * 512;
        }
    };

    regtest.set_outpoint_block_height_offset(max_block_height_offset as u64);
    regtest.set_outpoint_blocktime_offset(max_blocktime_offset as u64);

    let send_amount = claim.claim_amount - Amount::from_sat(1_000);
    let send_address = bitcoin::Address::<NetworkUnchecked>::from_str(
        "bcrt1q8df4sx3hz63tq44ve3q6tr4qz0q30usk5sntpt",
    )
    .unwrap()
    .assume_checked();
    let (tx, prevouts) = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match alice
                .create_send_on_chain_transaction(
                    send_address.clone(),
                    send_amount,
                    alice_wallet.get_onchain_address().unwrap(),
                )
                .await
            {
                Ok(result) => return result,
                Err(err) => {
                    tracing::debug!(%err, "Waiting for exited VTXO to become spendable on-chain");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    })
    .await
    .expect("timed out waiting for exited VTXO to become spendable on-chain");

    assert_eq!(tx.input.len(), prevouts.len());
    assert!(
        !prevouts.is_empty(),
        "expected the unilaterally exited VTXO to be spendable on-chain"
    );

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

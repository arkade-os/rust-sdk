#![allow(clippy::unwrap_used)]

use ark_core::send;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::server::GetVtxosRequest;
use ark_core::Vtxo;
use ark_core::VtxoList;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::opcodes::all::OP_CHECKSIG;
use bitcoin::opcodes::all::OP_CHECKSIGVERIFY;
use bitcoin::opcodes::all::OP_CSV;
use bitcoin::opcodes::all::OP_DROP;
use bitcoin::psbt;
use bitcoin::script::ScriptBuf;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Amount;
use bitcoin::XOnlyPublicKey;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::thread_rng;
use std::sync::Arc;

mod common;

/// Test a 2-of-3 multisig VTXO flow using tapscript multisig:
///
/// 1. Alice funds herself via boarding + settle.
/// 2. Alice sends to a shared VTXO with 3 tapscript leaf pairs (one per 2-of-3
///    combination: Alice+Bob, Alice+Carol, Bob+Carol). Each pair consists of a
///    cooperative forfeit leaf (server + two signers) and a unilateral exit leaf
///    (CSV + two signers).
/// 3. Alice and Bob cooperatively sign an offchain transaction spending the
///    shared VTXO to Bob's regular address using the Alice+Bob leaf.
///    Carol is not involved in this spend.
/// 4. Verify the shared VTXO is spent and Bob received the funds.
#[tokio::test]
#[ignore]
pub async fn e2e_2of3_escrow() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    // Set up Alice (for funding and sending to multisig)
    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    // Create keypairs for the 3 multisig participants
    let alice_kp = Keypair::new(&secp, &mut rng);
    let bob_kp = Keypair::new(&secp, &mut rng);
    let carol_kp = Keypair::new(&secp, &mut rng);

    let alice_pk = alice_kp.public_key().x_only_public_key().0;
    let bob_pk = bob_kp.public_key().x_only_public_key().0;
    let carol_pk = carol_kp.public_key().x_only_public_key().0;

    let server_pk = alice.server_info.signer_pk.x_only_public_key().0;
    let exit_delay = alice.server_info.unilateral_exit_delay;

    // Fund Alice via boarding
    let alice_boarding_address = alice.get_boarding_address().unwrap();
    let alice_fund_amount = Amount::ONE_BTC;

    let alice_boarding_outpoint = nigiri
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;

    tracing::debug!(?alice_boarding_outpoint, "Funded Alice's boarding output");

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_starting_balance = alice.offchain_balance().await.unwrap();
    tracing::info!(?alice_starting_balance, "Alice got confirmed VTXO");

    assert_eq!(alice_starting_balance.confirmed(), alice_fund_amount);

    // Build 6 tapscript leaves: 3 forfeit (server + pair) + 3 exit (CSV + pair)
    let forfeit_ab = forfeit_multisig_script(server_pk, alice_pk, bob_pk);
    let forfeit_ac = forfeit_multisig_script(server_pk, alice_pk, carol_pk);
    let forfeit_bc = forfeit_multisig_script(server_pk, bob_pk, carol_pk);

    let exit_ab = csv_multisig_script(exit_delay, alice_pk, bob_pk);
    let exit_ac = csv_multisig_script(exit_delay, alice_pk, carol_pk);
    let exit_bc = csv_multisig_script(exit_delay, bob_pk, carol_pk);

    // Create the shared VTXO with custom scripts.
    // The "owner" field is set to Alice's key (arbitrary — it's only used by
    // `forfeit_spend_info` which we don't call for custom VTXOs).
    let shared_vtxo = Vtxo::new_with_custom_scripts(
        &secp,
        server_pk,
        alice_pk,
        vec![
            forfeit_ab.clone(),
            forfeit_ac.clone(),
            forfeit_bc.clone(),
            exit_ab,
            exit_ac,
            exit_bc,
        ],
        exit_delay,
        alice.server_info.network,
    )
    .unwrap();

    // Alice sends to the shared multisig address
    let msig_amount = Amount::from_sat(100_000);

    let send_txid = alice
        .send_vtxo(shared_vtxo.to_ark_address(), msig_amount)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    tracing::info!(%send_txid, "Alice funded shared 2-of-3 multisig VTXO");

    // Look up the shared VTXO to get the outpoint
    let mut grpc_client = ark_grpc::Client::new("http://localhost:7070".to_string());
    grpc_client.connect().await.unwrap();

    let request = GetVtxosRequest::new_for_addresses(std::iter::once(shared_vtxo.to_ark_address()));
    let response = grpc_client.list_vtxos(request).await.unwrap();

    let vtxo_list = VtxoList::new(alice.server_info.dust, response.vtxos);
    let msig_vtxo = vtxo_list
        .spendable_offchain()
        .next()
        .expect("shared VTXO should be spendable");

    tracing::info!(
        outpoint = %msig_vtxo.outpoint,
        amount = %msig_vtxo.amount,
        "Found shared 2-of-3 multisig VTXO"
    );

    // Bob's payout VTXO address
    let bob_payout_vtxo = Vtxo::new_default(
        &secp,
        server_pk,
        bob_pk,
        exit_delay,
        alice.server_info.network,
    )
    .unwrap();

    // Check Bob's balance before the spend
    let bob_balance_before = {
        let request =
            GetVtxosRequest::new_for_addresses(std::iter::once(bob_payout_vtxo.to_ark_address()));
        let response = grpc_client.list_vtxos(request).await.unwrap();
        let list = VtxoList::new(alice.server_info.dust, response.vtxos);
        list.spendable_offchain()
            .fold(Amount::ZERO, |acc, v| acc + v.amount)
    };

    assert_eq!(bob_balance_before, Amount::ZERO);

    // Spend using the Alice+Bob forfeit leaf
    let control_block = shared_vtxo.get_spend_info(forfeit_ab.clone()).unwrap();

    let msig_input = send::VtxoInput::new(
        forfeit_ab,
        None,
        control_block,
        shared_vtxo.tapscripts(),
        shared_vtxo.script_pubkey(),
        msig_vtxo.amount,
        msig_vtxo.outpoint,
    );

    let offchain_txs = build_offchain_transactions(
        &[(&bob_payout_vtxo.to_ark_address(), msig_amount)],
        None,
        &[msig_input],
        &alice.server_info,
    )
    .unwrap();

    let send::OffchainTransactions {
        ark_tx: mut virtual_tx,
        checkpoint_txs,
    } = offchain_txs;

    let mut pre_signed_checkpoint = checkpoint_txs[0].clone();

    // --- Sign the virtual TX (Alice + Bob) ---
    {
        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let secp = Secp256k1::new();
                let alice_sig = secp.sign_schnorr_no_aux_rand(&msg, &alice_kp);
                let bob_sig = secp.sign_schnorr_no_aux_rand(&msg, &bob_kp);
                Ok(vec![(alice_sig, alice_pk), (bob_sig, bob_pk)])
            };
        sign_ark_transaction(sign_fn, &mut virtual_tx, 0).unwrap();
    }

    // --- Sign the checkpoint TX (Alice + Bob) ---
    {
        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let secp = Secp256k1::new();
                let alice_sig = secp.sign_schnorr_no_aux_rand(&msg, &alice_kp);
                let bob_sig = secp.sign_schnorr_no_aux_rand(&msg, &bob_kp);
                Ok(vec![(alice_sig, alice_pk), (bob_sig, bob_pk)])
            };
        sign_checkpoint_transaction(sign_fn, &mut pre_signed_checkpoint).unwrap();
    }

    // Submit the signed virtual TX + unsigned checkpoint to the server
    let virtual_txid = virtual_tx.unsigned_tx.compute_txid();

    let res = grpc_client
        .submit_offchain_transaction_request(virtual_tx, checkpoint_txs)
        .await
        .unwrap();

    // Combine our pre-signed checkpoint with the server's signature
    pre_signed_checkpoint
        .combine(res.signed_checkpoint_txs[0].clone())
        .unwrap();

    // Finalize the offchain transaction
    grpc_client
        .finalize_offchain_transaction(virtual_txid, vec![pre_signed_checkpoint])
        .await
        .unwrap();

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    tracing::info!(
        %virtual_txid,
        "Cooperatively spent 2-of-3 multisig VTXO to Bob's address (Alice+Bob signed)"
    );

    // Verify: shared VTXO is spent
    let request = GetVtxosRequest::new_for_addresses(std::iter::once(shared_vtxo.to_ark_address()));
    let response = grpc_client.list_vtxos(request).await.unwrap();
    let vtxo_list = VtxoList::new(alice.server_info.dust, response.vtxos);

    assert_eq!(
        vtxo_list.spendable_offchain().count(),
        0,
        "shared VTXO should be spent"
    );

    // Verify: Bob's balance increased by the multisig amount
    let bob_balance_after = {
        let request =
            GetVtxosRequest::new_for_addresses(std::iter::once(bob_payout_vtxo.to_ark_address()));
        let response = grpc_client.list_vtxos(request).await.unwrap();
        let list = VtxoList::new(alice.server_info.dust, response.vtxos);
        list.spendable_offchain()
            .fold(Amount::ZERO, |acc, v| acc + v.amount)
    };

    assert_eq!(
        bob_balance_after - bob_balance_before,
        msig_amount,
        "Bob's balance should have increased by the multisig amount"
    );

    tracing::info!(
        %bob_balance_before,
        %bob_balance_after,
        "Bob received funds from 2-of-3 multisig VTXO"
    );
}

/// 3-of-3 forfeit script: server + two signers.
fn forfeit_multisig_script(
    server_pk: XOnlyPublicKey,
    pk_a: XOnlyPublicKey,
    pk_b: XOnlyPublicKey,
) -> ScriptBuf {
    ScriptBuf::builder()
        .push_x_only_key(&server_pk)
        .push_opcode(OP_CHECKSIGVERIFY)
        .push_x_only_key(&pk_a)
        .push_opcode(OP_CHECKSIGVERIFY)
        .push_x_only_key(&pk_b)
        .push_opcode(OP_CHECKSIG)
        .into_script()
}

/// CSV + 2-of-2 exit script: timelock + two signers.
fn csv_multisig_script(
    locktime: bitcoin::Sequence,
    pk_a: XOnlyPublicKey,
    pk_b: XOnlyPublicKey,
) -> ScriptBuf {
    ScriptBuf::builder()
        .push_int(locktime.to_consensus_u32() as i64)
        .push_opcode(OP_CSV)
        .push_opcode(OP_DROP)
        .push_x_only_key(&pk_a)
        .push_opcode(OP_CHECKSIGVERIFY)
        .push_x_only_key(&pk_b)
        .push_opcode(OP_CHECKSIG)
        .into_script()
}

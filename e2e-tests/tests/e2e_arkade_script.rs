#![allow(clippy::unwrap_used)]

use crate::common::wait_until_balance;
use ark_core::introspector::packet::add_packet_to_psbt;
use ark_core::introspector::packet::IntrospectorEntry;
use ark_core::introspector::packet::Packet;
use ark_core::script::csv_sig_script;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::SendReceiver;
use ark_core::send::VtxoInput;
use ark_core::server::GetVtxosRequest;
use ark_core::Vtxo;
use ark_core::VtxoList;
use ark_introspector_client::IntrospectorClient;
use ark_script::opcodes::op;
use ark_script::ArkadeLeaf;
use ark_script::ArkadeTapscript;
use ark_script::ArkadeVtxoInput;
use ark_script::ArkadeVtxoScript;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::opcodes::all::OP_EQUAL;
use bitcoin::opcodes::all::OP_EQUALVERIFY;
use bitcoin::opcodes::all::OP_PUSHNUM_1;
use bitcoin::script::PushBytesBuf;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Amount;
use bitcoin::ScriptBuf;
use bitcoin::Witness;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::thread_rng;
use std::sync::Arc;
use std::time::Duration;

mod common;

#[tokio::test]
#[ignore]
pub async fn e2e_arkade_script_submit_tx_to_bob() {
    init_tracing();

    let nigiri = Arc::new(Nigiri::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let (bob, _) = set_up_client("bob".to_string(), nigiri.clone(), secp.clone()).await;

    let mut grpc_client = ark_grpc::Client::new("http://localhost:7070".to_string());
    grpc_client.connect().await.unwrap();
    let server_info = grpc_client.get_info().await.unwrap();

    let introspector_url =
        std::env::var("INTROSPECTOR_URL").unwrap_or_else(|_| "http://127.0.0.1:7073".to_string());
    let introspector = IntrospectorClient::new(introspector_url.clone());
    let introspector_info = introspector.get_info().await.unwrap();
    let introspector_pk = introspector_info.signer_xonly();

    tracing::info!(
        introspector_url,
        introspector_signer = %introspector_pk,
        arkd_signer = %server_info.signer_pk,
        "connected to arkd and introspector"
    );

    let fund_amount = Amount::ONE_BTC;
    let alice_boarding_address = alice.get_boarding_address().unwrap();
    nigiri
        .faucet_fund(&alice_boarding_address, fund_amount)
        .await;

    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    let custom_owner_kp = Keypair::new(&secp, &mut rng);
    let custom_owner_pk = custom_owner_kp.public_key().x_only_public_key().0;
    let arkd_pk = server_info.signer_pk.x_only_public_key().0;

    let (alice_offchain_address, _) = alice.get_offchain_address().unwrap();
    let (bob_address, _) = bob.get_offchain_address().unwrap();
    let receiver_amount = Amount::from_sat(100_000);
    let expected_receiver_script = bob_address.to_p2tr_script_pubkey();
    let expected_receiver_program = &expected_receiver_script.as_bytes()[2..];

    let arkade_script = ScriptBuf::builder()
        .push_int(0)
        .push_opcode(op::INSPECTOUTPUTSCRIPTPUBKEY)
        .push_opcode(OP_PUSHNUM_1)
        .push_opcode(OP_EQUALVERIFY)
        .push_slice(PushBytesBuf::try_from(expected_receiver_program.to_vec()).unwrap())
        .push_opcode(OP_EQUAL)
        .into_script();

    let arkade_vtxo_script = ArkadeVtxoScript::new(vec![
        ArkadeVtxoInput::Arkade(ArkadeLeaf {
            arkade_script: arkade_script.clone(),
            tapscript: ArkadeTapscript::Multisig {
                pubkeys: vec![custom_owner_pk, arkd_pk],
            },
            introspectors: vec![introspector_pk],
        }),
        ArkadeVtxoInput::Plain(csv_sig_script(
            server_info.unilateral_exit_delay,
            custom_owner_pk,
        )),
    ])
    .unwrap();

    let spend_script = arkade_vtxo_script.scripts[0].clone();
    let custom_vtxo = Vtxo::new_with_custom_scripts(
        &secp,
        arkd_pk,
        custom_owner_pk,
        arkade_vtxo_script.scripts,
        server_info.unilateral_exit_delay,
        server_info.network,
    )
    .unwrap();

    let funding_txid = alice
        .send(vec![SendReceiver::bitcoin(
            custom_vtxo.to_ark_address(),
            receiver_amount,
        )])
        .await
        .unwrap();

    tracing::info!(%funding_txid, "funded custom arkade vtxo");

    let custom_vtxo_outpoint = wait_for_vtxo(&grpc_client, &custom_vtxo, server_info.dust).await;
    let control_block = custom_vtxo.get_spend_info(spend_script.clone()).unwrap();
    let custom_input = VtxoInput::new(
        spend_script,
        None,
        control_block,
        custom_vtxo.tapscripts(),
        custom_vtxo.script_pubkey(),
        custom_vtxo_outpoint.amount,
        custom_vtxo_outpoint.outpoint,
        custom_vtxo_outpoint.assets.clone(),
    );

    let (invalid_ark_tx, invalid_checkpoint_txs) = build_signed_submit_txs(
        &secp,
        &custom_owner_kp,
        custom_owner_pk,
        &custom_vtxo,
        &custom_input,
        &server_info,
        alice_offchain_address,
        receiver_amount,
        &arkade_script,
    );

    let err = introspector
        .submit_tx(&invalid_ark_tx, &invalid_checkpoint_txs)
        .await
        .unwrap_err();
    tracing::info!(error = %err, "invalid arkade spend to alice was rejected as expected");

    let bob_balance = bob.offchain_balance().await.unwrap();
    assert_eq!(bob_balance.total(), Amount::ZERO);

    let (ark_tx, checkpoint_txs) = build_signed_submit_txs(
        &secp,
        &custom_owner_kp,
        custom_owner_pk,
        &custom_vtxo,
        &custom_input,
        &server_info,
        bob_address,
        receiver_amount,
        &arkade_script,
    );

    let response = introspector
        .submit_tx(&ark_tx, &checkpoint_txs)
        .await
        .unwrap();

    tracing::info!(
        ark_txid = %response.signed_ark_tx.unsigned_tx.compute_txid(),
        checkpoints = response.signed_checkpoint_txs.len(),
        "submitted arkade transaction to introspector"
    );

    wait_until_balance!(&bob, pre_confirmed: receiver_amount);

    bob.settle(&mut rng).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;

    wait_until_balance!(&bob, confirmed: receiver_amount, pre_confirmed: Amount::ZERO);
}

fn build_signed_submit_txs(
    secp: &Secp256k1<secp256k1::All>,
    custom_owner_kp: &Keypair,
    custom_owner_pk: bitcoin::XOnlyPublicKey,
    custom_vtxo: &Vtxo,
    custom_input: &VtxoInput,
    server_info: &ark_core::server::Info,
    recipient: ark_core::ArkAddress,
    amount: Amount,
    arkade_script: &ScriptBuf,
) -> (bitcoin::Psbt, Vec<bitcoin::Psbt>) {
    let offchain = build_offchain_transactions(
        &[SendReceiver::bitcoin(recipient, amount)],
        &custom_vtxo.to_ark_address(),
        std::slice::from_ref(custom_input),
        server_info,
    )
    .unwrap();

    let mut ark_tx = offchain.ark_tx;
    let mut checkpoint_txs = offchain.checkpoint_txs;

    add_packet_to_psbt(
        &mut ark_tx,
        &Packet::new(vec![IntrospectorEntry {
            vin: 0,
            script: arkade_script.clone(),
            witness: Witness::default(),
        }])
        .unwrap(),
    )
    .unwrap();

    sign_ark_transaction(
        |_,
         msg: secp256k1::Message|
         -> Result<Vec<(schnorr::Signature, bitcoin::XOnlyPublicKey)>, ark_core::Error> {
            let sig = secp.sign_schnorr_no_aux_rand(&msg, custom_owner_kp);
            Ok(vec![(sig, custom_owner_pk)])
        },
        &mut ark_tx,
        0,
    )
    .unwrap();

    sign_checkpoint_transaction(
        |_,
         msg: secp256k1::Message|
         -> Result<Vec<(schnorr::Signature, bitcoin::XOnlyPublicKey)>, ark_core::Error> {
            let sig = secp.sign_schnorr_no_aux_rand(&msg, custom_owner_kp);
            Ok(vec![(sig, custom_owner_pk)])
        },
        &mut checkpoint_txs[0],
    )
    .unwrap();

    (ark_tx, checkpoint_txs)
}

async fn wait_for_vtxo(
    grpc_client: &ark_grpc::Client,
    vtxo: &Vtxo,
    dust: Amount,
) -> ark_core::server::VirtualTxOutPoint {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let response = grpc_client
                .list_vtxos(GetVtxosRequest::new_for_addresses(std::iter::once(
                    vtxo.to_ark_address(),
                )))
                .await
                .unwrap();

            let list = VtxoList::new(dust, response.vtxos);
            if let Some(vtxo_outpoint) = list.spendable_offchain().next().cloned() {
                return vtxo_outpoint;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap()
}

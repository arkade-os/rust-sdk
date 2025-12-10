use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use ark_core::ArkAddress;
use ark_core::BoardingOutput;
use ark_core::ExplorerUtxo;
use ark_core::TxGraph;
use ark_core::Vtxo;
use ark_core::VtxoList;
use ark_core::batch;
use ark_core::batch::aggregate_nonces;
use ark_core::batch::create_and_sign_forfeit_txs;
use ark_core::batch::generate_nonce_tree;
use ark_core::batch::sign_batch_tree_tx;
use ark_core::batch::sign_commitment_psbt;
use ark_core::boarding_output::BoardingOutpoints;
use ark_core::boarding_output::list_boarding_outpoints;
use ark_core::intent;
use ark_core::script::csv_sig_script;
use ark_core::script::multisig_script;
use ark_core::send;
use ark_core::send::OffchainTransactions;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::server;
use ark_core::server::BatchTreeEventType;
use ark_core::server::GetVtxosRequest;
use ark_core::server::StreamEvent;
use ark_core::server::VirtualTxOutPoint;
use ark_core::vtxo::VirtualTxOutPoints;
use ark_core::vtxo::list_virtual_tx_outpoints;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::ScriptBuf;
use bitcoin::Transaction;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use bitcoin::hashes::Hash;
use bitcoin::hashes::sha256;
use bitcoin::hex::DisplayHex;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::opcodes::all::OP_CHECKSIG;
use bitcoin::opcodes::all::OP_CHECKSIGVERIFY;
use bitcoin::opcodes::all::OP_CLTV;
use bitcoin::opcodes::all::OP_DROP;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::SecretKey;
use bitcoin::secp256k1::schnorr;
use esplora_client::FromHex;
use futures::StreamExt;
use rand::Rng;
use rand::thread_rng;
use regex::Regex;
use std::collections::HashMap;
use std::process::Command;
use std::time::Duration;
use tokio::task::block_in_place;
use zkp::musig::MusigAggNonce;
use zkp::musig::MusigKeyAggCache;
use zkp::musig::MusigSession;
use zkp::musig::MusigSessionId;
use zkp::musig::new_musig_nonce_pair;

const RUN_REFUND_SCENARIO: bool = false;

#[tokio::main]
#[allow(clippy::unwrap_in_result)]
async fn main() -> Result<()> {
    init_tracing();

    // We instantiate an oracle that attests to coin flips.
    let mut oracle = Oracle::new();

    let secp = Secp256k1::new();
    let zkp = zkp::Secp256k1::new();
    let mut rng = thread_rng();

    let mut grpc_client = ark_grpc::Client::new("http://localhost:7070".to_string());

    grpc_client.connect().await?;
    let server_info = grpc_client.get_info().await?;
    let server_pk = server_info.signer_pk.x_only_public_key().0;

    let esplora_client = EsploraClient::new("http://localhost:30000")?;

    let alice_kp = Keypair::new(&secp, &mut rng);
    let alice_pk = alice_kp.public_key();
    let alice_xonly_pk = alice_pk.x_only_public_key().0;

    let bob_kp = Keypair::new(&secp, &mut rng);
    let bob_pk = bob_kp.public_key();
    let bob_xonly_pk = bob_pk.x_only_public_key().0;

    // Alice and Bob need liquidity to fund the DLC.
    //
    // We need VTXOs as inputs to the DLC, because we must be able to presign several transactions
    // on top of the DLC. That is, we can't build the DLC protocol on top of a boarding output!

    let alice_fund_amount = Amount::from_sat(100_000_000);
    let alice_dlc_input = fund_vtxo(
        &esplora_client,
        &grpc_client,
        &server_info,
        &alice_kp,
        alice_fund_amount,
    )
    .await?;

    let bob_fund_amount = Amount::from_sat(100_000_000);
    let bob_dlc_input = fund_vtxo(
        &esplora_client,
        &grpc_client,
        &server_info,
        &bob_kp,
        bob_fund_amount,
    )
    .await?;

    // Using Musig2, the server is not even aware that this is a shared VTXO.
    let musig_key_agg_cache =
        MusigKeyAggCache::new(&zkp, &[to_zkp_pk(alice_pk), to_zkp_pk(bob_pk)]);
    let shared_pk = musig_key_agg_cache.agg_pk();
    let shared_pk = from_zkp_xonly(shared_pk);

    // A path that lets Alice and Bob reclaim (with the server's help) their funds, some time after
    // the oracle attests to the outcome of a relevant event, but _before_ the batch ends. Thus,
    // choosing the timelock correctly is very important.
    //
    // We don't use this path in this example, but including it in the Tapscript demonstrates that
    // the server accepts it.
    let refund_locktime = bitcoin::absolute::LockTime::from_height(1_000)?;
    let dlc_refund_script = ScriptBuf::builder()
        .push_int(refund_locktime.to_consensus_u32() as i64)
        .push_opcode(OP_CLTV)
        .push_opcode(OP_DROP)
        .push_x_only_key(&shared_pk)
        .push_opcode(OP_CHECKSIGVERIFY)
        .push_x_only_key(&server_pk)
        .push_opcode(OP_CHECKSIG)
        .into_script();

    let dlc_multisig_script = multisig_script(server_pk, shared_pk);

    let dlc_vtxo = {
        let redeem_script = csv_sig_script(server_info.unilateral_exit_delay, shared_pk);

        Vtxo::new_with_custom_scripts(
            &secp,
            server_info.signer_pk.into(),
            shared_pk,
            vec![
                dlc_multisig_script.clone(),
                redeem_script,
                dlc_refund_script.clone(),
            ],
            server_info.unilateral_exit_delay,
            server_info.network,
        )?
    };

    // We build the DLC funding transaction, but we don't "broadcast" it yet. We use it as a
    // reference point to build the rest of the DLC.
    let OffchainTransactions {
        ark_tx: mut dlc_virtual_tx,
        checkpoint_txs: dlc_checkpoint_txs,
    } = build_offchain_transactions(
        &[(
            &dlc_vtxo.to_ark_address(),
            alice_fund_amount + bob_fund_amount,
        )],
        None,
        &[alice_dlc_input.clone(), bob_dlc_input.clone()],
        &server_info,
    )
    .context("building DLC TX")?;

    let dlc_output = dlc_virtual_tx.unsigned_tx.output[0].clone();
    let dlc_outpoint = OutPoint {
        txid: dlc_virtual_tx.unsigned_tx.compute_txid(),
        vout: 0,
    };

    // Generate payout addresses for Alice and Bob.

    let alice_payout_vtxo = Vtxo::new_default(
        &secp,
        server_info.signer_pk.x_only_public_key().0,
        alice_xonly_pk,
        server_info.unilateral_exit_delay,
        server_info.network,
    )?;

    let bob_payout_vtxo = Vtxo::new_default(
        &secp,
        server_info.signer_pk.x_only_public_key().0,
        bob_xonly_pk,
        server_info.unilateral_exit_delay,
        server_info.network,
    )?;

    let control_block = dlc_vtxo.get_spend_info(dlc_refund_script.clone())?;

    let refund_dlc_vtxo_input = send::VtxoInput::new(
        dlc_refund_script,
        Some(refund_locktime),
        control_block,
        dlc_vtxo.tapscripts(),
        dlc_vtxo.script_pubkey(),
        dlc_output.value,
        dlc_outpoint,
    );

    // We build a refund transaction spending from the DLC VTXO.
    let alice_refund_payout = alice_fund_amount;
    let bob_refund_payout = bob_fund_amount;
    let refund_offchain_txs = build_offchain_transactions(
        &[
            (&alice_payout_vtxo.to_ark_address(), alice_refund_payout),
            (&bob_payout_vtxo.to_ark_address(), bob_refund_payout),
        ],
        None,
        std::slice::from_ref(&refund_dlc_vtxo_input),
        &server_info,
    )
    .context("building refund TX")?;

    let control_block = dlc_vtxo.get_spend_info(dlc_multisig_script.clone())?;

    let cet_dlc_vtxo_input = send::VtxoInput::new(
        dlc_multisig_script,
        None,
        control_block,
        dlc_vtxo.tapscripts(),
        dlc_vtxo.script_pubkey(),
        dlc_output.value,
        dlc_outpoint,
    );

    // We build CETs spending from the DLC VTXO.
    let alice_heads_payout = Amount::from_sat(70_000_000);
    let bob_heads_payout = dlc_output.value - alice_heads_payout;
    let heads_cet_offchain_txs = build_offchain_transactions(
        &[
            (&alice_payout_vtxo.to_ark_address(), alice_heads_payout),
            (&bob_payout_vtxo.to_ark_address(), bob_heads_payout),
        ],
        None,
        std::slice::from_ref(&cet_dlc_vtxo_input),
        &server_info,
    )
    .context("building heads CET")?;

    let alice_tails_payout = Amount::from_sat(25_000_000);
    let bob_tails_payout = dlc_output.value - alice_tails_payout;
    let tails_cet_offchain_txs = build_offchain_transactions(
        &[
            (&alice_payout_vtxo.to_ark_address(), alice_tails_payout),
            (&bob_payout_vtxo.to_ark_address(), bob_tails_payout),
        ],
        None,
        std::slice::from_ref(&cet_dlc_vtxo_input),
        &server_info,
    )
    .context("building tails CET")?;

    // First, Alice and Bob sign the refund TX.

    let SignedRefundOffchainTransactions {
        virtual_tx: refund_virtual_tx,
        checkpoint_tx: mut signed_refund_checkpoint_tx,
    } = sign_refund_offchain_transactions(
        refund_offchain_txs.clone(),
        &alice_kp,
        &bob_kp,
        &musig_key_agg_cache,
    )
    .context("signing refund offchain TXs")?;

    // Then, Alice and Bob sign the coin flip CETs.

    // The oracle announces the next coin flip.
    let (event, nonce_pk) = oracle.announce();

    // Alice and Bob can construct adaptor PKs based on the oracle's announcement and the
    // oracle's public key.
    let (heads_adaptor_pk, tails_adaptor_pk) = {
        let oracle_pk = oracle.public_key();

        let heads_adaptor_pk = nonce_pk.mul_tweak(&zkp, &heads())?.combine(&oracle_pk)?;
        let tails_adaptor_pk = nonce_pk.mul_tweak(&zkp, &tails())?.combine(&oracle_pk)?;

        (heads_adaptor_pk, tails_adaptor_pk)
    };

    // Both parties end up with a copy of every CET (one per outcome). The transactions cannot yet
    // be published because the adaptor signatures need to be completed with the oracle's adaptor.

    let SignedCetOffchainTransactions {
        virtual_cet: heads_virtual_cet,
        virtual_cet_parity: heads_virtual_cet_parity,
        checkpoint_tx: heads_signed_checkpoint_tx,
        checkpoint_tx_parity: heads_checkpoint_tx_parity,
    } = sign_cet_offchain_txs(
        heads_cet_offchain_txs.clone(),
        &alice_kp,
        &bob_kp,
        &musig_key_agg_cache,
        heads_adaptor_pk,
    )
    .context("signing heads CET offchain TXs")?;

    let SignedCetOffchainTransactions {
        virtual_cet: tails_virtual_cet,
        virtual_cet_parity: tails_virtual_cet_parity,
        checkpoint_tx: tails_signed_checkpoint_tx,
        checkpoint_tx_parity: tails_checkpoint_tx_parity,
    } = sign_cet_offchain_txs(
        tails_cet_offchain_txs.clone(),
        &alice_kp,
        &bob_kp,
        &musig_key_agg_cache,
        tails_adaptor_pk,
    )
    .context("signing tails CET offchain TXs")?;

    // Finally, Alice and Bob sign the DLC funding transaction.

    sign_ark_transaction(
        |_,
         msg: secp256k1::Message|
         -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
            let sig = secp.sign_schnorr_no_aux_rand(&msg, &alice_kp);

            Ok(vec![(sig, alice_xonly_pk)])
        },
        &mut dlc_virtual_tx,
        0, // Alice's DLC-funding virtual TX input.
    )
    .context("failed to sign DLC-funding virtual TX as Alice")?;

    sign_ark_transaction(
        |_,
         msg: secp256k1::Message|
         -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
            let sig = secp.sign_schnorr_no_aux_rand(&msg, &bob_kp);

            Ok(vec![(sig, bob_xonly_pk)])
        },
        &mut dlc_virtual_tx,
        1, // Bob's DLC-funding virtual TX input.
    )
    .context("failed to sign DLC-funding virtual TX as Bob")?;

    let dlc_funding_virtual_txid = dlc_virtual_tx.unsigned_tx.compute_txid();

    // Submit DLC funding transaction.
    let res = grpc_client
        .submit_offchain_transaction_request(dlc_virtual_tx, dlc_checkpoint_txs.clone())
        .await
        .context("failed to submit offchain TX request to fund DLC")?;

    let mut alice_signed_checkpoint_psbt = res.signed_checkpoint_txs[0].clone();
    alice_signed_checkpoint_psbt.inputs[0].witness_script =
        dlc_checkpoint_txs[0].inputs[0].witness_script.clone();

    sign_checkpoint_transaction(
        |_,
         msg: secp256k1::Message|
         -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
            let sig = secp.sign_schnorr_no_aux_rand(&msg, &alice_kp);

            Ok(vec![(sig, alice_xonly_pk)])
        },
        &mut alice_signed_checkpoint_psbt,
    )
    .context("failed to sign Alice's DLC-funding checkpoint TX")?;

    let mut bob_signed_checkpoint_psbt = res.signed_checkpoint_txs[1].clone();
    bob_signed_checkpoint_psbt.inputs[0].witness_script =
        dlc_checkpoint_txs[1].inputs[0].witness_script.clone();

    sign_checkpoint_transaction(
        |_,
         msg: secp256k1::Message|
         -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
            let sig = secp.sign_schnorr_no_aux_rand(&msg, &bob_kp);

            Ok(vec![(sig, bob_xonly_pk)])
        },
        &mut bob_signed_checkpoint_psbt,
    )
    .context("failed to sign Bob's DLC-funding checkpoint TX")?;

    grpc_client
        .finalize_offchain_transaction(
            dlc_funding_virtual_txid,
            vec![alice_signed_checkpoint_psbt, bob_signed_checkpoint_psbt],
        )
        .await
        .context("failed to finalize DLC-funding offchain transaction")?;

    tokio::time::sleep(Duration::from_secs(2)).await;

    if RUN_REFUND_SCENARIO {
        let refund_virtual_txid = refund_virtual_tx.unsigned_tx.compute_txid();

        let res = grpc_client
            .submit_offchain_transaction_request(
                refund_virtual_tx,
                refund_offchain_txs.checkpoint_txs,
            )
            .await
            .context("failed to submit offchain TX request to refund DLC")?;

        signed_refund_checkpoint_tx
            .combine(res.signed_checkpoint_txs[0].clone())
            .context("failed to combine signed refund TXs")?;

        grpc_client
            .finalize_offchain_transaction(refund_virtual_txid, vec![signed_refund_checkpoint_tx])
            .await
            .context("failed to finalize DLC-refund offchain transaction")?;

        tokio::time::sleep(Duration::from_secs(2)).await;

        {
            let spendable_vtxos =
                spendable_vtxos(&grpc_client, server_info.dust, &[alice_payout_vtxo]).await?;
            let virtual_tx_outpoints = list_virtual_tx_outpoints(
                |address: &bitcoin::Address| -> Result<Vec<ExplorerUtxo>, ark_core::Error> {
                    find_outpoints(tokio::runtime::Handle::current(), &esplora_client, address)
                },
                spendable_vtxos,
            )?;

            assert_eq!(virtual_tx_outpoints.spendable_balance(), alice_fund_amount);
        }
        {
            let spendable_vtxos =
                spendable_vtxos(&grpc_client, server_info.dust, &[bob_payout_vtxo]).await?;
            let virtual_tx_outpoints = list_virtual_tx_outpoints(
                |address: &bitcoin::Address| -> Result<Vec<ExplorerUtxo>, ark_core::Error> {
                    find_outpoints(tokio::runtime::Handle::current(), &esplora_client, address)
                },
                spendable_vtxos,
            )?;

            assert_eq!(virtual_tx_outpoints.spendable_balance(), bob_fund_amount);
        }

        return Ok(());
    }

    // Wait until the oracle attests to the outcome of the relevant event.

    let is_heads = flip_coin();
    let attestation = oracle.attest(event, is_heads)?;

    // Only one of the CETs is "unlocked".
    let (
        mut unlocked_virtual_cet_psbt,
        unlocked_virtual_cet_parity,
        mut unlocked_signed_checkpoint_psbt,
        unlocked_checkpoint_psbt,
        unlocked_checkpoint_parity,
    ) = if is_heads {
        (
            heads_virtual_cet,
            heads_virtual_cet_parity,
            heads_signed_checkpoint_tx,
            heads_cet_offchain_txs.checkpoint_txs[0].clone(),
            heads_checkpoint_tx_parity,
        )
    } else {
        (
            tails_virtual_cet,
            tails_virtual_cet_parity,
            tails_signed_checkpoint_tx,
            tails_cet_offchain_txs.checkpoint_txs[0].clone(),
            tails_checkpoint_tx_parity,
        )
    };

    let mut input = unlocked_virtual_cet_psbt.inputs[0]
        .tap_script_sigs
        .first_entry()
        .context("one sig")?;
    let input_sig = input.get_mut();

    let adaptor_sig = zkp::schnorr::Signature::from_slice(input_sig.signature.as_ref())
        .context("invalid signature")?;

    let adaptor = zkp::Tweak::from_slice(attestation.as_ref()).context("invalid tweak")?;

    // Complete the adaptor signature, producing a valid signature for this CET.

    let sig = zkp::musig::adapt(adaptor_sig, adaptor, unlocked_virtual_cet_parity);
    let sig = schnorr::Signature::from_slice(sig.as_ref()).context("invalid signature")?;

    input_sig.signature = sig;

    // Publish the CET.

    let unlocked_virtual_cet_txid = unlocked_virtual_cet_psbt.unsigned_tx.compute_txid();

    let res = grpc_client
        .submit_offchain_transaction_request(
            unlocked_virtual_cet_psbt,
            vec![unlocked_checkpoint_psbt],
        )
        .await
        .context("failed to submit offchain TX request for CET")?;

    let mut input = unlocked_signed_checkpoint_psbt.inputs[0]
        .tap_script_sigs
        .first_entry()
        .context("one sig")?;
    let input_sig = input.get_mut();

    let adaptor_sig = zkp::schnorr::Signature::from_slice(input_sig.signature.as_ref())
        .context("invalid signature")?;

    let adaptor = zkp::Tweak::from_slice(attestation.as_ref()).context("invalid tweak")?;

    // Complete the adaptor signature, producing a valid signature for this checkpoint transaction.

    let sig = zkp::musig::adapt(adaptor_sig, adaptor, unlocked_checkpoint_parity);
    let sig = schnorr::Signature::from_slice(sig.as_ref()).context("invalid signature")?;

    input_sig.signature = sig;

    unlocked_signed_checkpoint_psbt
        .combine(res.signed_checkpoint_txs[0].clone())
        .context("failed to combine signed CETs")?;

    grpc_client
        .finalize_offchain_transaction(
            unlocked_virtual_cet_txid,
            vec![unlocked_signed_checkpoint_psbt],
        )
        .await
        .context("failed to finalize CET offchain transaction")?;

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Verify that Alice and Bob receive the expected payouts.

    {
        let spendable_vtxos =
            spendable_vtxos(&grpc_client, server_info.dust, &[alice_payout_vtxo]).await?;
        let virtual_tx_outpoints = list_virtual_tx_outpoints(
            |address: &bitcoin::Address| -> Result<Vec<ExplorerUtxo>, ark_core::Error> {
                find_outpoints(tokio::runtime::Handle::current(), &esplora_client, address)
            },
            spendable_vtxos,
        )?;

        if is_heads {
            assert_eq!(
                virtual_tx_outpoints.spendable_balance(),
                Amount::from_sat(70_000_000)
            );
        } else {
            assert_eq!(
                virtual_tx_outpoints.spendable_balance(),
                Amount::from_sat(25_000_000)
            );
        }
    };

    {
        let spendable_vtxos =
            spendable_vtxos(&grpc_client, server_info.dust, &[bob_payout_vtxo]).await?;
        let virtual_tx_outpoints = list_virtual_tx_outpoints(
            |address: &bitcoin::Address| -> Result<Vec<ExplorerUtxo>, ark_core::Error> {
                find_outpoints(tokio::runtime::Handle::current(), &esplora_client, address)
            },
            spendable_vtxos,
        )?;

        if is_heads {
            assert_eq!(
                virtual_tx_outpoints.spendable_balance(),
                Amount::from_sat(130_000_000)
            );
        } else {
            assert_eq!(
                virtual_tx_outpoints.spendable_balance(),
                Amount::from_sat(175_000_000)
            );
        }
    };

    Ok(())
}

fn find_outpoints(
    runtime: tokio::runtime::Handle,
    esplora_client: &EsploraClient,
    address: &bitcoin::Address,
) -> Result<Vec<ExplorerUtxo>, ark_core::Error> {
    block_in_place(|| {
        runtime.block_on(async {
            let outpoints = esplora_client
                .find_outpoints(address)
                .await
                .map_err(ark_core::Error::ad_hoc)?;

            Ok(outpoints)
        })
    })
}

async fn fund_vtxo(
    esplora_client: &EsploraClient,
    grpc_client: &ark_grpc::Client,
    server_info: &server::Info,
    kp: &Keypair,
    amount: Amount,
) -> Result<send::VtxoInput> {
    let secp = Secp256k1::new();

    let pk = kp.public_key().x_only_public_key().0;

    let boarding_output = BoardingOutput::new(
        &secp,
        server_info.signer_pk.x_only_public_key().0,
        pk,
        server_info.boarding_exit_delay,
        server_info.network,
    )?;

    faucet_fund(boarding_output.address(), amount).await?;

    let boarding_outpoints = list_boarding_outpoints(
        |address: &bitcoin::Address| -> Result<Vec<ExplorerUtxo>, ark_core::Error> {
            find_outpoints(tokio::runtime::Handle::current(), esplora_client, address)
        },
        &[boarding_output],
    )?;
    assert_eq!(boarding_outpoints.spendable_balance(), amount);

    let vtxo = Vtxo::new_default(
        &secp,
        server_info.signer_pk.x_only_public_key().0,
        pk,
        server_info.unilateral_exit_delay,
        server_info.network,
    )?;

    let commitment_txid = settle(
        grpc_client,
        server_info,
        kp.secret_key(),
        VirtualTxOutPoints::default(),
        boarding_outpoints,
        vtxo.to_ark_address(),
    )
    .await?
    .ok_or(anyhow!("did not join batch"))?;

    tokio::time::sleep(Duration::from_secs(2)).await;

    let request = GetVtxosRequest::new_for_addresses(std::iter::once(vtxo.to_ark_address()));
    let virtual_tx_outpoints = grpc_client.list_vtxos(request).await?;

    let vtxo_list = VtxoList::new(server_info.dust, virtual_tx_outpoints);
    let virtual_tx_outpoint = vtxo_list
        .spendable_offchain()
        .find(|v| v.commitment_txids[0] == commitment_txid)
        .ok_or(anyhow!("could not find input in batch"))?;

    let (forfeit_script, control_block) = vtxo.forfeit_spend_info()?;

    let vtxo_input = send::VtxoInput::new(
        forfeit_script,
        None,
        control_block,
        vtxo.tapscripts(),
        vtxo.script_pubkey(),
        virtual_tx_outpoint.amount,
        virtual_tx_outpoint.outpoint,
    );

    Ok(vtxo_input)
}

/// The result of signing DLC refund transactions between Alice and Bob.
///
/// These do not include the server signature yet.
struct SignedRefundOffchainTransactions {
    virtual_tx: Psbt,
    checkpoint_tx: Psbt,
}

/// Sign the [`OffchainTransactions`] for the refund path.
///
/// This function represents a session between the two signing parties. It would normally be
/// performed over the internet.
fn sign_refund_offchain_transactions(
    offchain_txs: OffchainTransactions,
    alice_kp: &Keypair,
    bob_kp: &Keypair,
    musig_key_agg_cache: &MusigKeyAggCache,
) -> Result<SignedRefundOffchainTransactions> {
    let zkp = zkp::Secp256k1::new();
    let mut rng = thread_rng();

    let OffchainTransactions {
        ark_tx: mut refund_virtual_tx,
        checkpoint_txs: refund_checkpoint_txs,
    } = offchain_txs;

    // For a transaction spending a DLC output, there can only be one input.
    let mut refund_checkpoint_psbt = refund_checkpoint_txs[0].clone();

    // Signing the virtual TX.
    {
        let shared_pk = from_zkp_xonly(musig_key_agg_cache.agg_pk());

        let alice_pk = alice_kp.public_key();

        let (alice_musig_nonce, alice_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand = rng.r#gen();
            new_musig_nonce_pair(
                &zkp,
                session_id,
                None,
                None,
                to_zkp_pk(alice_pk),
                None,
                Some(extra_rand),
            )?
        };

        let bob_pk = bob_kp.public_key();

        let (bob_musig_nonce, bob_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand = rng.r#gen();
            new_musig_nonce_pair(
                &zkp,
                session_id,
                None,
                None,
                to_zkp_pk(bob_pk),
                None,
                Some(extra_rand),
            )?
        };

        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let musig_agg_nonce =
                    MusigAggNonce::new(&zkp, &[alice_musig_pub_nonce, bob_musig_pub_nonce]);
                let msg = zkp::Message::from_digest_slice(msg.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                let musig_session =
                    MusigSession::new(&zkp, musig_key_agg_cache, musig_agg_nonce, msg);

                let alice_kp = zkp::Keypair::from_seckey_slice(&zkp, &alice_kp.secret_bytes())
                    .map_err(ark_core::Error::ad_hoc)?;

                let alice_sig = musig_session
                    .partial_sign(&zkp, alice_musig_nonce, &alice_kp, musig_key_agg_cache)
                    .map_err(ark_core::Error::ad_hoc)?;

                let bob_kp = zkp::Keypair::from_seckey_slice(&zkp, &bob_kp.secret_bytes())
                    .map_err(ark_core::Error::ad_hoc)?;

                let bob_sig = musig_session
                    .partial_sign(&zkp, bob_musig_nonce, &bob_kp, musig_key_agg_cache)
                    .map_err(ark_core::Error::ad_hoc)?;

                let sig = musig_session.partial_sig_agg(&[alice_sig, bob_sig]);
                let sig = schnorr::Signature::from_slice(sig.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                Ok(vec![(sig, shared_pk)])
            };

        sign_ark_transaction(sign_fn, &mut refund_virtual_tx, 0)
            .context("signing refund virtual TX")?;
    }

    // Signing the checkpoint TX. Some unnecessary duplication here.
    {
        let shared_pk = from_zkp_xonly(musig_key_agg_cache.agg_pk());

        let alice_pk = alice_kp.public_key();

        let (alice_musig_nonce, alice_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand = rng.r#gen();
            new_musig_nonce_pair(
                &zkp,
                session_id,
                None,
                None,
                to_zkp_pk(alice_pk),
                None,
                Some(extra_rand),
            )?
        };

        let bob_pk = bob_kp.public_key();

        let (bob_musig_nonce, bob_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand = rng.r#gen();
            new_musig_nonce_pair(
                &zkp,
                session_id,
                None,
                None,
                to_zkp_pk(bob_pk),
                None,
                Some(extra_rand),
            )?
        };

        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let musig_agg_nonce =
                    MusigAggNonce::new(&zkp, &[alice_musig_pub_nonce, bob_musig_pub_nonce]);
                let msg = zkp::Message::from_digest_slice(msg.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                let musig_session =
                    MusigSession::new(&zkp, musig_key_agg_cache, musig_agg_nonce, msg);

                let alice_kp = zkp::Keypair::from_seckey_slice(&zkp, &alice_kp.secret_bytes())
                    .map_err(ark_core::Error::ad_hoc)?;

                let alice_sig = musig_session
                    .partial_sign(&zkp, alice_musig_nonce, &alice_kp, musig_key_agg_cache)
                    .map_err(ark_core::Error::ad_hoc)?;

                let bob_kp = zkp::Keypair::from_seckey_slice(&zkp, &bob_kp.secret_bytes())
                    .map_err(ark_core::Error::ad_hoc)?;

                let bob_sig = musig_session
                    .partial_sign(&zkp, bob_musig_nonce, &bob_kp, musig_key_agg_cache)
                    .map_err(ark_core::Error::ad_hoc)?;

                let sig = musig_session.partial_sig_agg(&[alice_sig, bob_sig]);
                let sig = schnorr::Signature::from_slice(sig.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                Ok(vec![(sig, shared_pk)])
            };

        // Normally we would sign this one after communicating with the server, but since this
        // output is owned by two parties we need to do this ahead of time.
        sign_checkpoint_transaction(sign_fn, &mut refund_checkpoint_psbt)
            .context("signing refund checkpoint TX")?;
    }

    Ok(SignedRefundOffchainTransactions {
        virtual_tx: refund_virtual_tx,
        checkpoint_tx: refund_checkpoint_psbt,
    })
}

/// The result of signing CET transactions between Alice and Bob.
///
/// These do not include the server signature yet.
///
/// TODO: Do we really need to have adaptor signatures for both the virtual TX and the checkpoint
/// TX. It seems like only the checkpoint TX needs an adaptor signature, but it feels weird.
struct SignedCetOffchainTransactions {
    virtual_cet: Psbt,
    virtual_cet_parity: zkp::Parity,
    checkpoint_tx: Psbt,
    checkpoint_tx_parity: zkp::Parity,
}

/// Sign the [`OffchainTransactions`] for the CET path.
///
/// This function represents a session between the two signing parties. It would normally be
/// performed over the internet.
fn sign_cet_offchain_txs(
    offchain_txs: OffchainTransactions,
    alice_kp: &Keypair,
    bob_kp: &Keypair,
    musig_key_agg_cache: &MusigKeyAggCache,
    adaptor_pk: zkp::PublicKey,
) -> Result<SignedCetOffchainTransactions> {
    let zkp = zkp::Secp256k1::new();
    let mut rng = thread_rng();

    let OffchainTransactions {
        ark_tx: mut virtual_cet,
        checkpoint_txs: cet_checkpoint_txs,
    } = offchain_txs;

    // For a transaction spending a DLC output, there can only be one input.
    let mut cet_checkpoint_psbt = cet_checkpoint_txs[0].clone();

    // Signing the virtual CET.
    let virtual_cet_parity = {
        let shared_pk = from_zkp_xonly(musig_key_agg_cache.agg_pk());

        let alice_pk = alice_kp.public_key();

        let (alice_musig_nonce, alice_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand = rng.r#gen();
            new_musig_nonce_pair(
                &zkp,
                session_id,
                None,
                None,
                to_zkp_pk(alice_pk),
                None,
                Some(extra_rand),
            )?
        };

        let bob_pk = bob_kp.public_key();

        let (bob_musig_nonce, bob_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand = rng.r#gen();
            new_musig_nonce_pair(
                &zkp,
                session_id,
                None,
                None,
                to_zkp_pk(bob_pk),
                None,
                Some(extra_rand),
            )?
        };

        let mut musig_nonce_parity = None;
        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let musig_agg_nonce =
                    MusigAggNonce::new(&zkp, &[alice_musig_pub_nonce, bob_musig_pub_nonce]);
                let msg = zkp::Message::from_digest_slice(msg.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                let musig_session = MusigSession::with_adaptor(
                    &zkp,
                    musig_key_agg_cache,
                    musig_agg_nonce,
                    msg,
                    adaptor_pk,
                );

                musig_nonce_parity = Some(musig_session.nonce_parity());

                let alice_kp = zkp::Keypair::from_seckey_slice(&zkp, &alice_kp.secret_bytes())
                    .map_err(ark_core::Error::ad_hoc)?;

                let alice_sig = musig_session
                    .partial_sign(&zkp, alice_musig_nonce, &alice_kp, musig_key_agg_cache)
                    .map_err(ark_core::Error::ad_hoc)?;

                let bob_kp = zkp::Keypair::from_seckey_slice(&zkp, &bob_kp.secret_bytes())
                    .map_err(ark_core::Error::ad_hoc)?;

                let bob_sig = musig_session
                    .partial_sign(&zkp, bob_musig_nonce, &bob_kp, musig_key_agg_cache)
                    .map_err(ark_core::Error::ad_hoc)?;

                let sig = musig_session.partial_sig_agg(&[alice_sig, bob_sig]);
                let sig = schnorr::Signature::from_slice(sig.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                Ok(vec![(sig, shared_pk)])
            };

        sign_ark_transaction(sign_fn, &mut virtual_cet, 0).context("signing virtual CET")?;

        musig_nonce_parity.context("to be set")?
    };

    // Signing the checkpoint TX. Some unnecessary duplication here.
    let checkpoint_tx_parity = {
        let shared_pk = from_zkp_xonly(musig_key_agg_cache.agg_pk());

        let alice_pk = alice_kp.public_key();

        let (alice_musig_nonce, alice_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand = rng.r#gen();
            new_musig_nonce_pair(
                &zkp,
                session_id,
                None,
                None,
                to_zkp_pk(alice_pk),
                None,
                Some(extra_rand),
            )?
        };

        let bob_pk = bob_kp.public_key();

        let (bob_musig_nonce, bob_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand = rng.r#gen();
            new_musig_nonce_pair(
                &zkp,
                session_id,
                None,
                None,
                to_zkp_pk(bob_pk),
                None,
                Some(extra_rand),
            )?
        };

        let mut musig_nonce_parity = None;
        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let musig_agg_nonce =
                    MusigAggNonce::new(&zkp, &[alice_musig_pub_nonce, bob_musig_pub_nonce]);
                let msg = zkp::Message::from_digest_slice(msg.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                let musig_session = MusigSession::with_adaptor(
                    &zkp,
                    musig_key_agg_cache,
                    musig_agg_nonce,
                    msg,
                    adaptor_pk,
                );

                musig_nonce_parity = Some(musig_session.nonce_parity());

                let alice_kp = zkp::Keypair::from_seckey_slice(&zkp, &alice_kp.secret_bytes())
                    .map_err(ark_core::Error::ad_hoc)?;

                let alice_sig = musig_session
                    .partial_sign(&zkp, alice_musig_nonce, &alice_kp, musig_key_agg_cache)
                    .map_err(ark_core::Error::ad_hoc)?;

                let bob_kp = zkp::Keypair::from_seckey_slice(&zkp, &bob_kp.secret_bytes())
                    .map_err(ark_core::Error::ad_hoc)?;

                let bob_sig = musig_session
                    .partial_sign(&zkp, bob_musig_nonce, &bob_kp, musig_key_agg_cache)
                    .map_err(ark_core::Error::ad_hoc)?;

                let sig = musig_session.partial_sig_agg(&[alice_sig, bob_sig]);
                let sig = schnorr::Signature::from_slice(sig.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                Ok(vec![(sig, shared_pk)])
            };

        // Normally we would sign this one after communicating with the server, but since this
        // output is owned by two parties we need to do this ahead of time.
        sign_checkpoint_transaction(sign_fn, &mut cet_checkpoint_psbt)
            .context("signing CET checkpoint TX")?;

        musig_nonce_parity.context("to be set")?
    };

    Ok(SignedCetOffchainTransactions {
        virtual_cet,
        virtual_cet_parity,
        checkpoint_tx: cet_checkpoint_psbt,
        checkpoint_tx_parity,
    })
}

/// Simulation of a DLC oracle.
///
/// This oracle attests to the outcome of flipping a coin: either heads (1) or tails (2).
struct Oracle {
    kp: zkp::Keypair,
    nonces: Vec<zkp::SecretKey>,
}

impl Oracle {
    fn new() -> Self {
        let zkp = zkp::Secp256k1::new();
        let mut rng = thread_rng();

        let kp = zkp::Keypair::new(&zkp, &mut rng);

        Self {
            kp,
            nonces: Vec::new(),
        }
    }

    /// The oracle's public key.
    fn public_key(&self) -> zkp::PublicKey {
        self.kp.public_key()
    }

    /// Announce the public nonce that will be used to attest to the outcome of a future event.
    fn announce(&mut self) -> (usize, zkp::PublicKey) {
        let zkp = zkp::Secp256k1::new();
        let mut rng = thread_rng();

        let sk = zkp::SecretKey::new(&mut rng);
        let pk = zkp::PublicKey::from_secret_key(&zkp, &sk);

        self.nonces.push(sk);

        (self.nonces.len() - 1, pk)
    }

    /// The oracle attests to the outcome of a coin flip.
    fn attest(&self, event: usize, is_heads: bool) -> Result<zkp::SecretKey> {
        let nonce = self.nonces.get(event).context("missing event")?;

        let outcome = if is_heads { heads() } else { tails() };

        let sk = zkp::Scalar::from_be_bytes(self.kp.secret_key().secret_bytes())?;

        let attestation = nonce.mul_tweak(&outcome)?.add_tweak(&sk)?;

        Ok(attestation)
    }
}

const fn heads() -> zkp::Scalar {
    zkp::Scalar::ONE
}

fn tails() -> zkp::Scalar {
    zkp::Scalar::from_be_bytes([
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 2,
    ])
    .expect("valid scalar")
}

/// Flip a fair coin.
///
/// # Returns
///
/// - Heads => `true`.
/// - Tails => `false`.
fn flip_coin() -> bool {
    let mut rng = thread_rng();

    let is_heads = rng.gen_bool(0.5);

    if is_heads {
        tracing::info!("Flipped a coin: got heads!");
    } else {
        tracing::info!("Flipped a coin: got tails!");
    }

    is_heads
}

async fn settle(
    grpc_client: &ark_grpc::Client,
    server_info: &server::Info,
    sk: SecretKey,
    virtual_tx_outpoints: VirtualTxOutPoints,
    boarding_outpoints: BoardingOutpoints,
    to_address: ArkAddress,
) -> Result<Option<Txid>> {
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    if virtual_tx_outpoints.spendable.is_empty() && boarding_outpoints.spendable.is_empty() {
        return Ok(None);
    }

    let cosigner_kp = Keypair::new(&secp, &mut rng);

    let batch_inputs = {
        let boarding_inputs = boarding_outpoints.spendable.clone().into_iter().map(
            |(outpoint, amount, boarding_output)| {
                intent::Input::new(
                    outpoint,
                    boarding_output.exit_delay(),
                    TxOut {
                        value: amount,
                        script_pubkey: boarding_output.script_pubkey(),
                    },
                    boarding_output.tapscripts(),
                    boarding_output.exit_spend_info(),
                    true,
                    false,
                )
            },
        );

        let vtxo_inputs = virtual_tx_outpoints
            .spendable
            .clone()
            .into_iter()
            .map(|(virtual_tx_outpoint, vtxo)| {
                anyhow::Ok(intent::Input::new(
                    virtual_tx_outpoint.outpoint,
                    vtxo.exit_delay(),
                    TxOut {
                        value: virtual_tx_outpoint.amount,
                        script_pubkey: vtxo.script_pubkey(),
                    },
                    vtxo.tapscripts(),
                    vtxo.exit_spend_info()?,
                    false,
                    virtual_tx_outpoint.is_swept,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;

        boarding_inputs.chain(vtxo_inputs).collect::<Vec<_>>()
    };
    let n_batch_inputs = batch_inputs.len();

    let spendable_amount =
        boarding_outpoints.spendable_balance() + virtual_tx_outpoints.spendable_balance();
    let batch_outputs = vec![intent::Output::Offchain(TxOut {
        value: spendable_amount,
        script_pubkey: to_address.to_p2tr_script_pubkey(),
    })];

    let own_cosigner_kps = [cosigner_kp];
    let own_cosigner_pks = own_cosigner_kps
        .iter()
        .map(|k| k.public_key())
        .collect::<Vec<_>>();

    let signing_kp = Keypair::from_secret_key(&secp, &sk);

    let sign_for_vtxo_fn =
        |_: &mut psbt::Input,
         msg: secp256k1::Message|
         -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
            let sig = secp.sign_schnorr_no_aux_rand(&msg, &signing_kp);
            Ok(vec![(sig, signing_kp.public_key().into())])
        };

    let sign_for_onchain_fn =
        |_: &mut psbt::Input,
         msg: secp256k1::Message|
         -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
            let sig = secp.sign_schnorr_no_aux_rand(&msg, &signing_kp);
            Ok((sig, signing_kp.public_key().into()))
        };

    let signing_kp = Keypair::from_secret_key(&secp, &sk);
    let intent = intent::make_intent(
        sign_for_vtxo_fn,
        sign_for_onchain_fn,
        batch_inputs,
        batch_outputs,
        own_cosigner_pks.clone(),
    )?;

    let intent_id = grpc_client.register_intent(intent).await?;

    tracing::info!(intent_id, "Registered intent");

    let topics = virtual_tx_outpoints
        .spendable
        .iter()
        .map(|(o, _)| o.outpoint.to_string())
        .chain(
            own_cosigner_pks
                .iter()
                .map(|pk| pk.serialize().to_lower_hex_string()),
        )
        .collect();

    let mut event_stream = grpc_client.get_event_stream(topics).await?;

    let mut vtxo_graph_chunks = Vec::new();

    let batch_started_event = match event_stream.next().await {
        Some(Ok(StreamEvent::BatchStarted(e))) => e,
        other => bail!("Did not get batch signing event: {other:?}"),
    };

    let hash = sha256::Hash::hash(intent_id.as_bytes());
    let hash = hash.as_byte_array().to_vec().to_lower_hex_string();

    if batch_started_event
        .intent_id_hashes
        .iter()
        .any(|h| h == &hash)
    {
        grpc_client.confirm_registration(intent_id.clone()).await?;
    } else {
        bail!(
            "Did not find intent ID {} in batch: {}",
            intent_id,
            batch_started_event.id
        )
    }

    let batch_expiry = batch_started_event.batch_expiry;

    let batch_signing_event;
    loop {
        match event_stream.next().await {
            Some(Ok(StreamEvent::TreeTx(e))) => match e.batch_tree_event_type {
                BatchTreeEventType::Vtxo => vtxo_graph_chunks.push(e.tx_graph_chunk),
                BatchTreeEventType::Connector => {
                    bail!("Unexpected connector batch tree event");
                }
            },
            Some(Ok(StreamEvent::TreeSigningStarted(e))) => {
                batch_signing_event = e;
                break;
            }
            other => bail!("Unexpected event while waiting for batch signing: {other:?}"),
        }
    }

    let mut vtxo_graph = TxGraph::new(vtxo_graph_chunks)?;

    let batch_id = batch_signing_event.id;
    tracing::info!(batch_id, "Batch signing started");

    let mut nonce_tree = generate_nonce_tree(
        &mut rng,
        &vtxo_graph,
        cosigner_kp.public_key(),
        &batch_signing_event.unsigned_commitment_tx,
    )?;

    grpc_client
        .submit_tree_nonces(
            &batch_id,
            cosigner_kp.public_key(),
            nonce_tree.to_nonce_pks(),
        )
        .await?;

    let tree_nonces_event = match event_stream.next().await {
        Some(Ok(StreamEvent::TreeNonces(e))) => e,
        other => bail!("Did not get batch signing nonces generated event: {other:?}"),
    };

    tracing::info!(batch_id, "Tree nonces event");

    let batch_id = tree_nonces_event.id;

    let tree_tx_nonce_pks = tree_nonces_event.nonces;

    tree_tx_nonce_pks
        .0
        .iter()
        .find(|(pk, _)| {
            own_cosigner_pks
                .iter()
                .any(|p| &&p.x_only_public_key().0 == pk)
        })
        .context("received unexpected irrelevant TreeNonces event")?;

    let agg_nonce_pk = aggregate_nonces(tree_tx_nonce_pks);

    let partial_sig_tree = sign_batch_tree_tx(
        tree_nonces_event.txid,
        batch_expiry,
        server_info.forfeit_pk.into(),
        &cosigner_kp,
        agg_nonce_pk,
        &vtxo_graph,
        &batch_signing_event.unsigned_commitment_tx,
        &mut nonce_tree,
    )?;

    grpc_client
        .submit_tree_signatures(&batch_id, cosigner_kp.public_key(), partial_sig_tree)
        .await?;

    let mut connectors_graph_chunks = Vec::new();

    let batch_finalization_event;
    loop {
        match event_stream.next().await {
            Some(Ok(StreamEvent::TreeNoncesAggregated(_))) => {
                // TreeNoncesAggregated is now deprecated.
            }
            Some(Ok(StreamEvent::TreeTx(e))) => match e.batch_tree_event_type {
                BatchTreeEventType::Vtxo => {
                    bail!("Unexpected VTXO batch tree event");
                }
                BatchTreeEventType::Connector => {
                    connectors_graph_chunks.push(e.tx_graph_chunk);
                }
            },
            Some(Ok(StreamEvent::TreeSignature(e))) => match e.batch_tree_event_type {
                BatchTreeEventType::Vtxo => {
                    vtxo_graph.apply(|graph| {
                        if graph.root().unsigned_tx.compute_txid() != e.txid {
                            Ok(true)
                        } else {
                            graph.set_signature(e.signature);

                            Ok(false)
                        }
                    })?;
                }
                BatchTreeEventType::Connector => {
                    bail!("received batch tree signature for connectors tree");
                }
            },
            Some(Ok(StreamEvent::BatchFinalization(e))) => {
                batch_finalization_event = e;
                break;
            }
            other => bail!("Unexpected event while waiting for batch finalization: {other:?}"),
        }
    }

    let batch_id = batch_finalization_event.id;

    tracing::info!(batch_id, "Batch finalization started");

    let keypair = Keypair::from_secret_key(&secp, &sk);

    let vtxo_inputs = virtual_tx_outpoints
        .spendable
        .into_iter()
        .map(|(virtual_tx_outpoint, vtxo)| {
            let spend_info = vtxo.forfeit_spend_info()?;

            anyhow::Ok(intent::Input::new(
                virtual_tx_outpoint.outpoint,
                vtxo.exit_delay(),
                TxOut {
                    value: virtual_tx_outpoint.amount,
                    script_pubkey: vtxo.script_pubkey(),
                },
                vtxo.tapscripts(),
                spend_info,
                false,
                virtual_tx_outpoint.is_swept,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let signed_forfeit_psbts = if !vtxo_inputs.is_empty() {
        let connectors_graph = TxGraph::new(connectors_graph_chunks)?;

        create_and_sign_forfeit_txs(
            |_, msg| {
                let sig = secp.sign_schnorr_no_aux_rand(&msg, &signing_kp);
                let pk = signing_kp.x_only_public_key().0;

                Ok(vec![(sig, pk)])
            },
            vtxo_inputs.as_slice(),
            &connectors_graph.leaves(),
            &server_info.forfeit_address,
            server_info.dust,
        )?
    } else {
        Vec::new()
    };

    let onchain_inputs = boarding_outpoints
        .spendable
        .into_iter()
        .map(|(outpoint, amount, boarding_output)| {
            batch::OnChainInput::new(boarding_output, amount, outpoint)
        })
        .collect::<Vec<_>>();

    let commitment_pstb = if n_batch_inputs == 0 {
        None
    } else {
        let mut commitment_pstb = batch_finalization_event.commitment_tx;

        let sign_for_pk_fn = |_: &XOnlyPublicKey,
                              msg: &secp256k1::Message|
         -> Result<schnorr::Signature, ark_core::Error> {
            Ok(secp.sign_schnorr_no_aux_rand(msg, &keypair))
        };

        sign_commitment_psbt(sign_for_pk_fn, &mut commitment_pstb, &onchain_inputs)?;

        Some(commitment_pstb)
    };

    grpc_client
        .submit_signed_forfeit_txs(signed_forfeit_psbts, commitment_pstb)
        .await?;

    let batch_finalized_event = match event_stream.next().await {
        Some(Ok(StreamEvent::BatchFinalized(e))) => e,
        other => bail!("Did not get batch finalized event: {other:?}"),
    };

    let batch_id = batch_finalized_event.id;

    tracing::info!(batch_id, "Batch finalized");

    Ok(Some(batch_finalized_event.commitment_txid))
}

async fn spendable_vtxos(
    grpc_client: &ark_grpc::Client,
    dust: Amount,
    vtxos: &[Vtxo],
) -> Result<HashMap<Vtxo, Vec<VirtualTxOutPoint>>> {
    let mut spendable_vtxos = HashMap::new();
    for vtxo in vtxos.iter() {
        // The VTXOs for the given Ark address that the Ark server tells us about.
        let request = GetVtxosRequest::new_for_addresses(std::iter::once(vtxo.to_ark_address()));
        let virtual_tx_outpoints = grpc_client.list_vtxos(request).await?;
        let vtxo_list = VtxoList::new(dust, virtual_tx_outpoints);

        spendable_vtxos.insert(
            vtxo.clone(),
            vtxo_list.spendable_offchain().cloned().collect(),
        );
    }

    Ok(spendable_vtxos)
}

pub struct EsploraClient {
    esplora_client: esplora_client::AsyncClient,
}

#[derive(Clone, Copy, Debug)]
pub struct SpendStatus {
    pub spend_txid: Option<Txid>,
}

impl EsploraClient {
    fn new(url: &str) -> Result<Self> {
        let builder = esplora_client::Builder::new(url);
        let esplora_client = builder.build_async()?;

        Ok(Self { esplora_client })
    }

    async fn find_outpoints(&self, address: &bitcoin::Address) -> Result<Vec<ExplorerUtxo>> {
        let script_pubkey = address.script_pubkey();
        let txs = self
            .esplora_client
            .scripthash_txs(&script_pubkey, None)
            .await?;

        let outputs = txs
            .into_iter()
            .flat_map(|tx| {
                let txid = tx.txid;
                tx.vout
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| v.scriptpubkey == script_pubkey)
                    .map(|(i, v)| ExplorerUtxo {
                        outpoint: OutPoint {
                            txid,
                            vout: i as u32,
                        },
                        amount: Amount::from_sat(v.value),
                        confirmation_blocktime: tx.status.block_time,
                        // Assume the output is unspent until we dig deeper, further down.
                        is_spent: false,
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        let mut utxos = Vec::new();
        for output in outputs.iter() {
            let outpoint = output.outpoint;
            let status = self
                .esplora_client
                .get_output_status(&outpoint.txid, outpoint.vout as u64)
                .await?;

            match status {
                Some(esplora_client::OutputStatus { spent: false, .. }) | None => {
                    utxos.push(*output);
                }
                Some(esplora_client::OutputStatus { spent: true, .. }) => {
                    utxos.push(ExplorerUtxo {
                        is_spent: true,
                        ..*output
                    });
                }
            }
        }

        Ok(utxos)
    }

    async fn _get_output_status(&self, txid: &Txid, vout: u32) -> Result<SpendStatus> {
        let status = self
            .esplora_client
            .get_output_status(txid, vout as u64)
            .await?;

        Ok(SpendStatus {
            spend_txid: status.and_then(|s| s.txid),
        })
    }
}

async fn faucet_fund(address: &bitcoin::Address, amount: Amount) -> Result<OutPoint> {
    let res = Command::new("nigiri")
        .args(["faucet", &address.to_string(), &amount.to_btc().to_string()])
        .output()?;

    assert!(res.status.success());

    let text = String::from_utf8(res.stdout)?;
    let re = Regex::new(r"txId: ([0-9a-fA-F]{64})")?;

    let txid = match re.captures(&text) {
        Some(captures) => match captures.get(1) {
            Some(txid) => txid.as_str(),
            _ => panic!("Could not parse TXID"),
        },
        None => {
            panic!("Could not parse TXID");
        }
    };

    let txid: Txid = txid.parse()?;

    let res = Command::new("nigiri")
        .args(["rpc", "getrawtransaction", &txid.to_string()])
        .output()?;

    let tx = String::from_utf8(res.stdout)?;

    let tx = Vec::from_hex(tx.trim())?;
    let tx: Transaction = bitcoin::consensus::deserialize(&tx)?;

    let (vout, _) = tx
        .output
        .iter()
        .enumerate()
        .find(|(_, o)| o.script_pubkey == address.script_pubkey())
        .context("could not find vout")?;

    // Wait for output to be confirmed.
    tokio::time::sleep(Duration::from_secs(5)).await;

    Ok(OutPoint {
        txid,
        vout: vout as u32,
    })
}

fn to_zkp_pk(pk: secp256k1::PublicKey) -> zkp::PublicKey {
    zkp::PublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}

pub fn from_zkp_xonly(pk: zkp::XOnlyPublicKey) -> XOnlyPublicKey {
    XOnlyPublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            "debug,\
             bdk=info,\
             tower=info,\
             hyper_util=info,\
             hyper=info,\
             reqwest=info,\
             h2=warn",
        )
        .init()
}

#![allow(clippy::unwrap_used)]

use ark_core::script::csv_sig_script;
use ark_core::script::multisig_script;
use ark_core::send;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::server::GetVtxosRequest;
use ark_core::Vtxo;
use ark_core::VtxoList;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Amount;
use bitcoin::XOnlyPublicKey;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::CryptoRng;
use rand::thread_rng;
use rand::Rng;
use std::sync::Arc;
use zkp::musig::new_musig_nonce_pair;
use zkp::musig::MusigAggNonce;
use zkp::musig::MusigKeyAggCache;
use zkp::musig::MusigSession;
use zkp::musig::MusigSessionId;

mod common;

/// Test a 2-of-3 multisig VTXO flow using MuSig2:
///
/// 1. Alice funds herself via boarding + settle.
/// 2. Alice sends to a shared VTXO with 3 tapscript leaf pairs (one per 2-of-3
///    combination: Alice+Bob, Alice+Carol, Bob+Carol). Each pair consists of a
///    cooperative forfeit leaf (server + MuSig2 aggregate key) and a unilateral
///    exit leaf (CSV + MuSig2 aggregate key).
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
    let zkp_secp = zkp::Secp256k1::new();
    let mut rng = thread_rng();

    // Set up Alice (for funding and sending to multisig)
    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    // Create keypairs for the 3 multisig participants
    let alice_kp = Keypair::new(&secp, &mut rng);
    let bob_kp = Keypair::new(&secp, &mut rng);
    let carol_kp = Keypair::new(&secp, &mut rng);

    // Create MuSig2 aggregated keys — one per 2-of-3 combination
    let musig_ab = MusigKeyAggCache::new(
        &zkp_secp,
        &[
            to_zkp_pk(alice_kp.public_key()),
            to_zkp_pk(bob_kp.public_key()),
        ],
    );
    let shared_pk_ab = from_zkp_xonly(musig_ab.agg_pk());

    let musig_ac = MusigKeyAggCache::new(
        &zkp_secp,
        &[
            to_zkp_pk(alice_kp.public_key()),
            to_zkp_pk(carol_kp.public_key()),
        ],
    );
    let shared_pk_ac = from_zkp_xonly(musig_ac.agg_pk());

    let musig_bc = MusigKeyAggCache::new(
        &zkp_secp,
        &[
            to_zkp_pk(bob_kp.public_key()),
            to_zkp_pk(carol_kp.public_key()),
        ],
    );
    let shared_pk_bc = from_zkp_xonly(musig_bc.agg_pk());

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
    let forfeit_ab = multisig_script(server_pk, shared_pk_ab);
    let forfeit_ac = multisig_script(server_pk, shared_pk_ac);
    let forfeit_bc = multisig_script(server_pk, shared_pk_bc);

    let exit_ab = csv_sig_script(exit_delay, shared_pk_ab);
    let exit_ac = csv_sig_script(exit_delay, shared_pk_ac);
    let exit_bc = csv_sig_script(exit_delay, shared_pk_bc);

    // Create the shared VTXO with custom scripts.
    // The "owner" field is set to Alice+Bob's aggregate key (arbitrary — it's
    // only used by `forfeit_spend_info` which we don't call for custom VTXOs).
    let shared_vtxo = Vtxo::new_with_custom_scripts(
        &secp,
        server_pk,
        shared_pk_ab,
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
        bob_kp.public_key().x_only_public_key().0,
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

    // --- MuSig2 signing of the virtual TX (Alice + Bob) ---
    {
        let sign_fn = musig2_sign_fn(
            &zkp_secp,
            &mut rng,
            &alice_kp,
            &bob_kp,
            &musig_ab,
            shared_pk_ab,
        );
        sign_ark_transaction(sign_fn, &mut virtual_tx, 0).unwrap();
    }

    // --- MuSig2 signing of the checkpoint TX (Alice + Bob) ---
    {
        let sign_fn = musig2_sign_fn(
            &zkp_secp,
            &mut rng,
            &alice_kp,
            &bob_kp,
            &musig_ab,
            shared_pk_ab,
        );
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

/// Create a MuSig2 signing closure for two parties.
///
/// Generates fresh nonces, performs partial signing for both parties, and
/// aggregates the result into a single Schnorr signature.
fn musig2_sign_fn(
    zkp_secp: &zkp::Secp256k1<zkp::All>,
    rng: &mut (impl Rng + CryptoRng),
    kp_a: &Keypair,
    kp_b: &Keypair,
    musig_cache: &MusigKeyAggCache,
    shared_pk: XOnlyPublicKey,
) -> impl FnOnce(&mut psbt::Input, secp256k1::Message) -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error>
{
    let (a_nonce, a_pub_nonce) = {
        let session_id = MusigSessionId::new(rng);
        let extra_rand: [u8; 32] = rng.r#gen();
        new_musig_nonce_pair(
            zkp_secp,
            session_id,
            None,
            None,
            to_zkp_pk(kp_a.public_key()),
            None,
            Some(extra_rand),
        )
        .unwrap()
    };

    let (b_nonce, b_pub_nonce) = {
        let session_id = MusigSessionId::new(rng);
        let extra_rand: [u8; 32] = rng.r#gen();
        new_musig_nonce_pair(
            zkp_secp,
            session_id,
            None,
            None,
            to_zkp_pk(kp_b.public_key()),
            None,
            Some(extra_rand),
        )
        .unwrap()
    };

    let zkp_secp = zkp_secp.clone();
    let kp_a = *kp_a;
    let kp_b = *kp_b;
    let musig_cache = musig_cache.clone();

    move |_: &mut psbt::Input, msg: secp256k1::Message| {
        let agg_nonce = MusigAggNonce::new(&zkp_secp, &[a_pub_nonce, b_pub_nonce]);
        let msg =
            zkp::Message::from_digest_slice(msg.as_ref()).map_err(ark_core::Error::ad_hoc)?;

        let session = MusigSession::new(&zkp_secp, &musig_cache, agg_nonce, msg);

        let a_zkp_kp = zkp::Keypair::from_seckey_slice(&zkp_secp, &kp_a.secret_bytes())
            .map_err(ark_core::Error::ad_hoc)?;
        let a_sig = session
            .partial_sign(&zkp_secp, a_nonce, &a_zkp_kp, &musig_cache)
            .map_err(ark_core::Error::ad_hoc)?;

        let b_zkp_kp = zkp::Keypair::from_seckey_slice(&zkp_secp, &kp_b.secret_bytes())
            .map_err(ark_core::Error::ad_hoc)?;
        let b_sig = session
            .partial_sign(&zkp_secp, b_nonce, &b_zkp_kp, &musig_cache)
            .map_err(ark_core::Error::ad_hoc)?;

        let sig = session.partial_sig_agg(&[a_sig, b_sig]);
        let sig =
            schnorr::Signature::from_slice(sig.as_ref()).map_err(ark_core::Error::ad_hoc)?;

        Ok(vec![(sig, shared_pk)])
    }
}

fn to_zkp_pk(pk: secp256k1::PublicKey) -> zkp::PublicKey {
    zkp::PublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}

fn from_zkp_xonly(pk: zkp::XOnlyPublicKey) -> XOnlyPublicKey {
    XOnlyPublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}

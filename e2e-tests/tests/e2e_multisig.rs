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
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Amount;
use bitcoin::XOnlyPublicKey;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::thread_rng;
use rand::Rng;
use std::sync::Arc;
use zkp::musig::new_musig_nonce_pair;
use zkp::musig::MusigAggNonce;
use zkp::musig::MusigKeyAggCache;
use zkp::musig::MusigSession;
use zkp::musig::MusigSessionId;

mod common;

/// Test a normal 2-of-2 multisig VTXO flow using MuSig2:
///
/// 1. Alice funds herself via boarding + settle.
/// 2. Alice sends to a shared VTXO owned by a MuSig2 aggregate key (Alice + Bob).
///    The server doesn't even know this is a multisig.
/// 3. Both parties cooperatively sign an offchain transaction spending the shared
///    VTXO to Bob's regular address, using the `send` module (no delegates).
/// 4. Verify the shared VTXO is spent and Bob received the funds.
#[tokio::test]
#[ignore]
pub async fn e2e_multisig() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let zkp_secp = zkp::Secp256k1::new();
    let mut rng = thread_rng();

    // Set up Alice (for funding and sending to multisig)
    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    // Create keypairs for the multisig participants
    let alice_kp = Keypair::new(&secp, &mut rng);
    let bob_kp = Keypair::new(&secp, &mut rng);

    // Create MuSig2 aggregated key — the server sees a single pubkey
    let musig_key_agg_cache = MusigKeyAggCache::new(
        &zkp_secp,
        &[
            to_zkp_pk(alice_kp.public_key()),
            to_zkp_pk(bob_kp.public_key()),
        ],
    );
    let shared_pk = from_zkp_xonly(musig_key_agg_cache.agg_pk());

    let server_pk = alice.server_info.signer_pk.x_only_public_key().0;

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

    // Create a standard VTXO with the MuSig2 shared key as owner.
    // This produces the same forfeit/redeem scripts as any normal VTXO:
    //   forfeit: server_pk CHECKSIGVERIFY shared_pk CHECKSIG
    //   redeem:  CSV shared_pk CHECKSIG
    let shared_vtxo = Vtxo::new_default(
        &secp,
        server_pk,
        shared_pk,
        alice.server_info.unilateral_exit_delay,
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

    tracing::info!(%send_txid, "Alice funded shared multisig VTXO");

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
        "Found shared multisig VTXO"
    );

    // Bob's payout VTXO address
    let bob_payout_vtxo = Vtxo::new_default(
        &secp,
        server_pk,
        bob_kp.public_key().x_only_public_key().0,
        alice.server_info.unilateral_exit_delay,
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

    // Build the cooperative spend: shared VTXO → Bob's address
    let (forfeit_script, control_block) = shared_vtxo.forfeit_spend_info().unwrap();

    let msig_input = send::VtxoInput::new(
        forfeit_script,
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

    // --- MuSig2 signing of the virtual TX ---
    {
        let (alice_musig_nonce, alice_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand: [u8; 32] = rng.r#gen();
            new_musig_nonce_pair(
                &zkp_secp,
                session_id,
                None,
                None,
                to_zkp_pk(alice_kp.public_key()),
                None,
                Some(extra_rand),
            )
            .unwrap()
        };

        let (bob_musig_nonce, bob_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand: [u8; 32] = rng.r#gen();
            new_musig_nonce_pair(
                &zkp_secp,
                session_id,
                None,
                None,
                to_zkp_pk(bob_kp.public_key()),
                None,
                Some(extra_rand),
            )
            .unwrap()
        };

        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let musig_agg_nonce =
                    MusigAggNonce::new(&zkp_secp, &[alice_musig_pub_nonce, bob_musig_pub_nonce]);
                let msg = zkp::Message::from_digest_slice(msg.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                let session =
                    MusigSession::new(&zkp_secp, &musig_key_agg_cache, musig_agg_nonce, msg);

                let alice_zkp_kp =
                    zkp::Keypair::from_seckey_slice(&zkp_secp, &alice_kp.secret_bytes())
                        .map_err(ark_core::Error::ad_hoc)?;
                let alice_sig = session
                    .partial_sign(
                        &zkp_secp,
                        alice_musig_nonce,
                        &alice_zkp_kp,
                        &musig_key_agg_cache,
                    )
                    .map_err(ark_core::Error::ad_hoc)?;

                let bob_zkp_kp = zkp::Keypair::from_seckey_slice(&zkp_secp, &bob_kp.secret_bytes())
                    .map_err(ark_core::Error::ad_hoc)?;
                let bob_sig = session
                    .partial_sign(
                        &zkp_secp,
                        bob_musig_nonce,
                        &bob_zkp_kp,
                        &musig_key_agg_cache,
                    )
                    .map_err(ark_core::Error::ad_hoc)?;

                let sig = session.partial_sig_agg(&[alice_sig, bob_sig]);
                let sig = schnorr::Signature::from_slice(sig.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                Ok(vec![(sig, shared_pk)])
            };

        sign_ark_transaction(sign_fn, &mut virtual_tx, 0).unwrap();
    }

    // --- MuSig2 signing of the checkpoint TX (must be done before submitting) ---
    {
        let (alice_musig_nonce, alice_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand: [u8; 32] = rng.r#gen();
            new_musig_nonce_pair(
                &zkp_secp,
                session_id,
                None,
                None,
                to_zkp_pk(alice_kp.public_key()),
                None,
                Some(extra_rand),
            )
            .unwrap()
        };

        let (bob_musig_nonce, bob_musig_pub_nonce) = {
            let session_id = MusigSessionId::new(&mut rng);
            let extra_rand: [u8; 32] = rng.r#gen();
            new_musig_nonce_pair(
                &zkp_secp,
                session_id,
                None,
                None,
                to_zkp_pk(bob_kp.public_key()),
                None,
                Some(extra_rand),
            )
            .unwrap()
        };

        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let musig_agg_nonce =
                    MusigAggNonce::new(&zkp_secp, &[alice_musig_pub_nonce, bob_musig_pub_nonce]);
                let msg = zkp::Message::from_digest_slice(msg.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                let session =
                    MusigSession::new(&zkp_secp, &musig_key_agg_cache, musig_agg_nonce, msg);

                let alice_zkp_kp =
                    zkp::Keypair::from_seckey_slice(&zkp_secp, &alice_kp.secret_bytes())
                        .map_err(ark_core::Error::ad_hoc)?;
                let alice_sig = session
                    .partial_sign(
                        &zkp_secp,
                        alice_musig_nonce,
                        &alice_zkp_kp,
                        &musig_key_agg_cache,
                    )
                    .map_err(ark_core::Error::ad_hoc)?;

                let bob_zkp_kp = zkp::Keypair::from_seckey_slice(&zkp_secp, &bob_kp.secret_bytes())
                    .map_err(ark_core::Error::ad_hoc)?;
                let bob_sig = session
                    .partial_sign(
                        &zkp_secp,
                        bob_musig_nonce,
                        &bob_zkp_kp,
                        &musig_key_agg_cache,
                    )
                    .map_err(ark_core::Error::ad_hoc)?;

                let sig = session.partial_sig_agg(&[alice_sig, bob_sig]);
                let sig = schnorr::Signature::from_slice(sig.as_ref())
                    .map_err(ark_core::Error::ad_hoc)?;

                Ok(vec![(sig, shared_pk)])
            };

        // For multisig VTXOs we must pre-sign the checkpoint before submitting,
        // since the "owner" signature requires coordination between both parties.
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
        "Cooperatively spent multisig VTXO to Bob's address"
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
        "Bob received funds from multisig VTXO"
    );
}

fn to_zkp_pk(pk: secp256k1::PublicKey) -> zkp::PublicKey {
    zkp::PublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}

fn from_zkp_xonly(pk: zkp::XOnlyPublicKey) -> XOnlyPublicKey {
    XOnlyPublicKey::from_slice(&pk.serialize()).expect("valid conversion")
}

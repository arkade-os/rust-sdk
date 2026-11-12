#![allow(clippy::unwrap_used)]

//! The nine-leaf `VhtlcScript::new_v2` tree against a live arkd and emulator.
//!
//! The unit tests pin the derivation to the TS SDK's cross-SDK vectors; these tests prove the
//! other half: that arkd accepts a spend from that tree, and that the emulator co-signs the
//! covenant leaves built from our arkade scripts and tweaked keys.

use crate::common::wait_until_balance;
use ark_bdk_wallet::Wallet;
use ark_client::Client;
use ark_client::InMemorySwapStorage;
use ark_core::introspector::packet::add_packet_to_psbt;
use ark_core::introspector::packet::IntrospectorEntry;
use ark_core::introspector::packet::Packet;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::SendReceiver;
use ark_core::send::VtxoInput;
use ark_core::server::GetVtxosRequest;
use ark_core::server::VirtualTxOutPoint;
use ark_core::vhtlc::NonInteractiveParameters;
use ark_core::vhtlc::VhtlcOptions;
use ark_core::vhtlc::VhtlcScript;
use ark_core::ArkAddress;
use ark_core::VtxoList;
use ark_core::VTXO_CONDITION_KEY;
use ark_introspector_client::IntrospectorClient;
use bitcoin::absolute;
use bitcoin::consensus::Encodable;
use bitcoin::hashes::hash160;
use bitcoin::hashes::ripemd160;
use bitcoin::hashes::Hash;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::taproot::LeafVersion;
use bitcoin::Amount;
use bitcoin::ScriptBuf;
use bitcoin::VarInt;
use bitcoin::Witness;
use bitcoin::XOnlyPublicKey;
use common::init_tracing;
use common::set_up_client;
use common::Regtest;
use rand::thread_rng;
use rand::Rng;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

mod common;

const VHTLC_AMOUNT: Amount = Amount::from_sat(100_000);

/// Seconds-typed and long in the past, so the timelocked leaf is mature the moment the VHTLC is
/// funded. arkd refuses a block-height `refund_locktime` on a forfeit-eligible leaf regardless of
/// maturity, so a seconds-typed value is the only kind the covenant suite can carry.
const EXPIRED_REFUND_LOCKTIME: u32 = 1_700_000_000;

/// A refund locktime that stays closed for the lifetime of the test. Kept below `i32::MAX`: a
/// larger value needs a 5-byte script number, which arkd's closure parser does not round-trip, so
/// it rebuilds a different taproot root and refuses the input.
fn unexpired_refund_locktime() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    u32::try_from(now + 24 * 60 * 60).unwrap()
}

/// The receiver claims the nine-leaf VHTLC through the plain `claim` leaf: preimage, receiver
/// signature and arkd's co-signature. This is the path a swap client takes after the solver's
/// Lightning payment reveals the preimage, and the one that proves arkd accepts the tree and the
/// V2 `OP_SIZE 32` preimage condition.
#[tokio::test]
#[ignore]
pub async fn e2e_vhtlc_v2_nine_leaf_claim() {
    init_tracing();

    let fixture = FundedVhtlc::set_up(unexpired_refund_locktime()).await;

    let claim_script = fixture.vhtlc.claim_script();
    let (mut ark_tx, checkpoint_txs) =
        fixture.build_spend(claim_script, None, fixture.receiver_payout);

    let receiver_kp = fixture.receiver_kp;
    let preimage = fixture.preimage;
    let sign_fn = |input: &mut psbt::Input,
                   msg: secp256k1::Message|
     -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
        // The preimage travels as the VTXO condition witness, as the Boltz claim does.
        let mut bytes = vec![1];
        VarInt::from(preimage.len() as u64)
            .consensus_encode(&mut bytes)
            .unwrap();
        bytes.write_all(&preimage).unwrap();
        input.unknown.insert(
            psbt::raw::Key {
                type_value: 222,
                key: VTXO_CONDITION_KEY.to_vec(),
            },
            bytes,
        );

        let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &receiver_kp);
        Ok(vec![(sig, receiver_kp.x_only_public_key().0)])
    };

    sign_ark_transaction(sign_fn, &mut ark_tx, 0).unwrap();
    let ark_txid = ark_tx.unsigned_tx.compute_txid();

    let response = fixture
        .grpc_client
        .submit_offchain_transaction_request(ark_tx, checkpoint_txs)
        .await
        .unwrap();

    let mut checkpoint_psbt = response.signed_checkpoint_txs.first().unwrap().clone();
    sign_checkpoint_transaction(sign_fn, &mut checkpoint_psbt).unwrap();

    fixture
        .grpc_client
        .finalize_offchain_transaction(ark_txid, vec![checkpoint_psbt])
        .await
        .unwrap();

    tracing::info!(%ark_txid, "claimed nine-leaf VHTLC through the claim leaf");

    wait_until_balance!(&fixture.bob, pre_confirmed: VHTLC_AMOUNT);
}

/// The `non_interactive_refund` leaf: server + receiver + the covenant-tweaked emulator key,
/// no timelock. The emulator only co-signs if `enforce_pay_to(sender_pk_script)` holds, which
/// is what proves our arkade script bytes and tweak agree with the running emulator.
#[tokio::test]
#[ignore]
pub async fn e2e_vhtlc_v2_non_interactive_refund_via_emulator() {
    init_tracing();

    let fixture = FundedVhtlc::set_up(unexpired_refund_locktime()).await;

    let leaf = fixture.vhtlc.non_interactive_refund_script().unwrap();
    let arkade_script = fixture
        .vhtlc
        .non_interactive_refund_arkade_script()
        .unwrap();

    let (mut ark_tx, mut checkpoint_txs) = fixture.build_spend(leaf, None, fixture.sender_payout);
    attach_covenant(&mut ark_tx, &arkade_script);

    let receiver_kp = fixture.receiver_kp;
    let sign_fn = |_: &mut psbt::Input,
                   msg: secp256k1::Message|
     -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
        let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &receiver_kp);
        Ok(vec![(sig, receiver_kp.x_only_public_key().0)])
    };
    sign_ark_transaction(sign_fn, &mut ark_tx, 0).unwrap();
    sign_checkpoint_transaction(sign_fn, &mut checkpoint_txs[0]).unwrap();

    let response = fixture
        .introspector
        .submit_tx(&ark_tx, &checkpoint_txs)
        .await
        .unwrap();

    tracing::info!(
        ark_txid = %response.signed_ark_tx.unsigned_tx.compute_txid(),
        "emulator co-signed the non-interactive refund"
    );

    wait_until_balance!(&fixture.carol, pre_confirmed: VHTLC_AMOUNT);
}

/// The `non_interactive_refund_without_receiver` leaf: server + covenant-tweaked emulator key
/// after `refund_locktime`. Nobody holding a participant key signs; the refund is pushed by the
/// covenant alone, which is the solver's "client vanished after funding" recourse.
#[tokio::test]
#[ignore]
pub async fn e2e_vhtlc_v2_timelocked_non_interactive_refund_via_emulator() {
    init_tracing();

    let fixture = FundedVhtlc::set_up(EXPIRED_REFUND_LOCKTIME).await;

    let leaf = fixture
        .vhtlc
        .non_interactive_refund_without_receiver_script()
        .unwrap();
    let arkade_script = fixture
        .vhtlc
        .non_interactive_refund_arkade_script()
        .unwrap();

    let (mut ark_tx, checkpoint_txs) = fixture.build_spend(
        leaf,
        Some(absolute::LockTime::from_consensus(EXPIRED_REFUND_LOCKTIME)),
        fixture.sender_payout,
    );
    attach_covenant(&mut ark_tx, &arkade_script);

    // No participant signature: only arkd and the emulator sign this leaf.
    let response = fixture
        .introspector
        .submit_tx(&ark_tx, &checkpoint_txs)
        .await
        .unwrap();

    tracing::info!(
        ark_txid = %response.signed_ark_tx.unsigned_tx.compute_txid(),
        "emulator co-signed the timelocked non-interactive refund"
    );

    wait_until_balance!(&fixture.carol, pre_confirmed: VHTLC_AMOUNT);
}

/// A funded nine-leaf VHTLC: alice funds it, carol is the sender's refund destination, bob the
/// receiver's payout, arkd the server, and the introspector the emulator. Bob and carol start
/// empty, so a payout to either is observable as an exact balance.
struct FundedVhtlc {
    bob: Client<Regtest, Wallet, InMemorySwapStorage>,
    carol: Client<Regtest, Wallet, InMemorySwapStorage>,
    grpc_client: ark_grpc::Client,
    introspector: IntrospectorClient,
    server_info: ark_core::server::Info,
    vhtlc: VhtlcScript,
    outpoint: VirtualTxOutPoint,
    receiver_kp: Keypair,
    preimage: [u8; 32],
    sender_payout: ArkAddress,
    receiver_payout: ArkAddress,
}

impl FundedVhtlc {
    async fn set_up(refund_locktime: u32) -> Self {
        let regtest = Arc::new(Regtest::new());
        let secp = Secp256k1::new();
        let mut rng = thread_rng();

        let (alice, _) = set_up_client("alice".to_string(), regtest.clone(), secp.clone()).await;
        let (bob, _) = set_up_client("bob".to_string(), regtest.clone(), secp.clone()).await;
        let (carol, _) = set_up_client("carol".to_string(), regtest.clone(), secp.clone()).await;

        let mut grpc_client = ark_grpc::Client::new("http://localhost:7070".to_string());
        grpc_client.connect().await.unwrap();
        let server_info = grpc_client.get_info().await.unwrap();

        let introspector_url = std::env::var("INTROSPECTOR_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7073".to_string());
        let introspector = IntrospectorClient::new(introspector_url);
        let emulator_pubkey = introspector.get_info().await.unwrap().signer_xonly();

        let alice_boarding_address = alice.get_boarding_address().await.unwrap();
        regtest
            .faucet_fund(&alice_boarding_address, Amount::ONE_BTC)
            .await;
        alice.settle_all(&mut rng).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        let sender_kp = Keypair::new(&secp, &mut rng);
        let receiver_kp = Keypair::new(&secp, &mut rng);
        let server = server_info.signer_pk.x_only_public_key().0;

        let preimage: [u8; 32] = rng.r#gen();
        let preimage_hash =
            ripemd160::Hash::from_byte_array(hash160::Hash::hash(&preimage).to_byte_array());

        let (sender_payout, _) = carol.get_offchain_address().await.unwrap();
        let (receiver_payout, _) = bob.get_offchain_address().await.unwrap();

        let options = VhtlcOptions {
            sender: sender_kp.x_only_public_key().0,
            receiver: receiver_kp.x_only_public_key().0,
            server,
            preimage_hash,
            refund_locktime,
            unilateral_claim_delay: server_info.unilateral_exit_delay,
            unilateral_refund_delay: server_info.unilateral_exit_delay,
            unilateral_refund_without_receiver_delay: server_info.unilateral_exit_delay,
        };
        let non_interactive = NonInteractiveParameters {
            emulator_pubkey,
            receiver_pk_script: receiver_payout.to_p2tr_script_pubkey(),
            sender_pk_script: sender_payout.to_p2tr_script_pubkey(),
            legacy_pre_timelocked_refund: false,
        };
        let vhtlc =
            VhtlcScript::new_v2(options, Some(non_interactive), server_info.network).unwrap();
        assert_eq!(vhtlc.tapscripts().len(), 9);

        let funding_txid = alice
            .send(vec![SendReceiver::bitcoin(vhtlc.address(), VHTLC_AMOUNT)])
            .await
            .unwrap();
        tracing::info!(%funding_txid, address = %vhtlc.address(), "funded nine-leaf VHTLC");

        let outpoint = wait_for_vtxo(&grpc_client, vhtlc.address(), server_info.dust).await;

        Self {
            bob,
            carol,
            grpc_client,
            introspector,
            server_info,
            vhtlc,
            outpoint,
            receiver_kp,
            preimage,
            sender_payout,
            receiver_payout,
        }
    }

    /// Drain the VHTLC to `payout` through `leaf`.
    fn build_spend(
        &self,
        leaf: ScriptBuf,
        locktime: Option<absolute::LockTime>,
        payout: ArkAddress,
    ) -> (bitcoin::Psbt, Vec<bitcoin::Psbt>) {
        let control_block = self
            .vhtlc
            .taproot_spend_info()
            .control_block(&(leaf.clone(), LeafVersion::TapScript))
            .unwrap();

        let input = VtxoInput::new(
            leaf,
            locktime,
            control_block,
            self.vhtlc.tapscripts(),
            self.vhtlc.script_pubkey(),
            self.outpoint.amount,
            self.outpoint.outpoint,
            self.outpoint.assets.clone(),
        );

        let offchain = build_offchain_transactions(
            &[SendReceiver::bitcoin(payout, self.outpoint.amount)],
            // Superfluous: the VHTLC is drained.
            &payout,
            std::slice::from_ref(&input),
            &self.server_info,
        )
        .unwrap();

        (offchain.ark_tx, offchain.checkpoint_txs)
    }
}

/// Hand the emulator the arkade script it must evaluate for input 0. The script pushes its own
/// input index, so the witness stays empty.
fn attach_covenant(ark_tx: &mut bitcoin::Psbt, arkade_script: &ScriptBuf) {
    add_packet_to_psbt(
        ark_tx,
        &Packet::new(vec![IntrospectorEntry {
            vin: 0,
            script: arkade_script.clone(),
            witness: Witness::default(),
        }])
        .unwrap(),
    )
    .unwrap();
}

async fn wait_for_vtxo(
    grpc_client: &ark_grpc::Client,
    address: ArkAddress,
    dust: Amount,
) -> VirtualTxOutPoint {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let response = grpc_client
                .list_vtxos(GetVtxosRequest::new_for_addresses(std::iter::once(address)))
                .await
                .unwrap();

            let list = VtxoList::new(dust, response.vtxos);
            if let Some(outpoint) = list.spendable_offchain().next().cloned() {
                return outpoint;
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .unwrap()
}

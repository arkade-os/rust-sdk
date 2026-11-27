#![allow(clippy::unwrap_used)]

//! Spending VHTLC outputs through the client's contract API (`add_contract`,
//! `send_contract_vtxos`, `settle_contract_vtxos`) against a live arkd and emulator.
//!
//! The handler picks the leaf from what the spender holds: wallet keys, a co-signer, the preimage
//! and the time. These tests prove that each pick produces a spend arkd (and, for the covenant
//! leaves, the emulator) accepts, and that the wallet's own flows leave a VHTLC alone.

use crate::common::wait_until_balance;
use ark_bdk_wallet::Wallet;
use ark_client::wallet::OnchainWallet;
use ark_client::Client;
use ark_client::ContractSpendOptions;
use ark_client::ExternalSigner;
use ark_core::contract::SpendPathKind;
use ark_core::contract::VhtlcContract;
use ark_core::contract::VhtlcV2Contract;
use ark_core::send::SendReceiver;
use ark_core::server::GetVtxosRequest;
use ark_core::server::VirtualTxOutPoint;
use ark_core::vhtlc::NonInteractiveParameters;
use ark_core::vhtlc::VhtlcOptions;
use ark_core::vhtlc::VhtlcScript;
use ark_core::ArkAddress;
use ark_core::VtxoList;
use ark_introspector_client::IntrospectorClient;
use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::hash160;
use bitcoin::hashes::ripemd160;
use bitcoin::hashes::Hash;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::relative;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Amount;
use bitcoin::Transaction;
use bitcoin::TxOut;
use bitcoin::XOnlyPublicKey;
use common::init_tracing;
use common::set_up_client;
use common::Regtest;
use rand::thread_rng;
use rand::Rng;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

mod common;

const VHTLC_AMOUNT: Amount = Amount::from_sat(100_000);

/// Seconds-typed and long in the past, so `refund_without_receiver` is open the moment the VHTLC
/// is funded.
const EXPIRED_REFUND_LOCKTIME: u32 = 1_700_000_000;

/// A refund locktime that stays closed for the lifetime of the test.
fn unexpired_refund_locktime() -> u32 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    u32::try_from(now + 24 * 60 * 60).unwrap()
}

/// The receiver claims with the preimage: bob holds the receiver key, so the handler picks
/// `claim`. Without the preimage, or with the wrong one, the spend is refused locally.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_spend_v2_claim() {
    init_tracing();

    let f = Fixture::set_up(Setup::v2(unexpired_refund_locktime())).await;

    let err = f
        .bob
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.receiver_payout,
            ContractSpendOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("needs the preimage"), "{err:#}");

    let err = f
        .bob
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.receiver_payout,
            ContractSpendOptions {
                preimage: Some([0; 32]),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        format!("{err:#}").contains("does not match the preimage hash"),
        "{err:#}"
    );

    let txid = f
        .bob
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.receiver_payout,
            ContractSpendOptions {
                preimage: Some(f.preimage),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tracing::info!(%txid, "claimed VHTLC");

    f.wait_for_payout(f.receiver_payout).await;
}

/// The sender refunds with the receiver's co-signature before the locktime: carol holds the
/// sender key and a co-signer holds the receiver's, so the handler picks `refund`. Without the
/// co-signer, `refund_without_receiver` is still closed and the spend is refused locally.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_spend_v2_collaborative_refund() {
    init_tracing();

    let f = Fixture::set_up(Setup {
        external_receiver: true,
        ..Setup::v2(unexpired_refund_locktime())
    })
    .await;

    let err = f
        .carol
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("opens at Unix time"), "{err:#}");

    let receiver = KeypairSigner(f.receiver_kp.unwrap());

    // A batch spend signs with the wallet's keys only.
    let err = f
        .carol
        .settle_contract_vtxos(
            &mut thread_rng(),
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions {
                cosigner: Some(&receiver),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("wallet's keys only"), "{err:#}");

    // Naming the same output twice is refused up front.
    let err = f
        .carol
        .send_contract_vtxos(
            &[f.outpoint.outpoint, f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions {
                cosigner: Some(&receiver),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("duplicate outpoint"), "{err:#}");

    let txid = f
        .carol
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions {
                cosigner: Some(&receiver),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tracing::info!(%txid, "refunded VHTLC with the receiver");

    f.wait_for_payout(f.sender_payout).await;
}

/// After the locktime the sender refunds alone through `refund_without_receiver`.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_spend_v2_refund_without_receiver() {
    init_tracing();

    let f = Fixture::set_up(Setup::v2(EXPIRED_REFUND_LOCKTIME)).await;

    let txid = f
        .carol
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions::default(),
        )
        .await
        .unwrap();
    tracing::info!(%txid, "refunded VHTLC without the receiver");

    f.wait_for_payout(f.sender_payout).await;
}

/// The receiver claims in a batch: the forfeit carries the preimage as its condition witness.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_settle_v2_claim() {
    init_tracing();

    let f = Fixture::set_up(Setup::v2(unexpired_refund_locktime())).await;

    let commitment_txid = f
        .bob
        .settle_contract_vtxos(
            &mut thread_rng(),
            &[f.outpoint.outpoint],
            f.receiver_payout,
            ContractSpendOptions {
                preimage: Some(f.preimage),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tracing::info!(%commitment_txid, "claimed VHTLC in a batch");

    f.wait_for_payout(f.receiver_payout).await;
}

/// The sender refunds alone in a batch: the forfeit carries the leaf's CLTV as its locktime.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_settle_v2_refund_without_receiver() {
    init_tracing();

    let f = Fixture::set_up(Setup::v2(EXPIRED_REFUND_LOCKTIME)).await;

    let commitment_txid = f
        .carol
        .settle_contract_vtxos(
            &mut thread_rng(),
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions::default(),
        )
        .await
        .unwrap();
    tracing::info!(%commitment_txid, "refunded VHTLC without the receiver in a batch");

    f.wait_for_payout(f.sender_payout).await;
}

/// `non_interactive_claim`: anyone with the preimage can push the funds to the receiver's
/// script; the emulator enforces the destination.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_spend_v2_non_interactive_claim() {
    init_tracing();

    let f = Fixture::set_up(Setup {
        non_interactive: true,
        ..Setup::v2(unexpired_refund_locktime())
    })
    .await;

    // Carol holds no key the leaf needs; the preimage and the emulator are enough.
    let txid = f
        .carol
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.receiver_payout,
            ContractSpendOptions {
                preimage: Some(f.preimage),
                path: Some(&SpendPathKind::VhtlcNonInteractiveClaim),
                introspector: Some(&f.introspector),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tracing::info!(%txid, "claimed VHTLC through the covenant leaf");

    f.wait_for_payout(f.receiver_payout).await;
}

/// `non_interactive_refund`: the receiver hands the funds back to the sender's script without
/// the sender.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_spend_v2_non_interactive_refund() {
    init_tracing();

    let f = Fixture::set_up(Setup {
        non_interactive: true,
        ..Setup::v2(unexpired_refund_locktime())
    })
    .await;

    let txid = f
        .bob
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions {
                path: Some(&SpendPathKind::VhtlcNonInteractiveRefund),
                introspector: Some(&f.introspector),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tracing::info!(%txid, "refunded VHTLC through the covenant leaf");

    f.wait_for_payout(f.sender_payout).await;
}

/// `non_interactive_refund_without_receiver`: after the locktime anyone can push the funds back
/// to the sender's script.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_spend_v2_non_interactive_refund_without_receiver() {
    init_tracing();

    let f = Fixture::set_up(Setup {
        non_interactive: true,
        ..Setup::v2(EXPIRED_REFUND_LOCKTIME)
    })
    .await;

    // Bob is the receiver, but the leaf needs no key at all.
    let txid = f
        .bob
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions {
                path: Some(&SpendPathKind::VhtlcNonInteractiveRefundWithoutReceiver),
                introspector: Some(&f.introspector),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    tracing::info!(%txid, "refunded VHTLC without the receiver through the covenant leaf");

    f.wait_for_payout(f.sender_payout).await;
}

/// The six-leaf v1 VHTLC goes through the same API: claim with the preimage...
#[tokio::test]
#[ignore]
pub async fn e2e_contract_spend_v1_claim() {
    init_tracing();

    let f = Fixture::set_up(Setup {
        v1: true,
        ..Setup::v2(unexpired_refund_locktime())
    })
    .await;

    f.bob
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.receiver_payout,
            ContractSpendOptions {
                preimage: Some(f.preimage),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    f.wait_for_payout(f.receiver_payout).await;
}

/// ...and refund after the locktime.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_spend_v1_refund_without_receiver() {
    init_tracing();

    let f = Fixture::set_up(Setup {
        v1: true,
        ..Setup::v2(EXPIRED_REFUND_LOCKTIME)
    })
    .await;

    f.carol
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions::default(),
        )
        .await
        .unwrap();

    f.wait_for_payout(f.sender_payout).await;
}

/// A tracked VHTLC is escrow, not the wallet's money: `send` and `settle_all` never pick it,
/// even when the wallet could spend it alone.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_vhtlc_not_spent_by_wallet_flows() {
    init_tracing();

    let f = Fixture::set_up(Setup::v2(EXPIRED_REFUND_LOCKTIME)).await;

    // Carol can spend the VHTLC alone, but holds nothing else.
    let err = f
        .carol
        .send(vec![SendReceiver::bitcoin(f.receiver_payout, VHTLC_AMOUNT)])
        .await
        .unwrap_err();
    tracing::info!("send refused as expected: {err:#}");

    // Nothing for `settle_all` to pick up either; whether it errors or no-ops, the VHTLC stays.
    let _ = f.carol.settle_all(&mut thread_rng()).await;

    let vtxo = f.vhtlc_vtxo().await;
    assert!(!vtxo.is_spent, "the VHTLC must not be spent by the wallet");

    // The contract API still spends it.
    f.carol
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions::default(),
        )
        .await
        .unwrap();
    f.wait_for_payout(f.sender_payout).await;
}

/// After the batch expires, arkd sweeps the VHTLC; the sender recovers it in a batch.
///
/// Slow: waits out `ARKD_VTXO_TREE_EXPIRY` (512 s on the regtest stack), so CI skips it (`_slow`).
#[tokio::test]
#[ignore]
pub async fn e2e_contract_settle_swept_vhtlc_slow() {
    init_tracing();

    let f = Fixture::set_up(Setup::v2(EXPIRED_REFUND_LOCKTIME)).await;

    tokio::time::timeout(Duration::from_secs(20 * 60), async {
        loop {
            if f.vhtlc_vtxo().await.is_swept {
                break;
            }
            f.regtest.mine(1).await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    })
    .await
    .expect("arkd did not sweep the VHTLC");

    let err = f
        .carol
        .send_contract_vtxos(
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("swept"), "{err:#}");

    f.carol
        .settle_contract_vtxos(
            &mut thread_rng(),
            &[f.outpoint.outpoint],
            f.sender_payout,
            ContractSpendOptions::default(),
        )
        .await
        .unwrap();

    f.wait_for_payout(f.sender_payout).await;
}

/// The receiver exits unilaterally and claims on-chain through `unilateral_claim`: preimage,
/// receiver signature, CSV. This one also waits out the real CSV and broadcasts.
///
/// Slow: waits out `ARKD_UNILATERAL_EXIT_DELAY` (512 s on the regtest stack), so CI skips it
/// (`_slow`).
#[tokio::test]
#[ignore]
pub async fn e2e_contract_exit_v2_unilateral_claim_slow() {
    init_tracing();

    let f = Fixture::set_up(Setup::v2(unexpired_refund_locktime())).await;
    f.unroll(&f.bob, &f.bob_wallet).await;

    // Without the preimage, the receiver has no unilateral leaf.
    let err = f
        .bob
        .create_contract_on_chain_transaction(
            &[f.outpoint.outpoint],
            onchain_address(),
            EXIT_FEE,
            ContractSpendOptions::default(),
        )
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("needs the preimage"), "{err:#}");

    let options = ContractSpendOptions {
        preimage: Some(f.preimage),
        ..Default::default()
    };
    let (tx, prevouts) = f.exit_tx(&f.bob, options).await;
    verify_consensus(&tx, &prevouts);

    // Consensus is happy with the witness; now prove the chain is too once the CSV matures. The
    // client's maturity check reads the wall clock, while the chain measures CSV in median time
    // past, which lags; so wait for the node itself to accept the spend.
    f.regtest.set_outpoint_blocktime_offset(0);
    tokio::time::timeout(Duration::from_secs(20 * 60), async {
        loop {
            let built = f
                .bob
                .create_contract_on_chain_transaction(
                    &[f.outpoint.outpoint],
                    onchain_address(),
                    EXIT_FEE,
                    options,
                )
                .await;
            match built.map(|(tx, _)| f.regtest.test_mempool_accept(&tx)) {
                Ok(Ok(())) => return,
                Ok(Err(reason)) => tracing::debug!(%reason, "node refuses the exit spend yet"),
                Err(err) => tracing::debug!("client refuses the exit spend yet: {err:#}"),
            }
            f.regtest.mine(1).await;
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    })
    .await
    .expect("the exit leaf never matured");

    let txid = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            match f
                .bob
                .send_contract_on_chain(
                    &[f.outpoint.outpoint],
                    onchain_address(),
                    EXIT_FEE,
                    options,
                )
                .await
            {
                Ok(txid) => return txid,
                Err(err) => {
                    tracing::warn!("broadcast refused: {err:#}");
                    tokio::time::sleep(Duration::from_secs(5)).await;
                }
            }
        }
    })
    .await
    .expect("the exit spend was never broadcast");
    tracing::info!(%txid, "claimed unrolled VHTLC on-chain");
}

/// The sender exits unilaterally with the receiver's co-signature through `unilateral_refund`.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_exit_v2_unilateral_refund() {
    init_tracing();

    let f = Fixture::set_up(Setup {
        external_receiver: true,
        ..Setup::v2(unexpired_refund_locktime())
    })
    .await;
    f.unroll(&f.carol, &f.carol_wallet).await;

    let receiver = KeypairSigner(f.receiver_kp.unwrap());
    let (tx, prevouts) = f
        .exit_tx(
            &f.carol,
            ContractSpendOptions {
                cosigner: Some(&receiver),
                path: Some(&SpendPathKind::VhtlcUnilateralRefund),
                ..Default::default()
            },
        )
        .await;
    verify_consensus(&tx, &prevouts);
}

/// The sender exits alone through `unilateral_refund_without_receiver`.
#[tokio::test]
#[ignore]
pub async fn e2e_contract_exit_v2_unilateral_refund_without_receiver() {
    init_tracing();

    let f = Fixture::set_up(Setup::v2(unexpired_refund_locktime())).await;
    f.unroll(&f.carol, &f.carol_wallet).await;

    let (tx, prevouts) = f.exit_tx(&f.carol, ContractSpendOptions::default()).await;
    verify_consensus(&tx, &prevouts);
}

/// Signs with a keypair the spending wallet does not hold.
struct KeypairSigner(Keypair);

impl ExternalSigner for KeypairSigner {
    fn public_keys(&self) -> Vec<XOnlyPublicKey> {
        vec![self.0.x_only_public_key().0]
    }

    fn sign(
        &self,
        pk: &XOnlyPublicKey,
        msg: &secp256k1::Message,
    ) -> Result<schnorr::Signature, ark_client::Error> {
        assert_eq!(*pk, self.0.x_only_public_key().0);
        Ok(Secp256k1::new().sign_schnorr_no_aux_rand(msg, &self.0))
    }
}

struct Setup {
    refund_locktime: u32,
    v1: bool,
    non_interactive: bool,
    /// The receiver key lives outside bob's wallet, so only a co-signer can sign for it.
    external_receiver: bool,
}

impl Setup {
    fn v2(refund_locktime: u32) -> Self {
        Self {
            refund_locktime,
            v1: false,
            non_interactive: false,
            external_receiver: false,
        }
    }
}

/// A VHTLC funded by alice, with carol's wallet key as the sender and bob's (or an external key)
/// as the receiver. Both bob and carol track the contract.
struct Fixture {
    regtest: Arc<Regtest>,
    bob: Client<Regtest, Wallet>,
    bob_wallet: Arc<Wallet>,
    carol: Client<Regtest, Wallet>,
    carol_wallet: Arc<Wallet>,
    grpc_client: ark_grpc::Client,
    introspector: IntrospectorClient,
    dust: Amount,
    outpoint: VirtualTxOutPoint,
    receiver_kp: Option<Keypair>,
    preimage: [u8; 32],
    sender_payout: ArkAddress,
    receiver_payout: ArkAddress,
}

impl Fixture {
    async fn set_up(setup: Setup) -> Self {
        let regtest = Arc::new(Regtest::new());
        let secp = Secp256k1::new();
        let mut rng = thread_rng();

        let (alice, _) = set_up_client("alice".to_string(), regtest.clone(), secp.clone()).await;
        let (bob, bob_wallet) =
            set_up_client("bob".to_string(), regtest.clone(), secp.clone()).await;
        let (carol, carol_wallet) =
            set_up_client("carol".to_string(), regtest.clone(), secp.clone()).await;

        let mut grpc_client = ark_grpc::Client::new("http://localhost:7070".to_string());
        grpc_client.connect().await.unwrap();
        let server_info = grpc_client.get_info().await.unwrap();

        let introspector_url = std::env::var("INTROSPECTOR_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:7073".to_string());
        let introspector = IntrospectorClient::new(introspector_url);

        let alice_boarding_address = alice.get_boarding_address().await.unwrap();
        regtest
            .faucet_fund(&alice_boarding_address, Amount::ONE_BTC)
            .await;
        alice.settle_all(&mut rng).await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;

        // The payout addresses are separate from the contract keys, so the payouts are easy to
        // tell apart from the VHTLC.
        let (sender_payout, sender_vtxo) = carol.get_offchain_address().await.unwrap();
        let (receiver_payout, receiver_vtxo) = bob.get_offchain_address().await.unwrap();

        let (receiver, receiver_kp) = if setup.external_receiver {
            let kp = Keypair::new(&secp, &mut rng);
            (kp.x_only_public_key().0, Some(kp))
        } else {
            (receiver_vtxo.owner_pk(), None)
        };

        let preimage: [u8; 32] = rng.r#gen();
        let preimage_hash =
            ripemd160::Hash::from_byte_array(hash160::Hash::hash(&preimage).to_byte_array());

        let options = VhtlcOptions {
            sender: sender_vtxo.owner_pk(),
            receiver,
            server: server_info.signer_pk.x_only_public_key().0,
            preimage_hash,
            refund_locktime: setup.refund_locktime,
            unilateral_claim_delay: server_info.unilateral_exit_delay,
            unilateral_refund_delay: server_info.unilateral_exit_delay,
            unilateral_refund_without_receiver_delay: server_info.unilateral_exit_delay,
        };

        let vhtlc = if setup.v1 {
            let vhtlc = VhtlcScript::new(options.clone(), server_info.network).unwrap();
            for client in [&bob, &carol] {
                client
                    .add_contract(VhtlcContract {
                        options: options.clone(),
                    })
                    .unwrap();
            }
            vhtlc
        } else {
            let non_interactive = if setup.non_interactive {
                Some(NonInteractiveParameters {
                    emulator_pubkey: introspector.get_info().await.unwrap().signer_xonly(),
                    receiver_pk_script: receiver_payout.to_p2tr_script_pubkey(),
                    sender_pk_script: sender_payout.to_p2tr_script_pubkey(),
                    legacy_pre_timelocked_refund: false,
                })
            } else {
                None
            };
            let vhtlc = VhtlcScript::new_v2(
                options.clone(),
                non_interactive.clone(),
                server_info.network,
            )
            .unwrap();
            for client in [&bob, &carol] {
                client
                    .add_contract(VhtlcV2Contract {
                        options: options.clone(),
                        non_interactive: non_interactive.clone(),
                    })
                    .unwrap();
            }
            vhtlc
        };

        let funding_txid = alice
            .send(vec![SendReceiver::bitcoin(vhtlc.address(), VHTLC_AMOUNT)])
            .await
            .unwrap();
        tracing::info!(%funding_txid, address = %vhtlc.address(), "funded VHTLC");

        let outpoint = wait_for_vtxo(&grpc_client, vhtlc.address(), server_info.dust).await;

        // Neither wallet counts the VHTLC as its own balance.
        wait_until_balance!(&bob, pre_confirmed: Amount::ZERO);
        wait_until_balance!(&carol, pre_confirmed: Amount::ZERO);

        Self {
            regtest,
            bob,
            bob_wallet,
            carol,
            carol_wallet,
            grpc_client,
            introspector,
            dust: server_info.dust,
            outpoint,
            receiver_kp,
            preimage,
            sender_payout,
            receiver_payout,
        }
    }

    /// Put the VHTLC on-chain, fee-bumping through `client`'s on-chain wallet.
    async fn unroll(&self, client: &Client<Regtest, Wallet>, wallet: &Wallet) {
        let onchain = client.get_onchain_address().unwrap();
        for _ in 0..3 {
            self.regtest
                .faucet_fund(&onchain, Amount::from_sat(100_000))
                .await;
        }
        self.regtest.mine(1).await;
        wallet.sync().await.unwrap();

        let branches = client
            .build_contract_exit_trees(&[self.outpoint.outpoint])
            .await
            .unwrap();
        assert!(!branches.is_empty());

        for branch in &branches {
            while let Some(txid) = client
                .broadcast_next_unilateral_exit_node(branch)
                .await
                .unwrap()
            {
                tracing::info!(%txid, "broadcast exit tree node");
                // The next node spends this one's P2A output, so it needs a confirmation.
                self.regtest.mine(1).await;
                wallet.sync().await.unwrap();
            }
        }
        self.regtest.mine(1).await;

        // Once arkd sees it on-chain, an unrolled VHTLC can no longer be spent offchain.
        tokio::time::timeout(Duration::from_secs(30), async {
            while !self.vhtlc_vtxo().await.is_unrolled {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
        .await
        .expect("arkd never marked the VHTLC as unrolled");
        let err = client
            .send_contract_vtxos(
                &[self.outpoint.outpoint],
                self.sender_payout,
                ContractSpendOptions {
                    preimage: Some(self.preimage),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("is unrolled"), "{err:#}");
    }

    /// Build the on-chain exit transaction as if the exit delay had passed.
    async fn exit_tx(
        &self,
        client: &Client<Regtest, Wallet>,
        options: ContractSpendOptions<'_>,
    ) -> (Transaction, Vec<TxOut>) {
        let delay = client.server_info().await.unwrap().unilateral_exit_delay;
        match delay.to_relative_lock_time().unwrap() {
            relative::LockTime::Blocks(blocks) => {
                self.regtest
                    .set_outpoint_block_height_offset(u64::from(blocks.value()));
            }
            relative::LockTime::Time(time) => {
                self.regtest
                    .set_outpoint_blocktime_offset(u64::from(time.value()) * 512);
            }
        }

        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                match client
                    .create_contract_on_chain_transaction(
                        &[self.outpoint.outpoint],
                        onchain_address(),
                        EXIT_FEE,
                        options,
                    )
                    .await
                {
                    Ok(result) => return result,
                    Err(err) => {
                        tracing::debug!("waiting for the unrolled VHTLC: {err:#}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        })
        .await
        .expect("the unrolled VHTLC never became spendable")
    }

    async fn vhtlc_vtxo(&self) -> VirtualTxOutPoint {
        self.grpc_client
            .list_vtxos(GetVtxosRequest::new_for_outpoints(&[self
                .outpoint
                .outpoint]))
            .await
            .unwrap()
            .vtxos
            .into_iter()
            .find(|vtxo| vtxo.outpoint == self.outpoint.outpoint)
            .unwrap()
    }

    /// Wait for the drained VHTLC to land on `payout`, and check the VHTLC is spent.
    async fn wait_for_payout(&self, payout: ArkAddress) {
        let paid = wait_for_vtxo(&self.grpc_client, payout, self.dust).await;
        assert_eq!(paid.amount, VHTLC_AMOUNT);
        assert!(self.vhtlc_vtxo().await.is_spent);
    }
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

const EXIT_FEE: Amount = Amount::from_sat(1_000);

fn onchain_address() -> bitcoin::Address {
    bitcoin::Address::<NetworkUnchecked>::from_str("bcrt1q8df4sx3hz63tq44ve3q6tr4qz0q30usk5sntpt")
        .unwrap()
        .require_network(bitcoin::Network::Regtest)
        .unwrap()
}

/// Run every input through libbitcoinconsensus, which checks the witness against the leaf,
/// including the CSV against the input's sequence.
fn verify_consensus(tx: &Transaction, prevouts: &[TxOut]) {
    assert_eq!(tx.input.len(), prevouts.len());
    let spent_outputs = prevouts
        .iter()
        .map(|o| bitcoinconsensus::Utxo {
            script_pubkey: o.script_pubkey.as_bytes().as_ptr(),
            script_pubkey_len: o.script_pubkey.len() as u32,
            value: o.value.to_sat() as i64,
        })
        .collect::<Vec<_>>();

    for (i, prevout) in prevouts.iter().enumerate() {
        bitcoinconsensus::verify(
            prevout.script_pubkey.as_bytes(),
            prevout.value.to_sat(),
            bitcoin::consensus::serialize(tx).as_slice(),
            Some(&spent_outputs),
            i,
        )
        .expect("valid input");
    }
    assert_eq!(tx.output[0].value, VHTLC_AMOUNT - EXIT_FEE);
}

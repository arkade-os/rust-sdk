#![allow(clippy::unwrap_used)]

use crate::msig_output::MsigOutputScript;
use crate::msig_output::MsigOutputTaprootOptions;
use ark_core::batch;
use ark_core::intent;
use ark_core::server::GetVtxosRequest;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::TxOut;
use bitcoin::XOnlyPublicKey;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use common::Nigiri;
use common::init_tracing;
use common::set_up_client;
use rand::thread_rng;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn e2e_multisig_delegate() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    // Set up Alice and Bob
    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let (bob, _) = set_up_client("bob".to_string(), nigiri.clone(), secp.clone()).await;

    // Generate Bob's delegate cosigner keypair (ephemeral)
    let bob_delegate_cosigner_kp = Keypair::new(&secp, &mut rng);
    let bob_delegate_cosigner_pk = bob_delegate_cosigner_kp.public_key();

    let alice_boarding_address = alice.get_boarding_address().unwrap();
    let alice_fund_amount = Amount::ONE_BTC;

    let alice_boarding_outpoint = nigiri
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;

    tracing::debug!(?alice_boarding_outpoint, "Funded Alice's boarding output");

    alice.settle(&mut rng, false).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_starting_offchain_balance = alice.offchain_balance().await.unwrap();
    let alice_starting_vtxos = alice.list_vtxos(false).await.unwrap();

    tracing::info!(
        ?alice_starting_offchain_balance,
        ?alice_starting_vtxos,
        "Alice got confirmed VTXO"
    );

    assert_eq!(
        alice_starting_offchain_balance.confirmed(),
        alice_fund_amount
    );
    assert_eq!(alice_starting_offchain_balance.pending(), Amount::ZERO);

    let alice_msig_output_kp = Keypair::new(&secp, &mut rng);
    let alice_msig_output_pk = alice_msig_output_kp.public_key();

    let bob_msig_output_kp = Keypair::new(&secp, &mut rng);
    let bob_msig_output_pk = bob_msig_output_kp.public_key();

    let msig_output = MsigOutputScript::new(
        MsigOutputTaprootOptions {
            alice_pk: alice_msig_output_pk.into(),
            bob_pk: bob_msig_output_pk.into(),
            server_pk: alice.server_info.signer_pk.into(),
            unilateral_exit_delay: alice.server_info.unilateral_exit_delay,
        },
        alice.server_info.network,
    )
    .unwrap();

    let msig_output_amount = Amount::from_sat(10_000);

    let txid = alice
        .send_vtxo(msig_output.address(), msig_output_amount)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_offchain_balance_after_msig_fund = alice.offchain_balance().await.unwrap();
    let alice_vtxos_after_msig_fund = alice.list_vtxos(false).await.unwrap();

    tracing::info!(
        ?alice_offchain_balance_after_msig_fund,
        ?alice_vtxos_after_msig_fund,
        %txid,
        "Alice funded msig VTXO"
    );

    assert_eq!(
        alice_offchain_balance_after_msig_fund.total(),
        alice_fund_amount - msig_output_amount
    );

    let msig_output_as_input = intent::Input::new(
        OutPoint { txid, vout: 0 },
        // TODO: This should be modelled in the output type. I think it's supposed to be the
        // highest sequence number of all leaves, but I'm not sure.
        alice.server_info.unilateral_exit_delay,
        TxOut {
            value: msig_output_amount,
            script_pubkey: msig_output.script_pubkey(),
        },
        msig_output.tapscripts(),
        msig_output.bob_alice_spend_info(),
        false,
    );

    let mut delegate = batch::prepare_delegate_psbts(
        vec![msig_output_as_input],
        // We are just refreshing the VTXO, not sending to a different address.
        vec![intent::Output::Offchain(TxOut {
            value: msig_output_amount,
            script_pubkey: msig_output.script_pubkey(),
        })],
        bob_delegate_cosigner_pk,
        &alice.server_info.forfeit_address,
        alice.server_info.dust,
    )
    .unwrap();

    // Sign with Alice's key.
    let sign_fn = |_: &mut psbt::Input,
                   msg: secp256k1::Message|
     -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
        let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &alice_msig_output_kp);

        Ok((sig, alice_msig_output_pk.into()))
    };

    batch::sign_delegate_psbts(
        sign_fn,
        &mut delegate.intent.proof,
        &mut delegate.forfeit_psbts,
    )
    .unwrap();

    // Sign with Bob's key.
    let sign_fn = |_: &mut psbt::Input,
                   msg: secp256k1::Message|
     -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
        let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &bob_msig_output_kp);

        Ok((sig, bob_msig_output_pk.into()))
    };

    batch::sign_delegate_psbts(
        sign_fn,
        &mut delegate.intent.proof,
        &mut delegate.forfeit_psbts,
    )
    .unwrap();

    tracing::info!(
        delegate_cosigner_pk = %bob_delegate_cosigner_pk,
        partial_forfeit_txs_count = delegate.forfeit_psbts.len(),
        "Delegate PSBTs generated"
    );

    let commitment_txid = bob
        .settle_delegate(&mut rng, delegate, bob_delegate_cosigner_kp)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    tracing::info!(
        %commitment_txid,
        "Bob settled msig VTXO using delegate system"
    );

    let vtxo_list = alice
        .network_client()
        .list_vtxos(GetVtxosRequest::new_for_addresses(&[msig_output.address()]))
        .await
        .unwrap();

    assert_eq!(vtxo_list.spendable().len(), 1);
    assert_eq!(vtxo_list.spent().len(), 1);

    let settled_msig_outpoint = &vtxo_list.spent()[0];

    assert!(settled_msig_outpoint.is_spent);
    assert!(settled_msig_outpoint.is_preconfirmed);
    assert!(!settled_msig_outpoint.is_swept);
    assert!(!settled_msig_outpoint.is_unrolled);
    assert!(!settled_msig_outpoint.is_recoverable());
    assert_eq!(settled_msig_outpoint.settled_by, Some(commitment_txid));

    let new_msig_outpoint = &vtxo_list.spendable()[0];

    assert!(!new_msig_outpoint.is_spent);
    assert!(!new_msig_outpoint.is_preconfirmed);
    assert!(!new_msig_outpoint.is_swept);
    assert!(!new_msig_outpoint.is_unrolled);
    assert!(!new_msig_outpoint.is_recoverable());
    assert!(
        new_msig_outpoint
            .commitment_txids
            .contains(&commitment_txid)
    );
}

mod msig_output {
    use anyhow::Context;
    use anyhow::Result;
    use anyhow::anyhow;
    use anyhow::bail;
    use ark_core::ArkAddress;
    use ark_core::UNSPENDABLE_KEY;
    use bitcoin::Network;
    use bitcoin::PublicKey;
    use bitcoin::ScriptBuf;
    use bitcoin::XOnlyPublicKey;
    use bitcoin::opcodes::all::*;
    use bitcoin::taproot;
    use bitcoin::taproot::LeafVersion;
    use bitcoin::taproot::TaprootBuilder;
    use bitcoin::taproot::TaprootSpendInfo;
    use serde::Deserialize;
    use serde::Serialize;
    use std::str::FromStr;

    /// Represents a script with its weight for taproot tree construction.
    #[derive(Debug, Clone)]
    struct TaprootScriptItem {
        script: ScriptBuf,
        weight: u32,
    }

    /// Internal tree node for building the taproot tree structure.
    #[derive(Debug, Clone)]
    enum TaprootTreeNode {
        Leaf {
            script: ScriptBuf,
            weight: u32,
        },
        Branch {
            left: Box<TaprootTreeNode>,
            right: Box<TaprootTreeNode>,
            weight: u32,
        },
    }

    #[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
    pub struct MsigOutputTaprootOptions {
        pub alice_pk: XOnlyPublicKey,
        pub bob_pk: XOnlyPublicKey,
        /// The Arkade server's public key.
        pub server_pk: XOnlyPublicKey,
        pub unilateral_exit_delay: bitcoin::Sequence,
    }

    impl MsigOutputTaprootOptions {
        fn build_taproot(&self) -> Result<TaprootSpendInfo> {
            let internal_pubkey =
                PublicKey::from_str(UNSPENDABLE_KEY).context("invalid unspendable key")?;
            let internal_key = XOnlyPublicKey::from(internal_pubkey);

            let scripts = vec![
                TaprootScriptItem {
                    script: self.bob_alice_script(),
                    weight: 1,
                },
                TaprootScriptItem {
                    script: self.unilateral_bob_alice_script(),
                    weight: 1,
                },
            ];

            // Build the tree using the weight-based algorithm
            let tree = Self::taproot_list_to_tree(scripts)?;

            // Create TaprootBuilder and add the tree
            let builder = TaprootBuilder::new();
            let builder = Self::add_tree_to_builder(builder, &tree, 0)?;

            let secp = bitcoin::secp256k1::Secp256k1::new();
            let taproot_spend_info = builder
                .finalize(&secp, internal_key)
                .map_err(|_| anyhow!("Failed to finalize taproot"))?;

            Ok(taproot_spend_info)
        }

        pub fn bob_alice_script(&self) -> ScriptBuf {
            ScriptBuf::builder()
                .push_x_only_key(&self.bob_pk)
                .push_opcode(OP_CHECKSIGVERIFY)
                .push_x_only_key(&self.alice_pk)
                .push_opcode(OP_CHECKSIGVERIFY)
                .push_x_only_key(&self.server_pk)
                .push_opcode(OP_CHECKSIG)
                .into_script()
        }

        pub fn unilateral_bob_alice_script(&self) -> ScriptBuf {
            ScriptBuf::builder()
                .push_int(self.unilateral_exit_delay.to_consensus_u32() as i64)
                .push_opcode(OP_CSV)
                .push_opcode(OP_DROP)
                .push_x_only_key(&self.bob_pk)
                .push_opcode(OP_CHECKSIGVERIFY)
                .push_x_only_key(&self.alice_pk)
                .push_opcode(OP_CHECKSIG)
                .into_script()
        }

        /// Build a balanced taproot tree from a list of scripts with weights
        /// Following the TypeScript algorithm from scure-btc-signer
        fn taproot_list_to_tree(scripts: Vec<TaprootScriptItem>) -> Result<TaprootTreeNode> {
            if scripts.is_empty() {
                bail!("Empty script list");
            }

            // Clone input and convert to nodes
            let mut lst: Vec<TaprootTreeNode> = scripts
                .into_iter()
                .map(|item| TaprootTreeNode::Leaf {
                    script: item.script,
                    weight: item.weight,
                })
                .collect();

            // Build tree by combining nodes with smallest weights
            while lst.len() >= 2 {
                // Sort: elements with smallest weight are at the end of queue
                lst.sort_by(|a, b| {
                    let weight_a = match a {
                        TaprootTreeNode::Leaf { weight, .. } => *weight,
                        TaprootTreeNode::Branch { weight, .. } => *weight,
                    };
                    let weight_b = match b {
                        TaprootTreeNode::Leaf { weight, .. } => *weight,
                        TaprootTreeNode::Branch { weight, .. } => *weight,
                    };
                    // Reverse comparison to put smallest at end
                    weight_b.cmp(&weight_a)
                });

                // Pop the two smallest weight nodes
                let b = lst.pop().expect("an element");
                let a = lst.pop().expect("an element");

                // Calculate combined weight
                let weight_a = match &a {
                    TaprootTreeNode::Leaf { weight, .. } => *weight,
                    TaprootTreeNode::Branch { weight, .. } => *weight,
                };
                let weight_b = match &b {
                    TaprootTreeNode::Leaf { weight, .. } => *weight,
                    TaprootTreeNode::Branch { weight, .. } => *weight,
                };

                // Create branch with combined weight
                lst.push(TaprootTreeNode::Branch {
                    weight: weight_a + weight_b,
                    left: Box::new(a),
                    right: Box::new(b),
                });
            }

            // Return the root node
            Ok(lst.into_iter().next().expect("root node"))
        }

        /// Recursively add tree nodes to TaprootBuilder
        fn add_tree_to_builder(
            builder: TaprootBuilder,
            node: &TaprootTreeNode,
            depth: u8,
        ) -> Result<TaprootBuilder> {
            match node {
                TaprootTreeNode::Leaf { script, .. } => builder
                    .add_leaf(depth, script.clone())
                    .map_err(|_| anyhow!("Failed to add leaf")),
                TaprootTreeNode::Branch { left, right, .. } => {
                    let builder = Self::add_tree_to_builder(builder, left, depth + 1)?;
                    Self::add_tree_to_builder(builder, right, depth + 1)
                }
            }
        }
    }

    pub struct MsigOutputScript {
        options: MsigOutputTaprootOptions,
        taproot_spend_info: TaprootSpendInfo,
        network: Network,
    }

    impl MsigOutputScript {
        pub fn new(options: MsigOutputTaprootOptions, network: Network) -> Result<Self> {
            let taproot_spend_info = options.build_taproot()?;

            Ok(Self {
                options,
                taproot_spend_info,
                network,
            })
        }

        pub fn taproot_spend_info(&self) -> &TaprootSpendInfo {
            &self.taproot_spend_info
        }

        pub fn script_pubkey(&self) -> ScriptBuf {
            ScriptBuf::builder()
                .push_opcode(OP_PUSHNUM_1)
                .push_slice(self.taproot_spend_info.output_key().serialize())
                .into_script()
        }

        pub fn address(&self) -> ArkAddress {
            ArkAddress::new(
                self.network,
                self.options.server_pk,
                self.taproot_spend_info().output_key(),
            )
        }

        pub fn bob_alice_script(&self) -> ScriptBuf {
            self.options.bob_alice_script()
        }

        pub fn bob_alice_spend_info(&self) -> (ScriptBuf, taproot::ControlBlock) {
            let control_block = self
                .taproot_spend_info
                .control_block(&(self.bob_alice_script(), LeafVersion::TapScript))
                .unwrap();

            (self.bob_alice_script(), control_block)
        }

        pub fn unilateral_bob_alice_script(&self) -> ScriptBuf {
            self.options.unilateral_bob_alice_script()
        }

        pub fn tapscripts(&self) -> Vec<ScriptBuf> {
            vec![self.bob_alice_script(), self.unilateral_bob_alice_script()]
        }
    }
}

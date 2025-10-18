#![allow(clippy::unwrap_used)]

use crate::escrow::EscrowTaprootOptions;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::thread_rng;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn e2e_shared_delegation() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());

    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    // Set up Alice
    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;

    // Generate cosigner keypair for Alice (the settler)
    let alice_cosigner_kp = Keypair::new(&secp, &mut rng);
    let alice_cosigner_pk = alice_cosigner_kp.public_key();

    tracing::info!("Step 1: Fund Alice's boarding output");
    let alice_boarding_address = alice.get_boarding_address().unwrap();
    let alice_fund_amount = Amount::ONE_BTC;

    let alice_boarding_outpoint = nigiri
        .faucet_fund(&alice_boarding_address, alice_fund_amount)
        .await;

    tracing::info!(?alice_boarding_outpoint, "Funded Alice's boarding output");

    tracing::info!("Step 2: Alice settles to get a VTXO");
    alice.settle(&mut rng, false).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_offchain_balance = alice.offchain_balance().await.unwrap();
    let vtxos_before = alice.list_vtxos(false).await.unwrap();

    tracing::info!(
        ?alice_offchain_balance,
        vtxos = ?vtxos_before,
        "Alice settled - has confirmed VTXO"
    );

    assert_eq!(alice_offchain_balance.confirmed(), alice_fund_amount);
    assert_eq!(alice_offchain_balance.pending(), Amount::ZERO);

    // Wait for the server's timelock before the VTXO can be used for intent registration
    tracing::info!("Waiting for VTXO timelock (3 seconds)...");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    tracing::info!("Step 3: Alice prepares shared delegation with unsigned PSBTs");

    let escrow_script = escrow::EscrowScript::new(
        EscrowTaprootOptions {
            bob_pk: todo!(),
            claire_pk: todo!(),
            alice_pk: todo!(),
            server_pk: alice.server_info.signer_pk.into(),
            unilateral_exit_delay: alice.server_info.unilateral_exit_delay,
        },
        bitcoin::Network::Regtest,
    )
    .unwrap();

    alice
        .send_vtxo(escrow_script.address(), alice_fund_amount)
        .await
        .unwrap();

    // Alice prepares the shared delegation with unsigned PSBTs
    // This automatically fetches her VTXOs and creates outputs to her own address
    // She specifies her own cosigner key as the "settler"
    let mut shared_delegate = alice
        .prepare_shared_delegation_auto(alice_cosigner_pk, false)
        .await
        .unwrap();

    tracing::info!(
        settler_cosigner_pk = %alice_cosigner_pk,
        vtxo_inputs_count = shared_delegate.delegation_psbts.vtxo_inputs.len(),
        outputs_count = shared_delegate.delegation_psbts.outputs.len(),
        "Alice prepared shared delegation with unsigned PSBTs"
    );

    // Verify the delegation was prepared correctly
    assert_eq!(shared_delegate.settler_cosigner_pk, alice_cosigner_pk);
    assert!(!shared_delegate.delegation_psbts.vtxo_inputs.is_empty());

    tracing::info!("Step 4: Sign the shared delegation PSBTs");

    // In a real shared VTXO scenario (2-of-2 multisig), both Alice and Bob would sign here.
    // Since we don't have actual shared VTXOs in this test (the VTXO belongs to Alice),
    // we just demonstrate Alice signing. The sign_shared_delegation_psbts() function
    // is accumulative and can be called by multiple parties in sequence.

    // NOTE: In production with actual shared VTXOs:
    // 1. Bob would sign first: bob.sign_shared_delegation_psbts(&mut shared_delegate)
    // 2. Alice would sign second: alice.sign_shared_delegation_psbts(&mut shared_delegate)

    alice
        .sign_shared_delegation_psbts(&mut shared_delegate)
        .unwrap();

    tracing::info!("Completed signing the shared delegation PSBTs");

    tracing::info!("Step 5: Alice performs settlement using the fully-signed PSBTs");

    let commitment_txid = alice
        .settle_shared_delegation(&mut rng, shared_delegate, alice_cosigner_kp)
        .await
        .unwrap();

    tracing::info!(
        %commitment_txid,
        "Alice successfully settled the shared VTXO using shared delegation"
    );

    // Wait for settlement to complete
    tracing::info!("Waiting for settlement to complete...");
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    tracing::info!("Step 6: Verify Alice's VTXO has been settled");
    let alice_offchain_balance_after = alice.offchain_balance().await.unwrap();
    let vtxos_after = alice.list_vtxos(false).await.unwrap();

    tracing::info!(
        ?alice_offchain_balance_after,
        vtxos = ?vtxos_after,
        "Alice's balance after shared delegation settlement"
    );

    // Verify the original VTXO is spent
    let pre_settlement_outpoint = vtxos_before.spendable[0].0[0].outpoint;
    let settled_outpoint = vtxos_after.spent[0].0[0].outpoint;

    assert_eq!(
        pre_settlement_outpoint, settled_outpoint,
        "original VTXO should be spent"
    );

    let old_vtxo_settlement_txid = vtxos_after.spent[0].0[0].settled_by.unwrap();

    assert_eq!(
        old_vtxo_settlement_txid, commitment_txid,
        "VTXO should be settled by the commitment transaction"
    );

    tracing::info!("Shared delegation test completed successfully!");
}

mod escrow {
    use anyhow::anyhow;
    use anyhow::bail;
    use anyhow::Result;
    use ark_core::ArkAddress;
    use ark_core::UNSPENDABLE_KEY;
    use bitcoin::opcodes::all::*;
    use bitcoin::taproot::TaprootBuilder;
    use bitcoin::taproot::TaprootSpendInfo;
    use bitcoin::Network;
    use bitcoin::PublicKey;
    use bitcoin::ScriptBuf;
    use bitcoin::XOnlyPublicKey;
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
    pub struct EscrowTaprootOptions {
        pub bob_pk: XOnlyPublicKey,
        pub claire_pk: XOnlyPublicKey,
        pub alice_pk: XOnlyPublicKey,
        /// The Arkade server's public key.
        pub server_pk: XOnlyPublicKey,
        pub unilateral_exit_delay: bitcoin::Sequence,
    }

    impl EscrowTaprootOptions {
        fn build_taproot(&self) -> Result<TaprootSpendInfo> {
            let internal_pubkey = PublicKey::from_str(UNSPENDABLE_KEY).expect("key");
            let internal_key = XOnlyPublicKey::from(internal_pubkey);

            // Create script list with weights
            // Lower weight = more likely to be used = shallower in tree
            let scripts = vec![
                TaprootScriptItem {
                    script: self.bob_alice_script(),
                    weight: 1,
                },
                TaprootScriptItem {
                    script: self.claire_alice_script(),
                    weight: 1,
                },
                TaprootScriptItem {
                    script: self.bob_claire_script(),
                    weight: 1,
                },
                TaprootScriptItem {
                    script: self.unilateral_bob_alice_script(),
                    weight: 1,
                },
                TaprootScriptItem {
                    script: self.unilateral_claire_alice_script(),
                    weight: 1,
                },
                TaprootScriptItem {
                    script: self.unilateral_bob_claire_script(),
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

        pub fn claire_alice_script(&self) -> ScriptBuf {
            ScriptBuf::builder()
                .push_x_only_key(&self.claire_pk)
                .push_opcode(OP_CHECKSIGVERIFY)
                .push_x_only_key(&self.alice_pk)
                .push_opcode(OP_CHECKSIGVERIFY)
                .push_x_only_key(&self.server_pk)
                .push_opcode(OP_CHECKSIG)
                .into_script()
        }

        pub fn bob_claire_script(&self) -> ScriptBuf {
            ScriptBuf::builder()
                .push_x_only_key(&self.bob_pk)
                .push_opcode(OP_CHECKSIGVERIFY)
                .push_x_only_key(&self.claire_pk)
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

        pub fn unilateral_claire_alice_script(&self) -> ScriptBuf {
            ScriptBuf::builder()
                .push_int(self.unilateral_exit_delay.to_consensus_u32() as i64)
                .push_opcode(OP_CSV)
                .push_opcode(OP_DROP)
                .push_x_only_key(&self.claire_pk)
                .push_opcode(OP_CHECKSIGVERIFY)
                .push_x_only_key(&self.alice_pk)
                .push_opcode(OP_CHECKSIG)
                .into_script()
        }

        pub fn unilateral_bob_claire_script(&self) -> ScriptBuf {
            ScriptBuf::builder()
                .push_int(self.unilateral_exit_delay.to_consensus_u32() as i64)
                .push_opcode(OP_CSV)
                .push_opcode(OP_DROP)
                .push_x_only_key(&self.bob_pk)
                .push_opcode(OP_CHECKSIGVERIFY)
                .push_x_only_key(&self.claire_pk)
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

    pub struct EscrowScript {
        options: EscrowTaprootOptions,
        taproot_spend_info: TaprootSpendInfo,
        network: Network,
    }

    impl EscrowScript {
        pub fn new(options: EscrowTaprootOptions, network: Network) -> Result<Self> {
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

        pub fn claire_alice_script(&self) -> ScriptBuf {
            self.options.claire_alice_script()
        }

        pub fn bob_claire_script(&self) -> ScriptBuf {
            self.options.bob_claire_script()
        }

        pub fn unilateral_bob_alice_script(&self) -> ScriptBuf {
            self.options.unilateral_bob_alice_script()
        }

        pub fn unilateral_claire_alice_script(&self) -> ScriptBuf {
            self.options.unilateral_claire_alice_script()
        }

        pub fn unilateral_bob_claire_script(&self) -> ScriptBuf {
            self.options.unilateral_bob_claire_script()
        }

        pub fn tapscripts(self) -> Vec<ScriptBuf> {
            vec![
                self.bob_alice_script(),
                self.claire_alice_script(),
                self.bob_claire_script(),
                self.unilateral_bob_alice_script(),
                self.unilateral_claire_alice_script(),
                self.unilateral_bob_claire_script(),
            ]
        }
    }
}

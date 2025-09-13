//! Virtual Hash Time Lock Contract (VHTLC) implementation for Ark Lightning Swaps
//!
//! This module implements VHTLC scripts that enable atomic swaps and conditional
//! payments in the Ark protocol. The VHTLC provides multiple spending paths with
//! different conditions and participants.

use ark_core::ArkAddress;
use ark_core::UNSPENDABLE_KEY;
use bitcoin::opcodes::all::*;
use bitcoin::taproot::TaprootBuilder;
use bitcoin::taproot::TaprootSpendInfo;
use bitcoin::Network;
use bitcoin::PublicKey;
use bitcoin::ScriptBuf;
use bitcoin::Sequence;
use bitcoin::XOnlyPublicKey;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum VhtlcError {
    #[error("Invalid preimage hash length: expected 20 bytes, got {0}")]
    InvalidPreimageHashLength(usize),
    #[error("Invalid public key length: expected 32 bytes, got {0}")]
    InvalidPublicKeyLength(usize),
    #[error("Invalid locktime: {0}")]
    InvalidLocktime(String),
    #[error("Invalid delay: {0}")]
    InvalidDelay(String),
    #[error("Taproot construction failed: {0}")]
    TaprootError(String),
}

/// Represents a script with its weight for taproot tree construction
#[derive(Debug, Clone)]
struct TaprootScriptItem {
    script: ScriptBuf,
    weight: u32,
}

/// Internal tree node for building the taproot tree structure
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

/// Options for creating a VHTLC (Virtual Hash Time Lock Contract)
///
/// This structure contains all the necessary parameters to construct a VHTLC,
/// including the public keys of participants and various timeout values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VhtlcOptions {
    pub sender: XOnlyPublicKey,
    pub receiver: XOnlyPublicKey,
    pub server: XOnlyPublicKey,
    pub preimage_hash: [u8; 20],
    pub refund_locktime: u32,
    pub unilateral_claim_delay: Sequence,
    pub unilateral_refund_delay: Sequence,
    pub unilateral_refund_without_receiver_delay: Sequence,
}

impl VhtlcOptions {
    pub fn validate(&self) -> Result<(), VhtlcError> {
        if self.refund_locktime == 0 {
            return Err(VhtlcError::InvalidLocktime(
                "Refund locktime must be greater than 0".to_string(),
            ));
        }

        if !self.unilateral_claim_delay.is_relative_lock_time()
            || self.unilateral_claim_delay.to_consensus_u32() == 0
        {
            return Err(VhtlcError::InvalidDelay(
                "Unilateral claim delay must be a valid non-zero CSV relative lock time"
                    .to_string(),
            ));
        }

        if !self.unilateral_refund_delay.is_relative_lock_time()
            || self.unilateral_refund_delay.to_consensus_u32() == 0
        {
            return Err(VhtlcError::InvalidDelay(
                "Unilateral refund delay must be a valid non-zero CSV relative lock time"
                    .to_string(),
            ));
        }

        if !self
            .unilateral_refund_without_receiver_delay
            .is_relative_lock_time()
            || self
                .unilateral_refund_without_receiver_delay
                .to_consensus_u32()
                == 0
        {
            return Err(VhtlcError::InvalidDelay(
                "Unilateral refund without receiver delay must be a valid non-zero CSV relative lock time"
                    .to_string(),
            ));
        }

        Ok(())
    }
}

/// VHTLC Script builder and manager
///
/// This struct creates and manages VHTLC scripts with six different spending paths:
/// 1. **Claim**: Receiver reveals preimage (collaborative with server)
/// 2. **Refund**: Collaborative refund (all three parties)
/// 3. **Refund without Receiver**: Sender refunds after locktime (with server)
/// 4. **Unilateral Claim**: Receiver claims after delay (no server needed)
/// 5. **Unilateral Refund**: Collaborative unilateral refund after delay
/// 6. **Unilateral Refund without Receiver**: Sender unilateral refund after both timeouts
pub struct VhtlcScript {
    options: VhtlcOptions,
    taproot_info: Option<TaprootSpendInfo>,
}

impl VhtlcScript {
    /// Creates a new VHTLC script with the given options
    ///
    /// This will validate the options and build the complete taproot tree
    /// with all spending paths.
    pub fn new(options: VhtlcOptions) -> Result<Self, VhtlcError> {
        options.validate()?;
        let mut script = Self {
            options,
            taproot_info: None,
        };
        script.build_taproot()?;
        Ok(script)
    }

    /// Creates the claim script where receiver reveals the preimage
    ///
    /// Requires: preimage hash verification + receiver signature + server signature
    pub fn claim_script(&self) -> ScriptBuf {
        ScriptBuf::builder()
            .push_opcode(OP_HASH160)
            .push_slice(&self.options.preimage_hash)
            .push_opcode(OP_EQUAL)
            .push_opcode(OP_VERIFY)
            .push_x_only_key(&self.options.receiver)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&self.options.server)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Creates the collaborative refund script
    ///
    /// Requires: sender + receiver + server signatures
    pub fn refund_script(&self) -> ScriptBuf {
        ScriptBuf::builder()
            .push_x_only_key(&self.options.sender)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&self.options.receiver)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&self.options.server)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Creates the refund script when receiver is unavailable
    ///
    /// Requires: CLTV timeout + sender + server signatures
    pub fn refund_without_receiver_script(&self) -> ScriptBuf {
        ScriptBuf::builder()
            .push_int(self.options.refund_locktime as i64)
            .push_opcode(OP_CLTV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.options.sender)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&self.options.server)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Creates the unilateral claim script (no server cooperation needed)
    ///
    /// Requires: preimage hash verification + CSV delay + receiver signature
    pub fn unilateral_claim_script(&self) -> ScriptBuf {
        let sequence = self.options.unilateral_claim_delay;
        ScriptBuf::builder()
            .push_opcode(OP_HASH160)
            .push_slice(&self.options.preimage_hash)
            .push_opcode(OP_EQUAL)
            .push_opcode(OP_VERIFY)
            .push_int(sequence.to_consensus_u32() as i64)
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.options.receiver)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Creates the unilateral refund script
    ///
    /// Requires: CSV delay + sender + receiver signatures
    pub fn unilateral_refund_script(&self) -> ScriptBuf {
        let sequence = self.options.unilateral_refund_delay;
        ScriptBuf::builder()
            .push_int(sequence.to_consensus_u32() as i64)
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.options.sender)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&self.options.receiver)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Creates the unilateral refund script when receiver is unavailable
    ///
    /// Requires: CSV delay + sender signature
    pub fn unilateral_refund_without_receiver_script(&self) -> ScriptBuf {
        let sequence = self.options.unilateral_refund_without_receiver_delay;
        ScriptBuf::builder()
            .push_int(sequence.to_consensus_u32() as i64)
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.options.sender)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Build a balanced taproot tree from a list of scripts with weights
    /// Following the TypeScript algorithm from scure-btc-signer
    fn taproot_list_to_tree(
        scripts: Vec<TaprootScriptItem>,
    ) -> Result<TaprootTreeNode, VhtlcError> {
        if scripts.is_empty() {
            return Err(VhtlcError::TaprootError("Empty script list".to_string()));
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
            let b = lst.pop().unwrap();
            let a = lst.pop().unwrap();

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
        Ok(lst.into_iter().next().unwrap())
    }

    /// Recursively add tree nodes to TaprootBuilder
    fn add_tree_to_builder(
        builder: TaprootBuilder,
        node: &TaprootTreeNode,
        depth: u8,
    ) -> Result<TaprootBuilder, VhtlcError> {
        match node {
            TaprootTreeNode::Leaf { script, .. } => builder
                .add_leaf(depth, script.clone())
                .map_err(|e| VhtlcError::TaprootError(format!("Failed to add leaf: {}", e))),
            TaprootTreeNode::Branch { left, right, .. } => {
                let builder = Self::add_tree_to_builder(builder, left, depth + 1)?;
                Self::add_tree_to_builder(builder, right, depth + 1)
            }
        }
    }

    fn build_taproot(&mut self) -> Result<(), VhtlcError> {
        let internal_pubkey = PublicKey::from_str(UNSPENDABLE_KEY).map_err(|e| {
            VhtlcError::TaprootError(format!("Failed to parse internal key: {}", e))
        })?;
        let internal_key = XOnlyPublicKey::from(internal_pubkey);

        // Create script list with weights
        // Lower weight = more likely to be used = shallower in tree
        let scripts = vec![
            TaprootScriptItem {
                script: self.claim_script(),
                weight: 1, // Most likely - collaborative claim
            },
            TaprootScriptItem {
                script: self.refund_script(),
                weight: 1, // Most likely - collaborative refund
            },
            TaprootScriptItem {
                script: self.refund_without_receiver_script(),
                weight: 1, // Less common
            },
            TaprootScriptItem {
                script: self.unilateral_claim_script(),
                weight: 1, // Less common
            },
            TaprootScriptItem {
                script: self.unilateral_refund_script(),
                weight: 1, // Least common
            },
            TaprootScriptItem {
                script: self.unilateral_refund_without_receiver_script(),
                weight: 1, // Least common
            },
        ];

        // Build the tree using the weight-based algorithm
        let tree = Self::taproot_list_to_tree(scripts)?;

        // Create TaprootBuilder and add the tree
        let builder = TaprootBuilder::new();
        let builder = Self::add_tree_to_builder(builder, &tree, 0)?;

        let secp = bitcoin::secp256k1::Secp256k1::new();
        let taproot_info = builder.finalize(&secp, internal_key).map_err(|e| {
            VhtlcError::TaprootError(format!("Failed to finalize taproot: {:?}", e))
        })?;

        self.taproot_info = Some(taproot_info);
        Ok(())
    }

    pub fn taproot_info(&self) -> Option<&TaprootSpendInfo> {
        self.taproot_info.as_ref()
    }

    pub fn script_pubkey(&self) -> Option<ScriptBuf> {
        self.taproot_info.as_ref().map(|info| {
            ScriptBuf::builder()
                .push_opcode(OP_PUSHNUM_1)
                .push_slice(info.output_key().serialize())
                .into_script()
        })
    }

    pub fn address(&self, network: Network, server: XOnlyPublicKey) -> Option<ArkAddress> {
        ArkAddress::new(network, server, self.taproot_info()?.output_key()).into()
    }

    pub fn get_script_map(&self) -> BTreeMap<String, ScriptBuf> {
        let mut map = BTreeMap::new();
        map.insert("claim".to_string(), self.claim_script());
        map.insert("refund".to_string(), self.refund_script());
        map.insert(
            "refund_without_receiver".to_string(),
            self.refund_without_receiver_script(),
        );
        map.insert(
            "unilateral_claim".to_string(),
            self.unilateral_claim_script(),
        );
        map.insert(
            "unilateral_refund".to_string(),
            self.unilateral_refund_script(),
        );
        map.insert(
            "unilateral_refund_without_receiver".to_string(),
            self.unilateral_refund_without_receiver_script(),
        );
        map
    }
}

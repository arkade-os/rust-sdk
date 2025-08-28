//! Virtual Hash Time Lock Contract (VHTLC) implementation for Ark Lightning Swaps
//!
//! This module implements VHTLC scripts that enable atomic swaps and conditional
//! payments in the Ark protocol. The VHTLC provides multiple spending paths with
//! different conditions and participants.

use ark_core::ArkAddress;
use bitcoin::opcodes::all::*;
use bitcoin::taproot::TaprootBuilder;
use bitcoin::taproot::TaprootSpendInfo;
use bitcoin::Network;
use bitcoin::ScriptBuf;
use bitcoin::Sequence;
use bitcoin::XOnlyPublicKey;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;
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
            .push_opcode(OP_RIPEMD160)
            .push_slice(&self.options.preimage_hash)
            .push_opcode(OP_EQUALVERIFY)
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
            .push_opcode(OP_RIPEMD160)
            .push_slice(&self.options.preimage_hash)
            .push_opcode(OP_EQUALVERIFY)
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
    /// Requires: CLTV timeout + CSV delay + sender signature
    pub fn unilateral_refund_without_receiver_script(&self) -> ScriptBuf {
        let sequence = self.options.unilateral_refund_without_receiver_delay;
        ScriptBuf::builder()
            .push_int(self.options.refund_locktime as i64)
            .push_opcode(OP_CLTV)
            .push_opcode(OP_DROP)
            .push_int(sequence.to_consensus_u32() as i64)
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.options.sender)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    fn build_taproot(&mut self) -> Result<(), VhtlcError> {
        let internal_key = self.options.server;

        // For 6 scripts, we need a tree structure like:
        //            root
        //           /    \
        //          /      \
        //      (d=2)      (d=2)
        //       / \        / \
        //    (d=3)(d=3) (d=3)(d=3)
        //
        // This creates a balanced tree with 4 leaves at depth 3 and 2 internal nodes at depth 2

        let mut builder = TaprootBuilder::new();

        // Add the most likely scripts at shallower depths
        builder = builder
            .add_leaf(2, self.claim_script())
            .map_err(|e| VhtlcError::TaprootError(e.to_string()))?;

        builder = builder
            .add_leaf(2, self.refund_script())
            .map_err(|e| VhtlcError::TaprootError(e.to_string()))?;

        // Add less common scripts at deeper depths
        builder = builder
            .add_leaf(3, self.refund_without_receiver_script())
            .map_err(|e| VhtlcError::TaprootError(e.to_string()))?;

        builder = builder
            .add_leaf(3, self.unilateral_claim_script())
            .map_err(|e| VhtlcError::TaprootError(e.to_string()))?;

        builder = builder
            .add_leaf(3, self.unilateral_refund_script())
            .map_err(|e| VhtlcError::TaprootError(e.to_string()))?;

        builder = builder
            .add_leaf(3, self.unilateral_refund_without_receiver_script())
            .map_err(|e| VhtlcError::TaprootError(e.to_string()))?; // s5 - depth 2

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

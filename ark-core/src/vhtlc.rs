//! Virtual Hash Time Lock Contract (VHTLC) implementation for Ark Lightning Swaps.
//!
//! This module implements VHTLC scripts that enable atomic swaps and conditional
//! payments in the Ark protocol. The VHTLC provides multiple spending paths with
//! different conditions and participants.

use crate::ArkAddress;
use crate::UNSPENDABLE_KEY;
use bitcoin::hashes::ripemd160;
use bitcoin::hashes::Hash;
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
    pub preimage_hash: ripemd160::Hash,
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

    fn build_taproot(&self) -> Result<TaprootSpendInfo, VhtlcError> {
        let internal_pubkey = PublicKey::from_str(UNSPENDABLE_KEY)
            .map_err(|e| VhtlcError::TaprootError(format!("Failed to parse internal key: {e}")))?;
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
        let taproot_spend_info = builder
            .finalize(&secp, internal_key)
            .map_err(|e| VhtlcError::TaprootError(format!("Failed to finalize taproot: {e:?}")))?;

        Ok(taproot_spend_info)
    }

    /// Creates the claim script where receiver reveals the preimage
    ///
    /// Requires: preimage hash verification + receiver signature + server signature
    pub fn claim_script(&self) -> ScriptBuf {
        let preimage_hash = self.preimage_hash;

        ScriptBuf::builder()
            .push_opcode(OP_HASH160)
            .push_slice(preimage_hash.as_byte_array())
            .push_opcode(OP_EQUAL)
            .push_opcode(OP_VERIFY)
            .push_x_only_key(&self.receiver)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&self.server)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Creates the collaborative refund script
    ///
    /// Requires: sender + receiver + server signatures
    pub fn refund_script(&self) -> ScriptBuf {
        ScriptBuf::builder()
            .push_x_only_key(&self.sender)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&self.receiver)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&self.server)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Creates the refund script when receiver is unavailable
    ///
    /// Requires: CLTV timeout + sender + server signatures
    pub fn refund_without_receiver_script(&self) -> ScriptBuf {
        ScriptBuf::builder()
            .push_int(self.refund_locktime as i64)
            .push_opcode(OP_CLTV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.sender)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&self.server)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Creates the unilateral claim script (no server cooperation needed)
    ///
    /// Requires: preimage hash verification + CSV delay + receiver signature
    pub fn unilateral_claim_script(&self) -> ScriptBuf {
        let preimage_hash = self.preimage_hash;
        let sequence = self.unilateral_claim_delay;

        ScriptBuf::builder()
            .push_opcode(OP_HASH160)
            .push_slice(preimage_hash.as_byte_array())
            .push_opcode(OP_EQUAL)
            .push_opcode(OP_VERIFY)
            .push_int(sequence.to_consensus_u32() as i64)
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.receiver)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Creates the unilateral refund script
    ///
    /// Requires: CSV delay + sender + receiver signatures
    pub fn unilateral_refund_script(&self) -> ScriptBuf {
        let sequence = self.unilateral_refund_delay;
        ScriptBuf::builder()
            .push_int(sequence.to_consensus_u32() as i64)
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.sender)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&self.receiver)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Creates the unilateral refund script when receiver is unavailable
    ///
    /// Requires: CSV delay + sender signature
    pub fn unilateral_refund_without_receiver_script(&self) -> ScriptBuf {
        let sequence = self.unilateral_refund_without_receiver_delay;
        ScriptBuf::builder()
            .push_int(sequence.to_consensus_u32() as i64)
            .push_opcode(OP_CSV)
            .push_opcode(OP_DROP)
            .push_x_only_key(&self.sender)
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
                .map_err(|e| VhtlcError::TaprootError(format!("Failed to add leaf: {e}"))),
            TaprootTreeNode::Branch { left, right, .. } => {
                let builder = Self::add_tree_to_builder(builder, left, depth + 1)?;
                Self::add_tree_to_builder(builder, right, depth + 1)
            }
        }
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
    taproot_spend_info: TaprootSpendInfo,
    network: Network,
}

impl VhtlcScript {
    /// Creates a new VHTLC script with the given options
    ///
    /// This will validate the options and build the complete taproot tree
    /// with all spending paths.
    pub fn new(options: VhtlcOptions, network: Network) -> Result<Self, VhtlcError> {
        options.validate()?;

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
            self.options.server,
            self.taproot_spend_info().output_key(),
        )
    }

    /// Creates the claim script where receiver reveals the preimage
    ///
    /// Requires: preimage hash verification + receiver signature + server signature
    pub fn claim_script(&self) -> ScriptBuf {
        self.options.claim_script()
    }

    /// Creates the collaborative refund script
    ///
    /// Requires: sender + receiver + server signatures
    pub fn refund_script(&self) -> ScriptBuf {
        self.options.refund_script()
    }

    /// Creates the refund script when receiver is unavailable
    ///
    /// Requires: CLTV timeout + sender + server signatures
    pub fn refund_without_receiver_script(&self) -> ScriptBuf {
        self.options.refund_without_receiver_script()
    }

    /// Creates the unilateral claim script (no server cooperation needed)
    ///
    /// Requires: preimage hash verification + CSV delay + receiver signature
    pub fn unilateral_claim_script(&self) -> ScriptBuf {
        self.options.unilateral_claim_script()
    }

    /// Creates the unilateral refund script
    ///
    /// Requires: CSV delay + sender + receiver signatures
    pub fn unilateral_refund_script(&self) -> ScriptBuf {
        self.options.unilateral_refund_script()
    }

    /// Creates the unilateral refund script when receiver is unavailable
    ///
    /// Requires: CSV delay + sender signature
    pub fn unilateral_refund_without_receiver_script(&self) -> ScriptBuf {
        self.options.unilateral_refund_without_receiver_script()
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

    pub fn tapscripts(self) -> Vec<ScriptBuf> {
        vec![
            self.claim_script(),
            self.refund_script(),
            self.refund_without_receiver_script(),
            self.unilateral_claim_script(),
            self.unilateral_refund_script(),
            self.unilateral_refund_without_receiver_script(),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hex::DisplayHex;
    use bitcoin::hex::FromHex;
    use bitcoin::Network;
    use bitcoin::PublicKey;
    use bitcoin::Sequence;
    use bitcoin::XOnlyPublicKey;
    use serde::Deserialize;
    use serde::Serialize;
    use std::collections::HashMap;
    use std::fs;
    use std::str::FromStr;

    #[derive(Debug, Deserialize, Serialize)]
    struct Fixtures {
        valid: Vec<ValidTestCase>,
        invalid: Vec<InvalidTestCase>,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct ValidTestCase {
        description: String,
        #[serde(rename = "preimageHash")]
        preimage_hash: String,
        receiver: String,
        sender: String,
        server: String,
        #[serde(rename = "refundLocktime")]
        refund_locktime: u32,
        #[serde(rename = "unilateralClaimDelay")]
        unilateral_claim_delay: Delay,
        #[serde(rename = "unilateralRefundDelay")]
        unilateral_refund_delay: Delay,
        #[serde(rename = "unilateralRefundWithoutReceiverDelay")]
        unilateral_refund_without_receiver_delay: Delay,
        expected: String,
        scripts: ScriptHexes,
        taproot: TaprootInfo,
        #[serde(rename = "decodedScripts")]
        decoded_scripts: HashMap<String, String>,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct InvalidTestCase {
        description: String,
        #[serde(rename = "preimageHash")]
        preimage_hash: String,
        receiver: String,
        sender: String,
        server: String,
        #[serde(rename = "refundLocktime")]
        refund_locktime: u32,
        #[serde(rename = "unilateralClaimDelay")]
        unilateral_claim_delay: Delay,
        #[serde(rename = "unilateralRefundDelay")]
        unilateral_refund_delay: Delay,
        #[serde(rename = "unilateralRefundWithoutReceiverDelay")]
        unilateral_refund_without_receiver_delay: Delay,
        error: String,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct ScriptHexes {
        #[serde(rename = "claimScript")]
        claim_script: String,
        #[serde(rename = "refundScript")]
        refund_script: String,
        #[serde(rename = "refundWithoutReceiverScript")]
        refund_without_receiver_script: String,
        #[serde(rename = "unilateralClaimScript")]
        unilateral_claim_script: String,
        #[serde(rename = "unilateralRefundScript")]
        unilateral_refund_script: String,
        #[serde(rename = "unilateralRefundWithoutReceiverScript")]
        unilateral_refund_without_receiver_script: String,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct TaprootInfo {
        #[serde(rename = "tweakedPublicKey")]
        tweaked_public_key: String,
        #[serde(rename = "tapTree")]
        tap_tree: String,
        #[serde(rename = "internalKey")]
        internal_key: String,
    }

    #[derive(Debug, Deserialize, Serialize)]
    struct Delay {
        #[serde(rename = "type")]
        delay_type: String,
        value: u32,
    }

    impl Delay {
        fn to_sequence(&self) -> Result<Sequence, String> {
            match self.delay_type.as_str() {
                "blocks" => {
                    if self.value == 0 {
                        return Err("unilateral claim delay must greater than 0".to_string());
                    }
                    Ok(Sequence::from_height(self.value as u16))
                }
                "seconds" => {
                    if self.value < 512 {
                        return Err("seconds timelock must be greater or equal to 512".to_string());
                    }
                    if self.value % 512 != 0 {
                        return Err("seconds timelock must be multiple of 512".to_string());
                    }
                    Sequence::from_seconds_ceil(self.value)
                        .map_err(|e| format!("Invalid seconds value: {e}"))
                }
                _ => Err(format!("Unknown delay type: {}", self.delay_type)),
            }
        }
    }

    fn hex_to_bytes20(hex: &str) -> Result<[u8; 20], String> {
        let bytes = Vec::from_hex(hex).map_err(|e| format!("Invalid hex: {e}"))?;
        if bytes.len() != 20 {
            return Err("preimage hash must be 20 bytes".to_string());
        }
        let mut arr = [0u8; 20];
        arr.copy_from_slice(&bytes);
        Ok(arr)
    }

    fn pubkey_to_xonly(pubkey_hex: &str) -> XOnlyPublicKey {
        let pubkey = PublicKey::from_str(pubkey_hex).expect("valid public key");
        XOnlyPublicKey::from(pubkey.inner)
    }

    #[test]
    fn test_vhtlc_with_valid_fixtures() {
        let fixtures_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/vhtlc_fixtures/vhtlc.json");
        let fixtures_json = fs::read_to_string(fixtures_path).expect("to read fixtures file");
        let fixtures: Fixtures = serde_json::from_str(&fixtures_json).expect("to parse fixtures");

        for test_case in fixtures.valid {
            let preimage_hash =
                ripemd160::Hash::from_str(&test_case.preimage_hash).expect("valid hash");

            let sender = pubkey_to_xonly(&test_case.sender);
            let receiver = pubkey_to_xonly(&test_case.receiver);
            let server = pubkey_to_xonly(&test_case.server);

            let options = VhtlcOptions {
                sender,
                receiver,
                server,
                preimage_hash,
                refund_locktime: test_case.refund_locktime,
                unilateral_claim_delay: test_case
                    .unilateral_claim_delay
                    .to_sequence()
                    .expect("valid delay"),
                unilateral_refund_delay: test_case
                    .unilateral_refund_delay
                    .to_sequence()
                    .expect("valid delay"),
                unilateral_refund_without_receiver_delay: test_case
                    .unilateral_refund_without_receiver_delay
                    .to_sequence()
                    .expect("valid delay"),
            };

            let vhtlc = VhtlcScript::new(options, Network::Testnet).expect("to create VHTLC");

            // Test 1: Verify all script hex encodings
            let claim_hex = vhtlc.claim_script().as_bytes().to_lower_hex_string();
            assert_eq!(
                claim_hex, test_case.scripts.claim_script,
                "Claim script hex mismatch for test case: {}",
                test_case.description
            );

            let refund_hex = vhtlc.refund_script().as_bytes().to_lower_hex_string();
            assert_eq!(
                refund_hex, test_case.scripts.refund_script,
                "Refund script hex mismatch for test case: {}",
                test_case.description
            );

            let refund_without_receiver_hex = vhtlc
                .refund_without_receiver_script()
                .as_bytes()
                .to_lower_hex_string();
            assert_eq!(
                refund_without_receiver_hex, test_case.scripts.refund_without_receiver_script,
                "Refund without receiver script hex mismatch for test case: {}",
                test_case.description
            );

            let unilateral_claim_hex = vhtlc
                .unilateral_claim_script()
                .as_bytes()
                .to_lower_hex_string();
            assert_eq!(
                unilateral_claim_hex, test_case.scripts.unilateral_claim_script,
                "Unilateral claim script hex mismatch for test case: {}",
                test_case.description
            );

            let unilateral_refund_hex = vhtlc
                .unilateral_refund_script()
                .as_bytes()
                .to_lower_hex_string();
            assert_eq!(
                unilateral_refund_hex, test_case.scripts.unilateral_refund_script,
                "Unilateral refund script hex mismatch for test case: {}",
                test_case.description
            );

            let unilateral_refund_without_receiver_hex = vhtlc
                .unilateral_refund_without_receiver_script()
                .as_bytes()
                .to_lower_hex_string();

            assert_eq!(
            unilateral_refund_without_receiver_hex,
            test_case.scripts.unilateral_refund_without_receiver_script,
            "Unilateral refund without receiver script hex mismatch for test case: {}. Our impl includes CLTV locktime, fixture expects only CSV",
            test_case.description
        );

            // Test 2: Verify taproot information
            let taproot_info = vhtlc.taproot_spend_info();

            let internal_key = taproot_info.internal_key();
            let internal_key_hex = internal_key.serialize().to_lower_hex_string();

            // The internal key in fixtures is prefixed with version byte
            let pubkey = PublicKey::from_str(&test_case.taproot.internal_key)
                .expect("valid internal key in fixture");
            let expected_internal = XOnlyPublicKey::from(pubkey.inner)
                .serialize()
                .to_lower_hex_string();

            assert_eq!(
                internal_key_hex, expected_internal,
                "Internal key mismatch for test case: {}",
                test_case.description
            );

            let output_key = taproot_info.output_key();
            let output_key_hex = output_key.serialize().to_lower_hex_string();

            assert_eq!(
                output_key_hex, test_case.taproot.tweaked_public_key,
                "Tweaked public key mismatch for test case: {}",
                test_case.description
            );

            // Test 3: Verify address generation
            let addr = vhtlc.address();
            let address_str = addr.encode();

            assert_eq!(
                address_str, test_case.expected,
                "Address mismatch for test case: {}",
                test_case.description
            );
        }
    }

    #[test]
    fn test_vhtlc_with_invalid_fixtures() {
        let fixtures_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/vhtlc_fixtures/vhtlc.json");
        let fixtures_json = fs::read_to_string(fixtures_path).expect("to read fixtures file");
        let fixtures: Fixtures = serde_json::from_str(&fixtures_json).expect("to parse fixtures");

        for test_case in fixtures.invalid {
            // Try to parse preimage hash
            let preimage_hash_result = hex_to_bytes20(&test_case.preimage_hash);

            if let Err(e) = preimage_hash_result {
                assert!(
                    e.contains(&test_case.error),
                    "Expected error containing '{}', got '{}' for test case: {}",
                    test_case.error,
                    e,
                    test_case.description
                );
                continue;
            }

            // Check refund locktime
            if test_case.refund_locktime == 0 {
                assert!(
                    test_case
                        .error
                        .contains("refund locktime must be greater than 0"),
                    "Expected refund locktime error for test case: {}",
                    test_case.description
                );
                continue;
            }

            // Try to convert delays
            let claim_delay_result = test_case.unilateral_claim_delay.to_sequence();
            if let Err(e) = claim_delay_result {
                assert!(
                    e.contains(&test_case.error),
                    "Expected error containing '{}', got '{}' for claim delay in test case: {}",
                    test_case.error,
                    e,
                    test_case.description
                );
                continue;
            }

            let refund_delay_result = test_case.unilateral_refund_delay.to_sequence();
            if let Err(e) = refund_delay_result {
                assert!(
                    e.contains(&test_case.error),
                    "Expected error containing '{}', got '{}' for refund delay in test case: {}",
                    test_case.error,
                    e,
                    test_case.description
                );
                continue;
            }

            let refund_without_receiver_delay_result = test_case
                .unilateral_refund_without_receiver_delay
                .to_sequence();
            if let Err(e) = refund_without_receiver_delay_result {
                assert!(
                e.contains(&test_case.error),
                "Expected error containing '{}', got '{}' for refund without receiver delay in test case: {}",
                test_case.error,
                e,
                test_case.description
            );
                continue;
            }

            // If we got here, all validations passed but they shouldn't have
            panic!(
                "Invalid test case '{}' didn't fail as expected",
                test_case.description
            );
        }
    }

    #[test]
    fn test_specific_script_encodings() {
        // Test specific script encoding for the first valid case
        let sender =
            pubkey_to_xonly("030192e796452d6df9697c280542e1560557bcf79a347d925895043136225c7cb4");
        let receiver =
            pubkey_to_xonly("021e1bb85455fe3f5aed60d101aa4dbdb9e7714f6226769a97a17a5331dadcd53b");
        let server =
            pubkey_to_xonly("03aad52d58162e9eefeafc7ad8a1cdca8060b5f01df1e7583362d052e266208f88");
        let preimage_hash =
            ripemd160::Hash::from_str("4d487dd3753a89bc9fe98401d1196523058251fc").unwrap();

        let options = VhtlcOptions {
            sender,
            receiver,
            server,
            preimage_hash,
            refund_locktime: 265,
            unilateral_claim_delay: Sequence::from_height(17),
            unilateral_refund_delay: Sequence::from_height(144),
            unilateral_refund_without_receiver_delay: Sequence::from_height(144),
        };

        let vhtlc = VhtlcScript::new(options, Network::Testnet).expect("to create VHTLC");

        // Verify claim script
        let claim_script = vhtlc.claim_script();
        let claim_hex = claim_script.as_bytes().to_lower_hex_string();
        let expected_claim = "a9144d487dd3753a89bc9fe98401d1196523058251fc8769201e1bb85455fe3f5aed60d101aa4dbdb9e7714f6226769a97a17a5331dadcd53bad20aad52d58162e9eefeafc7ad8a1cdca8060b5f01df1e7583362d052e266208f88ac";
        assert_eq!(
            claim_hex, expected_claim,
            "Claim script should match fixture"
        );

        // Verify unilateral claim script (with CSV=17)
        let unilateral_claim = vhtlc.unilateral_claim_script();
        let unilateral_claim_hex = unilateral_claim.as_bytes().to_lower_hex_string();

        // Check the CSV encoding for value 17
        assert!(
            unilateral_claim_hex.contains("0111"),
            "Should contain CSV value 17 as 0x0111"
        );

        let expected_unilateral_claim = "a9144d487dd3753a89bc9fe98401d1196523058251fc87690111b275201e1bb85455fe3f5aed60d101aa4dbdb9e7714f6226769a97a17a5331dadcd53bac";
        assert_eq!(
            unilateral_claim_hex, expected_unilateral_claim,
            "Unilateral claim script should match fixture"
        );
    }
}

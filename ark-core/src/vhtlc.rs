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
    #[error("Invalid P2TR script for {0}: expected `OP_1 <32 bytes>`")]
    InvalidP2trScript(&'static str),
    #[error("Covenant key derivation failed: {0}")]
    Covenant(#[from] ark_script::TweakError),
}

/// Which preimage condition the preimage-gated leaves (`claim`, `unilateral_claim` and
/// `non_interactive_claim`) are built from.
///
/// The two versions produce different leaf bytes, and so different addresses, for the same
/// keys. Mirrors `VHTLC.Script` and `VHTLC.ScriptV2` in the TS SDK.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VhtlcVersion {
    /// `OP_HASH160 <hash> OP_EQUAL`.
    V1,
    /// `OP_SIZE 32 OP_EQUALVERIFY OP_HASH160 <hash> OP_EQUAL`: like BOLT3 HTLCs, the preimage
    /// must be exactly 32 bytes.
    V2,
}

/// The emulator covenant leaves, all or nothing.
///
/// Adds three leaves the emulator co-signs under a covenant pinning the payout to a
/// pre-committed destination:
///
/// - `non_interactive_claim`: `server` + covenant-tweaked emulator, paying `receiver_pk_script`;
///   the claim can be pushed without the receiver online.
/// - `non_interactive_refund`: `server` + `receiver` + covenant-tweaked emulator, paying
///   `sender_pk_script`, no timelock.
/// - `non_interactive_refund_without_receiver`: `server` + the SAME co-signer as
///   `non_interactive_refund`, paying `sender_pk_script` after `refund_locktime`; the only refund
///   tier needing no participant signature. Omitted when `legacy_pre_timelocked_refund` is set.
///
/// The suite is appended after the six signature leaves, so a script with it never collides
/// with one without. Mirrors `VHTLC.Options.nonInteractiveParameters` in the TS SDK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NonInteractiveParameters {
    /// Emulator public key. One key, tweaked per covenant destination.
    pub emulator_pubkey: XOnlyPublicKey,
    /// Where the claim covenant pays: the receiver's P2TR `script_pubkey`, 34 bytes.
    pub receiver_pk_script: ScriptBuf,
    /// Where BOTH refund covenants pay: the sender's P2TR `script_pubkey`, 34 bytes.
    pub sender_pk_script: ScriptBuf,
    /// Legacy rebuild only, never for a new lockup: build the suite WITHOUT the timelocked
    /// refund leaf (eight leaves), the shape of lockups funded before that leaf shipped.
    #[serde(default)]
    pub legacy_pre_timelocked_refund: bool,
}

impl NonInteractiveParameters {
    fn validate(&self) -> Result<(), VhtlcError> {
        if !is_p2tr_pk_script(&self.receiver_pk_script) {
            return Err(VhtlcError::InvalidP2trScript("receiver_pk_script"));
        }
        if !is_p2tr_pk_script(&self.sender_pk_script) {
            return Err(VhtlcError::InvalidP2trScript("sender_pk_script"));
        }
        Ok(())
    }
}

/// A v1 P2TR `script_pubkey` is exactly `OP_1 <32-byte-program>`, 34 bytes.
fn is_p2tr_pk_script(script: &ScriptBuf) -> bool {
    let bytes = script.as_bytes();
    bytes.len() == 34 && bytes[0] == 0x51 && bytes[1] == 0x20
}

/// The covenant: "the output at this input's index pays the given P2TR script, value >= input".
///
/// Byte-identical to `enforcePayTo` in the TS SDK. The introspection opcodes are executed by the
/// emulator, not by Bitcoin Script; see `ark-script`'s opcode table.
fn enforce_pay_to(destination_pk_script: &ScriptBuf) -> ScriptBuf {
    use bitcoin::opcodes::Opcode;

    let op_pushcurrentinputindex = Opcode::from(0xcd);
    let op_inspectoutputscriptpubkey = Opcode::from(0xd1);
    let op_inspectoutputvalue = Opcode::from(0xcf);
    let op_inspectinputvalue = Opcode::from(0xc9);

    let program: &[u8; 32] = destination_pk_script.as_bytes()[2..]
        .try_into()
        .expect("validated 34-byte P2TR script");

    ScriptBuf::builder()
        .push_opcode(op_pushcurrentinputindex)
        .push_opcode(OP_DUP)
        .push_opcode(op_inspectoutputscriptpubkey)
        .push_int(1)
        .push_opcode(OP_EQUALVERIFY)
        .push_slice(program)
        .push_opcode(OP_EQUALVERIFY)
        .push_opcode(op_inspectoutputvalue)
        .push_opcode(op_pushcurrentinputindex)
        .push_opcode(op_inspectinputvalue)
        .push_opcode(OP_GREATERTHANOREQUAL)
        .into_script()
}

/// The covenant leaves and their arkade scripts, derived once per [`VhtlcScript`].
#[derive(Debug, Clone, PartialEq, Eq)]
struct CovenantLeaves {
    non_interactive_claim: ScriptBuf,
    non_interactive_claim_arkade_script: ScriptBuf,
    non_interactive_refund: ScriptBuf,
    non_interactive_refund_arkade_script: ScriptBuf,
    /// `None` on a legacy (eight-leaf) rebuild.
    non_interactive_refund_without_receiver: Option<ScriptBuf>,
}

impl CovenantLeaves {
    fn new(
        options: &VhtlcOptions,
        version: VhtlcVersion,
        params: &NonInteractiveParameters,
    ) -> Result<Self, VhtlcError> {
        params.validate()?;

        let claim_arkade_script = enforce_pay_to(&params.receiver_pk_script);
        let claim_cosigner = ark_script::compute_arkade_script_public_key(
            &params.emulator_pubkey,
            &claim_arkade_script,
        )?;
        let non_interactive_claim = options
            .preimage_condition(version)
            .push_opcode(OP_VERIFY)
            .push_x_only_key(&options.server)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&claim_cosigner)
            .push_opcode(OP_CHECKSIG)
            .into_script();

        // Derived ONCE for both refund covenant leaves: same destination, same key.
        let refund_arkade_script = enforce_pay_to(&params.sender_pk_script);
        let refund_cosigner = ark_script::compute_arkade_script_public_key(
            &params.emulator_pubkey,
            &refund_arkade_script,
        )?;
        let non_interactive_refund = ScriptBuf::builder()
            .push_x_only_key(&options.server)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&options.receiver)
            .push_opcode(OP_CHECKSIGVERIFY)
            .push_x_only_key(&refund_cosigner)
            .push_opcode(OP_CHECKSIG)
            .into_script();

        let non_interactive_refund_without_receiver =
            (!params.legacy_pre_timelocked_refund).then(|| {
                ScriptBuf::builder()
                    .push_int(options.refund_locktime as i64)
                    .push_opcode(OP_CLTV)
                    .push_opcode(OP_DROP)
                    .push_x_only_key(&options.server)
                    .push_opcode(OP_CHECKSIGVERIFY)
                    .push_x_only_key(&refund_cosigner)
                    .push_opcode(OP_CHECKSIG)
                    .into_script()
            });

        Ok(Self {
            non_interactive_claim,
            non_interactive_claim_arkade_script: claim_arkade_script,
            non_interactive_refund,
            non_interactive_refund_arkade_script: refund_arkade_script,
            non_interactive_refund_without_receiver,
        })
    }

    fn scripts(&self) -> Vec<ScriptBuf> {
        let mut scripts = vec![
            self.non_interactive_claim.clone(),
            self.non_interactive_refund.clone(),
        ];
        scripts.extend(self.non_interactive_refund_without_receiver.clone());
        scripts
    }
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

    /// The six signature leaves, in the order the merkle root depends on.
    fn base_scripts(&self, version: VhtlcVersion) -> Vec<ScriptBuf> {
        vec![
            self.claim_script_for(version),
            self.refund_script(),
            self.refund_without_receiver_script(),
            self.unilateral_claim_script_for(version),
            self.unilateral_refund_script(),
            self.unilateral_refund_without_receiver_script(),
        ]
    }

    fn build_taproot(
        &self,
        version: VhtlcVersion,
        covenant: Option<&CovenantLeaves>,
    ) -> Result<TaprootSpendInfo, VhtlcError> {
        let internal_pubkey = PublicKey::from_str(UNSPENDABLE_KEY)
            .map_err(|e| VhtlcError::TaprootError(format!("Failed to parse internal key: {e}")))?;
        let internal_key = XOnlyPublicKey::from(internal_pubkey);

        // Leaf order fixes the merkle root: the covenant suite is appended after the six
        // signature leaves, so a script with it never collides with one without.
        let mut scripts = self.base_scripts(version);
        if let Some(covenant) = covenant {
            scripts.extend(covenant.scripts());
        }

        // btcd's tree assembly, as arkd and the TS SDK use it. A Huffman tree (what
        // `TaprootBuilder::with_huffman_tree` and the previous implementation here build) only
        // agrees with it for some leaf counts: six does, nine does not.
        let tree = crate::vtxo::assemble_taproot_tree(&scripts)
            .map_err(|e| VhtlcError::TaprootError(format!("Failed to assemble taproot: {e}")))?;

        let secp = bitcoin::secp256k1::Secp256k1::new();
        Ok(TaprootSpendInfo::from_node_info(&secp, internal_key, tree))
    }

    /// The preimage condition every preimage-gated leaf is built from, leaving the result of
    /// `OP_EQUAL` on the stack.
    fn preimage_condition(&self, version: VhtlcVersion) -> bitcoin::script::Builder {
        let builder = ScriptBuf::builder();
        let builder = match version {
            VhtlcVersion::V1 => builder,
            VhtlcVersion::V2 => builder
                .push_opcode(OP_SIZE)
                .push_int(32)
                .push_opcode(OP_EQUALVERIFY),
        };
        builder
            .push_opcode(OP_HASH160)
            .push_slice(self.preimage_hash.as_byte_array())
            .push_opcode(OP_EQUAL)
    }

    /// Creates the claim script where receiver reveals the preimage
    ///
    /// Requires: preimage hash verification + receiver signature + server signature
    ///
    /// This is the [`VhtlcVersion::V1`] encoding; see [`VhtlcScript::claim_script`] for the
    /// version-aware one.
    pub fn claim_script(&self) -> ScriptBuf {
        self.claim_script_for(VhtlcVersion::V1)
    }

    fn claim_script_for(&self, version: VhtlcVersion) -> ScriptBuf {
        self.preimage_condition(version)
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
    ///
    /// This is the [`VhtlcVersion::V1`] encoding; see [`VhtlcScript::unilateral_claim_script`]
    /// for the version-aware one.
    pub fn unilateral_claim_script(&self) -> ScriptBuf {
        self.unilateral_claim_script_for(VhtlcVersion::V1)
    }

    fn unilateral_claim_script_for(&self, version: VhtlcVersion) -> ScriptBuf {
        let sequence = self.unilateral_claim_delay;

        self.preimage_condition(version)
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
}

/// VHTLC Script builder and manager
///
/// This struct creates and manages VHTLC scripts with six signature spending paths:
/// 1. **Claim**: Receiver reveals preimage (collaborative with server)
/// 2. **Refund**: Collaborative refund (all three parties)
/// 3. **Refund without Receiver**: Sender refunds after locktime (with server)
/// 4. **Unilateral Claim**: Receiver claims after delay (no server needed)
/// 5. **Unilateral Refund**: Collaborative unilateral refund after delay
/// 6. **Unilateral Refund without Receiver**: Sender unilateral refund after both timeouts
///
/// and, when built with [`NonInteractiveParameters`], up to three emulator covenant paths:
/// 7. **Non-interactive Claim**: Server + emulator, payout pinned to the receiver
/// 8. **Non-interactive Refund**: Server + receiver + emulator, payout pinned to the sender
/// 9. **Non-interactive Refund without Receiver**: Server + emulator after locktime, payout pinned
///    to the sender
pub struct VhtlcScript {
    options: VhtlcOptions,
    version: VhtlcVersion,
    non_interactive: Option<NonInteractiveParameters>,
    covenant: Option<CovenantLeaves>,
    taproot_spend_info: TaprootSpendInfo,
    network: Network,
}

impl VhtlcScript {
    /// Creates a new [`VhtlcVersion::V1`] VHTLC script with the six signature paths.
    ///
    /// This will validate the options and build the complete taproot tree
    /// with all spending paths.
    pub fn new(options: VhtlcOptions, network: Network) -> Result<Self, VhtlcError> {
        Self::build(options, VhtlcVersion::V1, None, network)
    }

    /// Creates a new [`VhtlcVersion::V2`] VHTLC script, optionally with the emulator covenant
    /// paths. Mirrors `VHTLC.ScriptV2` in the TS SDK.
    pub fn new_v2(
        options: VhtlcOptions,
        non_interactive: Option<NonInteractiveParameters>,
        network: Network,
    ) -> Result<Self, VhtlcError> {
        Self::build(options, VhtlcVersion::V2, non_interactive, network)
    }

    /// Creates a VHTLC script of any version, optionally with the emulator covenant paths.
    pub fn build(
        options: VhtlcOptions,
        version: VhtlcVersion,
        non_interactive: Option<NonInteractiveParameters>,
        network: Network,
    ) -> Result<Self, VhtlcError> {
        options.validate()?;

        let covenant = non_interactive
            .as_ref()
            .map(|params| CovenantLeaves::new(&options, version, params))
            .transpose()?;

        let taproot_spend_info = options.build_taproot(version, covenant.as_ref())?;

        Ok(Self {
            options,
            version,
            non_interactive,
            covenant,
            taproot_spend_info,
            network,
        })
    }

    pub fn taproot_spend_info(&self) -> &TaprootSpendInfo {
        &self.taproot_spend_info
    }

    pub fn options(&self) -> &VhtlcOptions {
        &self.options
    }

    pub fn version(&self) -> VhtlcVersion {
        self.version
    }

    pub fn non_interactive_parameters(&self) -> Option<&NonInteractiveParameters> {
        self.non_interactive.as_ref()
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
        self.options.claim_script_for(self.version)
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
        self.options.unilateral_claim_script_for(self.version)
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

    /// The non-interactive claim leaf: preimage + server + covenant-tweaked emulator key.
    ///
    /// `None` unless built with [`NonInteractiveParameters`].
    pub fn non_interactive_claim_script(&self) -> Option<ScriptBuf> {
        self.covenant
            .as_ref()
            .map(|c| c.non_interactive_claim.clone())
    }

    /// The arkade script the emulator evaluates before co-signing
    /// [`Self::non_interactive_claim_script`]: pins the payout to the receiver's script.
    pub fn non_interactive_claim_arkade_script(&self) -> Option<ScriptBuf> {
        self.covenant
            .as_ref()
            .map(|c| c.non_interactive_claim_arkade_script.clone())
    }

    /// The non-interactive refund leaf: server + receiver + covenant-tweaked emulator key, no
    /// timelock.
    ///
    /// `None` unless built with [`NonInteractiveParameters`].
    pub fn non_interactive_refund_script(&self) -> Option<ScriptBuf> {
        self.covenant
            .as_ref()
            .map(|c| c.non_interactive_refund.clone())
    }

    /// The arkade script the emulator evaluates before co-signing either non-interactive refund
    /// leaf: pins the payout to the sender's script.
    pub fn non_interactive_refund_arkade_script(&self) -> Option<ScriptBuf> {
        self.covenant
            .as_ref()
            .map(|c| c.non_interactive_refund_arkade_script.clone())
    }

    /// The timelocked non-interactive refund leaf: server + covenant-tweaked emulator key after
    /// `refund_locktime`. Shares its arkade script with [`Self::non_interactive_refund_script`].
    ///
    /// `None` unless built with [`NonInteractiveParameters`] without the legacy flag.
    pub fn non_interactive_refund_without_receiver_script(&self) -> Option<ScriptBuf> {
        self.covenant
            .as_ref()
            .and_then(|c| c.non_interactive_refund_without_receiver.clone())
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
        if let Some(script) = self.non_interactive_claim_script() {
            map.insert("non_interactive_claim".to_string(), script);
        }
        if let Some(script) = self.non_interactive_refund_script() {
            map.insert("non_interactive_refund".to_string(), script);
        }
        if let Some(script) = self.non_interactive_refund_without_receiver_script() {
            map.insert(
                "non_interactive_refund_without_receiver".to_string(),
                script,
            );
        }
        map
    }

    /// Every leaf script, in the order the merkle root depends on.
    pub fn tapscripts(&self) -> Vec<ScriptBuf> {
        let mut scripts = self.options.base_scripts(self.version);
        if let Some(covenant) = &self.covenant {
            scripts.extend(covenant.scripts());
        }
        scripts
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

    /// Cross-SDK conformance vectors for the nine-leaf `VHTLC.ScriptV2` tree (the full
    /// covenant suite, no legacy marker). The TS, C# and Go implementations are checked against
    /// the same fixture; a byte diverging anywhere means a different taproot address and
    /// unspendable funds.
    mod nine_leaf {
        use super::*;

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Fixture {
            options: Options,
            pk_script: String,
            leaves: Leaves,
            arkade_scripts: ArkadeScripts,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Options {
            sender: String,
            receiver: String,
            server: String,
            preimage_hash: String,
            refund_locktime: String,
            unilateral_claim_delay: Delay,
            unilateral_refund_delay: Delay,
            unilateral_refund_without_receiver_delay: Delay,
            non_interactive_claim: NonInteractiveClaim,
            non_interactive_refund: NonInteractiveRefund,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Delay {
            #[serde(rename = "type")]
            delay_type: String,
            value: String,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct NonInteractiveClaim {
            receiver_pk_script: String,
            emulator_pubkey: String,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct NonInteractiveRefund {
            sender_pk_script: String,
            emulator_pubkey: String,
            without_receiver: bool,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Leaves {
            claim: String,
            refund: String,
            refund_without_receiver: String,
            unilateral_claim: String,
            unilateral_refund: String,
            unilateral_refund_without_receiver: String,
            non_interactive_claim: String,
            non_interactive_refund: String,
            non_interactive_refund_without_receiver: String,
        }

        #[derive(Debug, Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ArkadeScripts {
            non_interactive_claim: String,
            non_interactive_refund: String,
        }

        fn delay(d: &Delay) -> Sequence {
            let value: u32 = d.value.parse().unwrap();
            match d.delay_type.as_str() {
                "blocks" => Sequence::from_height(value as u16),
                "seconds" => Sequence::from_seconds_ceil(value).unwrap(),
                other => panic!("unknown delay type {other}"),
            }
        }

        fn script(hex: &str) -> ScriptBuf {
            ScriptBuf::from_bytes(Vec::from_hex(hex).unwrap())
        }

        fn load() -> (VhtlcOptions, NonInteractiveParameters, Fixture) {
            let path = concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/src/vhtlc_fixtures/vhtlc-v2-nine-leaf.json"
            );
            let fixture: Fixture =
                serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
            let o = &fixture.options;

            assert_eq!(
                o.non_interactive_claim.emulator_pubkey, o.non_interactive_refund.emulator_pubkey,
                "the fixture predates the grouped parameters; both leaves carry the same key"
            );

            let options = VhtlcOptions {
                sender: pubkey_to_xonly(&o.sender),
                receiver: pubkey_to_xonly(&o.receiver),
                server: pubkey_to_xonly(&o.server),
                preimage_hash: ripemd160::Hash::from_str(&o.preimage_hash).unwrap(),
                refund_locktime: o.refund_locktime.parse().unwrap(),
                unilateral_claim_delay: delay(&o.unilateral_claim_delay),
                unilateral_refund_delay: delay(&o.unilateral_refund_delay),
                unilateral_refund_without_receiver_delay: delay(
                    &o.unilateral_refund_without_receiver_delay,
                ),
            };
            let non_interactive = NonInteractiveParameters {
                emulator_pubkey: XOnlyPublicKey::from_str(&o.non_interactive_claim.emulator_pubkey)
                    .unwrap(),
                receiver_pk_script: script(&o.non_interactive_claim.receiver_pk_script),
                sender_pk_script: script(&o.non_interactive_refund.sender_pk_script),
                legacy_pre_timelocked_refund: !o.non_interactive_refund.without_receiver,
            };

            (options, non_interactive, fixture)
        }

        fn hex(script: ScriptBuf) -> String {
            script.as_bytes().to_lower_hex_string()
        }

        #[test]
        fn matches_the_cross_sdk_vectors() {
            let (options, non_interactive, fixture) = load();
            let vhtlc =
                VhtlcScript::new_v2(options, Some(non_interactive), Network::Regtest).unwrap();

            assert_eq!(hex(vhtlc.script_pubkey()), fixture.pk_script);

            let leaves = &fixture.leaves;
            assert_eq!(hex(vhtlc.claim_script()), leaves.claim);
            assert_eq!(hex(vhtlc.refund_script()), leaves.refund);
            assert_eq!(
                hex(vhtlc.refund_without_receiver_script()),
                leaves.refund_without_receiver
            );
            assert_eq!(
                hex(vhtlc.unilateral_claim_script()),
                leaves.unilateral_claim
            );
            assert_eq!(
                hex(vhtlc.unilateral_refund_script()),
                leaves.unilateral_refund
            );
            assert_eq!(
                hex(vhtlc.unilateral_refund_without_receiver_script()),
                leaves.unilateral_refund_without_receiver
            );
            assert_eq!(
                hex(vhtlc.non_interactive_claim_script().unwrap()),
                leaves.non_interactive_claim
            );
            assert_eq!(
                hex(vhtlc.non_interactive_refund_script().unwrap()),
                leaves.non_interactive_refund
            );
            assert_eq!(
                hex(vhtlc
                    .non_interactive_refund_without_receiver_script()
                    .unwrap()),
                leaves.non_interactive_refund_without_receiver
            );

            // Both covenant arkade scripts: a divergence in either compiles into a different
            // emulator co-signer key and so a different leaf.
            assert_eq!(
                hex(vhtlc.non_interactive_claim_arkade_script().unwrap()),
                fixture.arkade_scripts.non_interactive_claim
            );
            assert_eq!(
                hex(vhtlc.non_interactive_refund_arkade_script().unwrap()),
                fixture.arkade_scripts.non_interactive_refund
            );
        }

        #[test]
        fn has_nine_leaves_in_merkle_order() {
            let (options, non_interactive, fixture) = load();
            let vhtlc =
                VhtlcScript::new_v2(options, Some(non_interactive), Network::Regtest).unwrap();

            let l = &fixture.leaves;
            let expected = [
                &l.claim,
                &l.refund,
                &l.refund_without_receiver,
                &l.unilateral_claim,
                &l.unilateral_refund,
                &l.unilateral_refund_without_receiver,
                &l.non_interactive_claim,
                &l.non_interactive_refund,
                &l.non_interactive_refund_without_receiver,
            ];

            assert_eq!(vhtlc.get_script_map().len(), 9);

            let tapscripts: Vec<String> = vhtlc.tapscripts().into_iter().map(hex).collect();
            assert_eq!(tapscripts, expected.map(|s| s.to_string()));
        }

        #[test]
        fn legacy_shape_drops_the_timelocked_refund_leaf() {
            let (options, mut non_interactive, fixture) = load();
            non_interactive.legacy_pre_timelocked_refund = true;
            let vhtlc =
                VhtlcScript::new_v2(options, Some(non_interactive), Network::Regtest).unwrap();

            assert!(vhtlc
                .non_interactive_refund_without_receiver_script()
                .is_none());
            assert_eq!(vhtlc.get_script_map().len(), 8);
            assert_ne!(hex(vhtlc.script_pubkey()), fixture.pk_script);
        }

        #[test]
        fn v2_without_covenants_has_six_leaves_and_a_different_address() {
            let (options, _, fixture) = load();
            let v2 = VhtlcScript::new_v2(options.clone(), None, Network::Regtest).unwrap();
            let v1 = VhtlcScript::new(options, Network::Regtest).unwrap();

            assert_eq!(v2.get_script_map().len(), 6);
            assert_eq!(hex(v2.claim_script()), fixture.leaves.claim);
            assert_ne!(hex(v2.script_pubkey()), fixture.pk_script);
            assert_ne!(v1.claim_script(), v2.claim_script());
            assert_ne!(v1.script_pubkey(), v2.script_pubkey());
        }

        #[test]
        fn rejects_non_p2tr_destinations() {
            let (options, mut non_interactive, _) = load();
            non_interactive.sender_pk_script =
                script("0014aabbccddeeff00112233445566778899aabbccdd");

            let err = VhtlcScript::new_v2(options, Some(non_interactive), Network::Regtest)
                .err()
                .unwrap();
            assert!(matches!(
                err,
                VhtlcError::InvalidP2trScript("sender_pk_script")
            ));
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

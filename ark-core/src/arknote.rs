//! ArkNote: a transferable off-chain value token.
//!
//! An ArkNote encodes a preimage and a value. Anyone who knows the preimage can
//! spend the note by revealing it. This enables simple bearer-token-style transfers.

use crate::intent;
use crate::script::arknote_script;
use crate::script::tr_script_pubkey;
use crate::Error;
use crate::UNSPENDABLE_KEY;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::key::Secp256k1;
use bitcoin::taproot::LeafVersion;
use bitcoin::taproot::TaprootBuilder;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::PublicKey;
use bitcoin::ScriptBuf;
use bitcoin::Sequence;
use bitcoin::TxOut;
use bitcoin::Txid;
use std::fmt;

/// Default human-readable prefix for ArkNote string encoding.
pub const DEFAULT_HRP: &str = "arknote";

/// Length of the preimage in bytes.
pub const PREIMAGE_LENGTH: usize = 32;

/// Length of the value field in bytes (u32 big-endian).
const VALUE_LENGTH: usize = 4;

/// Total length of an encoded ArkNote payload.
const ARKNOTE_LENGTH: usize = PREIMAGE_LENGTH + VALUE_LENGTH;

/// Fake outpoint vout used for ArkNotes (they don't correspond to real UTXOs).
pub const FAKE_VOUT: u32 = 0;

/// ArkNote is a bearer token that can be redeemed by revealing its preimage.
///
/// The note encodes:
/// - A 32-byte preimage (the secret)
/// - A value in satoshis (up to u32::MAX)
///
/// The on-chain representation is a hash-locked taproot script that checks
/// `SHA256(witness) == hash(preimage)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArkNote {
    preimage: [u8; PREIMAGE_LENGTH],
    value: Amount,
    hrp: String,
}

impl ArkNote {
    /// Create a new ArkNote with the default HRP.
    pub fn new(preimage: [u8; PREIMAGE_LENGTH], value: Amount) -> Result<Self, Error> {
        Self::new_with_hrp(preimage, value, DEFAULT_HRP.to_string())
    }

    /// Create a new ArkNote with a custom HRP.
    pub fn new_with_hrp(
        preimage: [u8; PREIMAGE_LENGTH],
        value: Amount,
        hrp: String,
    ) -> Result<Self, Error> {
        // Validate that value fits in u32
        if value.to_sat() > u32::MAX as u64 {
            return Err(Error::ad_hoc(format!(
                "value {} exceeds maximum of {} sats",
                value.to_sat(),
                u32::MAX
            )));
        }

        Ok(Self {
            preimage,
            value,
            hrp,
        })
    }

    /// Parse an ArkNote from its string representation.
    pub fn from_string(s: &str) -> Result<Self, Error> {
        Self::from_string_with_hrp(s, DEFAULT_HRP)
    }

    /// Parse an ArkNote from its string representation with a custom HRP.
    pub fn from_string_with_hrp(s: &str, hrp: &str) -> Result<Self, Error> {
        let s = s.trim();

        if !s.starts_with(hrp) {
            return Err(Error::ad_hoc(format!(
                "invalid prefix: expected '{}', got '{}'",
                hrp,
                &s[..hrp.len().min(s.len())]
            )));
        }

        let encoded = &s[hrp.len()..];
        let decoded = bs58::decode(encoded)
            .into_vec()
            .map_err(|e| Error::ad_hoc(format!("invalid base58: {e}")))?;

        if decoded.len() != ARKNOTE_LENGTH {
            return Err(Error::ad_hoc(format!(
                "invalid payload length: expected {}, got {}",
                ARKNOTE_LENGTH,
                decoded.len()
            )));
        }

        let mut preimage = [0u8; PREIMAGE_LENGTH];
        preimage.copy_from_slice(&decoded[..PREIMAGE_LENGTH]);

        let value_bytes: [u8; 4] = decoded[PREIMAGE_LENGTH..]
            .try_into()
            .map_err(|_| Error::ad_hoc("invalid value bytes"))?;
        let value = Amount::from_sat(u32::from_be_bytes(value_bytes) as u64);

        Self::new_with_hrp(preimage, value, hrp.to_string())
    }

    /// Encode the ArkNote to its string representation.
    pub fn to_encoded_string(&self) -> String {
        self.to_string()
    }

    /// Get the preimage.
    pub fn preimage(&self) -> &[u8; PREIMAGE_LENGTH] {
        &self.preimage
    }

    /// Get the preimage hash.
    pub fn preimage_hash(&self) -> sha256::Hash {
        sha256::Hash::hash(&self.preimage)
    }

    /// Get the value in satoshis.
    pub fn value(&self) -> Amount {
        self.value
    }

    /// Get the HRP.
    pub fn hrp(&self) -> &str {
        &self.hrp
    }

    /// Get the script that locks this note (spendable by revealing the preimage).
    pub fn script(&self) -> ScriptBuf {
        arknote_script(&self.preimage_hash())
    }

    /// Get a synthetic txid derived from the preimage hash.
    ///
    /// This is used to create a unique identifier for the note in the VTXO system.
    pub fn txid(&self) -> Txid {
        Txid::from_byte_array(*self.preimage_hash().as_byte_array())
    }

    /// Get a synthetic outpoint for this note.
    pub fn outpoint(&self) -> OutPoint {
        OutPoint::new(self.txid(), FAKE_VOUT)
    }

    /// Convert this ArkNote to an intent input for settlement.
    ///
    /// The note creates a fake VTXO with a hash-lock script. When settling,
    /// the preimage is revealed as the witness instead of a signature.
    pub fn to_intent_input(&self) -> Result<intent::Input, Error> {
        let secp = Secp256k1::new();

        let unspendable_key: PublicKey = UNSPENDABLE_KEY
            .parse()
            .map_err(|e| Error::ad_hoc(format!("invalid unspendable key: {e}")))?;
        let (unspendable_xonly, _) = unspendable_key.inner.x_only_public_key();

        let note_script = self.script();

        // Build taproot tree with single leaf (the note script)
        let spend_info = TaprootBuilder::new()
            .add_leaf(0, note_script.clone())
            .map_err(|e| Error::ad_hoc(format!("failed to add leaf: {e:?}")))?
            .finalize(&secp, unspendable_xonly)
            .map_err(|e| Error::ad_hoc(format!("failed to finalize taproot: {e:?}")))?;

        let control_block = spend_info
            .control_block(&(note_script.clone(), LeafVersion::TapScript))
            .ok_or_else(|| Error::ad_hoc("failed to get control block for note script"))?;

        let script_pubkey = tr_script_pubkey(&spend_info);

        Ok(intent::Input::new_with_extra_witness(
            self.outpoint(),
            Sequence::MAX,
            None,
            TxOut {
                value: self.value,
                script_pubkey,
            },
            vec![note_script.clone()],
            (note_script, control_block),
            false, // not onchain
            false, // not swept
            vec![self.preimage.to_vec()],
        ))
    }
}

impl fmt::Display for ArkNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut payload = Vec::with_capacity(ARKNOTE_LENGTH);
        payload.extend_from_slice(&self.preimage);
        payload.extend_from_slice(&(self.value.to_sat() as u32).to_be_bytes());

        write!(f, "{}{}", self.hrp, bs58::encode(payload).into_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_to_array32(hex: &str) -> [u8; 32] {
        let bytes = hex::decode(hex).expect("valid hex");
        bytes.try_into().expect("32 bytes")
    }

    #[test]
    fn roundtrip_encoding() {
        let preimage =
            hex_to_array32("11d2a03264d0efd311d2a03264d0efd311d2a03264d0efd311d2a03264d0efd3");
        let value = Amount::from_sat(900_000);

        let note = ArkNote::new(preimage, value).unwrap();
        let encoded = note.to_string();
        let decoded = ArkNote::from_string(&encoded).unwrap();

        assert_eq!(decoded.preimage(), &preimage);
        assert_eq!(decoded.value(), value);
    }

    #[test]
    fn test_vectors() {
        // Test vectors matching TypeScript SDK
        let cases = [
            (
                "arknote",
                "arknote8rFzGqZsG9RCLripA6ez8d2hQEzFKsqCeiSnXhQj56Ysw7ZQT",
                "11d2a03264d0efd311d2a03264d0efd311d2a03264d0efd311d2a03264d0efd3",
                900_000u64,
            ),
            (
                "arknote",
                "arknoteSkB92YpWm4Q2ijQHH34cqbKkCZWszsiQgHVjtNeFF2Cwp59D",
                "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20",
                1_828_932u64,
            ),
            (
                "noteark",
                "noteark8rFzGqZsG9RCLripA6ez8d2hQEzFKsqCeiSnXhQj56Ysw7ZQT",
                "11d2a03264d0efd311d2a03264d0efd311d2a03264d0efd311d2a03264d0efd3",
                900_000u64,
            ),
        ];

        for (hrp, note_str, preimage_hex, expected_sats) in cases {
            let note = ArkNote::from_string_with_hrp(note_str, hrp).unwrap();

            assert_eq!(note.preimage(), &hex_to_array32(preimage_hex));
            assert_eq!(note.value(), Amount::from_sat(expected_sats));
            assert_eq!(note.hrp(), hrp);

            // Roundtrip
            let reconstructed = ArkNote::new_with_hrp(
                hex_to_array32(preimage_hex),
                Amount::from_sat(expected_sats),
                hrp.to_string(),
            )
            .unwrap();
            assert_eq!(reconstructed.to_string(), note_str);
        }
    }

    #[test]
    fn invalid_prefix() {
        let result = ArkNote::from_string("wrongprefix123456789");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("invalid prefix"));
    }

    #[test]
    fn invalid_base58() {
        let result = ArkNote::from_string("arknote!!!invalid!!!");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("base58"));
    }

    #[test]
    fn value_overflow() {
        let preimage = [0u8; 32];
        let result = ArkNote::new(preimage, Amount::from_sat(u64::MAX));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("exceeds maximum"));
    }

    #[test]
    fn script_is_hash_lock() {
        let preimage = [0x42u8; 32];
        let note = ArkNote::new(preimage, Amount::from_sat(1000)).unwrap();
        let script = note.script();

        // Should be: OP_SHA256 <32-byte hash> OP_EQUAL
        let bytes = script.as_bytes();
        assert_eq!(bytes[0], bitcoin::opcodes::all::OP_SHA256.to_u8());
        assert_eq!(bytes[1], 0x20); // push 32 bytes
        assert_eq!(bytes[34], bitcoin::opcodes::all::OP_EQUAL.to_u8());
    }

    #[test]
    fn whitespace_handling() {
        let note_str = "  arknote8rFzGqZsG9RCLripA6ez8d2hQEzFKsqCeiSnXhQj56Ysw7ZQT  ";
        let note = ArkNote::from_string(note_str).unwrap();
        assert_eq!(note.value(), Amount::from_sat(900_000));
    }
}

use crate::proof_of_funds;
use crate::Error;
use crate::VirtualUtxoScript;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::key::Secp256k1;
use bitcoin::opcodes::all::*;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use bitcoin::Sequence;
use bitcoin::TxOut;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;

/// Default human-readable prefix for ArkNote string encoding
pub const DEFAULT_HRP: &str = "arknote";

/// Length of the preimage in bytes
pub const PREIMAGE_LENGTH: usize = 32;

/// Length of the value field in bytes
pub const VALUE_LENGTH: usize = 4;

/// Total length of an encoded ArkNote
pub const ARKNOTE_LENGTH: usize = PREIMAGE_LENGTH + VALUE_LENGTH;

/// Fake outpoint index used for ArkNotes
pub const FAKE_OUTPOINT_INDEX: u32 = 0;

/// Status of a coin/VTXO
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub confirmed: bool,
}

impl fmt::Display for ArkNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let encoded = self.encode();
        let value = format!("{}{}", self.hrp, bs58::encode(encoded).into_string());
        write!(f, "{value}")
    }
}

/// ArkNote is a fake VTXO coin that can be spent by revealing the preimage
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArkNote {
    preimage: [u8; PREIMAGE_LENGTH],
    value: Amount,
    hrp: String,
    // Computed fields
    txid: String,
    vtxo_script: VirtualUtxoScript,
    tap_tree_bytes: Vec<String>, // Cache for tap_tree() method
    status: Status,
    extra_witness: Vec<Vec<u8>>,
}

impl ArkNote {
    /// Create a new ArkNote with the given preimage and value
    pub fn new(preimage: [u8; PREIMAGE_LENGTH], value: Amount) -> Self {
        Self::new_with_hrp(preimage, value, DEFAULT_HRP.to_string())
    }

    /// Create a new ArkNote with a custom HRP
    pub fn new_with_hrp(preimage: [u8; PREIMAGE_LENGTH], value: Amount, hrp: String) -> Self {
        let preimage_hash = sha256::Hash::hash(&preimage);

        // Create the note tapscript
        let note_script = Self::note_tapscript(&preimage_hash);

        // Create the VTXO script structure using VirtualUtxoScript
        let secp = Secp256k1::new();
        let vtxo_script = VirtualUtxoScript::new(&secp, vec![note_script])
            .expect("failed to create VirtualUtxoScript");

        // Create fake txid from reversed preimage hash
        let mut txid_bytes = preimage_hash.to_byte_array();
        txid_bytes.reverse();
        let txid = hex::encode(txid_bytes);

        // Convert the encoded hex strings to bytes for tap_tree_bytes
        let encoded_scripts = vtxo_script.encode();

        ArkNote {
            preimage,
            value,
            hrp,
            txid,
            vtxo_script,
            tap_tree_bytes: encoded_scripts,
            status: Status { confirmed: true },
            extra_witness: vec![preimage.to_vec()],
        }
    }

    /// Get the note value
    pub fn value(&self) -> Amount {
        self.value
    }

    /// Get the preimage
    pub fn preimage(&self) -> &[u8; PREIMAGE_LENGTH] {
        &self.preimage
    }

    /// Get the HRP
    pub fn hrp(&self) -> &str {
        &self.hrp
    }

    /// Get the txid
    pub fn txid(&self) -> &str {
        &self.txid
    }

    /// Get the vout (always returns FAKE_OUTPOINT_INDEX)
    pub fn vout(&self) -> u32 {
        FAKE_OUTPOINT_INDEX
    }

    /// Get the status
    pub fn status(&self) -> &Status {
        &self.status
    }

    /// Get the extra witness
    pub fn extra_witness(&self) -> Option<&[Vec<u8>]> {
        Some(&self.extra_witness)
    }

    /// Get the tap tree
    pub fn tap_tree(&self) -> Vec<String> {
        self.tap_tree_bytes.clone()
    }

    /// Get the forfeit tap leaf script
    pub fn forfeit_tap_leaf_script(&self) -> &ScriptBuf {
        // The note script is the first (and only) script in our VirtualUtxoScript
        &self.vtxo_script.scripts()[0]
    }

    /// Get the intent tap leaf script
    pub fn intent_tap_leaf_script(&self) -> &ScriptBuf {
        // For ArkNote, forfeit and intent scripts are the same
        &self.vtxo_script.scripts()[0]
    }

    /// Get the underlying VirtualUtxoScript
    pub fn vtxo_script(&self) -> &VirtualUtxoScript {
        &self.vtxo_script
    }

    /// Encode the ArkNote to bytes
    pub fn encode(&self) -> Vec<u8> {
        let mut result = Vec::with_capacity(ARKNOTE_LENGTH);
        result.extend_from_slice(&self.preimage);
        // Use big-endian to match TypeScript's writeUInt32BE
        result.extend_from_slice(&(self.value.to_sat() as u32).to_be_bytes());
        result
    }

    /// Decode bytes into an ArkNote
    pub fn decode(data: &[u8]) -> Result<Self, Error> {
        Self::decode_with_hrp(data, DEFAULT_HRP)
    }

    /// Decode bytes into an ArkNote with custom HRP
    pub fn decode_with_hrp(data: &[u8], hrp: &str) -> Result<Self, Error> {
        if data.len() != ARKNOTE_LENGTH {
            return Err(Error::ad_hoc(format!(
                "invalid data length: expected {} bytes, got {}",
                ARKNOTE_LENGTH,
                data.len()
            )));
        }

        let mut preimage = [0u8; PREIMAGE_LENGTH];
        preimage.copy_from_slice(&data[..PREIMAGE_LENGTH]);

        let value_bytes = &data[PREIMAGE_LENGTH..];
        let value = u32::from_be_bytes([
            value_bytes[0],
            value_bytes[1],
            value_bytes[2],
            value_bytes[3],
        ]);

        Ok(Self::new_with_hrp(
            preimage,
            Amount::from_sat(value as u64),
            hrp.to_string(),
        ))
    }

    /// Parse an ArkNote from a string
    pub fn from_string(note_str: &str) -> Result<Self, Error> {
        Self::from_string_with_hrp(note_str, DEFAULT_HRP)
    }

    /// Parse an ArkNote from a string with custom HRP
    pub fn from_string_with_hrp(note_str: &str, hrp: &str) -> Result<Self, Error> {
        let note_str = note_str.trim();
        if !note_str.starts_with(hrp) {
            return Err(Error::ad_hoc(format!(
                "invalid human-readable part: expected {} prefix (note '{}')",
                hrp, note_str
            )));
        }

        let encoded = &note_str[hrp.len()..];
        let decoded = bs58::decode(encoded)
            .into_vec()
            .map_err(|e| Error::ad_hoc(format!("failed to decode base58: {}", e)))?;

        if decoded.is_empty() {
            return Err(Error::ad_hoc("failed to decode base58 string".to_string()));
        }

        Self::decode_with_hrp(&decoded, hrp)
    }

    /// Get the outpoint for this ArkNote
    pub fn outpoint(&self) -> OutPoint {
        let txid = bitcoin::Txid::from_slice(&hex::decode(&self.txid).expect("valid hex string"))
            .expect("valid txid");
        OutPoint::new(txid, FAKE_OUTPOINT_INDEX)
    }

    /// Convert to a TxOut
    pub fn to_tx_out(&self) -> TxOut {
        let script_pubkey = self.vtxo_script.script_pubkey();
        TxOut {
            value: self.value,
            script_pubkey,
        }
    }

    /// Create a note tapscript that checks the preimage hash
    pub fn note_tapscript(preimage_hash: &sha256::Hash) -> ScriptBuf {
        ScriptBuf::builder()
            .push_opcode(OP_SHA256)
            .push_slice(preimage_hash.as_byte_array())
            .push_opcode(OP_EQUAL)
            .into_script()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde::Serialize;

    #[derive(Debug, Serialize, Deserialize)]
    struct TestVectors {
        address: AddressTestVectors,
        note: NoteTestVectors,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct AddressTestVectors {
        valid: Vec<AddressValidTest>,
        invalid: Vec<AddressInvalidTest>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct AddressValidTest {
        addr: String,
        #[serde(rename = "expectedVersion")]
        expected_version: u8,
        #[serde(rename = "expectedPrefix")]
        expected_prefix: String,
        #[serde(rename = "expectedUserKey")]
        expected_user_key: String,
        #[serde(rename = "expectedServerKey")]
        expected_server_key: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct AddressInvalidTest {
        addr: String,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct NoteTestVectors {
        valid: Vec<NoteValidTest>,
        invalid: Vec<NoteInvalidTest>,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct NoteValidTest {
        hrp: String,
        str: String,
        #[serde(rename = "expectedPreimage")]
        expected_preimage: String,
        #[serde(rename = "expectedValue")]
        expected_value: u64,
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct NoteInvalidTest {
        str: String,
    }

    // Helper function for converting hex to bytes
    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

    // Helper function for converting hex to 32-byte array
    fn hex_to_array32(hex: &str) -> [u8; 32] {
        let bytes = hex_to_bytes(hex);
        let mut array = [0u8; 32];
        array.copy_from_slice(&bytes);
        array
    }

    #[test]
    fn test_arknote_test_vectors() {
        // First test with hardcoded test vectors for reliable testing
        let test_cases = vec![
            // Test case 1: Default HRP
            (
                "arknote",
                "arknote8rFzGqZsG9RCLripA6ez8d2hQEzFKsqCeiSnXhQj56Ysw7ZQT",
                "11d2a03264d0efd311d2a03264d0efd311d2a03264d0efd311d2a03264d0efd3",
                900000_u64,
            ),
            // Test case 2: Default HRP with different values
            (
                "arknote",
                "arknoteSkB92YpWm4Q2ijQHH34cqbKkCZWszsiQgHVjtNeFF2Cwp59D",
                "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20",
                1828932_u64,
            ),
            // Test case 3: Custom HRP
            (
                "noteark",
                "noteark8rFzGqZsG9RCLripA6ez8d2hQEzFKsqCeiSnXhQj56Ysw7ZQT",
                "11d2a03264d0efd311d2a03264d0efd311d2a03264d0efd311d2a03264d0efd3",
                900000_u64,
            ),
            // Test case 4: Custom HRP with different values
            (
                "noteark",
                "notearkSkB92YpWm4Q2ijQHH34cqbKkCZWszsiQgHVjtNeFF2Cwp59D",
                "0102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f20",
                1828932_u64,
            ),
        ];

        for (i, (hrp, note_str, expected_preimage_hex, expected_value)) in
            test_cases.iter().enumerate()
        {
            // Parse the note from string
            let parsed_note = ArkNote::from_string_with_hrp(note_str, hrp).unwrap();

            // Convert expected preimage from hex
            let expected_preimage = hex_to_array32(expected_preimage_hex);

            // Validate preimage
            assert_eq!(
                parsed_note.preimage(),
                &expected_preimage,
                "Preimage mismatch for test case {}",
                i + 1
            );

            // Validate value
            let expected_value = Amount::from_sat(*expected_value);
            assert_eq!(
                parsed_note.value(),
                expected_value,
                "Value mismatch for test case {}",
                i + 1
            );

            // Validate HRP
            assert_eq!(
                parsed_note.hrp(),
                *hrp,
                "HRP mismatch for test case {}",
                i + 1
            );

            // Test round-trip: create note from expected values and verify string matches
            let reconstructed_note =
                ArkNote::new_with_hrp(expected_preimage, expected_value, hrp.to_string());
            let reconstructed_string = reconstructed_note.to_string();
            assert_eq!(
                reconstructed_string,
                *note_str,
                "Round-trip string mismatch for test case {}",
                i + 1
            );
        }
    }

    #[test]
    fn test_arknote_test_vectors_from_json() {
        // Try to load test vectors from JSON file, skip test if file not found
        let test_vectors_result = std::fs::read_to_string("test_vectors.json");

        if test_vectors_result.is_err() {
            // Skip test if JSON file not found
            return;
        }

        let test_vectors_json = test_vectors_result.unwrap();
        let test_vectors: TestVectors =
            serde_json::from_str(&test_vectors_json).expect("Failed to parse test_vectors.json");

        // Verify we have the expected number of test cases
        assert!(
            !test_vectors.note.valid.is_empty(),
            "Should have valid test cases"
        );
        assert!(
            !test_vectors.note.invalid.is_empty(),
            "Should have invalid test cases"
        );

        // Test valid notes
        for (i, test_case) in test_vectors.note.valid.iter().enumerate() {
            // Parse the note from string
            let parsed_note = ArkNote::from_string_with_hrp(&test_case.str, &test_case.hrp)
                .unwrap_or_else(|e| panic!("Failed to parse note for test case {}: {}", i + 1, e));

            // Validate preimage
            let expected_preimage = hex_to_array32(&test_case.expected_preimage);
            assert_eq!(
                parsed_note.preimage(),
                &expected_preimage,
                "Preimage mismatch for test case {}",
                i + 1
            );

            // Validate value
            let expected_value = Amount::from_sat(test_case.expected_value);
            assert_eq!(
                parsed_note.value(),
                expected_value,
                "Value mismatch for test case {}",
                i + 1
            );

            // Validate HRP
            assert_eq!(
                parsed_note.hrp(),
                test_case.hrp,
                "HRP mismatch for test case {}",
                i + 1
            );

            // Validate that the string starts with the HRP (like TypeScript test)
            assert!(
                test_case.str.starts_with(&test_case.hrp),
                "String should start with HRP '{}' for test case {}",
                test_case.hrp,
                i + 1
            );

            // Validate that the HRP length matches the prefix length
            let hrp_len = test_case.hrp.len();
            assert_eq!(
                &test_case.str[..hrp_len],
                test_case.hrp,
                "String prefix should match HRP for test case {}",
                i + 1
            );

            // Test encoding: create note from expected values and verify string matches (TypeScript
            // pattern)
            let new_note =
                ArkNote::new_with_hrp(expected_preimage, expected_value, test_case.hrp.clone());
            let encoded_string = new_note.to_string();
            assert_eq!(
                encoded_string,
                test_case.str,
                "Encoded string mismatch for test case {}",
                i + 1
            );

            // Test decode-then-encode pattern (matching TypeScript test exactly)
            let decoded_note = ArkNote::from_string_with_hrp(&test_case.str, &test_case.hrp)
                .unwrap_or_else(|e| panic!("Failed to decode note for test case {}: {}", i + 1, e));

            let new_note_from_decoded = ArkNote::new_with_hrp(
                *decoded_note.preimage(),
                decoded_note.value(),
                decoded_note.hrp().to_string(),
            );

            let encoded_back = new_note_from_decoded.to_string();
            assert_eq!(
                encoded_back,
                test_case.str,
                "Decode-then-encode pattern failed for test case {}",
                i + 1
            );

            // Test round-trip: create note from expected values and verify string matches
            let reconstructed_note =
                ArkNote::new_with_hrp(expected_preimage, expected_value, test_case.hrp.clone());
            let reconstructed_string = reconstructed_note.to_string();
            assert_eq!(
                reconstructed_string,
                test_case.str,
                "Round-trip string mismatch for test case {}",
                i + 1
            );

            // Additional comprehensive assertions
            assert!(
                parsed_note.status().confirmed,
                "Status should be confirmed for test case {}",
                i + 1
            );
            assert_eq!(
                parsed_note.vout(),
                0,
                "Vout should be 0 for test case {}",
                i + 1
            );
            assert!(
                parsed_note.extra_witness().is_some(),
                "Extra witness should exist for test case {}",
                i + 1
            );
            assert_eq!(
                parsed_note.extra_witness().unwrap().len(),
                1,
                "Should have exactly one witness for test case {}",
                i + 1
            );
            assert_eq!(
                parsed_note.extra_witness().unwrap()[0],
                expected_preimage.to_vec(),
                "Witness should match preimage for test case {}",
                i + 1
            );

            // Verify VirtualUtxoScript properties
            let vtxo_script = parsed_note.vtxo_script();
            assert_eq!(
                vtxo_script.scripts().len(),
                1,
                "Should have exactly one script for test case {}",
                i + 1
            );
            assert_eq!(
                parsed_note.forfeit_tap_leaf_script(),
                parsed_note.intent_tap_leaf_script(),
                "Forfeit and intent scripts should be the same for test case {}",
                i + 1
            );

            // Verify tap tree is not empty
            let tap_tree = parsed_note.tap_tree();
            assert!(
                !tap_tree.is_empty(),
                "Tap tree should not be empty for test case {}",
                i + 1
            );

            // Verify txid format (should be valid hex)
            let txid = parsed_note.txid();
            assert_eq!(
                txid.len(),
                64,
                "TXID should be 64 characters for test case {}",
                i + 1
            );
            assert!(
                txid.chars().all(|c| c.is_ascii_hexdigit()),
                "TXID should be valid hex for test case {}",
                i + 1
            );

            // Verify outpoint creation
            let outpoint = parsed_note.outpoint();
            assert_eq!(
                outpoint.vout,
                0,
                "Outpoint vout should be 0 for test case {}",
                i + 1
            );

            // Verify TxOut creation
            let tx_out = parsed_note.to_tx_out();
            assert_eq!(
                tx_out.value,
                expected_value,
                "TxOut value should match for test case {}",
                i + 1
            );
            assert_eq!(
                tx_out.script_pubkey,
                vtxo_script.script_pubkey(),
                "TxOut script should match VirtualUtxoScript for test case {}",
                i + 1
            );

            // Verify encoding/decoding consistency
            let encoded = parsed_note.encode();
            assert_eq!(
                encoded.len(),
                ARKNOTE_LENGTH,
                "Encoded length should be correct for test case {}",
                i + 1
            );
            let decoded = ArkNote::decode(&encoded).unwrap();
            assert_eq!(
                decoded.preimage(),
                &expected_preimage,
                "Decode should preserve preimage for test case {}",
                i + 1
            );
            assert_eq!(
                decoded.value(),
                expected_value,
                "Decode should preserve value for test case {}",
                i + 1
            );
        }

        // Test invalid notes
        for (i, test_case) in test_vectors.note.invalid.iter().enumerate() {
            // Try to parse with default HRP - should fail
            let result = ArkNote::from_string(&test_case.str);
            assert!(
                result.is_err(),
                "Expected parsing to fail for invalid test case {}: {}",
                i + 1,
                test_case.str
            );

            // Ensure specific error types for known cases
            let error_msg = result.unwrap_err().to_string();
            if test_case.str == "arknoteshort" {
                assert!(
                    error_msg.contains("invalid data length"),
                    "Short note should fail with data length error for test case {}",
                    i + 1
                );
            }

            if test_case.str.starts_with("wrongprefix") {
                assert!(
                    error_msg.contains("invalid human-readable part"),
                    "Wrong prefix should fail with HRP error for test case {}",
                    i + 1
                );
            }
        }
    }
}

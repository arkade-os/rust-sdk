use ark_lightning::vhtlc::VhtlcOptions;
use ark_lightning::vhtlc::VhtlcScript;
use bitcoin::PublicKey;
use bitcoin::Sequence;
use bitcoin::XOnlyPublicKey;
use serde::Deserialize;
use serde::Serialize;
use std::fs;
use std::str::FromStr;

#[derive(Debug, Deserialize, Serialize)]
struct Fixtures {
    valid: Vec<TestCase>,
    invalid: Vec<TestCase>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TestCase {
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
    expected: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Delay {
    #[serde(rename = "type")]
    delay_type: String,
    value: u32,
}

impl Delay {
    fn to_sequence(&self) -> Sequence {
        match self.delay_type.as_str() {
            "blocks" => Sequence::from_height(self.value as u16),
            "seconds" => Sequence::from_seconds_ceil(self.value as u32).unwrap(),
            _ => panic!("Unknown delay type: {}", self.delay_type),
        }
    }
}

fn hex_to_bytes20(hex: &str) -> Result<[u8; 20], String> {
    let bytes = hex::decode(hex).map_err(|e| format!("Invalid hex: {}", e))?;
    if bytes.len() != 20 {
        return Err(format!("Expected 20 bytes, got {}", bytes.len()));
    }
    let mut arr = [0u8; 20];
    arr.copy_from_slice(&bytes);
    Ok(arr)
}

fn pubkey_to_xonly(pubkey_hex: &str) -> XOnlyPublicKey {
    let pubkey = PublicKey::from_str(pubkey_hex).expect("Invalid public key");
    XOnlyPublicKey::from(pubkey.inner)
}

#[test]
fn test_vhtlc_with_valid_fixtures() {
    let fixtures_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vhtlc.json");
    let fixtures_json = fs::read_to_string(fixtures_path).expect("Failed to read fixtures file");
    let fixtures: Fixtures =
        serde_json::from_str(&fixtures_json).expect("Failed to parse fixtures");

    for test_case in fixtures.valid {
        let preimage_hash = hex_to_bytes20(&test_case.preimage_hash)
            .expect("Valid fixtures should have valid preimage hash");

        let options = VhtlcOptions {
            sender: pubkey_to_xonly(&test_case.sender),
            receiver: pubkey_to_xonly(&test_case.receiver),
            server: pubkey_to_xonly(&test_case.server),
            preimage_hash,
            refund_locktime: test_case.refund_locktime,
            unilateral_claim_delay: test_case.unilateral_claim_delay.to_sequence(),
            unilateral_refund_delay: test_case.unilateral_refund_delay.to_sequence(),
            unilateral_refund_without_receiver_delay: test_case
                .unilateral_refund_without_receiver_delay
                .to_sequence(),
        };

        let vhtlc = VhtlcScript::new(options).expect("Failed to create VHTLC");
        let addr = vhtlc.address(
            bitcoin::Network::Regtest,
            pubkey_to_xonly(&test_case.server),
        );
        assert!(addr.is_some(), "VHTLC should have an address");
        let addr = addr.unwrap();
        if let Some(expected) = &test_case.expected {
            assert_eq!(
                addr.to_string(),
                *expected,
                "VHTLC address should match expected"
            );
        }
    }
}

#[test]
fn test_vhtlc_with_invalid_fixtures() {
    let fixtures_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vhtlc.json");
    let fixtures_json = fs::read_to_string(fixtures_path).expect("Failed to read fixtures file");
    let fixtures: Fixtures =
        serde_json::from_str(&fixtures_json).expect("Failed to parse fixtures");

    for test_case in fixtures.invalid {
        // Try to parse preimage hash - some invalid test cases may have invalid hex
        let preimage_hash_result = hex_to_bytes20(&test_case.preimage_hash);

        // If preimage hash parsing fails, that's expected for some invalid cases
        if preimage_hash_result.is_err() {
            continue;
        }

        let options = VhtlcOptions {
            sender: pubkey_to_xonly(&test_case.sender),
            receiver: pubkey_to_xonly(&test_case.receiver),
            server: pubkey_to_xonly(&test_case.server),
            preimage_hash: preimage_hash_result.unwrap(),
            refund_locktime: test_case.refund_locktime,
            unilateral_claim_delay: test_case.unilateral_claim_delay.to_sequence(),
            unilateral_refund_delay: test_case.unilateral_refund_delay.to_sequence(),
            unilateral_refund_without_receiver_delay: test_case
                .unilateral_refund_without_receiver_delay
                .to_sequence(),
        };

        let vhtlc = VhtlcScript::new(options);
        assert!(
            vhtlc.is_err(),
            "Expected error for invalid fixture: {}",
            test_case.description
        );
    }
}

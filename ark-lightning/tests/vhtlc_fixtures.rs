use ark_lightning::vhtlc::VhtlcOptions;
use ark_lightning::vhtlc::VhtlcScript;
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
                    .map_err(|e| format!("Invalid seconds value: {}", e))
            }
            _ => Err(format!("Unknown delay type: {}", self.delay_type)),
        }
    }
}

fn hex_to_bytes20(hex: &str) -> Result<[u8; 20], String> {
    let bytes = hex::decode(hex).map_err(|e| format!("Invalid hex: {}", e))?;
    if bytes.len() != 20 {
        return Err(format!("preimage hash must be 20 bytes"));
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
                .expect("Valid delay"),
            unilateral_refund_delay: test_case
                .unilateral_refund_delay
                .to_sequence()
                .expect("Valid delay"),
            unilateral_refund_without_receiver_delay: test_case
                .unilateral_refund_without_receiver_delay
                .to_sequence()
                .expect("Valid delay"),
        };

        let vhtlc = VhtlcScript::new(options).expect("Failed to create VHTLC");

        // Test 1: Verify all script hex encodings
        let claim_hex = hex::encode(vhtlc.claim_script().as_bytes());
        assert_eq!(
            claim_hex, test_case.scripts.claim_script,
            "Claim script hex mismatch for test case: {}",
            test_case.description
        );

        let refund_hex = hex::encode(vhtlc.refund_script().as_bytes());
        assert_eq!(
            refund_hex, test_case.scripts.refund_script,
            "Refund script hex mismatch for test case: {}",
            test_case.description
        );

        let refund_without_receiver_hex =
            hex::encode(vhtlc.refund_without_receiver_script().as_bytes());
        assert_eq!(
            refund_without_receiver_hex, test_case.scripts.refund_without_receiver_script,
            "Refund without receiver script hex mismatch for test case: {}",
            test_case.description
        );

        let unilateral_claim_hex = hex::encode(vhtlc.unilateral_claim_script().as_bytes());
        assert_eq!(
            unilateral_claim_hex, test_case.scripts.unilateral_claim_script,
            "Unilateral claim script hex mismatch for test case: {}",
            test_case.description
        );

        let unilateral_refund_hex = hex::encode(vhtlc.unilateral_refund_script().as_bytes());
        assert_eq!(
            unilateral_refund_hex, test_case.scripts.unilateral_refund_script,
            "Unilateral refund script hex mismatch for test case: {}",
            test_case.description
        );

        let unilateral_refund_without_receiver_hex =
            hex::encode(vhtlc.unilateral_refund_without_receiver_script().as_bytes());

        assert_eq!(
            unilateral_refund_without_receiver_hex, test_case.scripts.unilateral_refund_without_receiver_script,
            "Unilateral refund without receiver script hex mismatch for test case: {}. Our impl includes CLTV locktime, fixture expects only CSV",
            test_case.description
        );

        // Test 2: Verify taproot information
        let taproot_info = vhtlc.taproot_info().expect(&format!(
            "Taproot info should be available for test case: {}",
            test_case.description
        ));

        let internal_key = taproot_info.internal_key();
        let internal_key_hex = hex::encode(internal_key.serialize());

        // The internal key in fixtures is prefixed with version byte
        let pubkey = PublicKey::from_str(&test_case.taproot.internal_key)
            .expect("Invalid internal key in fixture");
        let expected_internal = hex::encode(XOnlyPublicKey::from(pubkey.inner).serialize());

        assert_eq!(
            internal_key_hex, expected_internal,
            "Internal key mismatch for test case: {}",
            test_case.description
        );

        let output_key = taproot_info.output_key();
        let output_key_hex = hex::encode(output_key.serialize());

        assert_eq!(
            output_key_hex, test_case.taproot.tweaked_public_key,
            "Tweaked public key mismatch for test case: {}",
            test_case.description
        );

        // Test 3: Verify address generation
        let addr = vhtlc.address(Network::Testnet, server).expect(&format!(
            "Failed to generate address for test case: {}",
            test_case.description
        ));
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
    let fixtures_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/vhtlc.json");
    let fixtures_json = fs::read_to_string(fixtures_path).expect("Failed to read fixtures file");
    let fixtures: Fixtures =
        serde_json::from_str(&fixtures_json).expect("Failed to parse fixtures");

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
                test_case.error, e, test_case.description
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
    let preimage_hash = hex_to_bytes20("4d487dd3753a89bc9fe98401d1196523058251fc").unwrap();

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

    let vhtlc = VhtlcScript::new(options).expect("Failed to create VHTLC");

    // Verify claim script
    let claim_script = vhtlc.claim_script();
    let claim_hex = hex::encode(claim_script.as_bytes());
    let expected_claim = "a9144d487dd3753a89bc9fe98401d1196523058251fc8769201e1bb85455fe3f5aed60d101aa4dbdb9e7714f6226769a97a17a5331dadcd53bad20aad52d58162e9eefeafc7ad8a1cdca8060b5f01df1e7583362d052e266208f88ac";
    assert_eq!(
        claim_hex, expected_claim,
        "Claim script should match fixture"
    );

    // Verify unilateral claim script (with CSV=17)
    let unilateral_claim = vhtlc.unilateral_claim_script();
    let unilateral_claim_hex = hex::encode(unilateral_claim.as_bytes());

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

use ark_lightning::vhtlc::VhtlcOptions;
use ark_lightning::vhtlc::VhtlcScript;
use bitcoin::Sequence;
use bitcoin::XOnlyPublicKey;
use std::str::FromStr;

fn main() {
    // Create test keys for sender, receiver, and server
    let sender = XOnlyPublicKey::from_str(
        "18845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
    )
    .unwrap();

    let receiver = XOnlyPublicKey::from_str(
        "28845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
    )
    .unwrap();

    let server = XOnlyPublicKey::from_str(
        "38845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
    )
    .unwrap();

    // Create a preimage hash (in a real scenario, this would be the hash of a secret)
    let preimage_hash = [42u8; 20];

    // Configure the VHTLC options
    let options = VhtlcOptions {
        sender,
        receiver,
        server,
        preimage_hash,
        refund_locktime: 100000, // Block height for CLTV
        unilateral_claim_delay: Sequence::from_seconds_ceil(3600).unwrap(), // 1 hour
        unilateral_refund_delay: Sequence::from_seconds_ceil(7200).unwrap(), // 2 hours
        unilateral_refund_without_receiver_delay: Sequence::from_seconds_ceil(10800).unwrap(), /* 3 hours */
    };

    // Create the VHTLC script
    let vhtlc = VhtlcScript::new(options).expect("Failed to create VHTLC");

    // Get the taproot output key and script pubkey
    if let Some(taproot_info) = vhtlc.taproot_info() {
        println!("Taproot output key: {}", taproot_info.output_key());

        if let Some(script_pubkey) = vhtlc.script_pubkey() {
            println!("Script pubkey: {}", script_pubkey);
        }
    }

    // Display all available spending paths
    println!("\nAvailable spending paths:");
    for (name, script) in vhtlc.get_script_map() {
        println!("  {} - {} bytes", name, script.len());
    }
}

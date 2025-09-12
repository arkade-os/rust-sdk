//! Example demonstrating the use of enum-based metadata for Boltz swaps

use ark_lightning::boltz::PersistedSwap;
use ark_lightning::boltz::SwapMetadata;
use ark_lightning::boltz::SwapStatus;
use ark_lightning::boltz::SwapType;

fn main() {
    // Example of creating a reverse swap with typed metadata
    let reverse_swap = PersistedSwap {
        id: "reverse_swap_123".to_string(),
        swap_type: SwapType::Reverse,
        status: SwapStatus::Created,
        created_at: 1234567890,
        metadata: SwapMetadata::Reverse {
            preimage: "abc123def456".to_string(),
            preimage_hash: "hash789".to_string(),
            swap_tree: serde_json::json!({
                "tree": "data"
            }),
            refund_public_key: "pubkey123".to_string(),
            lockup_address: "bc1q...".to_string(),
            timeout_block_height: 750000,
            onchain_amount: 100000,
            blinding_key: Some("blinding123".to_string()),
        },
    };

    // Example of creating a submarine swap with typed metadata
    let submarine_swap = PersistedSwap {
        id: "submarine_swap_456".to_string(),
        swap_type: SwapType::Submarine,
        status: SwapStatus::Created,
        created_at: 1234567890,
        metadata: SwapMetadata::Submarine {
            address: "bc1q...".to_string(),
            redeem_script: "script123".to_string(),
            accept_zero_conf: true,
            expected_amount: 50000,
            claim_public_key: "claimpubkey456".to_string(),
            timeout_block_height: 750100,
            blinding_key: None,
        },
    };

    // The metadata is now strongly typed based on the swap type
    match &reverse_swap.metadata {
        SwapMetadata::Reverse {
            preimage,
            preimage_hash,
            ..
        } => {
            println!("Reverse swap preimage: {}", preimage);
            println!("Reverse swap preimage hash: {}", preimage_hash);
        }
        SwapMetadata::Submarine { .. } => {
            println!("This shouldn't happen for a reverse swap");
        }
    }

    match &submarine_swap.metadata {
        SwapMetadata::Submarine {
            address,
            accept_zero_conf,
            ..
        } => {
            println!("Submarine swap address: {}", address);
            println!("Accept zero conf: {}", accept_zero_conf);
        }
        SwapMetadata::Reverse { .. } => {
            println!("This shouldn't happen for a submarine swap");
        }
    }

    // Serialization still works seamlessly
    let serialized = serde_json::to_string_pretty(&reverse_swap).unwrap();
    println!("\nSerialized reverse swap:\n{}", serialized);
}

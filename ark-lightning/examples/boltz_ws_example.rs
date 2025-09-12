//! Example demonstrating WebSocket-based Boltz swap monitoring
//!
//! This example shows how to:
//! - Create submarine and reverse swaps
//! - Monitor swap status updates via WebSocket without blocking
//! - Persist swap data for recovery

use anyhow::Result;
use ark_lightning::boltz::BoltzLightning;
use ark_lightning::boltz::Network;
use ark_lightning::boltz_ws::SwapUpdate;

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize BoltzLightning with WebSocket support
    let boltz = BoltzLightning::new(Network::Testnet).await?;

    // Example 1: Create a submarine swap and monitor its status
    println!("Creating submarine swap...");
    let swap_id = "test_submarine_123";

    // Register a callback that will be called whenever the swap status changes
    boltz
        .register_swap_callback(swap_id.to_string(), |update: SwapUpdate| {
            println!(
                "Submarine swap {} status changed to: {:?}",
                update.id, update.status
            );

            match update.status {
                ark_lightning::boltz_ws::SwapStatus::TransactionMempool => {
                    println!("Transaction detected in mempool!");
                }
                ark_lightning::boltz_ws::SwapStatus::TransactionConfirmed => {
                    println!("Transaction confirmed! Can claim the swap.");
                }
                ark_lightning::boltz_ws::SwapStatus::SwapExpired => {
                    println!("Swap expired! Need to refund.");
                }
                _ => {}
            }
        })
        .await;

    // Example 2: Check persisted swap status
    if let Some(swap) = boltz.get_swap_status_from_cache(swap_id).await {
        println!("Retrieved persisted swap: {:?}", swap);
        println!("Swap created at: {}", swap.created_at);
        println!("Current status: {:?}", swap.status);
    }

    // Example 3: Clean up completed swap
    println!("Cleaning up swap...");
    boltz.cleanup_swap(swap_id).await?;

    // Example 4: Keep connection alive with periodic pings
    // In a real application, you would want to keep this running
    // For this example, we'll just demonstrate one ping
    if let Err(e) = boltz.ping_ws().await {
        eprintln!("Failed to ping WebSocket: {}", e);
    }

    println!("WebSocket monitoring example completed!");
    Ok(())
}

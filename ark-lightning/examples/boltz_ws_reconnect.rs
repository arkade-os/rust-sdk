//! Example demonstrating WebSocket reconnection and resilience
//!
//! This example shows how the WebSocket client handles:
//! - Automatic reconnection with exponential backoff
//! - Re-subscription to swaps after reconnection
//! - Message queuing during disconnection
//! - Connection state monitoring

use anyhow::Result;
use ark_lightning::boltz::BoltzLightning;
use ark_lightning::boltz::Network;
use ark_lightning::boltz_ws::ConnectionState;
use ark_lightning::boltz_ws::SwapUpdate;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<()> {
    println!("Starting Boltz WebSocket with reconnection demo...");

    // Initialize BoltzLightning with WebSocket support
    let boltz = Arc::new(BoltzLightning::new(Network::Testnet).await?);

    // Monitor connection state
    tokio::spawn({
        let boltz = boltz.clone();
        async move {
            let mut last_state = ConnectionState::Disconnected;

            loop {
                let current_state = boltz.get_ws_connection_state().await;

                if current_state != last_state {
                    println!(
                        "Connection state changed: {:?} -> {:?}",
                        last_state, current_state
                    );
                    last_state = current_state;

                    match current_state {
                        ConnectionState::Connected => {
                            println!("✅ WebSocket connected and ready");
                        }
                        ConnectionState::Disconnected => {
                            println!("❌ WebSocket disconnected");
                        }
                        ConnectionState::Connecting => {
                            println!("🔄 Establishing WebSocket connection...");
                        }
                        ConnectionState::Reconnecting => {
                            println!("🔄 Reconnecting to WebSocket...");
                        }
                    }
                }

                sleep(Duration::from_secs(1)).await;
            }
        }
    });

    // Example swap monitoring with resilient callback
    let swap_id = "test_swap_123";

    boltz
        .register_swap_callback(swap_id.to_string(), |update: SwapUpdate| {
            println!(
                "Received update for swap {}: {:?}",
                update.id, update.status
            );

            // This callback will continue to work even after reconnections
            match update.status {
                ark_lightning::boltz_ws::SwapStatus::TransactionMempool => {
                    println!("  → Transaction in mempool");
                }
                ark_lightning::boltz_ws::SwapStatus::TransactionConfirmed => {
                    println!("  → Transaction confirmed!");
                }
                ark_lightning::boltz_ws::SwapStatus::Error { ref error } => {
                    println!("  → Error: {}", error);
                }
                _ => {}
            }
        })
        .await;

    // Subscribe to the swap
    match boltz.subscribe_to_swap(swap_id.to_string()).await {
        Ok(_) => println!("Subscribed to swap {}", swap_id),
        Err(e) => println!("Subscription queued (will retry on reconnection): {}", e),
    }

    // Simulate some operations that might happen during disconnection
    println!("\nSimulating operations during potential disconnection...");

    for i in 0..5 {
        sleep(Duration::from_secs(3)).await;

        // Check connection status
        if boltz.is_ws_connected().await {
            println!("Iteration {}: Connected ✓", i);

            // Try to ping
            if let Err(e) = boltz.ping_ws().await {
                println!("  Ping failed: {}", e);
            } else {
                println!("  Ping successful");
            }
        } else {
            println!("Iteration {}: Disconnected - messages will be queued", i);

            // This subscription will be queued and sent when reconnected
            let new_swap_id = format!("queued_swap_{}", i);
            match boltz.subscribe_to_swap(new_swap_id.clone()).await {
                Ok(_) => println!("  Subscribed to {}", new_swap_id),
                Err(e) => println!("  Subscription for {} queued: {}", new_swap_id, e),
            }
        }
    }

    // Demonstrate graceful shutdown
    println!("\nGracefully shutting down...");
    sleep(Duration::from_secs(2)).await;

    // Clean up
    boltz.cleanup_swap(swap_id).await?;
    boltz.disconnect_ws().await;

    println!("Demo completed!");
    Ok(())
}

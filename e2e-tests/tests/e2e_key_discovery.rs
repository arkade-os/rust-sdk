#![allow(clippy::unwrap_used)]

use bitcoin::Amount;
use bitcoin::key::Secp256k1;
use common::Nigiri;
use common::init_tracing;
use common::set_up_client_with_seed;
use rand::Rng;
use rand::thread_rng;
use std::sync::Arc;

mod common;

/// Test that key discovery correctly repopulates the cache after client restart.
///
/// This test:
/// 1. Creates a client with a specific seed
/// 2. Funds and settles a boarding output to give the client a VTXO
/// 3. Checks the balance
/// 4. Recreates the client from the same seed (simulating a restart)
/// 5. Verifies that discover_keys (called during connect) repopulates the cache
/// 6. Checks that the balance is the same before and after restarting the client
#[tokio::test]
#[ignore = "requires nigiri"]
pub async fn e2e_key_discovery() {
    init_tracing();

    let nigiri = Arc::new(Nigiri::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    // Generate a fixed seed for reproducible key derivation
    let seed: [u8; 32] = rng.r#gen();

    tracing::info!("Creating initial client with seed");

    // Create the first client with the seed
    let (client, _wallet) =
        set_up_client_with_seed("alice".to_string(), nigiri.clone(), secp.clone(), seed).await;

    // Verify initial balance is zero
    let initial_balance = client.offchain_balance().await.unwrap();
    assert_eq!(initial_balance.total(), Amount::ZERO);
    tracing::info!(?initial_balance, "Initial balance is zero");

    // Get a boarding address and fund it
    let boarding_address = client.get_boarding_address().unwrap();
    let fund_amount = Amount::ONE_BTC;

    tracing::info!(%boarding_address, %fund_amount, "Funding boarding output");

    let _outpoint = nigiri.faucet_fund(&boarding_address, fund_amount).await;

    // Settle the boarding output to create a VTXO
    tracing::info!("Settling boarding output");
    client.settle(&mut rng, false).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Check the balance after settling
    let balance_after_settle = client.offchain_balance().await.unwrap();
    tracing::info!(?balance_after_settle, "Balance after settle");

    assert_eq!(balance_after_settle.confirmed(), fund_amount);
    assert_eq!(balance_after_settle.pending(), Amount::ZERO);

    // Drop the first client to simulate a restart
    drop(client);
    tracing::info!("Dropped first client, simulating restart");

    // Create a new client with the same seed
    // The discover_keys method should be called during connect and repopulate the cache
    tracing::info!("Creating new client with same seed");
    let (client2, _wallet2) =
        set_up_client_with_seed("alice-restored".to_string(), nigiri.clone(), secp, seed).await;

    // Check the balance - it should be the same as before
    let balance_after_restore = client2.offchain_balance().await.unwrap();
    tracing::info!(?balance_after_restore, "Balance after restore");

    assert_eq!(
        balance_after_restore.confirmed(),
        balance_after_settle.confirmed(),
        "Confirmed balance should be the same after key discovery"
    );
    assert_eq!(
        balance_after_restore.pending(),
        balance_after_settle.pending(),
        "Pending balance should be the same after key discovery"
    );
    assert_eq!(
        balance_after_restore.total(),
        balance_after_settle.total(),
        "Total balance should be the same after key discovery"
    );

    tracing::info!(
        "Key discovery test passed: balance is {} after restore",
        balance_after_restore.total()
    );
}

#![allow(clippy::unwrap_used)]

use ark_core::asset::ControlAssetConfig;
use ark_core::send::AssetSendReceiver;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client;
use common::Nigiri;
use rand::thread_rng;
use std::sync::Arc;

mod common;

#[tokio::test]
#[ignore]
pub async fn e2e_assets() {
    init_tracing();
    let nigiri = Arc::new(Nigiri::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let (alice, _) = set_up_client("alice".to_string(), nigiri.clone(), secp.clone()).await;
    let (bob, _) = set_up_client("bob".to_string(), nigiri.clone(), secp).await;

    // Fund Alice with 1 BTC and settle.
    let alice_boarding_address = alice.get_boarding_address().unwrap();
    nigiri
        .faucet_fund(&alice_boarding_address, Amount::ONE_BTC)
        .await;
    alice.settle(&mut rng).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let balance = alice.offchain_balance().await.unwrap();
    assert_eq!(balance.confirmed(), Amount::ONE_BTC);
    assert!(balance.asset_balances().is_empty());

    tracing::info!("=== Step 1: Issue asset ===");

    let issue_amount: u64 = 1000;
    let control_amount: u64 = 1;

    let issue_result = alice
        .issue_asset(
            issue_amount,
            Some(ControlAssetConfig::new(control_amount).unwrap()),
            Some(vec![("name".to_string(), "TestToken".to_string())]),
        )
        .await
        .unwrap();

    tracing::info!(
        ark_txid = %issue_result.ark_txid,
        asset_ids = ?issue_result.asset_ids,
        "Issued asset"
    );

    assert_eq!(issue_result.asset_ids.len(), 2); // control asset + issued asset

    let control_asset_id = issue_result.asset_ids[0];
    let issued_asset_id = issue_result.asset_ids[1];

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let balance = alice.offchain_balance().await.unwrap();

    tracing::info!(?balance, "Balance after issuance");

    assert_eq!(
        balance.asset_balances().get(&control_asset_id).copied(),
        Some(control_amount),
        "control asset balance"
    );
    assert_eq!(
        balance.asset_balances().get(&issued_asset_id).copied(),
        Some(issue_amount),
        "issued asset balance"
    );

    tracing::info!("=== Step 2: Send asset to Bob ===");

    let send_amount: u64 = 200;
    let (bob_address, _) = bob.get_offchain_address().unwrap();

    let send_txid = alice
        .send_assets(vec![AssetSendReceiver {
            address: bob_address,
            amount: alice.dust(),
            assets: vec![ark_core::server::Asset {
                asset_id: issued_asset_id,
                amount: send_amount,
            }],
        }])
        .await
        .unwrap();

    tracing::info!(%send_txid, "Sent asset to Bob");

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_balance = alice.offchain_balance().await.unwrap();
    let bob_balance = bob.offchain_balance().await.unwrap();

    tracing::info!(?alice_balance, ?bob_balance, "Balances after send");

    assert_eq!(
        alice_balance
            .asset_balances()
            .get(&issued_asset_id)
            .copied(),
        Some(issue_amount - send_amount),
        "alice asset balance after send"
    );
    assert_eq!(
        bob_balance.asset_balances().get(&issued_asset_id).copied(),
        Some(send_amount),
        "bob asset balance after send"
    );

    tracing::info!("=== Step 3: Reissue asset ===");

    let reissue_amount: u64 = 500;

    let reissue_txid = alice
        .reissue_asset(issued_asset_id, reissue_amount)
        .await
        .unwrap();

    tracing::info!(%reissue_txid, "Reissued asset");

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_balance = alice.offchain_balance().await.unwrap();

    tracing::info!(?alice_balance, "Alice balance after reissue");

    assert_eq!(
        alice_balance
            .asset_balances()
            .get(&issued_asset_id)
            .copied(),
        Some(issue_amount - send_amount + reissue_amount),
        "alice asset balance after reissue"
    );
    // Control asset should still be 1.
    assert_eq!(
        alice_balance
            .asset_balances()
            .get(&control_asset_id)
            .copied(),
        Some(control_amount),
        "control asset preserved after reissue"
    );

    tracing::info!("=== Step 4: Burn asset ===");

    let burn_amount: u64 = 300;

    let burn_txid = alice
        .burn_asset(issued_asset_id, burn_amount)
        .await
        .unwrap();

    tracing::info!(%burn_txid, "Burned asset");

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let alice_balance = alice.offchain_balance().await.unwrap();

    tracing::info!(?alice_balance, "Alice balance after burn");

    let expected_after_burn = issue_amount - send_amount + reissue_amount - burn_amount;
    assert_eq!(
        alice_balance
            .asset_balances()
            .get(&issued_asset_id)
            .copied(),
        Some(expected_after_burn),
        "alice asset balance after burn: expected {expected_after_burn}"
    );

    // Bob's balance should be unchanged.
    let bob_balance = bob.offchain_balance().await.unwrap();
    assert_eq!(
        bob_balance.asset_balances().get(&issued_asset_id).copied(),
        Some(send_amount),
        "bob asset balance unchanged"
    );

    tracing::info!(
        alice_final_asset = expected_after_burn,
        bob_final_asset = send_amount,
        "All asset operations completed successfully"
    );
}

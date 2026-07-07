#![allow(clippy::unwrap_used)]

use ark_bdk_wallet::Wallet;
use ark_client::Client;
use ark_client::DeprecatedSignerStatus;
use ark_client::InMemorySwapStorage;
use ark_core::server;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client_with_seed;
use common::set_up_client_with_seed_and_server_info_ttl;
use common::wait_until_balance;
use common::Regtest;
use rand::thread_rng;
use rand::Rng;
use std::sync::Arc;
use std::time::Duration;

mod common;

/// Funds a VTXO, rotates arkd to a new signer with a future cutoff, and verifies that
/// the existing client refreshes server info through its zero TTL before migrating the
/// VTXO to the new signer.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_sweep_migration() {
    init_tracing();

    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let seed: [u8; 32] = rng.r#gen();
    let fund_amount = Amount::ONE_BTC;

    let (client, _wallet) = set_up_ttl_refresh_client(regtest.clone(), secp.clone(), seed).await;

    let boarding_address = client.get_boarding_address().await.unwrap();
    regtest.faucet_fund(&boarding_address, fund_amount).await;

    client.settle(&mut rng).await.unwrap();

    wait_until_balance!(&client, confirmed: fund_amount);
    tracing::info!("VTXO confirmed under current signer");

    let old_digest = client.server_info().await.unwrap().digest.clone();

    // Rotate with a future cutoff so the old signer is deprecated but still co-signs.
    regtest.rotate_signer("+86400");
    tracing::info!("Signer rotated with future cutoff (+86400)");

    let refreshed_info = wait_for_deprecated_signer(&client).await;
    assert_ne!(
        refreshed_info.digest, old_digest,
        "TTL refresh should update cached server_info"
    );

    let balance_before = client.offchain_balance().await.unwrap();
    tracing::info!(?balance_before, "Balance before migration");
    assert_eq!(
        balance_before.total(),
        fund_amount,
        "Total balance must be preserved after rotation"
    );

    let report = client
        .migrate_deprecated_signer_vtxos(&mut rng)
        .await
        .unwrap();
    assert!(
        report.rotated(),
        "migrate_deprecated_signer_vtxos must submit a settlement (no leg rotated)"
    );
    tracing::info!(
        ?report,
        "migrate_deprecated_signer_vtxos submitted settlement"
    );

    // Wait for the new batch to confirm.
    wait_until_balance!(&client, confirmed: fund_amount);

    // If migration actually moved VTXOs to the new signer, there is nothing left
    // to migrate — a second call must rotate nothing.
    let second = client
        .migrate_deprecated_signer_vtxos(&mut rng)
        .await
        .unwrap();
    assert!(
        !second.rotated(),
        "second migrate call should find nothing to migrate (VTXOs are already under new signer)"
    );
    tracing::info!("Sweep-migration test passed: all VTXOs settled under new signer");
}

/// Verifies that the digest-mismatch fast path still refreshes cached server info when a
/// guarded Ark RPC observes a signer change before the configured server-info TTL expires.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_digest_mismatch_refreshes_server_info() {
    init_tracing();

    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let seed: [u8; 32] = rng.r#gen();
    let fund_amount = Amount::ONE_BTC;

    let (client, _wallet) =
        set_up_client_with_seed("alice".to_string(), regtest.clone(), secp.clone(), seed).await;

    let boarding_address = client.get_boarding_address().await.unwrap();
    regtest.faucet_fund(&boarding_address, fund_amount).await;
    client.settle(&mut rng).await.unwrap();
    wait_until_balance!(&client, confirmed: fund_amount);

    let old_digest = client.server_info().await.unwrap().digest.clone();

    regtest.rotate_signer("+86400");
    tracing::info!("Signer rotated with future cutoff (+86400)");

    let mut refreshed_by_digest_mismatch = false;
    let mut last_probe_error = None;
    for _ in 0..30 {
        let (estimate_address, _) = client.get_offchain_address().await.unwrap();
        match client.estimate_batch_fees(&mut rng, estimate_address).await {
            Ok(_) => panic!("digest refresh probe unexpectedly succeeded"),
            Err(err) if err.is_server_info_changed() => {
                refreshed_by_digest_mismatch = true;
                break;
            }
            Err(err) => last_probe_error = Some(format!("{err:?}")),
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    assert!(
        refreshed_by_digest_mismatch,
        "expected digest mismatch / ServerInfoChanged, last probe error: {:?}",
        last_probe_error
    );

    let refreshed_info = client.server_info().await.unwrap();
    assert_ne!(
        refreshed_info.digest, old_digest,
        "digest refresh should update cached server_info"
    );
    assert!(
        !refreshed_info.deprecated_signers.is_empty(),
        "server_info should list the old signer as deprecated after digest refresh"
    );
}

/// Funds a VTXO, rotates arkd to a new signer with a past cutoff, and verifies that
/// funds under the old signer are reported as pending recovery instead of spendable.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_past_cutoff_held_back() {
    init_tracing();

    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let seed: [u8; 32] = rng.r#gen();
    let fund_amount = Amount::ONE_BTC;

    let (client, _wallet) = set_up_ttl_refresh_client(regtest.clone(), secp.clone(), seed).await;

    let boarding_address = client.get_boarding_address().await.unwrap();
    regtest.faucet_fund(&boarding_address, fund_amount).await;

    client.settle(&mut rng).await.unwrap();

    wait_until_balance!(&client, confirmed: fund_amount);
    tracing::info!("VTXO confirmed under current signer");

    // Rotate: past cutoff (-60 seconds) means the operator will NOT co-sign the old key.
    regtest.rotate_signer("-60");
    tracing::info!("Signer rotated with past cutoff (-60)");

    wait_for_deprecated_signer(&client).await;

    // The VTXO is under a past-cutoff deprecated signer: not spendable offchain,
    // not yet expired → must appear in pending_recovery, not in confirmed.
    wait_until_balance!(
        &client,
        confirmed: Amount::ZERO,
        pre_confirmed: Amount::ZERO,
        pending_recovery: fund_amount,
    );

    tracing::info!("Past-cutoff held-back test passed: VTXO is in pending_recovery, not spendable");
}

/// Funds a boarding address without settling it, rotates arkd with a future cutoff, and
/// verifies that migration spends the deprecated boarding input through the boarding leg.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_boarding_only_migration() {
    init_tracing();

    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let seed: [u8; 32] = rng.r#gen();
    let fund_amount = Amount::ONE_BTC;

    let (client, _wallet) = set_up_ttl_refresh_client(regtest.clone(), secp.clone(), seed).await;

    // Fund a boarding output under the current signer but deliberately do NOT settle it: this
    // isolates the boarding-input migration path from the VTXO one.
    let boarding_address = client.get_boarding_address().await.unwrap();
    regtest.faucet_fund(&boarding_address, fund_amount).await;

    // Rotate with a future cutoff so the boarding input can migrate cooperatively. The zero
    // server-info TTL lets normal public APIs pick up the rotation without an explicit force
    // refresh.
    regtest.rotate_signer("+86400");

    wait_for_deprecated_signer(&client).await;
    tracing::info!(
        "Signer rotated with future cutoff (+86400); boarding UTXO now under deprecated signer"
    );

    let report = client
        .migrate_deprecated_signer_vtxos(&mut rng)
        .await
        .unwrap();

    assert!(
        report.rotated(),
        "boarding-only migration must submit a settlement: {report:?}"
    );
    assert!(
        report.boarding.settle_txid.is_some(),
        "the deprecated boarding input must migrate through the boarding leg: {:?}",
        report.boarding
    );
    assert!(
        report.boarding.error.is_none(),
        "boarding leg must not error: {:?}",
        report.boarding.error
    );
    tracing::info!(
        ?report,
        "Boarding-only migration submitted boarding-leg settlement"
    );

    // After migration the funds live under the new signer; a second pass finds nothing to migrate.
    wait_until_balance!(&client, confirmed: fund_amount);
    let second = client
        .migrate_deprecated_signer_vtxos(&mut rng)
        .await
        .unwrap();
    assert!(
        !second.rotated(),
        "second migrate call should find nothing to migrate"
    );
    tracing::info!("Boarding-only migration test passed");
}

/// Funds a VTXO, rotates arkd with a future cutoff, and verifies that
/// `deprecated_signer_status()` reports the old signer as migratable.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_status_migratable() {
    init_tracing();

    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let seed: [u8; 32] = rng.r#gen();
    let fund_amount = Amount::ONE_BTC;

    let (client, _wallet) = set_up_ttl_refresh_client(regtest.clone(), secp.clone(), seed).await;

    let boarding_address = client.get_boarding_address().await.unwrap();
    regtest.faucet_fund(&boarding_address, fund_amount).await;
    client.settle(&mut rng).await.unwrap();
    wait_until_balance!(&client, confirmed: fund_amount);

    // Capture a lower bound on "now" before rotating so we can assert the cutoff is in the future.
    let before_rotate = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    regtest.rotate_signer("+86400");
    tracing::info!("Signer rotated with future cutoff (+86400)");

    wait_for_deprecated_signer(&client).await;

    let status = client.deprecated_signer_status().await.unwrap();
    assert_eq!(
        status.len(),
        1,
        "exactly one deprecated signer the wallet holds funds under"
    );
    let row = &status[0];
    assert_eq!(
        row.status,
        DeprecatedSignerStatus::Migratable,
        "a future cutoff should report Migratable"
    );
    assert!(
        row.cutoff_date > before_rotate,
        "cutoff_date ({}) should be a future timestamp (> {before_rotate})",
        row.cutoff_date
    );
    assert!(
        row.seconds_until_cutoff.is_some_and(|s| s > 0),
        "Migratable signer should have a positive seconds_until_cutoff, got {:?}",
        row.seconds_until_cutoff
    );
    assert!(
        row.vtxo_count >= 1,
        "the funded VTXO should be counted under the deprecated signer"
    );
    tracing::info!(?row, "Migratable status test passed");
}

/// Funds a VTXO, rotates arkd with cutoff `0`, and verifies that
/// `deprecated_signer_status()` reports the old signer as due now.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_status_due_now() {
    init_tracing();

    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let seed: [u8; 32] = rng.r#gen();
    let fund_amount = Amount::ONE_BTC;

    let (client, _wallet) = set_up_ttl_refresh_client(regtest.clone(), secp.clone(), seed).await;

    let boarding_address = client.get_boarding_address().await.unwrap();
    regtest.faucet_fund(&boarding_address, fund_amount).await;
    client.settle(&mut rng).await.unwrap();
    wait_until_balance!(&client, confirmed: fund_amount);

    // Cutoff "0" means rotate immediately: deprecated, no cutoff advertised, still co-signable.
    regtest.rotate_signer("0");
    tracing::info!("Signer rotated with zero cutoff (DueNow)");

    wait_for_deprecated_signer(&client).await;

    let status = client.deprecated_signer_status().await.unwrap();
    assert_eq!(
        status.len(),
        1,
        "exactly one deprecated signer the wallet holds funds under"
    );
    let row = &status[0];
    assert_eq!(
        row.status,
        DeprecatedSignerStatus::DueNow,
        "a zero cutoff should report DueNow"
    );
    assert_eq!(
        row.cutoff_date, 0,
        "DueNow signer advertises cutoff_date == 0"
    );
    assert_eq!(
        row.seconds_until_cutoff, None,
        "DueNow signer has no seconds_until_cutoff"
    );
    tracing::info!(?row, "DueNow status test passed");
}

type TestClient = Client<Regtest, Wallet, InMemorySwapStorage>;

async fn wait_for_deprecated_signer(client: &TestClient) -> server::Info {
    let mut last_error = None;
    for _ in 0..30 {
        match client.server_info().await {
            Ok(info) if !info.deprecated_signers.is_empty() => return info,
            Ok(_) => {}
            Err(err) => last_error = Some(format!("{err:?}")),
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    panic!(
        "server_info should list the old signer as deprecated after rotation, last error: {:?}",
        last_error
    );
}

async fn set_up_ttl_refresh_client(
    regtest: Arc<Regtest>,
    secp: Secp256k1<bitcoin::secp256k1::All>,
    seed: [u8; 32],
) -> (TestClient, Arc<Wallet>) {
    set_up_client_with_seed_and_server_info_ttl(
        "alice".to_string(),
        regtest,
        secp,
        seed,
        Duration::ZERO,
    )
    .await
}

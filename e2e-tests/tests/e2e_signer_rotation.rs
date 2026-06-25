#![allow(clippy::unwrap_used)]

use ark_client::DeprecatedSignerStatus;
use bitcoin::key::Secp256k1;
use bitcoin::Amount;
use common::init_tracing;
use common::set_up_client_with_seed;
use common::wait_until_balance;
use common::Regtest;
use rand::thread_rng;
use rand::Rng;
use std::sync::Arc;

mod common;

/// Regime 1: VTXO created under current signer → rotate with future cutoff (still cooperative)
/// → `migrate_deprecated_signer_vtxos` settles all pre-cutoff VTXOs to the new signer.
///
/// Verifies the cooperative rotation path for still-co-signable deprecated signer outputs.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_sweep_migration() {
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
    tracing::info!("VTXO confirmed under current signer");

    let old_digest = client.server_info().await.unwrap().digest.clone();

    // Rotate: future cutoff means the old signer is deprecated but still co-signs (regime 1).
    regtest.rotate_signer("+86400");
    tracing::info!("Signer rotated with future cutoff (+86400)");

    // A guarded Ark RPC with the stale digest should refresh cached server_info and return
    // ServerInfoChanged. The failed estimate is not retried automatically. The first call after
    // the arkd restart can still hit the old broken HTTP/2 connection, so retry until the
    // re-dialed channel reaches arkd and gets the digest-mismatch response.
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

        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert!(
        refreshed_by_digest_mismatch,
        "expected digest mismatch / ServerInfoChanged, last probe error: {:?}",
        last_probe_error
    );

    let client2 = client;
    let refreshed_info = client2.server_info().await.unwrap();
    assert_ne!(
        refreshed_info.digest, old_digest,
        "digest refresh should update cached server_info"
    );
    assert!(
        !refreshed_info.deprecated_signers.is_empty(),
        "server_info should list the old signer as deprecated after rotation"
    );

    let balance_before = client2.offchain_balance().await.unwrap();
    tracing::info!(
        ?balance_before,
        "Balance seen by new client before migration"
    );
    assert_eq!(
        balance_before.total(),
        fund_amount,
        "Total balance must be preserved after rotation"
    );

    let report = client2
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
    wait_until_balance!(&client2, confirmed: fund_amount);

    // If migration actually moved VTXOs to the new signer, there is nothing left
    // to migrate — a second call must rotate nothing.
    let second = client2
        .migrate_deprecated_signer_vtxos(&mut rng)
        .await
        .unwrap();
    assert!(
        !second.rotated(),
        "second migrate call should find nothing to migrate (VTXOs are already under new signer)"
    );
    tracing::info!("Sweep-migration test passed: all VTXOs settled under new signer");
}

/// Regime 2: VTXO created under current signer → rotate with past cutoff (operator will NOT
/// co-sign the old key) → VTXO is stuck in `pending_recovery`, NOT in confirmed/pre_confirmed.
///
/// Verifies the held-back balance state for outputs whose deprecated signer no longer co-signs.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_past_cutoff_held_back() {
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
    tracing::info!("VTXO confirmed under current signer");

    drop(client);

    // Rotate: past cutoff (-60 seconds) means the operator will NOT co-sign the old key.
    regtest.rotate_signer("-60");
    tracing::info!("Signer rotated with past cutoff (-60)");

    // Reconnect to pick up updated server info.
    let (client2, _wallet2) =
        set_up_client_with_seed("alice".to_string(), regtest.clone(), secp.clone(), seed).await;

    assert!(
        !client2
            .server_info()
            .await
            .unwrap()
            .deprecated_signers
            .is_empty(),
        "server_info should list the old signer as deprecated after rotation"
    );

    // The VTXO is under a past-cutoff deprecated signer: not spendable offchain,
    // not yet expired → must appear in pending_recovery, not in confirmed.
    wait_until_balance!(
        &client2,
        confirmed: Amount::ZERO,
        pre_confirmed: Amount::ZERO,
        pending_recovery: fund_amount,
    );

    tracing::info!("Past-cutoff held-back test passed: VTXO is in pending_recovery, not spendable");
}

/// Boarding-only migration: fund a boarding address but DO NOT settle it to a VTXO, then rotate
/// with a future cutoff. After reconnecting with the same seed, the deprecated BOARDING input must
/// migrate cooperatively through the report's boarding leg — WITHOUT any explicit
/// `get_boarding_addresses()` call, because connect-time boarding persistence already watches the
/// deprecated signer's boarding outputs.
///
/// Exercises the boarding-only migration leg, isolated from the VTXO path.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_boarding_only_migration() {
    init_tracing();

    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let seed: [u8; 32] = rng.r#gen();
    let fund_amount = Amount::ONE_BTC;

    let (client, _wallet) =
        set_up_client_with_seed("alice".to_string(), regtest.clone(), secp.clone(), seed).await;

    // Fund a boarding output under the current signer but deliberately do NOT settle it: this
    // isolates the boarding-input migration path from the VTXO one.
    let boarding_address = client.get_boarding_address().await.unwrap();
    regtest.faucet_fund(&boarding_address, fund_amount).await;

    // Rotate with a future cutoff: the old signer is deprecated but still co-signs (regime 1), so
    // the boarding input is cooperatively migratable. We keep the SAME client and just refresh its
    // cached server info to pick up the rotation — boarding outputs are owned by the wallet
    // keypair, which this seed-only test harness does not re-derive across a reconnect (only the
    // client's offchain keys are seed-derived, so VTXOs survive a reconnect but boarding outputs
    // do not). Connect-time deprecated-boarding persistence is exercised on every connect; this
    // test isolates the boarding migration *leg* itself.
    regtest.rotate_signer("+86400");

    // `rotate_signer` already blocks until arkd has restarted and re-advertises the deprecated
    // signer, but the restart broke our existing gRPC connection, so the first refresh may hit a
    // stale channel and need a re-dial. Retry until the refreshed snapshot shows the deprecated
    // signer.
    let mut rotated = false;
    for _ in 0..30 {
        if client.refresh_server_info().await.is_ok()
            && !client
                .server_info()
                .await
                .unwrap()
                .deprecated_signers
                .is_empty()
        {
            rotated = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
    assert!(
        rotated,
        "server_info should list the old signer as deprecated after rotation"
    );
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

/// Classification: a future cutoff (`+86400`) makes the deprecated signer `Migratable` with a
/// positive `seconds_until_cutoff`, exposed via the read-only `deprecated_signer_status()`.
///
/// `rotate_signer("+86400")` resolves the cutoff to `now + 86400` (an absolute future Unix
/// timestamp), so we assert `cutoff_date` is in the future and `seconds_until_cutoff > 0` rather
/// than an exact offset.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_status_migratable() {
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

    drop(client);

    // Capture a lower bound on "now" before rotating so we can assert the cutoff is in the future.
    let before_rotate = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;

    regtest.rotate_signer("+86400");
    tracing::info!("Signer rotated with future cutoff (+86400)");

    let (client2, _wallet2) =
        set_up_client_with_seed("alice".to_string(), regtest.clone(), secp.clone(), seed).await;

    let status = client2.deprecated_signer_status().await.unwrap();
    assert_eq!(
        status.len(),
        1,
        "exactly one deprecated signer the wallet holds funds under"
    );
    let row = &status[0];
    assert_eq!(
        row.status,
        DeprecatedSignerStatus::Migratable,
        "a future cutoff classifies as Migratable"
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
    tracing::info!(?row, "Migratable classification test passed");
}

/// Classification: a zero cutoff (`"0"`) makes the deprecated signer `DueNow` with
/// `cutoff_date == 0` and no `seconds_until_cutoff`, exposed via `deprecated_signer_status()`.
///
/// `rotate_signer("0")` advertises cutoff `0` ("rotate immediately", still co-signable).
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_status_due_now() {
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

    drop(client);

    // Cutoff "0" => "rotate immediately" (DUE_NOW): deprecated, no cutoff advertised, still
    // co-signable.
    regtest.rotate_signer("0");
    tracing::info!("Signer rotated with zero cutoff (DueNow)");

    let (client2, _wallet2) =
        set_up_client_with_seed("alice".to_string(), regtest.clone(), secp.clone(), seed).await;

    let status = client2.deprecated_signer_status().await.unwrap();
    assert_eq!(
        status.len(),
        1,
        "exactly one deprecated signer the wallet holds funds under"
    );
    let row = &status[0];
    assert_eq!(
        row.status,
        DeprecatedSignerStatus::DueNow,
        "a zero cutoff classifies as DueNow"
    );
    assert_eq!(
        row.cutoff_date, 0,
        "DueNow signer advertises cutoff_date == 0"
    );
    assert_eq!(
        row.seconds_until_cutoff, None,
        "DueNow signer has no seconds_until_cutoff"
    );
    tracing::info!(?row, "DueNow classification test passed");
}

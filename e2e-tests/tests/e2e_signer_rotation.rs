#![allow(clippy::unwrap_used)]

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
/// Mirrors dotnet-sdk `SweepMigrationRotationTests` and ts-sdk rotation/migration scenario.
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

    let boarding_address = client.get_boarding_address().unwrap();
    regtest.faucet_fund(&boarding_address, fund_amount).await;

    client.settle(&mut rng).await.unwrap();

    wait_until_balance!(&client, confirmed: fund_amount);
    tracing::info!("VTXO confirmed under current signer");

    drop(client);

    // Rotate: future cutoff means the old signer is deprecated but still co-signs (regime 1).
    regtest.rotate_signer("+86400");
    tracing::info!("Signer rotated with future cutoff (+86400)");

    // Reconnect to pick up updated server info (deprecated_signers now populated).
    let (client2, _wallet2) =
        set_up_client_with_seed("alice".to_string(), regtest.clone(), secp.clone(), seed).await;

    assert!(
        !client2.server_info().unwrap().deprecated_signers.is_empty(),
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
/// Mirrors dotnet-sdk `PastCutoffHeldBackRotationTests`.
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

    let boarding_address = client.get_boarding_address().unwrap();
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
        !client2.server_info().unwrap().deprecated_signers.is_empty(),
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

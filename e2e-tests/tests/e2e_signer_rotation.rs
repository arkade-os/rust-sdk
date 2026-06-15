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
        !client2.server_info.deprecated_signers.is_empty(),
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

    let txid = client2
        .migrate_deprecated_signer_vtxos(&mut rng)
        .await
        .unwrap();
    assert!(
        txid.is_some(),
        "migrate_deprecated_signer_vtxos must submit a settlement (returned None)"
    );
    tracing::info!(
        ?txid,
        "migrate_deprecated_signer_vtxos submitted settlement"
    );

    // Wait for the new batch to confirm.
    wait_until_balance!(&client2, confirmed: fund_amount);

    // If migration actually moved VTXOs to the new signer, there is nothing left
    // to migrate — a second call must return None.
    let second = client2
        .migrate_deprecated_signer_vtxos(&mut rng)
        .await
        .unwrap();
    assert!(
        second.is_none(),
        "second migrate call should find nothing to migrate (VTXOs are already under new signer)"
    );
    tracing::info!("Sweep-migration test passed: all VTXOs settled under new signer");
}

/// Verify that `migrate_deprecated_signer_vtxos` only moves VTXOs that sit under a
/// deprecated signer, leaving VTXOs already under the current signer untouched.
///
/// Setup: two VTXOs, one per signer generation.
///   1. Settle fund_a under signer-A (current).
///   2. Rotate → signer-A becomes deprecated (future cutoff), signer-B becomes current.
///   3. Settle fund_b under signer-B (current).
///   4. Call migrate → only fund_a moves; fund_b outpoint must stay identical.
#[tokio::test]
#[ignore = "requires regtest"]
pub async fn e2e_signer_rotation_only_deprecated_vtxos_migrated() {
    init_tracing();

    let regtest = Arc::new(Regtest::new());
    let secp = Secp256k1::new();
    let mut rng = thread_rng();

    let seed: [u8; 32] = rng.r#gen();
    let fund_a = Amount::from_btc(1.0).unwrap();
    let fund_b = Amount::from_btc(0.5).unwrap();

    // -- Step 1: settle fund_a under the current (soon-to-be-deprecated) signer --
    let (client_a, _wallet_a) =
        set_up_client_with_seed("alice".to_string(), regtest.clone(), secp.clone(), seed).await;

    let boarding = client_a.get_boarding_address().unwrap();
    regtest.faucet_fund(&boarding, fund_a).await;
    client_a.settle(&mut rng).await.unwrap();
    wait_until_balance!(&client_a, confirmed: fund_a);
    drop(client_a);

    // -- Step 2: rotate signer --
    regtest.rotate_signer("+86400");

    // -- Step 3: reconnect and settle fund_b under the new current signer --
    let (client_b, _wallet_b) =
        set_up_client_with_seed("alice".to_string(), regtest.clone(), secp.clone(), seed).await;

    assert!(
        !client_b.server_info.deprecated_signers.is_empty(),
        "signer-A must appear as deprecated after rotation"
    );

    let boarding2 = client_b.get_boarding_address().unwrap();
    regtest.faucet_fund(&boarding2, fund_b).await;
    client_b.settle(&mut rng).await.unwrap();
    wait_until_balance!(&client_b, confirmed: fund_a + fund_b);

    // Record outpoints of all VTXOs currently under the new (current) signer.
    let new_signer_pk = client_b.server_info.signer_pk.x_only_public_key().0;
    let (vtxo_list, script_map) = client_b.list_vtxos().await.unwrap();
    let current_signer_outpoints: std::collections::HashSet<_> = vtxo_list
        .all_unspent()
        .filter(|v| {
            script_map
                .get(&v.script)
                .map(|vtxo| vtxo.server_pk() == new_signer_pk)
                .unwrap_or(false)
        })
        .map(|v| v.outpoint)
        .collect();
    assert!(
        !current_signer_outpoints.is_empty(),
        "there must be at least one VTXO under the new signer before migration"
    );

    // -- Step 4: migrate --
    let txid = client_b
        .migrate_deprecated_signer_vtxos(&mut rng)
        .await
        .unwrap();
    assert!(
        txid.is_some(),
        "migrate must submit a settlement for the deprecated-signer VTXO"
    );

    wait_until_balance!(&client_b, confirmed: fund_a + fund_b);

    // Outpoints that were under the current signer MUST still be present — they were not
    // re-settled into a new batch.
    let (vtxo_list_after, _) = client_b.list_vtxos().await.unwrap();
    let outpoints_after: std::collections::HashSet<_> =
        vtxo_list_after.all_unspent().map(|v| v.outpoint).collect();

    for op in &current_signer_outpoints {
        assert!(
            outpoints_after.contains(op),
            "current-signer VTXO {op} was re-settled during migration — it should not have been"
        );
    }

    tracing::info!(
        "Selective-migration test passed: current-signer VTXOs untouched, \
         deprecated-signer VTXO migrated"
    );
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
        !client2.server_info.deprecated_signers.is_empty(),
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

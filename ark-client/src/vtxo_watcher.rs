//! Background VTXO watcher that auto-delegates and auto-renews VTXOs.
//!
//! Full behavior:
//! - On new VTXOs received: submit them to the delegator service for future renewal
//! - On new VTXOs received: self-renew VTXOs that are close to expiry (safety net)
//! - On stream error: reconnect with exponential backoff

use crate::error::ErrorContext;
use crate::swap_storage::SwapStorage;
use crate::wallet::OnchainWallet;
use crate::AnnotatedVtxo;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::intent;
use ark_core::server::SubscriptionFilter;
use ark_core::server::SubscriptionResponse;
use ark_core::server::VirtualTxOutPoint;
use ark_core::ArkAddress;
#[cfg(test)]
use ark_core::Vtxo;
use ark_delegator::DelegatorClient;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use bitcoin::TxOut;
use futures::StreamExt;
use rand::rngs::OsRng;
use std::collections::BTreeMap;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::watch;

/// Handle to stop the background VTXO watcher.
///
/// Dropping the handle will also stop the watcher.
pub struct VtxoWatcherHandle {
    stop_tx: watch::Sender<bool>,
}

impl VtxoWatcherHandle {
    /// Stop the background watcher.
    pub fn stop(self) {
        let _ = self.stop_tx.send(true);
    }
}

impl Drop for VtxoWatcherHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
    }
}

/// Backoff parameters for reconnection.
const INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Upper bound on how long to wait for the server to send the `subscription_started` frame that
/// opens a oneshot subscription.
const SUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Periodic key discovery settings for keeping script subscriptions fresh.
const KEY_DISCOVERY_INTERVAL: Duration = Duration::from_secs(10);

/// How often the background migration arm fires when healthy. The frequent cadence is safe
/// because [`Client::migrate_deprecated_signer_vtxos`] short-circuits to a no-op
/// `NothingMigratable` report when the server advertises no deprecated signers or the wallet holds
/// no pre-cutoff deprecated-signer outputs.
const MIGRATION_INTERVAL: Duration = Duration::from_secs(60);

/// Exponential-backoff bounds for the migration arm after a failing pass. The cooldown doubles per
/// consecutive failure, caps at five minutes, and resets to the base on a successful or no-op pass.
const MIGRATION_BASE_COOLDOWN: Duration = Duration::from_secs(30);
const MIGRATION_MAX_COOLDOWN: Duration = Duration::from_secs(300);

/// Configuration for [`Client::start_vtxo_watcher`].
#[derive(Debug, Clone, Copy)]
pub struct VtxoWatcherConfig {
    /// When `true` (the default), the watcher runs a periodic
    /// [`Client::migrate_deprecated_signer_vtxos`] pass that rotates funds off any deprecated
    /// server signer the wallet still holds pre-cutoff outputs under. Errors are logged and
    /// swallowed (never killing the loop), and a persistently failing pass backs off
    /// exponentially. Set to `false` to disable the migration arm entirely; renewal and delegation
    /// behavior are unaffected either way.
    pub migrate_deprecated_signers: bool,
}

impl Default for VtxoWatcherConfig {
    fn default() -> Self {
        Self {
            migrate_deprecated_signers: true,
        }
    }
}

enum WatcherWork {
    NewVtxos { vtxos: Vec<VirtualTxOutPoint> },
    RenewTick,
}

impl<B, W, S> Client<B, W, S>
where
    B: Blockchain + Send + Sync + 'static,
    W: OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
{
    /// Start a background task that watches for new VTXOs and:
    ///
    /// 1. **Delegates** them to the configured delegator service for future auto-renewal
    /// 2. **Self-renews** VTXOs that are close to expiry (safety net)
    /// 3. **Migrates** funds off deprecated server signers on a periodic, backed-off pass (unless
    ///    disabled via [`VtxoWatcherConfig::migrate_deprecated_signers`])
    ///
    /// Reconnects automatically with exponential backoff (1s → 2s → … → 30s) on stream errors.
    ///
    /// Requires the client to be wrapped in an `Arc` for shared ownership with the background
    /// task.
    ///
    /// Returns a [`VtxoWatcherHandle`] that stops the watcher when dropped.
    pub fn start_vtxo_watcher(
        self: &Arc<Self>,
        delegator: Arc<DelegatorClient>,
        config: VtxoWatcherConfig,
    ) -> VtxoWatcherHandle {
        let (stop_tx, stop_rx) = watch::channel(false);

        let client = Arc::clone(self);
        tokio::spawn(async move {
            run_watcher_loop(client, delegator, config, stop_rx).await;
            tracing::debug!("VTXO watcher stopped");
        });

        VtxoWatcherHandle { stop_tx }
    }
}

/// Outer loop that reconnects on stream errors with exponential backoff.
async fn run_watcher_loop<B, W, S>(
    client: Arc<Client<B, W, S>>,
    delegator: Arc<DelegatorClient>,
    config: VtxoWatcherConfig,
    mut stop_rx: watch::Receiver<bool>,
) where
    B: Blockchain + Send + Sync + 'static,
    W: OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
{
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if *stop_rx.borrow() {
            return;
        }

        let addresses = match client.active_offchain_contract_addresses() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("Failed to get active offchain contracts: {e}");
                return;
            }
        };

        let handshake = client.subscribe_to_scripts_stream(addresses.clone());
        let (subscription_id, mut stream) =
            match subscribe_within_deadline(&mut stop_rx, SUBSCRIBE_TIMEOUT, handshake).await {
                SubscribeOutcome::Ready(subscription) => subscription,
                SubscribeOutcome::Stopped => return,
                SubscribeOutcome::Retry => {
                    if wait_or_stop(&mut stop_rx, backoff).await {
                        return;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                    continue;
                }
            };

        tracing::info!("VTXO watcher connected");
        backoff = INITIAL_BACKOFF;
        let mut subscribed_addrs: HashSet<ArkAddress> = addresses.into_iter().collect();
        let mut renew_interval = tokio::time::interval(Duration::from_secs(60));
        let mut discovery_interval = tokio::time::interval(KEY_DISCOVERY_INTERVAL);
        let (work_tx, mut work_rx) = mpsc::channel::<WatcherWork>(128);

        let worker_handle = tokio::spawn({
            let client = client.clone();
            let delegator = delegator.clone();
            async move {
                let mut seen_unspent_outpoints = HashSet::<OutPoint>::new();

                while let Some(first) = work_rx.recv().await {
                    // Handle one guaranteed message to start the batch.
                    let (mut pending_vtxos, mut should_renew, mut should_sync) = match first {
                        WatcherWork::NewVtxos { vtxos } => (vtxos, true, false),
                        WatcherWork::RenewTick => (Vec::new(), true, true),
                    };

                    // Drain whatever else is already queued without waiting.
                    while let Ok(work) = work_rx.try_recv() {
                        match work {
                            WatcherWork::NewVtxos { vtxos } => {
                                pending_vtxos.extend(vtxos);
                                should_renew = true;
                            }
                            WatcherWork::RenewTick => {
                                should_renew = true;
                                should_sync = true;
                            }
                        }
                    }

                    if should_sync {
                        match collect_new_delegation_candidates(
                            &client,
                            &mut seen_unspent_outpoints,
                        )
                        .await
                        {
                            Ok(new_candidates) => {
                                if !new_candidates.is_empty() {
                                    tracing::debug!(
                                        count = new_candidates.len(),
                                        "Found new delegatable VTXOs from failsafe polling"
                                    );
                                    pending_vtxos.extend(new_candidates);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failsafe delegation poll failed: {e}");
                            }
                        }
                    }

                    if !pending_vtxos.is_empty() {
                        let mut deduped = Vec::new();
                        let mut seen = HashSet::new();
                        for vtxo in pending_vtxos {
                            if seen.insert(vtxo.outpoint) {
                                deduped.push(vtxo);
                            }
                        }

                        tracing::debug!(count = deduped.len(), "Processing VTXOs for delegation");
                        delegate_vtxos(&client, &delegator, &deduped).await;
                    }

                    if should_renew {
                        renew_expiring_vtxos(&client).await;
                    }
                }
            }
        });

        // Independent migration arm: rotates funds off deprecated server signers on its own
        // self-paced cooldown loop, separate from the renewal/delegation worker and the
        // subscription stream (it polls wallet state, like renewal). Spawned per connection and
        // aborted on reconnect/stop so passes never overlap. Disabled entirely when the config
        // flag is off.
        let migration_handle = config.migrate_deprecated_signers.then(|| {
            let client = client.clone();
            let mut stop_rx = stop_rx.clone();
            tokio::spawn(async move {
                run_migration_arm(&client, &mut stop_rx).await;
            })
        });

        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    drop(work_tx);
                    let _ = worker_handle.await;
                    if let Some(handle) = migration_handle {
                        handle.abort();
                    }
                    return;
                }
                _ = renew_interval.tick() => {
                    if work_tx.send(WatcherWork::RenewTick).await.is_err() {
                        tracing::warn!("VTXO worker channel closed, reconnecting in {backoff:?}");
                        break;
                    }
                }
                _ = discovery_interval.tick() => {
                    match refresh_subscription_scripts(
                        client.as_ref(),
                        &subscription_id,
                        &mut subscribed_addrs,
                    )
                    .await
                    {
                        Ok(()) => {}
                        Err(e) => {
                            tracing::warn!("Failed to refresh script subscription: {e}");
                        }
                    }
                }
                event = stream.next() => {
                    match event {
                        Some(Ok(SubscriptionResponse::Heartbeat)) => {}
                        Some(Ok(SubscriptionResponse::SubscriptionStarted { subscription_id })) => {
                            tracing::debug!(subscription_id, "Subscription started");
                        }
                        Some(Ok(SubscriptionResponse::Event(event))) => {
                            if !event.new_vtxos.is_empty() {
                                tracing::debug!(
                                    txid = %event.txid,
                                    new_vtxos = event.new_vtxos.len(),
                                    "Received subscription event with new VTXOs"
                                );

                                if work_tx.send(WatcherWork::NewVtxos {
                                    vtxos: event.new_vtxos,
                                })
                                .await.is_err()
                                {
                                    tracing::warn!("VTXO worker channel closed. Reconnecting in {backoff:?}");
                                    break;
                                }
                            }
                        }
                        Some(Err(e)) => {
                            tracing::warn!("VTXO subscription error: {e}, reconnecting in {backoff:?}");
                            break;
                        }
                        None => {
                            tracing::debug!("VTXO subscription stream ended, reconnecting in {backoff:?}");
                            break;
                        }
                    }
                }
            }
        }

        drop(work_tx);
        let _ = worker_handle.await;
        // Abort the per-connection migration arm; the next iteration spawns a fresh one. This
        // prevents two migration loops racing across a reconnect.
        if let Some(handle) = migration_handle {
            handle.abort();
        }

        if wait_or_stop(&mut stop_rx, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Background migration arm: periodically rotate funds off deprecated server signers.
///
/// On each fire it runs one [`Client::migrate_deprecated_signer_vtxos`] pass and logs the outcome.
/// Errors are swallowed (never propagated — this must never kill the watcher). Cadence is
/// [`MIGRATION_INTERVAL`] while healthy; a failing pass backs off exponentially between
/// [`MIGRATION_BASE_COOLDOWN`] and [`MIGRATION_MAX_COOLDOWN`], resetting to the base interval on a
/// fully successful or no-op pass. The frequent base cadence is cheap because the migration call is
/// a no-op (`NothingMigratable`) whenever there is nothing to rotate.
///
/// When [`Client::refresh_server_info`] updates the cached `deprecated_signers`, this arm picks up
/// the freshly advertised deprecated signers on its next pass and migrates.
async fn run_migration_arm<B, W, S>(client: &Client<B, W, S>, stop_rx: &mut watch::Receiver<bool>)
where
    B: Blockchain + Send + Sync + 'static,
    W: OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
{
    // Consecutive-failure count drives the exponential cooldown; `0` means healthy (use the base
    // interval). Reset to `0` on any fully successful or no-op pass.
    let mut consecutive_failures: u32 = 0;
    loop {
        let delay = migration_delay(consecutive_failures);
        if wait_or_stop(stop_rx, delay).await {
            return;
        }

        let mut rng = OsRng;
        match client.migrate_deprecated_signer_vtxos(&mut rng).await {
            Ok(report) => {
                if report.failed() {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    let next = migration_delay(consecutive_failures);
                    tracing::warn!(
                        txids = ?report.settle_txids(),
                        vtxo_error = ?report.vtxo.error.as_deref(),
                        boarding_error = ?report.boarding.error.as_deref(),
                        "Background migration pass had leg failure; backing off {next:?}"
                    );
                } else {
                    if report.rotated() {
                        tracing::info!(
                            txids = ?report.settle_txids(),
                            "Background migration rotated funds off deprecated signer(s)"
                        );
                    } else {
                        tracing::debug!("Background migration pass: nothing to migrate");
                    }
                    // Success or no-op: back to the healthy cadence.
                    consecutive_failures = 0;
                }
            }
            Err(e) => {
                // Back off so a persistently failing migration does not retry every interval.
                consecutive_failures = consecutive_failures.saturating_add(1);
                let next = migration_delay(consecutive_failures);
                tracing::warn!("Background migration pass failed: {e}; backing off {next:?}");
            }
        }
    }
}

/// Cooldown before the next migration pass given the consecutive-failure count.
///
/// `0` failures → the healthy [`MIGRATION_INTERVAL`]. Otherwise an exponential backoff of
/// `MIGRATION_BASE_COOLDOWN * 2^(failures - 1)`, saturating at [`MIGRATION_MAX_COOLDOWN`].
fn migration_delay(consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return MIGRATION_INTERVAL;
    }
    let shift = consecutive_failures - 1;
    let scaled = MIGRATION_BASE_COOLDOWN
        .checked_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
        .unwrap_or(MIGRATION_MAX_COOLDOWN);
    scaled.min(MIGRATION_MAX_COOLDOWN)
}

/// Wait for the given duration or until stop is signalled. Returns `true` if stopped.
async fn wait_or_stop(stop_rx: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    tokio::select! {
        _ = stop_rx.changed() => true,
        _ = tokio::time::sleep(duration) => false,
    }
}

/// Outcome of awaiting a subscription handshake under the stop signal and handshake deadline.
enum SubscribeOutcome<T> {
    /// The subscription opened, carries `(subscription_id, stream)`.
    Ready(T),
    /// Stop was signalled while waiting. The watcher should shut down.
    Stopped,
    /// The handshake failed or timed out. The watcher should back off and reconnect.
    Retry,
}

/// Await a subscription handshake while honoring the stop signal and a handshake deadline.
///
/// A stop cancels the in-flight handshake immediately rather than blocking shutdown until it
/// resolves. The deadline bounds a server that only sends heartbeats before `subscription_started`
/// so the caller can fall back to reconnect backoff.
async fn subscribe_within_deadline<F, T>(
    stop_rx: &mut watch::Receiver<bool>,
    deadline: Duration,
    handshake: F,
) -> SubscribeOutcome<T>
where
    F: Future<Output = Result<T, Error>>,
{
    tokio::select! {
        _ = stop_rx.changed() => SubscribeOutcome::Stopped,
        result = tokio::time::timeout(deadline, handshake) => match result {
            Ok(Ok(value)) => SubscribeOutcome::Ready(value),
            Ok(Err(e)) => {
                tracing::warn!("Failed to subscribe: {e}, reconnecting with backoff");
                SubscribeOutcome::Retry
            }
            Err(_) => {
                tracing::warn!(
                    "Timed out waiting for subscription to start, reconnecting with backoff"
                );
                SubscribeOutcome::Retry
            }
        },
    }
}

/// Add newly persisted active contract scripts to an existing subscription.
async fn refresh_subscription_scripts<B, W, S>(
    client: &Client<B, W, S>,
    subscription_id: &str,
    subscribed_addrs: &mut HashSet<ArkAddress>,
) -> Result<(), Error>
where
    B: Blockchain + Send + Sync + 'static,
    W: OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
{
    let addrs = client.active_offchain_contract_addresses()?;
    let Some((filter, new_addrs)) = additional_scripts_filter(addrs, subscribed_addrs) else {
        return Ok(());
    };

    client
        .update_subscription(subscription_id.to_string(), filter)
        .await?;

    let added = new_addrs.len();
    subscribed_addrs.extend(new_addrs);
    tracing::info!(
        added,
        "Updated watcher subscription with newly active contract addresses"
    );

    Ok(())
}

/// Build the `update_subscription` filter that adds the active addresses not already subscribed.
fn additional_scripts_filter(
    active: Vec<ArkAddress>,
    subscribed: &HashSet<ArkAddress>,
) -> Option<(SubscriptionFilter, Vec<ArkAddress>)> {
    let new_addrs: Vec<ArkAddress> = active
        .into_iter()
        .filter(|addr| !subscribed.contains(addr))
        .collect();

    if new_addrs.is_empty() {
        return None;
    }

    // `update_subscription` overwrites expressions as a whole but treats scripts as additive. This
    // flow adds scripts and never sets expressions, so the empty list has nothing to clear. A
    // future caller that opens the stream with expressions must carry them here instead of wiping
    // them on the first discovery tick.
    let filter = SubscriptionFilter {
        expressions: Vec::new(),
        add_scripts: new_addrs
            .iter()
            .map(|addr| addr.to_p2tr_script_pubkey())
            .collect(),
        remove_scripts: Vec::new(),
    };

    Some((filter, new_addrs))
}

/// Enumerate newly seen unspent delegate-eligible VTXOs from wallet state.
///
/// This is a failsafe path to catch outputs that may have been missed by subscription timing.
async fn collect_new_delegation_candidates<B, W, S>(
    client: &Client<B, W, S>,
    seen_unspent_outpoints: &mut HashSet<OutPoint>,
) -> Result<Vec<VirtualTxOutPoint>, Error>
where
    B: Blockchain + Send + Sync + 'static,
    W: OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
{
    let vtxo_list = client.list_vtxos().await?;

    let mut current_outpoints = HashSet::new();
    let mut newly_seen = Vec::new();

    for entry in vtxo_list.all_unspent() {
        if entry.contract().contract_type != ark_core::contract::ContractType::delegate_vtxo() {
            continue;
        }

        current_outpoints.insert(entry.vtxo().outpoint);

        if !seen_unspent_outpoints.contains(&entry.vtxo().outpoint) {
            newly_seen.push(entry.vtxo().clone());
        }
    }

    *seen_unspent_outpoints = current_outpoints;

    Ok(newly_seen)
}

/// Delegator info cached per delegation batch.
struct DelegatorState {
    cosigner_pk: PublicKey,
    fee: Amount,
    fee_address_script: ScriptBuf,
}

/// Fetch and parse delegator info into a usable form.
async fn fetch_delegator_state(delegator: &DelegatorClient) -> Result<DelegatorState, Error> {
    let info = delegator
        .info()
        .await
        .context(Error::ad_hoc("failed to get delegator info"))?;

    let cosigner_pk: PublicKey = info
        .pubkey
        .parse::<PublicKey>()
        .context("failed to parse delegator PK")?;

    let fee = info
        .fee
        .parse::<u64>()
        .map(Amount::from_sat)
        .context("failed to parse delegator fee")?;

    let fee_address_script = info
        .delegator_address
        .parse::<ArkAddress>()
        .context("failed to parse delegator fee address")?
        .to_p2tr_script_pubkey();

    Ok(DelegatorState {
        cosigner_pk,
        fee,
        fee_address_script,
    })
}

/// Number of seconds in a UTC day.
const SECONDS_PER_DAY: i64 = 86_400;

/// Normalize a unix timestamp (seconds) to UTC midnight of that day.
fn day_timestamp(ts: i64) -> i64 {
    ts - ts.rem_euclid(SECONDS_PER_DAY)
}

/// Group VTXOs by their expiry day (UTC midnight), returning groups sorted by expiry.
///
/// Recoverable VTXOs (expired or sub-dust) are collected separately and merged into the earliest
/// non-recoverable group.
fn group_by_expiry_day(vtxos: &[AnnotatedVtxo], dust: Amount) -> Vec<(i64, Vec<&AnnotatedVtxo>)> {
    let mut groups: BTreeMap<i64, Vec<&AnnotatedVtxo>> = BTreeMap::new();
    let mut recoverable: Vec<&AnnotatedVtxo> = Vec::new();

    for entry in vtxos {
        if entry.vtxo().is_spent {
            continue;
        }

        if entry.contract().contract_type != ark_core::contract::ContractType::delegate_vtxo() {
            continue;
        }

        if entry.vtxo().is_recoverable(dust) {
            recoverable.push(entry);
        } else if entry.vtxo().expires_at > 0 {
            let day = day_timestamp(entry.vtxo().expires_at);
            groups.entry(day).or_default().push(entry);
        }
    }

    if !recoverable.is_empty() {
        if let Some((&earliest_day, _)) = groups.iter().next() {
            groups.entry(earliest_day).or_default().extend(recoverable);
        } else {
            groups.insert(0, recoverable);
        }
    }

    groups.into_iter().collect()
}

/// Calculate the `valid_at` timestamp for a delegation group.
///
/// For each non-recoverable VTXO, compute activation at 90% of its full lifetime:
/// `created_at + (expires_at - created_at) * 0.9`. The earliest of those activations is used.
///
/// If the group only contains recoverable/expired VTXOs (or activation is already in the past),
/// schedule soon (`now + 60s`).
fn calculate_valid_at(group_vtxos: &[&AnnotatedVtxo], dust: Amount) -> u64 {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let earliest_activation = group_vtxos
        .iter()
        .filter(|entry| {
            !entry.vtxo().is_recoverable(dust)
                && entry.vtxo().created_at > 0
                && entry.vtxo().expires_at > 0
                && entry.vtxo().expires_at > entry.vtxo().created_at
        })
        .map(|entry| {
            let created_at = entry.vtxo().created_at as u64;
            let lifetime = (entry.vtxo().expires_at - entry.vtxo().created_at) as u64;
            created_at + (lifetime * 9 / 10)
        })
        .min();

    match earliest_activation {
        Some(valid_at) if valid_at > now_secs => valid_at,
        _ => now_secs + 60,
    }
}

/// Submit newly received VTXOs to the delegator service for future auto-renewal.
///
/// Only the affected outpoints are delegated; spend metadata is resolved through contracts.
async fn delegate_vtxos<B, W, S>(
    client: &Arc<Client<B, W, S>>,
    delegator: &DelegatorClient,
    new_vtxos: &[VirtualTxOutPoint],
) where
    B: Blockchain + Send + Sync + 'static,
    W: OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
{
    let vtxo_list = match client.list_vtxos().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to list VTXOs for delegation: {e}");
            return;
        }
    };

    // The subscription event tells us which outpoints are new, but we need the full
    // VirtualTxOutPoint (with expires_at, created_at) from the server for grouping.
    let new_outpoints: HashSet<_> = new_vtxos.iter().map(|v| v.outpoint).collect();
    let enriched: Vec<_> = vtxo_list
        .all_unspent()
        .filter(|entry| new_outpoints.contains(&entry.vtxo().outpoint))
        .cloned()
        .collect();

    let server_info = match client.server_info().await {
        Ok(server_info) => server_info,
        Err(e) => {
            tracing::error!("Failed to read server info for delegation: {e}");
            return;
        }
    };

    let groups = group_by_expiry_day(&enriched, server_info.dust);
    if groups.is_empty() {
        tracing::debug!("No delegate-eligible VTXOs after enrichment/grouping; skipping");
        return;
    }

    let delegator_state = match fetch_delegator_state(delegator).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!("{e}");
            return;
        }
    };

    let (to_address, _) = match client.get_offchain_address().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to get offchain address for delegation: {e}");
            return;
        }
    };
    let dest_script = to_address.to_p2tr_script_pubkey();

    let mut handles = Vec::new();

    for (_day, group_vtxos) in groups {
        let valid_at = calculate_valid_at(&group_vtxos, server_info.dust);

        let mut vtxo_inputs = Vec::new();
        let mut total_amount = Amount::ZERO;

        for entry in &group_vtxos {
            let spend_selection = match entry
                .spend_selection(ark_core::contract::SpendPathKind::Delegate)
            {
                Ok(selection) => selection,
                Err(e) => {
                    tracing::warn!(outpoint = %entry.vtxo().outpoint, "Cannot get delegate spend selection: {e}");
                    continue;
                }
            };

            let exit_delay = match entry.exit_delay() {
                Ok(exit_delay) => exit_delay,
                Err(e) => {
                    tracing::warn!(outpoint = %entry.vtxo().outpoint, "Cannot get delegate exit delay: {e}");
                    continue;
                }
            };

            vtxo_inputs.push(intent::Input::new_with_spend_selection(
                entry.vtxo().outpoint,
                exit_delay,
                TxOut {
                    value: entry.vtxo().amount,
                    script_pubkey: entry.script_pubkey(),
                },
                entry.tapscripts(),
                spend_selection,
                entry.vtxo().is_spent,
                entry.vtxo().is_swept,
                entry.vtxo().assets.clone(),
            ));

            total_amount += entry.vtxo().amount;
        }

        if vtxo_inputs.is_empty() {
            continue;
        }

        let fee = delegator_state.fee;
        if fee >= total_amount {
            tracing::warn!(
                %total_amount, %fee,
                "Delegator fee exceeds VTXO group value, skipping"
            );
            continue;
        }
        let net_amount = total_amount - fee;

        if net_amount < server_info.dust {
            tracing::warn!(%net_amount, "Net amount after fee is below dust, skipping");
            continue;
        }

        let mut outputs = Vec::new();
        if fee > Amount::ZERO {
            outputs.push(intent::Output::Offchain(TxOut {
                value: fee,
                script_pubkey: delegator_state.fee_address_script.clone(),
            }));
        }
        outputs.push(intent::Output::Offchain(TxOut {
            value: net_amount,
            script_pubkey: dest_script.clone(),
        }));

        let server_info_forfeit_addr = server_info.forfeit_address.clone();
        let dust = server_info.dust;
        let ds = Arc::clone(&delegator_state);

        let delegator = delegator.clone();
        let client = Arc::clone(client);
        handles.push(tokio::spawn(async move {
            delegate_group(
                &client,
                &delegator,
                vtxo_inputs,
                outputs,
                ds.cosigner_pk,
                &server_info_forfeit_addr,
                dust,
                valid_at,
            )
            .await;
        }));
    }

    for handle in handles {
        let _ = handle.await;
    }
}

/// Prepare, sign, and submit a single delegation group.
async fn delegate_group<B, W, S>(
    client: &Client<B, W, S>,
    delegator: &DelegatorClient,
    vtxo_inputs: Vec<intent::Input>,
    outputs: Vec<intent::Output>,
    cosigner_pk: PublicKey,
    forfeit_address: &bitcoin::Address,
    dust: Amount,
    valid_at: u64,
) where
    B: Blockchain + Send + Sync + 'static,
    W: OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
{
    let input_count = vtxo_inputs.len();

    let mut delegate = match ark_core::batch::prepare_delegate_psbts_at(
        vtxo_inputs,
        outputs,
        cosigner_pk,
        forfeit_address,
        dust,
        Some(valid_at),
    ) {
        Ok(d) => d,
        Err(e) => {
            tracing::error!("Failed to prepare delegate PSBTs: {e}");
            return;
        }
    };

    if let Err(e) =
        client.sign_delegate_psbts(&mut delegate.intent.proof, &mut delegate.forfeit_psbts)
    {
        tracing::error!("Failed to sign delegate PSBTs: {e}");
        return;
    }

    if let Err(e) = delegator
        .delegate(&delegate.intent, &delegate.forfeit_psbts, None)
        .await
    {
        tracing::error!("Failed to submit delegation: {e}");
        return;
    }

    tracing::info!(
        vtxo_count = input_count,
        valid_at,
        "Delegated VTXO group to delegator service"
    );
}

/// Fraction of VTXO lifetime remaining at which we self-renew as a safety net.
const SELF_RENEW_REMAINING_FRACTION: f64 = 0.10;

/// Select VTXOs that should be self-renewed.
///
/// Includes all recoverable VTXOs (expired, swept, or sub-dust) plus VTXOs whose remaining lifetime
/// is less than [`SELF_RENEW_REMAINING_FRACTION`] of their total lifetime.
fn select_vtxos_for_self_renewal(vtxos: &[AnnotatedVtxo], dust: Amount, now: i64) -> Vec<OutPoint> {
    let selected: Vec<_> = vtxos
        .iter()
        .filter(|entry| {
            if entry.vtxo().is_recoverable(dust) {
                return true;
            }

            if entry.vtxo().expires_at <= 0 || entry.vtxo().created_at <= 0 {
                return false;
            }
            let total_lifetime = entry.vtxo().expires_at - entry.vtxo().created_at;
            let remaining = entry.vtxo().expires_at - now;
            remaining > 0
                && (remaining as f64) < (total_lifetime as f64 * SELF_RENEW_REMAINING_FRACTION)
        })
        .collect();

    let total_amount = selected
        .iter()
        .fold(Amount::ZERO, |total, entry| total + entry.vtxo().amount);
    if total_amount < dust {
        return Vec::new();
    }

    selected.iter().map(|entry| entry.vtxo().outpoint).collect()
}

/// Self-renew VTXOs that are close to expiry or already recoverable.
async fn renew_expiring_vtxos<B, W, S>(client: &Client<B, W, S>)
where
    B: Blockchain + Send + Sync + 'static,
    W: OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
{
    let vtxo_list = match client.list_vtxos().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to list VTXOs for renewal check: {e}");
            return;
        }
    };

    let server_info = match client.server_info().await {
        Ok(server_info) => server_info,
        Err(e) => {
            tracing::warn!("Failed to read server info for renewal check: {e}");
            return;
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let unspent: Vec<_> = vtxo_list.all_unspent().cloned().collect();
    let expiring_outpoints = select_vtxos_for_self_renewal(&unspent, server_info.dust, now);

    if expiring_outpoints.is_empty() {
        return;
    }

    tracing::info!(
        count = expiring_outpoints.len(),
        "Self-renewing expiring/recoverable VTXOs"
    );

    let mut rng = OsRng;
    match client
        .settle_vtxos(&mut rng, &expiring_outpoints, &[])
        .await
    {
        Ok(Some(txid)) => {
            tracing::info!(%txid, "Self-renewed expiring VTXOs");
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!("Failed to self-renew VTXOs: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::hashes::Hash;
    use bitcoin::key::Secp256k1;
    use bitcoin::Network;
    use bitcoin::Sequence;
    use bitcoin::Txid;
    use bitcoin::XOnlyPublicKey;
    use std::str::FromStr;

    fn test_keys() -> (XOnlyPublicKey, XOnlyPublicKey, XOnlyPublicKey) {
        let server = XOnlyPublicKey::from_str(
            "18845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();
        let owner = XOnlyPublicKey::from_str(
            "28845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();
        let delegator = XOnlyPublicKey::from_str(
            "38845781f631c48f1c9709e23092067d06837f30aa0cd0544ac887fe91ddd166",
        )
        .unwrap();
        (server, owner, delegator)
    }

    fn delegated_vtxo() -> (ArkAddress, Vtxo) {
        let secp = Secp256k1::new();
        let (server, owner, delegator) = test_keys();
        let vtxo = Vtxo::new_with_delegator(
            &secp,
            server,
            owner,
            delegator,
            Sequence::from_seconds_ceil(86400).unwrap(),
            Network::Regtest,
        )
        .unwrap();
        (vtxo.to_ark_address(), vtxo)
    }

    fn mk_contract_vtxo(
        script: ScriptBuf,
        amount_sat: u64,
        expires_at: i64,
        vout: u32,
    ) -> AnnotatedVtxo {
        use ark_core::contract::ContractState;
        use ark_core::contract::ContractType;
        use ark_core::contract::DelegateVtxoContract;
        use ark_core::contract::SpendPath;
        use ark_core::contract::SpendPathKind;
        use ark_core::contract::StoredContract;

        let (server, owner, delegator) = test_keys();
        let contract = DelegateVtxoContract {
            server,
            owner,
            delegator,
            exit_delay: Sequence::from_seconds_ceil(86400).unwrap(),
        };
        let vtxo = VirtualTxOutPoint {
            outpoint: OutPoint::new(Txid::all_zeros(), vout),
            created_at: expires_at - 1000,
            expires_at,
            amount: Amount::from_sat(amount_sat),
            script: script.clone(),
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: vec![],
            settled_by: None,
            ark_txid: None,
            assets: vec![],
            depth: 0,
        };
        AnnotatedVtxo::new(
            StoredContract {
                contract_type: ContractType::delegate_vtxo(),
                contract_version: 1,
                script_pubkey: script,
                state: ContractState::Active,
                created_at: 0,
                key_index: None,
                data: serde_json::to_value(contract).unwrap(),
            },
            vtxo,
            vec![SpendPath::new(
                SpendPathKind::Delegate,
                ScriptBuf::new(),
                dummy_control_block(),
            )
            .select()],
        )
    }

    fn dummy_control_block() -> bitcoin::taproot::ControlBlock {
        let secp = Secp256k1::new();
        let internal_key = test_keys().0;
        let spend_info = bitcoin::taproot::TaprootBuilder::new()
            .add_leaf(0, ScriptBuf::new())
            .unwrap()
            .finalize(&secp, internal_key)
            .unwrap();
        spend_info
            .control_block(&(ScriptBuf::new(), bitcoin::taproot::LeafVersion::TapScript))
            .unwrap()
    }

    #[test]
    fn migration_delay_uses_base_interval_when_healthy() {
        assert_eq!(migration_delay(0), MIGRATION_INTERVAL);
    }

    #[test]
    fn migration_delay_backs_off_exponentially_and_caps() {
        // 30s * 2^(failures-1): 30s, 60s, 120s, 240s, then saturates at the 5min cap.
        assert_eq!(migration_delay(1), MIGRATION_BASE_COOLDOWN);
        assert_eq!(migration_delay(2), MIGRATION_BASE_COOLDOWN * 2);
        assert_eq!(migration_delay(3), MIGRATION_BASE_COOLDOWN * 4);
        assert_eq!(migration_delay(4), MIGRATION_BASE_COOLDOWN * 8);
        assert_eq!(migration_delay(5), MIGRATION_MAX_COOLDOWN);
        // Large failure counts must not panic (no shift overflow) and stay at the cap.
        assert_eq!(migration_delay(100), MIGRATION_MAX_COOLDOWN);
        assert_eq!(migration_delay(u32::MAX), MIGRATION_MAX_COOLDOWN);
    }

    #[test]
    fn day_timestamp_normalizes_to_midnight() {
        let ts = 1705322700; // 2024-01-15 13:45:00 UTC
        let day = day_timestamp(ts);
        assert_eq!(day % SECONDS_PER_DAY, 0);
        assert!(day <= ts);
        assert!(ts - day < SECONDS_PER_DAY);
    }

    #[test]
    fn day_timestamp_already_midnight() {
        let ts = SECONDS_PER_DAY * 19738;
        assert_eq!(day_timestamp(ts), ts);
    }

    #[test]
    fn group_by_expiry_day_merges_recoverable_into_earliest_group() {
        let (addr, _) = delegated_vtxo();
        let script = addr.to_p2tr_script_pubkey();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let day1_midnight = day_timestamp(now) + SECONDS_PER_DAY;
        let day2_midnight = day1_midnight + SECONDS_PER_DAY;

        let recoverable = mk_contract_vtxo(script.clone(), 100, day1_midnight + 500, 0); // sub-dust
        let non_recoverable_day1 = mk_contract_vtxo(script.clone(), 10_000, day1_midnight + 800, 1);
        let non_recoverable_day2 = mk_contract_vtxo(script, 10_000, day2_midnight + 800, 2);

        let vtxos = [non_recoverable_day2, recoverable, non_recoverable_day1];
        let groups = group_by_expiry_day(&vtxos, Amount::from_sat(500));

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, day_timestamp(day1_midnight + 800));
        assert_eq!(groups[1].0, day_timestamp(day2_midnight + 800));
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].1.len(), 1);
    }

    #[test]
    fn calculate_valid_at_for_non_recoverable_group_is_before_expiry() {
        let script = ScriptBuf::new();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let later = mk_contract_vtxo(script, 10_000, now + 10_000, 1);
        let group = vec![&later];

        let valid_at = calculate_valid_at(&group, Amount::from_sat(500));

        assert!(valid_at > now as u64);
        assert!(valid_at < later.vtxo().expires_at as u64);
    }

    #[test]
    fn select_vtxos_for_self_renewal_includes_expired_and_subdust() {
        let script = ScriptBuf::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let expired = mk_contract_vtxo(script.clone(), 10_000, now - 1, 0);
        let subdust = mk_contract_vtxo(script.clone(), 100, now + 10_000, 1);
        let fresh = mk_contract_vtxo(script, 10_000, now + 10_000, 2);

        let selected = select_vtxos_for_self_renewal(
            &[expired.clone(), subdust.clone(), fresh],
            Amount::from_sat(500),
            now,
        );

        assert_eq!(
            selected,
            vec![expired.vtxo().outpoint, subdust.vtxo().outpoint]
        );
    }

    #[test]
    fn select_vtxos_for_self_renewal_includes_near_expiry() {
        let script = ScriptBuf::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let near_expiry = mk_contract_vtxo(script, 10_000, now + 50, 0);

        let selected = select_vtxos_for_self_renewal(
            std::slice::from_ref(&near_expiry),
            Amount::from_sat(500),
            now,
        );

        assert_eq!(selected, vec![near_expiry.vtxo().outpoint]);
    }

    #[test]
    fn select_vtxos_for_self_renewal_skips_when_total_is_below_dust() {
        let script = ScriptBuf::new();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let subdust1 = mk_contract_vtxo(script.clone(), 100, now + 10_000, 0);
        let subdust2 = mk_contract_vtxo(script, 200, now + 10_000, 1);

        let selected =
            select_vtxos_for_self_renewal(&[subdust1, subdust2], Amount::from_sat(500), now);

        assert!(selected.is_empty());
    }

    #[test]
    fn calculate_valid_at_for_recoverable_only_group_is_soon() {
        let script = ScriptBuf::new();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let recoverable = mk_contract_vtxo(script, 100, now + 5_000, 0); // sub-dust at dust=500
        let group = vec![&recoverable];

        let start = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let valid_at = calculate_valid_at(&group, Amount::from_sat(500));
        let end = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        assert!(valid_at >= start + 60);
        assert!(valid_at <= end + 61);
    }

    /// Distinct valid Arkade address derived from a seed byte, for exercising the script-diff
    /// logic.
    fn ark_address(seed: u8) -> ArkAddress {
        let secp = Secp256k1::new();
        let (server, _owner, delegator) = test_keys();
        let sk = bitcoin::secp256k1::SecretKey::from_slice(&[seed; 32]).unwrap();
        let owner = sk.public_key(&secp).x_only_public_key().0;
        Vtxo::new_with_delegator(
            &secp,
            server,
            owner,
            delegator,
            Sequence::from_seconds_ceil(86400).unwrap(),
            Network::Regtest,
        )
        .unwrap()
        .to_ark_address()
    }

    #[test]
    fn additional_scripts_filter_selects_only_unsubscribed_addresses() {
        let a = ark_address(1);
        let b = ark_address(2);
        let mut subscribed = HashSet::new();
        subscribed.insert(a);

        let (filter, new_addrs) =
            additional_scripts_filter(vec![a, b], &subscribed).expect("b is not yet subscribed");

        assert_eq!(new_addrs, vec![b]);
        assert_eq!(filter.add_scripts, vec![b.to_p2tr_script_pubkey()]);
        assert!(filter.expressions.is_empty());
        assert!(filter.remove_scripts.is_empty());
    }

    #[test]
    fn additional_scripts_filter_none_when_all_already_subscribed() {
        let a = ark_address(1);
        let subscribed: HashSet<_> = [a].into_iter().collect();
        assert!(additional_scripts_filter(vec![a], &subscribed).is_none());
    }

    #[tokio::test]
    async fn subscribe_within_deadline_returns_ready_subscription() {
        let (_tx, mut rx) = watch::channel(false);
        let outcome = subscribe_within_deadline(&mut rx, Duration::from_secs(30), async {
            Ok::<_, Error>(7)
        })
        .await;
        assert!(matches!(outcome, SubscribeOutcome::Ready(7)));
    }

    #[tokio::test]
    async fn subscribe_within_deadline_stops_on_signal() {
        let (tx, mut rx) = watch::channel(false);
        tx.send(true).unwrap();
        let outcome = subscribe_within_deadline(
            &mut rx,
            Duration::from_secs(30),
            std::future::pending::<Result<i32, Error>>(),
        )
        .await;
        assert!(matches!(outcome, SubscribeOutcome::Stopped));
    }

    #[tokio::test]
    async fn subscribe_within_deadline_retries_on_timeout() {
        let (_tx, mut rx) = watch::channel(false);
        let outcome = subscribe_within_deadline(
            &mut rx,
            Duration::from_millis(10),
            std::future::pending::<Result<i32, Error>>(),
        )
        .await;
        assert!(matches!(outcome, SubscribeOutcome::Retry));
    }

    #[tokio::test]
    async fn subscribe_within_deadline_retries_on_error() {
        let (_tx, mut rx) = watch::channel(false);
        let outcome = subscribe_within_deadline(&mut rx, Duration::from_secs(30), async {
            Err::<i32, _>(Error::ad_hoc("boom"))
        })
        .await;
        assert!(matches!(outcome, SubscribeOutcome::Retry));
    }
}

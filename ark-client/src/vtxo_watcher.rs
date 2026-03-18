//! Background VTXO watcher that auto-delegates and auto-renews VTXOs.
//!
//! Full behavior:
//! - On new VTXOs received: submit them to the delegator service for future renewal
//! - On new VTXOs received: self-renew VTXOs that are close to expiry (safety net)
//! - On stream error: reconnect with exponential backoff

use crate::key_provider::KeyProvider;
use crate::swap_storage::SwapStorage;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use ark_core::intent;
use ark_core::server::SubscriptionResponse;
use ark_core::server::VirtualTxOutPoint;
use ark_core::Vtxo;
use ark_delegator::DelegatorClient;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Amount;
use bitcoin::ScriptBuf;
use bitcoin::TxOut;
use futures::StreamExt;
use rand::rngs::OsRng;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
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

impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain + Send + Sync + 'static,
    W: BoardingWallet + OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
    K: KeyProvider + Send + Sync + 'static,
{
    /// Start a background task that watches for new VTXOs and:
    ///
    /// 1. **Delegates** them to the configured delegator service for future auto-renewal
    /// 2. **Self-renews** VTXOs that are close to expiry (safety net)
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
    ) -> VtxoWatcherHandle {
        let (stop_tx, stop_rx) = watch::channel(false);

        let client = Arc::clone(self);
        tokio::spawn(async move {
            run_watcher_loop(client, delegator, stop_rx).await;
            tracing::debug!("VTXO watcher stopped");
        });

        VtxoWatcherHandle { stop_tx }
    }
}

/// Outer loop that reconnects on stream errors with exponential backoff.
async fn run_watcher_loop<B, W, S, K>(
    client: Arc<Client<B, W, S, K>>,
    delegator: Arc<DelegatorClient>,
    mut stop_rx: watch::Receiver<bool>,
) where
    B: Blockchain + Send + Sync + 'static,
    W: BoardingWallet + OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
    K: KeyProvider + Send + Sync + 'static,
{
    let mut backoff = INITIAL_BACKOFF;

    loop {
        if *stop_rx.borrow() {
            return;
        }

        let addresses = match client.get_offchain_addresses() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("Failed to get offchain addresses: {e}");
                return;
            }
        };
        let ark_addresses: Vec<_> = addresses.iter().map(|(addr, _)| *addr).collect();

        let subscription_id = match client.subscribe_to_scripts(ark_addresses, None).await {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!("Failed to subscribe: {e}, retrying in {backoff:?}");
                if wait_or_stop(&mut stop_rx, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        let mut stream = match client.get_subscription(subscription_id.clone()).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to get subscription stream: {e}, retrying in {backoff:?}");
                if wait_or_stop(&mut stop_rx, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            }
        };

        tracing::info!("VTXO watcher connected");
        backoff = INITIAL_BACKOFF; // Reset on successful connection.

        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    return;
                }
                event = stream.next() => {
                    match event {
                        Some(Ok(SubscriptionResponse::Event(event))) => {
                            if !event.new_vtxos.is_empty() {
                                let client = Arc::clone(&client);
                                let delegator = Arc::clone(&delegator);
                                let new_vtxos = event.new_vtxos;
                                tokio::spawn(async move {
                                    delegate_vtxos(&client, &delegator, &new_vtxos).await;
                                    renew_expiring_vtxos(&client).await;
                                });
                            }
                        }
                        Some(Ok(SubscriptionResponse::Heartbeat)) => {}
                        Some(Err(e)) => {
                            tracing::warn!("VTXO subscription error: {e}, reconnecting in {backoff:?}");
                            break; // Break inner loop to reconnect.
                        }
                        None => {
                            tracing::debug!("VTXO subscription stream ended, reconnecting in {backoff:?}");
                            break;
                        }
                    }
                }
            }
        }

        // Wait before reconnecting.
        if wait_or_stop(&mut stop_rx, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Wait for the given duration or until stop is signalled. Returns `true` if stopped.
async fn wait_or_stop(stop_rx: &mut watch::Receiver<bool>, duration: Duration) -> bool {
    tokio::select! {
        _ = stop_rx.changed() => true,
        _ = tokio::time::sleep(duration) => false,
    }
}

/// Delegator info cached per delegation batch.
struct DelegatorState {
    cosigner_pk: PublicKey,
    fee: Amount,
    fee_address_script: ScriptBuf,
}

/// Fetch and parse delegator info into a usable form.
async fn fetch_delegator_state(delegator: &DelegatorClient) -> Option<DelegatorState> {
    let info = match delegator.info().await {
        Ok(info) => info,
        Err(e) => {
            tracing::error!("Failed to get delegator info: {e}");
            return None;
        }
    };

    let cosigner_pk: PublicKey = match info.pubkey.parse() {
        Ok(pk) => pk,
        Err(e) => {
            tracing::error!("Failed to parse delegator pubkey: {e}");
            return None;
        }
    };

    let fee = match info.fee.parse::<u64>() {
        Ok(f) => Amount::from_sat(f),
        Err(e) => {
            tracing::error!("Failed to parse delegator fee: {e}");
            return None;
        }
    };

    let fee_address: bitcoin::Address<bitcoin::address::NetworkUnchecked> =
        match info.delegator_address.parse() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("Failed to parse delegator address: {e}");
                return None;
            }
        };
    let fee_address_script = fee_address.assume_checked().script_pubkey();

    Some(DelegatorState {
        cosigner_pk,
        fee,
        fee_address_script,
    })
}

/// Normalize a unix timestamp (seconds) to UTC midnight of that day.
fn day_timestamp(ts: i64) -> i64 {
    // 86400 seconds per day. Integer division floors toward zero, which is correct for positive
    // timestamps (anything after 1970).
    (ts / 86400) * 86400
}

/// Group VTXOs by their expiry day (UTC midnight), returning groups sorted by expiry.
///
/// Recoverable VTXOs (expired or sub-dust) are collected separately and merged into the earliest
/// group, matching the ts-sdk behaviour.
fn group_by_expiry_day<'a>(
    new_vtxos: &'a [VirtualTxOutPoint],
    script_pubkey_to_vtxo: &'a HashMap<ScriptBuf, Vtxo>,
    dust: Amount,
) -> Vec<(i64, Vec<(&'a VirtualTxOutPoint, &'a Vtxo)>)> {
    let mut groups: BTreeMap<i64, Vec<(&'a VirtualTxOutPoint, &'a Vtxo)>> = BTreeMap::new();
    let mut recoverable: Vec<(&'a VirtualTxOutPoint, &'a Vtxo)> = Vec::new();

    for vtp in new_vtxos {
        if vtp.is_spent {
            continue;
        }

        let vtxo = match script_pubkey_to_vtxo.get(&vtp.script) {
            Some(v) => v,
            None => continue,
        };

        // Only delegate VTXOs that have a delegate spending path.
        if vtxo.delegator_pk().is_none() {
            continue;
        }

        if vtp.is_recoverable(dust) {
            recoverable.push((vtp, vtxo));
        } else if vtp.expires_at > 0 {
            let day = day_timestamp(vtp.expires_at);
            groups.entry(day).or_default().push((vtp, vtxo));
        }
    }

    // Merge recoverable VTXOs into the earliest group.
    if !recoverable.is_empty() {
        if let Some((&earliest_day, _)) = groups.iter().next() {
            groups.entry(earliest_day).or_default().extend(recoverable);
        } else {
            // No normal groups — create a standalone group for recoverables.
            groups.insert(0, recoverable);
        }
    }

    groups.into_iter().collect()
}

/// Calculate the `valid_at` timestamp for a delegation group.
///
/// `valid_at` is set to 90% through the VTXO lifetime (10% before expiry), matching the ts-sdk.
/// For recoverable/expired VTXOs (group day = 0 or expiry in the past), returns `now + 60s`.
fn calculate_valid_at(group_vtxos: &[(&VirtualTxOutPoint, &Vtxo)]) -> u64 {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    // Find the earliest expiry in the group.
    let earliest_expiry = group_vtxos
        .iter()
        .filter(|(vtp, _)| !vtp.is_recoverable(Amount::ZERO) && vtp.expires_at > 0)
        .map(|(vtp, _)| vtp.expires_at as u64)
        .min();

    match earliest_expiry {
        Some(expiry) if expiry > now_secs => {
            let remaining = expiry - now_secs;
            // Delegate at 90% through the remaining lifetime (10% before expiry).
            expiry - remaining / 10
        }
        _ => {
            // Recoverable or already expired: delegate 1 minute from now.
            now_secs + 60
        }
    }
}

/// Submit newly received VTXOs to the delegator service for future auto-renewal.
///
/// VTXOs are grouped by expiry day and each group is delegated in parallel with the appropriate
/// `valid_at` timestamp, matching the ts-sdk behaviour.
async fn delegate_vtxos<B, W, S, K>(
    client: &Arc<Client<B, W, S, K>>,
    delegator: &DelegatorClient,
    new_vtxos: &[VirtualTxOutPoint],
) where
    B: Blockchain + Send + Sync + 'static,
    W: BoardingWallet + OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
    K: KeyProvider + Send + Sync + 'static,
{
    // Pretty rough to fetch the stuff for every VTXO, when we know the outpoints we
    let (_, script_pubkey_to_vtxo) = match client.list_vtxos().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to list VTXOs for delegation: {e}");
            return;
        }
    };

    let groups = group_by_expiry_day(new_vtxos, &script_pubkey_to_vtxo, client.server_info.dust);
    if groups.is_empty() {
        return;
    }

    let delegator_state = match fetch_delegator_state(delegator).await {
        Some(s) => Arc::new(s),
        None => return,
    };

    let (to_address, _) = match client.get_offchain_address() {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to get offchain address for delegation: {e}");
            return;
        }
    };
    let dest_script = to_address.to_p2tr_script_pubkey();

    let mut handles = Vec::new();

    for (_day, group_vtxos) in groups {
        let valid_at = calculate_valid_at(&group_vtxos);

        // Build inputs for this group.
        let mut vtxo_inputs = Vec::new();
        let mut total_amount = Amount::ZERO;

        for (vtp, vtxo) in &group_vtxos {
            let spend_info = match vtxo.delegate_spend_info() {
                Ok(info) => info,
                Err(e) => {
                    tracing::warn!(outpoint = %vtp.outpoint, "Cannot get delegate spend info: {e}");
                    continue;
                }
            };

            vtxo_inputs.push(intent::Input::new(
                vtp.outpoint,
                vtxo.exit_delay(),
                None,
                TxOut {
                    value: vtp.amount,
                    script_pubkey: vtp.script.clone(),
                },
                vtxo.tapscripts(),
                spend_info,
                vtp.is_spent,
                false,
                vtp.assets.clone(),
            ));

            total_amount += vtp.amount;
        }

        if vtxo_inputs.is_empty() {
            continue;
        }

        // Deduct delegator fee.
        let fee = delegator_state.fee;
        if fee >= total_amount {
            tracing::warn!(
                %total_amount, %fee,
                "Delegator fee exceeds VTXO group value, skipping"
            );
            continue;
        }
        let net_amount = total_amount - fee;

        if net_amount < client.server_info.dust {
            tracing::warn!(
                %net_amount,
                "Net amount after fee is below dust, skipping"
            );
            continue;
        }

        // Build outputs: fee to delegator (if non-zero), remainder to self.
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

        let server_info_forfeit_addr = client.server_info.forfeit_address.clone();
        let dust = client.server_info.dust;
        let ds = Arc::clone(&delegator_state);

        // Spawn each group's delegation in parallel.
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
async fn delegate_group<B, W, S, K>(
    client: &Client<B, W, S, K>,
    delegator: &DelegatorClient,
    vtxo_inputs: Vec<intent::Input>,
    outputs: Vec<intent::Output>,
    cosigner_pk: PublicKey,
    forfeit_address: &bitcoin::Address,
    dust: Amount,
    valid_at: u64,
) where
    B: Blockchain + Send + Sync + 'static,
    W: BoardingWallet + OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
    K: KeyProvider + Send + Sync + 'static,
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
///
/// The ts-sdk delegates at 10% remaining. We self-renew at 5% remaining, giving the delegator
/// most of the window while still catching stragglers.
const SELF_RENEW_REMAINING_FRACTION: f64 = 0.05;

/// Self-renew VTXOs that are close to expiry. Acts as a safety net in case the delegator service
/// is slow or unavailable.
///
/// Only renews VTXOs whose remaining lifetime is less than [`SELF_RENEW_REMAINING_FRACTION`] of
/// their total lifetime, so freshly-received VTXOs (already handed to the delegator) are left
/// alone.
async fn renew_expiring_vtxos<B, W, S, K>(client: &Client<B, W, S, K>)
where
    B: Blockchain + Send + Sync + 'static,
    W: BoardingWallet + OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
    K: KeyProvider + Send + Sync + 'static,
{
    let (vtxo_list, _) = match client.list_vtxos().await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("Failed to list VTXOs for renewal check: {e}");
            return;
        }
    };

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    let has_expiring = vtxo_list.all_unspent().any(|vtp| {
        if vtp.expires_at <= 0 || vtp.created_at <= 0 {
            return false;
        }
        let total_lifetime = vtp.expires_at - vtp.created_at;
        let remaining = vtp.expires_at - now;
        remaining > 0
            && (remaining as f64) < (total_lifetime as f64 * SELF_RENEW_REMAINING_FRACTION)
    });

    if !has_expiring {
        return;
    }

    // We should only settle the VTXOs that meet the condition, not all.
    let mut rng = OsRng;
    match client.settle(&mut rng).await {
        Ok(Some(txid)) => {
            tracing::info!(%txid, "Self-renewed expiring VTXOs");
        }
        Ok(None) => {}
        Err(e) => {
            let msg = e.to_string();
            if !msg.contains("no inputs") {
                tracing::warn!("Failed to self-renew VTXOs: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn day_timestamp_normalizes_to_midnight() {
        // 2024-01-15 13:45:00 UTC → 2024-01-15 00:00:00 UTC
        let ts = 1705322700;
        let day = day_timestamp(ts);
        assert_eq!(day % 86400, 0);
        assert!(day <= ts);
        assert!(ts - day < 86400);
    }

    #[test]
    fn day_timestamp_already_midnight() {
        let ts = 86400 * 19738; // exact midnight
        assert_eq!(day_timestamp(ts), ts);
    }
}

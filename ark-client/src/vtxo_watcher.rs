//! Background VTXO watcher that auto-delegates and auto-renews VTXOs.
//!
//! Full behavior:
//! - On new VTXOs received: submit them to the delegator service for future renewal
//! - On new VTXOs received: self-renew VTXOs that are close to expiry (safety net)
//! - On stream error: reconnect with exponential backoff

use crate::error::ErrorContext;
use crate::key_provider::KeyProvider;
use crate::swap_storage::SwapStorage;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::intent;
use ark_core::server::SubscriptionResponse;
use ark_core::server::VirtualTxOutPoint;
use ark_core::ArkAddress;
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
use std::collections::HashMap;
use std::collections::HashSet;
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

/// Pre-computed mapping from script pubkeys to their Vtxo metadata and ArkAddress.
///
/// Built once per (re)connection from `get_offchain_addresses()`. Used both for the subscription
/// and for resolving VTXO metadata from subscription events, so they can never diverge.
struct ScriptMap {
    vtxo_by_script: HashMap<ScriptBuf, Vtxo>,
    addr_by_script: HashMap<ScriptBuf, ArkAddress>,
}

impl ScriptMap {
    fn from_addresses(addresses: &[(ArkAddress, Vtxo)]) -> Self {
        let mut vtxo_by_script = HashMap::with_capacity(addresses.len());
        let mut addr_by_script = HashMap::with_capacity(addresses.len());
        for (addr, vtxo) in addresses {
            let script = addr.to_p2tr_script_pubkey();
            vtxo_by_script.insert(script.clone(), vtxo.clone());
            addr_by_script.insert(script, *addr);
        }
        Self {
            vtxo_by_script,
            addr_by_script,
        }
    }

    /// Get the unique ArkAddresses that appear in the given VTXO outpoints.
    fn addresses_for(&self, vtxos: &[VirtualTxOutPoint]) -> Vec<ArkAddress> {
        let mut seen = HashSet::new();
        let mut result = Vec::new();
        for vtp in vtxos {
            if let Some(addr) = self.addr_by_script.get(&vtp.script) {
                if seen.insert(&vtp.script) {
                    result.push(*addr);
                }
            }
        }
        result
    }
}

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

        // Build the script map and subscription from the same address set.
        let addresses = match client.get_offchain_addresses() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!("Failed to get offchain addresses: {e}");
                return;
            }
        };
        let script_map = Arc::new(ScriptMap::from_addresses(&addresses));
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
        backoff = INITIAL_BACKOFF;
        let mut known_key_count = addresses.len();
        let mut script_map = script_map;

        loop {
            tokio::select! {
                _ = stop_rx.changed() => {
                    return;
                }
                event = stream.next() => {
                    match event {
                        Some(Ok(SubscriptionResponse::Heartbeat)) => {
                            // Check if new keys have been derived since we subscribed.
                            if let Ok(addrs) = client.get_offchain_addresses() {
                                if addrs.len() > known_key_count {
                                    let new_addrs: Vec<_> = addrs[known_key_count..]
                                        .iter()
                                        .map(|(addr, _)| *addr)
                                        .collect();
                                    tracing::debug!(
                                        count = new_addrs.len(),
                                        "Adding newly derived addresses to subscription"
                                    );
                                    match client
                                        .subscribe_to_scripts(
                                            new_addrs,
                                            Some(subscription_id.clone()),
                                        )
                                        .await
                                    {
                                        Ok(()) => {
                                            script_map = Arc::new(ScriptMap::from_addresses(&addrs));
                                            known_key_count = addrs.len();
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "Failed to add scripts to subscription: {e}"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                        Some(Ok(SubscriptionResponse::Event(event))) => {
                            if !event.new_vtxos.is_empty() {
                                let client = Arc::clone(&client);
                                let delegator = Arc::clone(&delegator);
                                let script_map = Arc::clone(&script_map);
                                let new_vtxos = event.new_vtxos;
                                tokio::spawn(async move {
                                    delegate_vtxos(&client, &delegator, &new_vtxos, &script_map).await;
                                    renew_expiring_vtxos(&client).await;
                                });
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
async fn fetch_delegator_state(
    delegator: &DelegatorClient,
    network: bitcoin::Network,
) -> Result<DelegatorState, Error> {
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

    let fee_address: bitcoin::Address<bitcoin::address::NetworkUnchecked> = info
        .delegator_address
        .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
        .context("failed to parse delegator fee address")?;

    let fee_address = fee_address
        .require_network(network)
        .context("wrong network for delegator fee address")?;

    Ok(DelegatorState {
        cosigner_pk,
        fee,
        fee_address_script: fee_address.script_pubkey(),
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
fn group_by_expiry_day<'a>(
    vtxos: &'a [VirtualTxOutPoint],
    script_map: &'a ScriptMap,
    dust: Amount,
) -> Vec<(i64, Vec<(&'a VirtualTxOutPoint, &'a Vtxo)>)> {
    let mut groups: BTreeMap<i64, Vec<(&'a VirtualTxOutPoint, &'a Vtxo)>> = BTreeMap::new();
    let mut recoverable: Vec<(&'a VirtualTxOutPoint, &'a Vtxo)> = Vec::new();

    for vtp in vtxos {
        if vtp.is_spent {
            continue;
        }

        let vtxo = match script_map.vtxo_by_script.get(&vtp.script) {
            Some(v) => v,
            None => continue,
        };

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
/// Uses the earliest non-recoverable expiry in the group as a reference and schedules renewal at
/// 90% of the remaining lifetime (i.e. roughly 10% before expiry).
///
/// If the group only contains recoverable/expired VTXOs, schedule soon (`now + 60s`).
fn calculate_valid_at(group_vtxos: &[(&VirtualTxOutPoint, &Vtxo)], dust: Amount) -> u64 {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let earliest_expiry = group_vtxos
        .iter()
        .filter(|(vtp, _)| !vtp.is_recoverable(dust) && vtp.expires_at > 0)
        .map(|(vtp, _)| vtp.expires_at as u64)
        .min();

    match earliest_expiry {
        // Schedule roughly 10% before expiry based on remaining lifetime.
        Some(expiry) if expiry > now_secs => {
            let remaining_lifetime = expiry - now_secs;
            let renewal_lead_time = remaining_lifetime / 10;
            expiry.saturating_sub(renewal_lead_time)
        }
        // Recoverable-only or already-expired groups: renew quickly.
        _ => now_secs + 60,
    }
}

/// Submit newly received VTXOs to the delegator service for future auto-renewal.
///
/// The `script_map` provides VTXO metadata (tapscripts, spend info) without a network call.
/// Only the affected addresses are queried for expiry data.
async fn delegate_vtxos<B, W, S, K>(
    client: &Arc<Client<B, W, S, K>>,
    delegator: &DelegatorClient,
    new_vtxos: &[VirtualTxOutPoint],
    script_map: &ScriptMap,
) where
    B: Blockchain + Send + Sync + 'static,
    W: BoardingWallet + OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
    K: KeyProvider + Send + Sync + 'static,
{
    // Query only the addresses that appear in the event, not all wallet addresses.
    let affected_addresses = script_map.addresses_for(new_vtxos);
    if affected_addresses.is_empty() {
        return;
    }

    let vtxo_list = match client
        .list_vtxos_for_addresses(affected_addresses.into_iter())
        .await
    {
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
        .filter(|vtp| new_outpoints.contains(&vtp.outpoint))
        .cloned()
        .collect();

    let groups = group_by_expiry_day(&enriched, script_map, client.server_info.dust);
    if groups.is_empty() {
        return;
    }

    let delegator_state = match fetch_delegator_state(delegator, client.server_info.network).await {
        Ok(s) => Arc::new(s),
        Err(e) => {
            tracing::error!("{e}");
            return;
        }
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
        let valid_at = calculate_valid_at(&group_vtxos, client.server_info.dust);

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

        let server_info_forfeit_addr = client.server_info.forfeit_address.clone();
        let dust = client.server_info.dust;
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
const SELF_RENEW_REMAINING_FRACTION: f64 = 0.05;

/// Self-renew VTXOs that are close to expiry.
///
/// Only settles VTXOs whose remaining lifetime is less than [`SELF_RENEW_REMAINING_FRACTION`] of
/// their total lifetime, leaving freshly-received VTXOs alone.
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

    let expiring_outpoints: Vec<OutPoint> = vtxo_list
        .all_unspent()
        .filter(|vtp| {
            if vtp.expires_at <= 0 || vtp.created_at <= 0 {
                return false;
            }
            let total_lifetime = vtp.expires_at - vtp.created_at;
            let remaining = vtp.expires_at - now;
            remaining > 0
                && (remaining as f64) < (total_lifetime as f64 * SELF_RENEW_REMAINING_FRACTION)
        })
        .map(|vtp| vtp.outpoint)
        .collect();

    if expiring_outpoints.is_empty() {
        return;
    }

    tracing::info!(
        count = expiring_outpoints.len(),
        "Self-renewing expiring VTXOs"
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

    fn mk_vtp(script: ScriptBuf, amount_sat: u64, expires_at: i64, vout: u32) -> VirtualTxOutPoint {
        VirtualTxOutPoint {
            outpoint: OutPoint::new(Txid::all_zeros(), vout),
            created_at: expires_at - 1000,
            expires_at,
            amount: Amount::from_sat(amount_sat),
            script,
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent: false,
            spent_by: None,
            commitment_txids: vec![],
            settled_by: None,
            ark_txid: None,
            assets: vec![],
        }
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
        let (addr, vtxo) = delegated_vtxo();
        let script = addr.to_p2tr_script_pubkey();
        let script_map = ScriptMap::from_addresses(&[(addr, vtxo)]);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let day1_midnight = day_timestamp(now) + SECONDS_PER_DAY;
        let day2_midnight = day1_midnight + SECONDS_PER_DAY;

        let recoverable = mk_vtp(script.clone(), 100, day1_midnight + 500, 0); // sub-dust
        let non_recoverable_day1 = mk_vtp(script.clone(), 10_000, day1_midnight + 800, 1);
        let non_recoverable_day2 = mk_vtp(script, 10_000, day2_midnight + 800, 2);

        let vtxos = [non_recoverable_day2, recoverable, non_recoverable_day1];
        let groups = group_by_expiry_day(&vtxos, &script_map, Amount::from_sat(500));

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, day_timestamp(day1_midnight + 800));
        assert_eq!(groups[1].0, day_timestamp(day2_midnight + 800));
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].1.len(), 1);
    }

    #[test]
    fn calculate_valid_at_for_non_recoverable_group_is_before_expiry() {
        let (_addr, vtxo) = delegated_vtxo();
        let script = ScriptBuf::new();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let later = mk_vtp(script, 10_000, now + 10_000, 1);
        let group = vec![(&later, &vtxo)];

        let valid_at = calculate_valid_at(&group, Amount::from_sat(500));

        assert!(valid_at > now as u64);
        assert!(valid_at < later.expires_at as u64);
    }

    #[test]
    fn calculate_valid_at_for_recoverable_only_group_is_soon() {
        let (_addr, vtxo) = delegated_vtxo();
        let script = ScriptBuf::new();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let recoverable = mk_vtp(script, 100, now + 5_000, 0); // sub-dust at dust=500
        let group = vec![(&recoverable, &vtxo)];

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
}

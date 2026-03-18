//! Background VTXO watcher that auto-delegates and auto-renews VTXOs.
//!
//! This mirrors the ts-sdk wallet's behavior:
//! - On new VTXOs received: submit them to the delegator service for future renewal
//! - Periodically: self-renew VTXOs that are close to expiry (safety net if delegator is slow)

use crate::key_provider::KeyProvider;
use crate::swap_storage::SwapStorage;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use ark_core::intent;
use ark_core::server::SubscriptionResponse;
use ark_core::server::VirtualTxOutPoint;
use ark_delegator::DelegatorClient;
use bitcoin::Amount;
use bitcoin::TxOut;
use futures::Stream;
use futures::StreamExt;
use rand::rngs::OsRng;
use std::sync::Arc;
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
    /// Requires the client to be wrapped in an `Arc` for shared ownership with the background
    /// task.
    ///
    /// Returns a [`VtxoWatcherHandle`] that stops the watcher when dropped.
    pub async fn start_vtxo_watcher(
        self: &Arc<Self>,
        delegator: Arc<DelegatorClient>,
    ) -> Result<VtxoWatcherHandle, crate::Error> {
        let addresses = self.get_offchain_addresses()?;
        let ark_addresses: Vec<_> = addresses.iter().map(|(addr, _)| *addr).collect();

        let subscription_id = self.subscribe_to_scripts(ark_addresses, None).await?;

        let stream = self.get_subscription(subscription_id.clone()).await?;

        let (stop_tx, stop_rx) = watch::channel(false);

        let client = Arc::clone(self);
        tokio::spawn(async move {
            run_watcher(client, delegator, stream, stop_rx).await;
            tracing::debug!("VTXO watcher stopped");
        });

        Ok(VtxoWatcherHandle { stop_tx })
    }
}

async fn run_watcher<B, W, S, K>(
    client: Arc<Client<B, W, S, K>>,
    delegator: Arc<DelegatorClient>,
    mut stream: impl Stream<Item = Result<SubscriptionResponse, ark_grpc::Error>> + Unpin,
    mut stop_rx: watch::Receiver<bool>,
) where
    B: Blockchain + Send + Sync + 'static,
    W: BoardingWallet + OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
    K: KeyProvider + Send + Sync + 'static,
{
    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                break;
            }
            event = stream.next() => {
                match event {
                    Some(Ok(SubscriptionResponse::Event(event))) => {
                        if !event.new_vtxos.is_empty() {
                            // Fire-and-forget: delegate new VTXOs to the delegator service.
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
                        tracing::warn!("VTXO subscription error: {e}");
                        break;
                    }
                    None => {
                        tracing::debug!("VTXO subscription stream ended");
                        break;
                    }
                }
            }
        }
    }
}

/// Submit newly received VTXOs to the delegator service for future auto-renewal.
async fn delegate_vtxos<B, W, S, K>(
    client: &Client<B, W, S, K>,
    delegator: &DelegatorClient,
    new_vtxos: &[VirtualTxOutPoint],
) where
    B: Blockchain + Send + Sync + 'static,
    W: BoardingWallet + OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
    K: KeyProvider + Send + Sync + 'static,
{
    let (_, script_pubkey_to_vtxo) = match client.list_vtxos().await {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to list VTXOs for delegation: {e}");
            return;
        }
    };

    // Build intent inputs from the newly received VTXOs.
    let mut vtxo_inputs = Vec::new();
    let mut total_amount = Amount::ZERO;

    for vtp in new_vtxos {
        if vtp.is_spent {
            continue;
        }

        let vtxo = match script_pubkey_to_vtxo.get(&vtp.script) {
            Some(v) => v,
            None => {
                tracing::warn!(outpoint = %vtp.outpoint, "Unknown script for VTXO, skipping");
                continue;
            }
        };

        // Only delegate VTXOs that have a delegate spending path.
        if vtxo.delegator_pk().is_none() {
            continue;
        }

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
        ));

        total_amount += vtp.amount;
    }

    if vtxo_inputs.is_empty() {
        return;
    }

    // Get the delegator info to use as the cosigner.
    let delegator_info = match delegator.info().await {
        Ok(info) => info,
        Err(e) => {
            tracing::error!("Failed to get delegator info: {e}");
            return;
        }
    };

    let delegator_cosigner_pk = match delegator_info.pubkey.parse() {
        Ok(pk) => pk,
        Err(e) => {
            tracing::error!("Failed to parse delegator pubkey: {e}");
            return;
        }
    };

    // Build the destination (send back to own address).
    let (to_address, _) = match client.get_offchain_address() {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("Failed to get offchain address for delegation: {e}");
            return;
        }
    };

    let outputs = vec![intent::Output::Offchain(TxOut {
        value: total_amount,
        script_pubkey: to_address.to_p2tr_script_pubkey(),
    })];

    let server_info = &client.server_info;

    // Prepare and sign the delegate PSBTs.
    let mut delegate = match ark_core::batch::prepare_delegate_psbts(
        vtxo_inputs,
        outputs,
        delegator_cosigner_pk,
        &server_info.forfeit_address,
        server_info.dust,
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

    // Submit to the delegator service.
    if let Err(e) = delegator
        .delegate(&delegate.intent, &delegate.forfeit_psbts, None)
        .await
    {
        tracing::error!("Failed to submit delegation: {e}");
        return;
    }

    tracing::info!(
        vtxo_count = new_vtxos.len(),
        %total_amount,
        "Delegated VTXOs to delegator service"
    );
}

/// Fraction of VTXO lifetime remaining at which we self-renew as a safety net.
///
/// The ts-sdk uses 10% (i.e. delegate at 90% elapsed). We self-renew at 5% remaining, giving the
/// delegator most of the window while still catching stragglers.
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
        remaining > 0 && (remaining as f64) < (total_lifetime as f64 * SELF_RENEW_REMAINING_FRACTION)
    });

    if !has_expiring {
        return;
    }

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

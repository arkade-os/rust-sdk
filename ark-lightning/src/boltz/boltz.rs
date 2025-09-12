//! Boltz API https://api.docs.boltz.exchange/
//!
//! Author: Vincenzo Palazzo <vincenzopalazzodev@gmail.com>

use super::boltz_ws::BoltzWebSocketClient;
use super::boltz_ws::ConnectionState;
use super::boltz_ws::PersistedSwap;
use super::boltz_ws::SwapMetadata;
use super::boltz_ws::SwapStatus;
use super::boltz_ws::SwapType as WsSwapType;
use super::model::CreateReverseSwapRequest;
use super::model::CreateReverseSwapResponse;
use super::model::CreateSubmarineSwapRequest;
use super::model::CreateSubmarineSwapResponse;
use super::model::GetSwapPreimageResponse;
use super::model::GetSwapStatusResponse;
use super::model::PairLimits;
use crate::arkln::DummyEventHandler;
use crate::arkln::EventHandle;
use crate::arkln::Lightning;
use crate::arkln::RcvOptions;
use crate::arkln::SentOptions;
use crate::boltz::boltz_ws::SwapUpdate;
use crate::ldk::bolt11_invoice as invoice;
use crate::ldk::offers;
use anyhow::Ok;
use anyhow::Result;
use bitcoin::hashes::sha256;
use futures_util::lock::Mutex;
use lightning::events::EventHandler;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy)]
pub enum Network {
    Bitcoin,
    Testnet,
    Mutinynet,
    Regtest,
}

impl Network {
    fn api_url(&self) -> &str {
        match self {
            Network::Bitcoin => "https://api.boltz.exchange",
            Network::Testnet => "https://api.testnet.boltz.exchange",
            Network::Mutinynet => "https://api.testnet.boltz.exchange",
            Network::Regtest => "http://localhost:9001",
        }
    }
}

pub struct BoltzLightning {
    client: reqwest::Client,
    _network: Network,
    api_url: String,
    ws_client: Arc<RwLock<BoltzWebSocketClient>>,

    receiver: lampo_common::event::Subscriber<SwapUpdate>,

    handler: Mutex<Arc<dyn EventHandle + Send + Sync>>,
}

impl BoltzLightning {
    pub async fn new(network: Network) -> Result<Self> {
        let client = reqwest::Client::new();
        let api_url = network.api_url().to_string();

        let mut ws_client = BoltzWebSocketClient::new(network.clone());
        ws_client.connect().await?;

        let receiver = ws_client.subscribe();
        Ok(Self {
            client,
            _network: network,
            api_url,
            ws_client: Arc::new(RwLock::new(ws_client)),
            receiver,

            handler: Mutex::new(Arc::new(DummyEventHandler)),
        })
    }

    pub async fn set_event_handler(&self, handler: Arc<dyn EventHandle + Send + Sync>) {
        let mut guard = self.handler.lock().await;
        *guard = handler;
    }

    /// Build the Boltz API from the env variables!
    pub async fn build_from_env() -> Result<Self> {
        let network_str = std::env::var("BOLTZ_NETWORK").unwrap_or_else(|_| "testnet".to_string());
        let network = match network_str.as_str() {
            "bitcoin" | "mainnet" => Network::Bitcoin,
            "testnet" => Network::Testnet,
            "mutinynet" => Network::Mutinynet,
            "regtest" => Network::Regtest,
            _ => Network::Testnet,
        };

        Self::new(network).await
    }

    /// See: https://github.com/arkade-os/boltz-swap/blob/d7b321840e8f90d70ab8d74990c61bb25aa92dc1/src/arkade-lightning.ts#L254
    pub(crate) async fn claim_htlc(&self, _swap: &PersistedSwap) -> Result<()> {
        unimplemented!()
    }

    /// See https://github.com/arkade-os/boltz-swap/blob/d7b321840e8f90d70ab8d74990c61bb25aa92dc1/src/arkade-lightning.ts#L373C9-L373C20
    pub(crate) async fn refund_vhtlc(&self, _swap: &PersistedSwap) -> Result<()> {
        unimplemented!()
    }

    pub async fn spawn(self: Arc<Self>) {
        let this = self.clone();
        tokio::spawn(async move {
            let mut receiver = this.receiver.subscribe();
            while let Some(SwapUpdate { id, status }) = receiver.recv().await {
                let ws = this.ws_client.read().await;
                let result = ws.update_swap_status(id.clone(), status.clone()).await;
                if let Err(err) = result {
                    eprintln!("Failed to update swap status: {}", err);
                    continue;
                }

                let handler = this.handler.lock().await;
                let handler = handler.clone();
                match status {
                    SwapStatus::Created => {
                        let status = ws.get_swap(&id).await;
                        assert!(status.is_some(), "Swap with id `{}` must exist", id);
                    }
                    SwapStatus::TransactionMempool => {
                        // Log it
                        println!("Swap {} transaction in mempool!", id);
                    }
                    SwapStatus::TransactionConfirmed => {
                        // make a double check with what we see on chain or in the virtual mempool
                        println!("Swap {} failed!", id);
                    }
                    SwapStatus::InvoiceSet => {
                        println!("Swap {} invoice settled!", id);
                        // FIXME: https://github.com/arkade-os/boltz-swap/blob/master/src/arkade-lightning.ts#L498
                    }
                    SwapStatus::InvoicePending => {
                        println!("Swap {} invoice pending!", id);
                        handler.on_payment_pending(bitcoin::Amount::from_sat(0));
                    }
                    SwapStatus::InvoicePaid => {
                        println!("Swap {} invoice paid!", id);
                        // TODO: we should claim vthlc here!
                        handler.on_payment_received(bitcoin::Amount::from_sat(0));
                    }
                    SwapStatus::InvoiceFailedToPay => {
                        println!("Swap {} invoice failed to pay!", id);
                        // We should drop the swap from the storage, and probably keep track
                        // somehow in the failure
                        handler.on_payment_failed(bitcoin::Amount::from_sat(0));
                    }
                    SwapStatus::TransactionClaimed => {
                        println!("Swap {} transaction claimed!", id);

                        let swap = ws.get_swap(&id).await;
                        assert!(swap.is_some());
                        // SAFETY: it should be never None here
                        let swap = swap.unwrap();
                        if let Err(err) = self.claim_htlc(&swap).await {
                            eprintln!("Failed to claim htlc: {}", err);
                        }

                        handler.on_payment_received(bitcoin::Amount::from_sat(0));
                    }
                    SwapStatus::SwapExpired => {
                        println!("Swap {} expired!", id);
                        // We should drop the swap from the storage, and probably keep track
                        // somehow in the failure
                        handler.on_payment_failed(bitcoin::Amount::from_sat(0));
                    }
                    SwapStatus::Error { error } => {
                        println!("Swap {} error: {}", id, error.clone());
                        // We should drop the swap from the storage, and probably keep track
                        // somehow in the failure
                    }
                }
            }
        });
    }

    pub async fn get_limits(&self) -> Result<HashMap<String, PairLimits>> {
        let url = format!("{}/v2/swap/submarine", self.api_url);
        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to get limits: {}", response.status());
        }

        let limits: HashMap<String, PairLimits> = response.json().await?;
        Ok(limits)
    }

    pub async fn create_submarine_swap(
        &self,
        request: CreateSubmarineSwapRequest,
    ) -> Result<CreateSubmarineSwapResponse> {
        let url = format!("{}/v2/swap/submarine", self.api_url);
        let response = self.client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            anyhow::bail!("Failed to create submarine swap: {}", error_text);
        }

        let swap_response: CreateSubmarineSwapResponse = response.json().await?;
        Ok(swap_response)
    }

    pub async fn create_reverse_swap(
        &self,
        request: CreateReverseSwapRequest,
    ) -> Result<CreateReverseSwapResponse> {
        let url = format!("{}/v2/swap/reverse", self.api_url);
        let response = self.client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let error_text = response.text().await?;
            anyhow::bail!("Failed to create reverse swap: {}", error_text);
        }

        let swap_response: CreateReverseSwapResponse = response.json().await?;
        Ok(swap_response)
    }

    pub async fn get_swap_status(&self, swap_id: &str) -> Result<GetSwapStatusResponse> {
        let url = format!("{}/v2/swap/{}", self.api_url, swap_id);
        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to get swap status: {}", response.status());
        }

        let status: GetSwapStatusResponse = response.json().await?;
        Ok(status)
    }

    pub async fn get_swap_preimage(&self, swap_id: &str) -> Result<GetSwapPreimageResponse> {
        let url = format!("{}/v2/swap/submarine/{}/preimage", self.api_url, swap_id);
        let response = self.client.get(&url).send().await?;

        if !response.status().is_success() {
            anyhow::bail!("Failed to get swap preimage: {}", response.status());
        }

        let preimage: GetSwapPreimageResponse = response.json().await?;
        Ok(preimage)
    }

    pub fn format_public_key(public_key: &str) -> String {
        let key = public_key.trim_start_matches("0x");
        if key.len() == 64 {
            format!("02{}", key)
        } else {
            key.to_string()
        }
    }

    /// Get the current status of a persisted swap
    pub async fn get_swap_status_from_cache(&self, swap_id: &str) -> Option<PersistedSwap> {
        let ws_client = self.ws_client.read().await;
        ws_client.get_swap(swap_id).await
    }

    /// Remove a swap from persistence and stop monitoring it
    pub async fn cleanup_swap(&self, swap_id: &str) -> Result<()> {
        let ws_client = self.ws_client.read().await;
        ws_client.remove_swap(swap_id).await
    }

    /// Manually trigger a WebSocket ping to keep the connection alive
    pub async fn ping_ws(&self) -> Result<()> {
        let ws_client = self.ws_client.read().await;
        ws_client.ping().await
    }

    /// Check if WebSocket is connected
    pub async fn is_ws_connected(&self) -> bool {
        let ws_client = self.ws_client.read().await;
        ws_client.is_connected().await
    }

    /// Get current WebSocket connection state
    pub async fn get_ws_connection_state(&self) -> ConnectionState {
        let ws_client = self.ws_client.read().await;
        ws_client.get_connection_state().await
    }

    /// Manually disconnect WebSocket (useful for cleanup)
    pub async fn disconnect_ws(&self) {
        let ws_client = self.ws_client.read().await;
        ws_client.disconnect().await;
    }
}

impl Lightning for BoltzLightning {
    fn get_invoice(
        &self,
        opts: RcvOptions,
    ) -> impl Future<Output = Result<invoice::Bolt11Invoice>> + Send {
        async move {
            // create the random number called preimage!
            // hash this preimage with sha256 and call it preimage_hash
            let preimage: [u8; 32] = musig::rand::random();
            let preimage_hash = sha256::Hash::const_hash(&preimage).to_string();

            let request = CreateReverseSwapRequest {
                invoice_amount: opts.invoice_amount.to_sat() as u64,
                // FIXME: this need to came from the wallet!
                claim_public_key: opts.claim_public_key.to_string(),
                preimage_hash: preimage_hash.clone(),
                description: opts.description,
            };
            let response = self.create_reverse_swap(request).await?;
            let invoice: invoice::Bolt11Invoice = response
                .invoice
                .parse()
                .map_err(|err| anyhow::anyhow!("Parsing invoice `{err}`"))?;

            // Persist the swap and subscribe to WebSocket updates
            let swap = PersistedSwap {
                id: response.id.clone(),
                swap_type: WsSwapType::Reverse,
                status: SwapStatus::Created,
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                metadata: SwapMetadata::Reverse {
                    preimage: hex::encode(preimage),
                    preimage_hash,
                    swap_tree: serde_json::to_value(&response.swap_tree).unwrap(),
                    refund_public_key: response.refund_public_key.clone(),
                    lockup_address: response.lockup_address.clone(),
                    timeout_block_height: response.timeout_block_height,
                    onchain_amount: response.onchain_amount,
                    blinding_key: response.blinding_key.clone(),
                    invoice: response.invoice.clone(),
                },
            };

            let ws_client = self.ws_client.read().await;
            ws_client.persist_swap(swap).await?;
            Ok(invoice)
        }
    }

    fn get_offer(
        &self,
        _opts: RcvOptions,
    ) -> impl Future<Output = Result<offers::offer::Offer>> + Send {
        async { unimplemented!() }
    }

    fn pay_invoice(&self, opts: SentOptions) -> impl Future<Output = Result<()>> + Send {
        async move {
            let request = CreateSubmarineSwapRequest {
                invoice: opts.invoice.to_string(),
                refund_public_key: opts.refund_public_key.to_string(),
            };

            let response = self.create_submarine_swap(request).await?;

            // Persist the swap and subscribe to WebSocket updates
            let swap = PersistedSwap {
                id: response.id.clone(),
                swap_type: WsSwapType::Submarine,
                status: SwapStatus::Created,
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                metadata: SwapMetadata::Submarine {
                    address: response.address.clone(),
                    redeem_script: response.redeem_script.clone(),
                    accept_zero_conf: response.accept_zero_conf,
                    expected_amount: response.expected_amount,
                    claim_public_key: response.claim_public_key.clone(),
                    timeout_block_height: response.timeout_block_height,
                    blinding_key: response.blinding_key.clone(),
                },
            };

            // TODO: The actual wallet transaction broadcast should happen here
            let ws_client = self.ws_client.read().await;
            ws_client.persist_swap(swap).await?;

            Ok(())
        }
    }

    fn pay_offer(&self, _opts: SentOptions) -> impl Future<Output = Result<()>> + Send {
        async { unimplemented!() }
    }

    fn pay_bip321(&self, _bip321: &str) -> impl Future<Output = Result<()>> + Send {
        async { unimplemented!() }
    }
}

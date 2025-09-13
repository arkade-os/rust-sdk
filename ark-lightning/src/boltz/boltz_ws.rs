//! Boltz WebSocket client for real-time swap monitoring
//!
//! This module provides a WebSocket client for monitoring Boltz submarine and reverse
//! swaps in real-time.
//!
//! # Features
//!
//! - **Real-time Updates**: Receive instant notifications when swap status changes
//! - **Callback System**: Register custom handlers for swap status updates
//! - **Network Support**: Works with Bitcoin mainnet, testnet, mutinynet, and regtest
//!
//! # Example
//!
//! ```rust
//! use ark_lightning::boltz_ws::{BoltzWebSocketClient, SwapUpdate};
//! use ark_lightning::boltz::Network;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create and connect client
//!     let mut client = BoltzWebSocketClient::new(Network::Testnet);
//!     client.connect().await?;
//!
//!     // Subscribe to swap updates
//!     client.subscribe_to_swap("swap_id_123".to_string()).await?;
//!
//!     // Register a callback for status updates
//!     let callback = Arc::new(|update: SwapUpdate| {
//!         println!("Swap {} status: {:?}", update.id, update.status);
//!     });
//!     client.register_callback("swap_id_123".to_string(), callback).await;
//!
//!     Ok(())
//! }
//! ```
//!
//! Author: Vincenzo Palazzo <vincenzopalazzodev@gmail.com>

use crate::boltz::storage::NoSqlStorage;
use crate::boltz::storage::SwapStorage;
use crate::boltz::storage::SwapStorageOptions;
use crate::boltz::Network;
use anyhow::Result;
use bitcoin::Amount;
use core::fmt;
use futures_util::SinkExt;
use futures_util::StreamExt;
use lampo_common::event::Subscriber;
use lightning::bolt11_invoice::Bolt11Invoice;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::protocol::Message;

/// WebSocket request messages sent to the Boltz server
///
/// These messages control subscriptions and maintain the connection.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum WsRequest {
    /// Subscribe to updates for specific swaps
    ///
    /// # Fields
    /// - `channel`: Usually "swap.update" for swap status updates
    /// - `args`: List of swap IDs to monitor
    Subscribe { channel: String, args: Vec<String> },
    /// Unsubscribe from swap updates
    ///
    /// # Fields
    /// - `channel`: The channel to unsubscribe from
    /// - `args`: List of swap IDs to stop monitoring
    Unsubscribe { channel: String, args: Vec<String> },
    /// Heartbeat message to keep the connection alive
    Ping,
}

/// WebSocket response messages received from the Boltz server
///
/// These messages provide swap updates and connection status information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum WsResponse {
    /// Confirmation that subscription was successful
    Subscribe { channel: String, args: Vec<String> },
    /// Confirmation that unsubscription was successful
    Unsubscribe { channel: String, args: Vec<String> },
    /// Swap status update notification
    ///
    /// Contains one or more swap updates with new status information
    Update {
        channel: String,
        args: Vec<SwapUpdate>,
    },
    /// Error response from the server
    Error { channel: String, reason: String },
    /// Response to a ping request
    Pong,
}

/// Represents a swap status update received via WebSocket
///
/// This is the primary data structure for tracking swap progress.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapUpdate {
    /// Unique identifier of the swap
    pub id: String,
    /// Current status of the swap
    pub status: SwapStatus,
}

/// All possible states of a Boltz swap
///
/// Swaps progress through these states during their lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwapStatus {
    /// Initial state when swap is created
    Created,
    /// Lockup transaction detected in mempool
    TransactionMempool,
    /// Lockup transaction confirmed on-chain
    TransactionConfirmed,
    /// Transaction Refunded
    TransactionRefunded,
    /// Transaction Failed
    TransactionFailed,
    /// Transaction Claimed
    TransactionClaimed,
    /// Lightning invoice has been set
    InvoiceSet,
    /// Waiting for Lightning invoice payment
    InvoicePending,
    /// Lightning invoice successfully paid
    InvoicePaid,
    /// Lightning invoice payment failed
    InvoiceFailedToPay,
    /// Invoice Expired
    InvoiceExpired,
    /// Swap expired - can be refunded
    SwapExpired,
    /// Swap failed with error
    Error { error: String },
}

unsafe impl Send for SwapStatus {}
unsafe impl Sync for SwapStatus {}

impl fmt::Display for SwapStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwapStatus::Created => write!(f, "created"),
            SwapStatus::TransactionMempool => write!(f, "transaction_mempool"),
            SwapStatus::TransactionConfirmed => write!(f, "transaction_confirmed"),
            SwapStatus::TransactionRefunded => write!(f, "transaction_refunded"),
            SwapStatus::TransactionFailed => write!(f, "transaction_failed"),
            SwapStatus::TransactionClaimed => write!(f, "transaction_claimed"),
            SwapStatus::InvoiceSet => write!(f, "invoice_set"),
            SwapStatus::InvoicePending => write!(f, "invoice_pending"),
            SwapStatus::InvoicePaid => write!(f, "invoice_paid"),
            SwapStatus::InvoiceFailedToPay => write!(f, "invoice_failed_to_pay"),
            SwapStatus::InvoiceExpired => write!(f, "invoice_expired"),
            SwapStatus::SwapExpired => write!(f, "swap_expired"),
            SwapStatus::Error { error } => write!(f, "error: {}", error),
        }
    }
}

/// Swap metadata fields based on swap type
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SwapMetadata {
    /// Metadata for reverse submarine swaps (Lightning to on-chain)
    Reverse {
        /// Preimage for the swap
        preimage: String,
        /// Hash of the preimage
        preimage_hash: String,
        /// Swap tree structure
        swap_tree: serde_json::Value,
        /// Public key for refund
        refund_public_key: String,
        /// Address where funds are locked
        lockup_address: String,
        /// Block height when swap times out
        timeout_block_height: u64,
        /// Amount to be sent on-chain
        onchain_amount: u64,
        /// Optional blinding key for confidential transactions
        blinding_key: Option<String>,
        /// Invoice associated with the swap
        invoice: String,
    },
    /// Metadata for submarine swaps (ark to Lightning)
    Submarine {
        /// Address to send funds to
        address: String,
        /// Redeem script for the swap
        redeem_script: String,
        /// Whether zero-confirmation transactions are accepted
        accept_zero_conf: bool,
        /// Expected amount to be received
        expected_amount: u64,
        /// Public key for claiming funds
        claim_public_key: String,
        /// Block height when swap times out
        timeout_block_height: u64,
        /// Optional blinding key for confidential transactions
        blinding_key: Option<String>,
    },
}

impl SwapMetadata {
    /// Retrieves the preimage if available
    ///
    /// # Returns
    /// - `Some(String)` containing the preimage for reverse swaps
    /// - `None` for submarine swaps
    pub fn preimage(&self) -> Option<String> {
        match self {
            SwapMetadata::Reverse { preimage, .. } => Some(preimage.clone()),
            SwapMetadata::Submarine { .. } => None,
        }
    }

    pub fn preimage_hash(&self) -> Option<String> {
        match self {
            SwapMetadata::Reverse { preimage_hash, .. } => Some(preimage_hash.clone()),
            SwapMetadata::Submarine { .. } => None,
        }
    }

    pub fn address(&self) -> String {
        match self {
            SwapMetadata::Reverse { lockup_address, .. } => lockup_address.clone(),
            SwapMetadata::Submarine { address, .. } => address.clone(),
        }
    }

    pub fn amount(&self) -> Amount {
        let amount = match self {
            SwapMetadata::Reverse { onchain_amount, .. } => *onchain_amount,
            SwapMetadata::Submarine {
                expected_amount, ..
            } => *expected_amount,
        };
        Amount::from_sat(amount)
    }

    pub fn invoice(&self) -> Option<Bolt11Invoice> {
        match self {
            SwapMetadata::Reverse { invoice, .. } => {
                let invoice = invoice.parse::<Bolt11Invoice>().unwrap();
                Some(invoice)
            }
            SwapMetadata::Submarine { .. } => None,
        }
    }

    pub fn timeout_block_height(&self) -> u64 {
        match self {
            SwapMetadata::Reverse {
                timeout_block_height,
                ..
            } => *timeout_block_height,
            SwapMetadata::Submarine {
                timeout_block_height,
                ..
            } => *timeout_block_height,
        }
    }

    pub fn refund_xpub(&self) -> Option<String> {
        match self {
            SwapMetadata::Reverse {
                refund_public_key, ..
            } => Some(refund_public_key.clone()),
            SwapMetadata::Submarine { .. } => None,
        }
    }
}

/// Persistent swap data
///
/// This structure maintains swap state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedSwap {
    /// Unique swap identifier
    pub id: String,
    /// Type of swap (submarine or reverse)
    pub swap_type: SwapType,
    /// Current swap status
    pub status: SwapStatus,
    /// Unix timestamp when swap was created
    pub created_at: u64,
    /// Swap-specific metadata
    pub metadata: SwapMetadata,
}

/// Type of Boltz swap
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwapType {
    /// On-chain to Lightning swap
    Submarine,
    /// Lightning to on-chain swap
    Reverse,
}

impl fmt::Display for SwapType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SwapType::Submarine => write!(f, "submarine"),
            SwapType::Reverse => write!(f, "reverse_submarine"),
        }
    }
}

/// WebSocket connection states
///
/// Tracks the current state of the WebSocket connection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ConnectionState {
    /// Not connected to the server
    Disconnected,
    /// Successfully connected and operational
    Connected,
}

/// Main WebSocket client for Boltz swap monitoring
///
/// This client provides a WebSocket connection to the Boltz server
/// for real-time swap status updates.
///
/// # Features
/// - Persistent swap tracking
/// - Concurrent callback system
/// - Network-specific endpoint selection
pub struct BoltzWebSocketClient {
    #[allow(dead_code)]
    network: Network,
    ws_url: String,
    swaps: NoSqlStorage,
    connection_state: Arc<Mutex<ConnectionState>>,

    sender: Arc<Mutex<Option<mpsc::UnboundedSender<WsRequest>>>>,
    receiver: Arc<Mutex<Option<mpsc::UnboundedReceiver<WsResponse>>>>,

    // Event base system, the ws when receive a new update from the
    // `receiver` channel will emit a new event with the emitter that
    // is the location when all the subscription are listening!
    emitter: lampo_common::event::Emitter<SwapUpdate>,
}

impl BoltzWebSocketClient {
    /// Creates a new WebSocket client for the specified network
    ///
    /// # Arguments
    /// - `network`: The Bitcoin network to connect to (mainnet, testnet, mutinynet, or regtest)
    ///
    /// # Returns
    /// A new unconnected WebSocket client instance
    ///
    /// # Example
    /// ```
    /// let client = BoltzWebSocketClient::new(Network::Testnet);
    /// ```
    pub fn new(network: Network) -> Self {
        let ws_url = match network {
            Network::Bitcoin => "wss://api.boltz.exchange/v2/ws",
            Network::Testnet | Network::Mutinynet => "wss://api.testnet.boltz.exchange/v2/ws",
            Network::Regtest => "ws://localhost:9001/v2/ws",
        };

        Self {
            network,
            ws_url: ws_url.to_string(),
            swaps: NoSqlStorage::new(SwapStorageOptions {
                path: "boltz_swaps_db".to_string(),
            })
            .unwrap(),
            connection_state: Arc::new(Mutex::new(ConnectionState::Disconnected)),

            sender: Arc::new(Mutex::new(None)),
            receiver: Arc::new(Mutex::new(None)),

            emitter: lampo_common::event::Emitter::default(),
        }
    }

    pub fn subscribe(&self) -> Subscriber<SwapUpdate> {
        self.emitter.subscriber()
    }

    /// Establishes WebSocket connection to the Boltz server
    ///
    /// # Returns
    /// - `Ok(())` if connection is successful
    /// - `Err` if connection fails
    ///
    /// # Example
    /// ```
    /// let mut client = BoltzWebSocketClient::new(Network::Testnet);
    /// client.connect().await?;
    /// ```
    pub async fn connect(&mut self) -> Result<()> {
        // Connect to WebSocket
        let (ws_stream, _) = connect_async(&self.ws_url).await?;
        let (mut write, mut read) = ws_stream.split();
        let (tx, mut rx) = mpsc::unbounded_channel::<WsRequest>();

        // Update sender
        {
            let mut sender_guard = self.sender.lock().await;
            *sender_guard = Some(tx.clone());
        }

        // Update connection state
        {
            let mut state = self.connection_state.lock().await;
            *state = ConnectionState::Connected;
        }

        let connection_state_clone = self.connection_state.clone();
        let sender_clone = self.sender.clone();
        let receiver_clone = self.receiver.clone();

        // Spawn task to handle outgoing messages
        tokio::spawn(async move {
            while let Some(request) = rx.recv().await {
                let msg = serde_json::to_string(&request).unwrap();
                if let Err(e) = write.send(Message::Text(msg)).await {
                    eprintln!("Failed to send WebSocket message: {}", e);
                    break;
                }
            }
        });

        // Spawn task to handle incoming messages
        let emitter_clone = self.emitter.clone();
        tokio::spawn(async move {
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(Message::Text(text)) => {
                        if let Ok(response) = serde_json::from_str::<WsResponse>(&text) {
                            // FIXME: emit a new event here for the end user!
                            match response {
                                WsResponse::Update { args, .. } => {
                                    for update in args {
                                        // Emit event for each swap update
                                        emitter_clone.emit(update.clone());
                                    }
                                }
                                _ => {
                                    println!("Received message: {:?}", response);
                                }
                            }
                        }
                    }
                    Ok(Message::Close(_)) => {
                        println!("WebSocket connection closed by server");
                        break;
                    }
                    Err(e) => {
                        eprintln!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }

            // Update state to disconnected
            let mut state = connection_state_clone.lock().await;
            *state = ConnectionState::Disconnected;

            // Clear the sender
            let mut sender_guard = sender_clone.lock().await;
            *sender_guard = None;

            let mut receiver_guard = receiver_clone.lock().await;
            *receiver_guard = None;
        });

        Ok(())
    }

    /// Subscribes to status updates for a specific swap
    ///
    /// # Arguments
    /// - `swap_id`: The unique identifier of the swap to monitor
    ///
    /// # Returns
    /// - `Ok(())` if subscription request is sent successfully
    /// - `Err` if the client is not connected
    ///
    /// # Example
    /// ```
    /// client.subscribe_to_swap("swap_123".to_string()).await?;
    /// ```
    pub async fn subscribe_to_swap(&self, swap_id: String) -> Result<()> {
        let request = WsRequest::Subscribe {
            channel: "swap.update".to_string(),
            args: vec![swap_id],
        };

        self.send_request(request).await
    }

    async fn send_request(&self, request: WsRequest) -> Result<()> {
        let sender_guard = self.sender.lock().await;
        if let Some(sender) = &*sender_guard {
            sender.send(request)?;
            Ok(())
        } else {
            anyhow::bail!("WebSocket not connected")
        }
    }

    /// Unsubscribes from status updates for a specific swap
    ///
    /// # Arguments
    /// - `swap_id`: The swap ID to stop monitoring
    ///
    /// # Returns
    /// - `Ok(())` if unsubscription request is sent successfully
    /// - `Err` if the client is not connected
    pub async fn unsubscribe_from_swap(&self, swap_id: String) -> Result<()> {
        let request = WsRequest::Unsubscribe {
            channel: "swap.update".to_string(),
            args: vec![swap_id],
        };

        self.send_request(request).await
    }

    /// Persists swap data and automatically subscribes to its updates
    ///
    /// # Arguments
    /// - `swap`: The swap data to persist
    ///
    /// # Returns
    /// - `Ok(())` if swap is persisted and subscription is successful
    /// - `Err` if subscription fails
    ///
    /// # Example
    /// ```
    /// let swap = PersistedSwap {
    ///     id: "swap_123".to_string(),
    ///     swap_type: SwapType::Submarine,
    ///     status: SwapStatus::Created,
    ///     created_at: 1234567890,
    ///     metadata: HashMap::new(),
    /// };
    /// client.persist_swap(swap).await?;
    /// ```
    pub async fn persist_swap(&self, swap: PersistedSwap) -> Result<()> {
        let swap = self.swaps.save_swap(swap.clone()).await?.value;
        // Subscribe to updates for this swap
        self.subscribe_to_swap(swap.id).await?;
        Ok(())
    }

    /// Retrieves persisted swap data by ID
    ///
    /// # Arguments
    /// - `swap_id`: The ID of the swap to retrieve
    ///
    /// # Returns
    /// - `Some(PersistedSwap)` if the swap exists
    /// - `None` if the swap is not found
    pub async fn get_swap(&self, swap_id: &str) -> Option<PersistedSwap> {
        self.swaps
            .get_swap(swap_id.to_string())
            .await
            .ok()
            .flatten()
    }

    pub async fn update_swap_status(&self, swap_id: String, new_status: SwapStatus) -> Result<()> {
        self.swaps
            .update_swap_with_status(&swap_id, new_status)
            .await?;
        Ok(())
    }

    /// Removes a swap from tracking and unsubscribes from its updates
    ///
    /// # Arguments
    /// - `swap_id`: The ID of the swap to remove
    ///
    /// # Returns
    /// - `Ok(())` if removal is successful
    /// - `Err` if unsubscription fails
    pub async fn remove_swap(&self, swap_id: &str) -> Result<()> {
        let swap = self.swaps.delete_swap(swap_id).await?;
        let Some(swap) = swap else {
            anyhow::bail!("Swap with ID `{swap_id}` not found");
        };

        // Unsubscribe from updates
        self.unsubscribe_from_swap(swap.id.to_string()).await?;

        Ok(())
    }

    /// Sends a ping message to keep the connection alive
    ///
    /// # Returns
    /// - `Ok(())` if ping is sent successfully
    /// - `Err` if the client is not connected
    pub async fn ping(&self) -> Result<()> {
        let request = WsRequest::Ping;
        self.send_request(request).await
    }

    /// Gets the current connection state
    ///
    /// # Returns
    /// The current `ConnectionState` enum value
    pub async fn get_connection_state(&self) -> ConnectionState {
        *self.connection_state.lock().await
    }

    /// Checks if the client is currently connected
    ///
    /// # Returns
    /// - `true` if the WebSocket is connected and operational
    /// - `false` otherwise
    pub async fn is_connected(&self) -> bool {
        *self.connection_state.lock().await == ConnectionState::Connected
    }

    /// Gracefully disconnects from the WebSocket server
    pub async fn disconnect(&self) {
        // Clear sender to trigger disconnection
        let mut sender_guard = self.sender.lock().await;
        *sender_guard = None;

        // Update state
        let mut state = self.connection_state.lock().await;
        *state = ConnectionState::Disconnected;
    }
}

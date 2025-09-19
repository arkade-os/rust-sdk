use crate::error::ErrorContext;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use bitcoin::hashes::sha256;
use bitcoin::hex::DisplayHex;
use bitcoin::Amount;
use bitcoin::Txid;
use lightning_invoice::Bolt11Invoice;
use lightning_invoice::ParseOrSemanticError;
use serde::Deserialize;
use serde::Serialize;

const BOLTZ_URL: &str = "http://localhost:9001";

impl<B, W> Client<B, W>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
{
    // SUBMARINE SWAP

    // This is a submarine swap (Ark to Lightning).
    //
    // Returns:
    //
    // - TXID of VHTLC transaction.
    // - Swap ID.
    pub async fn pay_ln_invoice(&self, invoice: String, amount: Amount) -> Result<(), Error> {
        unimplemented!()
    }

    // Caller could provide specific Swap ID OR we could just refund all refundable VHTLCs
    // (persisted somehow).
    pub async fn refund_vhtlc(&self) -> Result<Txid, Error> {
        unimplemented!()
    }

    // REVERSE SUBMARINE SWAP

    // This is a reverse submarine swap (Lightning to Ark).
    //
    // For now, generate secret and claim PK internally (could extend to allow caller to pass these
    // in).
    //
    // Returns:
    //
    // - Lightning invoice.
    pub async fn get_ln_invoice(&self, amount: Amount) -> Result<Bolt11Invoice, Error> {
        let preimage: [u8; 32] = musig::rand::random();
        let preimage_hash = sha256::Hash::const_hash(&preimage).to_string();

        let claim_public_key = self.inner.kp.public_key();

        let request = CreateReverseSwapRequest {
            from: Asset::Btc,
            to: Asset::Ark,
            invoice_amount: amount.to_sat(),
            claim_public_key: claim_public_key.to_string(),
            preimage_hash: preimage_hash.clone(),
        };

        let url = format!("{BOLTZ_URL}/v2/swap/reverse");

        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to send reverse swap request")?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))
                .context("failed to read error text")?;

            return Err(Error::ad_hoc(format!(
                "failed to create reverse swap: {error_text}"
            )));
        }

        let response: CreateReverseSwapResponse = response
            .json()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to deserialize reverse swap response")?;

        let invoice: Bolt11Invoice = response
            .invoice
            .parse()
            .map_err(|e: ParseOrSemanticError| Error::ad_hoc(e.to_string()))
            .context("failed to parse BOLT11 invoice")?;

        // Persist the swap and subscribe to WebSocket updates
        let swap = SwapData {
            id: response.id.clone(),
            swap_type: SwapType::Reverse,
            status: SwapStatus::Created,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            metadata: SwapMetadata::Reverse {
                preimage: preimage.to_lower_hex_string(),
                preimage_hash,
                refund_public_key: response.refund_public_key.clone(),
                lockup_address: response.lockup_address.clone(),
                timeout_block_heights: response.timeout_block_heights,
                onchain_amount: response.onchain_amount,
                invoice: response.invoice.clone(),
            },
        };

        // TODO: Introduce SwapStorage trait.
        let mut swaps = self.swaps.lock().expect("to get lock");
        swaps.insert(response.id, swap);

        Ok(invoice)
    }

    // Misc (not definitive)

    pub async fn get_swap_status() -> Result<(), Error> {
        unimplemented!()
    }

    pub async fn subscribe_to_swap_updates() -> Result<(), Error> {
        unimplemented!()
    }
}

/// Persistent swap data
///
/// This structure maintains swap state.
#[derive(Debug, Clone)]
pub struct SwapData {
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
#[derive(Debug, Clone)]
pub enum SwapType {
    /// On-chain to Lightning swap
    Submarine,
    /// Lightning to on-chain swap
    Reverse,
}

/// All possible states of a Boltz swap
///
/// Swaps progress through these states during their lifecycle.
#[derive(Debug, Clone)]
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
        /// Public key for refund
        refund_public_key: String,
        /// Address where funds are locked
        lockup_address: String,
        /// Block height when swap times out
        timeout_block_heights: TimeoutBlockHeights,
        /// Amount to be sent on-chain
        onchain_amount: u64,
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

    pub fn refund_xpub(&self) -> Option<String> {
        match self {
            SwapMetadata::Reverse {
                refund_public_key, ..
            } => Some(refund_public_key.clone()),
            SwapMetadata::Submarine { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReverseSwapRequest {
    pub from: Asset,
    pub to: Asset,
    pub invoice_amount: u64,
    pub claim_public_key: String,
    pub preimage_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Asset {
    Btc,
    Ark,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReverseSwapResponse {
    pub id: String,
    pub lockup_address: String,
    pub refund_public_key: String,
    pub timeout_block_heights: TimeoutBlockHeights,
    pub invoice: String,
    pub onchain_amount: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimeoutBlockHeights {
    pub refund: u64,
    pub unilateral_claim: u64,
    pub unilateral_refund: u64,
    pub unilateral_refund_without_receiver: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapTree {
    #[serde(rename = "claimLeaf")]
    pub claim_leaf: TreeLeaf,
    #[serde(rename = "refundLeaf")]
    pub refund_leaf: TreeLeaf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TreeLeaf {
    pub version: u8,
    pub output: String,
}

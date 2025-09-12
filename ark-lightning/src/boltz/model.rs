//! Boltz API models
//!
//! Author: Vincenzo Palazzo <vincenzopalazzodev@gmail.com

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapLimits {
    pub minimal: u64,
    pub maximal: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairLimits {
    #[serde(rename = "swapType")]
    pub swap_type: SwapType,
    pub rate: f64,
    pub limits: SwapLimits,
    pub fees: SwapFees,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapFees {
    pub percentage: f64,
    #[serde(rename = "minerFees")]
    pub miner_fees: MinerFees,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinerFees {
    pub base: u64,
    pub variable: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SwapType {
    Submarine,
    Reverse,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSubmarineSwapRequest {
    pub invoice: String,
    #[serde(rename = "refundPublicKey")]
    pub refund_public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSubmarineSwapResponse {
    pub id: String,
    pub address: String,
    #[serde(rename = "redeemScript")]
    pub redeem_script: String,
    #[serde(rename = "acceptZeroConf")]
    pub accept_zero_conf: bool,
    #[serde(rename = "expectedAmount")]
    pub expected_amount: u64,
    #[serde(rename = "claimPublicKey")]
    pub claim_public_key: String,
    #[serde(rename = "timeoutBlockHeight")]
    pub timeout_block_height: u64,
    #[serde(rename = "blindingKey", skip_serializing_if = "Option::is_none")]
    pub blinding_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateReverseSwapRequest {
    #[serde(rename = "invoiceAmount")]
    pub invoice_amount: u64,
    #[serde(rename = "claimPublicKey")]
    pub claim_public_key: String,
    #[serde(rename = "preimageHash")]
    pub preimage_hash: String,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateReverseSwapResponse {
    pub id: String,
    pub invoice: String,
    #[serde(rename = "swapTree")]
    pub swap_tree: SwapTree,
    #[serde(rename = "refundPublicKey")]
    pub refund_public_key: String,
    #[serde(rename = "lockupAddress")]
    pub lockup_address: String,
    #[serde(rename = "timeoutBlockHeight")]
    pub timeout_block_height: u64,
    #[serde(rename = "onchainAmount")]
    pub onchain_amount: u64,
    #[serde(rename = "blindingKey", skip_serializing_if = "Option::is_none")]
    pub blinding_key: Option<String>,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetSwapStatusResponse {
    pub status: String,
    #[serde(rename = "zeroConfRejected", skip_serializing_if = "Option::is_none")]
    pub zero_conf_rejected: Option<bool>,
    pub transaction: Option<TransactionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransactionInfo {
    pub id: String,
    pub hex: Option<String>,
    #[serde(rename = "blockHeight", skip_serializing_if = "Option::is_none")]
    pub block_height: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetSwapPreimageResponse {
    pub preimage: String,
}

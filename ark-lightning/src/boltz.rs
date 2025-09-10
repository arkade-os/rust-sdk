//! Boltz API https://api.docs.boltz.exchange/
//!
//! Author: Vincenzo Palazzo <vincenzopalazzodev@gmail.com>

use crate::arkln::Lightning;
use crate::arkln::RcvOptions;
use crate::arkln::SentOptions;
use crate::ldk::bolt11_invoice as invoice;
use crate::ldk::offers;
use anyhow::Ok;
use anyhow::Result;
use bitcoin::hashes::sha256;
use bitcoin::key::rand;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::future::Future;

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

#[derive(Debug, Clone)]
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
}

impl BoltzLightning {
    pub fn new(network: Network) -> Result<Self> {
        let client = reqwest::Client::new();
        let api_url = network.api_url().to_string();

        Ok(Self {
            client,
            _network: network,
            api_url,
        })
    }

    /// Build the Boltz API from the env variables!
    pub fn build_from_env() -> Result<Self> {
        let network_str = std::env::var("BOLTZ_NETWORK").unwrap_or_else(|_| "testnet".to_string());
        let network = match network_str.as_str() {
            "bitcoin" | "mainnet" => Network::Bitcoin,
            "testnet" => Network::Testnet,
            "mutinynet" => Network::Mutinynet,
            "regtest" => Network::Regtest,
            _ => Network::Testnet,
        };

        Self::new(network)
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
}

impl Lightning for BoltzLightning {
    fn get_invoice(
        &self,
        opts: RcvOptions,
    ) -> impl Future<Output = Result<invoice::Bolt11Invoice>> + Send {
        async move {
            // create the random number called preimage!
            // hash this preimage with sha256 and call it preimage_hash
            let preimage: [u8; 32] = rand::random();
            let preimage_hash = sha256::Hash::const_hash(&preimage).to_string();

            let request = CreateReverseSwapRequest {
                invoice_amount: opts.invoice_amount.to_sat() as u64,
                // FIXME: this need to came from the wallet!
                claim_public_key: opts.claim_public_key.to_string(),
                preimage_hash,
                description: opts.description,
            };
            let response = self.create_reverse_swap(request).await?;
            let invoice: invoice::Bolt11Invoice = response
                .invoice
                .parse()
                .map_err(|err| anyhow::anyhow!("Parsing invoice `{err}`"))?;
            // we should monitor it somehow probably by payment_hash!
            // See https://github.com/arkade-os/boltz-swap/blob/master/src/arkade-lightning.ts#L239
            // TODO: this need be claim but we need to make an event base system!
            Ok(invoice)
        }
    }

    fn get_offer(
        &self,
        opts: RcvOptions,
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
            // We should make stuff persistant to track the swap!!
            // TODO: make a wallet call to brodcast the transaction!
            // - we need to wait for settlement
            // - we need to refund hltcs https://github.com/arkade-os/boltz-swap/blob/master/src/arkade-lightning.ts#L159
            Ok(())
        }
    }

    fn pay_offer(&self, opts: SentOptions) -> impl Future<Output = Result<()>> + Send {
        async { unimplemented!() }
    }

    fn pay_bip321(&self, _bip321: &str) -> impl Future<Output = Result<()>> + Send {
        async { unimplemented!() }
    }
}

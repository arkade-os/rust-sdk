//! REST client for [Ark delegator services](https://github.com/arkade-os).
//!
//! A delegator is a third-party service that automatically renews VTXOs before they expire,
//! allowing wallets to stay offline without losing funds.
//!
//! This crate implements the client side of the delegator REST protocol:
//! - `GET /v1/delegator/info` — fetch the delegator's public key, fee, and address
//! - `POST /v1/delegate` — submit signed intent and forfeit PSBTs for delegation
//!
//! # Usage
//!
//! ```no_run
//! use ark_delegator::DelegatorClient;
//!
//! # async fn example() -> Result<(), ark_delegator::Error> {
//! let client = DelegatorClient::new("https://delegator.example.com".into());
//!
//! // Fetch delegator info (pubkey needed to construct delegate VTXOs).
//! let info = client.info().await?;
//! println!("delegator pubkey: {}", info.pubkey);
//!
//! // After preparing + signing delegate PSBTs via ark_core::batch,
//! // submit them to the delegator service:
//! // client.delegate(&intent, &forfeit_psbts, None).await?;
//! # Ok(())
//! # }
//! ```

mod error;

use ark_core::intent::Intent;
use bitcoin::base64;
use bitcoin::base64::Engine;
use bitcoin::Psbt;
pub use error::Error;
use serde::Deserialize;
use serde::Serialize;

/// Information about a delegator service.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DelegatorInfo {
    /// The delegator's public key (hex-encoded).
    pub pubkey: String,
    /// The fee charged by the delegator (in satoshis, as a string).
    pub fee: String,
    /// The delegator's on-chain address for fee payments.
    pub delegator_address: String,
}

/// Options for a delegate request.
#[derive(Debug, Clone, Default)]
pub struct DelegateOptions {
    /// If true, the delegator will reject the request if it would replace an existing delegation
    /// that includes at least one VTXO from this request.
    pub reject_replace: bool,
}

#[derive(Serialize)]
struct DelegateRequestIntent {
    message: String,
    proof: String,
}

#[derive(Serialize)]
struct DelegateRequestBody {
    intent: DelegateRequestIntent,
    forfeit_txs: Vec<String>,
    reject_replace: bool,
}

/// REST client for an Ark delegator service.
#[derive(Debug, Clone)]
pub struct DelegatorClient {
    url: String,
    http: reqwest::Client,
}

impl DelegatorClient {
    pub fn new(url: String) -> Self {
        Self {
            url,
            http: reqwest::Client::new(),
        }
    }

    /// Fetch the delegator's public information.
    pub async fn info(&self) -> Result<DelegatorInfo, Error> {
        let url = format!("{}/v1/delegator/info", self.url);

        let response = self.http.get(&url).send().await.map_err(Error::Http)?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Server {
                status: status.as_u16(),
                body,
            });
        }

        response.json().await.map_err(Error::Http)
    }

    /// Submit signed intent and forfeit PSBTs to the delegator for renewal.
    ///
    /// The `intent` and `forfeit_psbts` should be prepared and signed using
    /// [`ark_core::batch::prepare_delegate_psbts`] and [`ark_core::batch::sign_delegate_psbts`].
    pub async fn delegate(
        &self,
        intent: &Intent,
        forfeit_psbts: &[Psbt],
        options: Option<DelegateOptions>,
    ) -> Result<(), Error> {
        let options = options.unwrap_or_default();
        let url = format!("{}/v1/delegate", self.url);

        let b64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let body = DelegateRequestBody {
            intent: DelegateRequestIntent {
                message: intent.serialize_message().map_err(Error::Intent)?,
                proof: intent.serialize_proof(),
            },
            forfeit_txs: forfeit_psbts
                .iter()
                .map(|psbt| b64.encode(psbt.serialize()))
                .collect(),
            reject_replace: options.reject_replace,
        };

        let response = self
            .http
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(Error::Http)?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Server {
                status: status.as_u16(),
                body,
            });
        }

        Ok(())
    }
}

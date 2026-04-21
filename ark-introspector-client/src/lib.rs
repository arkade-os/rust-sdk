use bitcoin::base64;
use bitcoin::base64::Engine;
use bitcoin::Psbt;
use bitcoin::PublicKey;
use bitcoin::XOnlyPublicKey;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Base64(#[from] base64::DecodeError),
    #[error("invalid signer public key: {0}")]
    InvalidSignerPubkey(#[source] bitcoin::key::ParsePublicKeyError),
    #[error("psbt error: {0}")]
    Psbt(#[from] bitcoin::psbt::Error),
}

#[derive(Clone, Debug)]
pub struct Info {
    pub version: String,
    pub signer_pubkey: PublicKey,
}

impl Info {
    pub fn signer_xonly(&self) -> XOnlyPublicKey {
        self.signer_pubkey.inner.x_only_public_key().0
    }
}

#[derive(Clone, Debug)]
pub struct SubmitTxResponse {
    pub signed_ark_tx: Psbt,
    pub signed_checkpoint_txs: Vec<Psbt>,
}

#[derive(Clone, Debug)]
pub struct SubmitOnchainTxResponse {
    pub signed_tx: Psbt,
}

#[derive(Clone, Debug)]
pub struct Intent {
    pub proof: String,
    pub message: String,
}

#[derive(Clone, Debug)]
pub struct TxTreeNode {
    pub txid: String,
    pub tx: String,
    pub children: std::collections::BTreeMap<u32, String>,
}

#[derive(Clone, Debug)]
pub struct IntrospectorClient {
    base_url: String,
    http: reqwest::Client,
}

impl IntrospectorClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            http: reqwest::Client::new(),
        }
    }

    pub fn with_http_client(base_url: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            http,
        }
    }

    pub async fn get_info(&self) -> Result<Info, Error> {
        let response: GetInfoResponse = self
            .http
            .get(format!("{}/v1/info", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(Info {
            version: response.version,
            signer_pubkey: response
                .signer_pubkey
                .parse()
                .map_err(Error::InvalidSignerPubkey)?,
        })
    }

    pub async fn submit_tx(
        &self,
        ark_tx: &Psbt,
        checkpoint_txs: &[Psbt],
    ) -> Result<SubmitTxResponse, Error> {
        let response: SubmitTxResponseWire = self
            .http
            .post(format!("{}/v1/tx", self.base_url))
            .json(&SubmitTxRequest {
                ark_tx: encode_psbt(ark_tx),
                checkpoint_txs: checkpoint_txs.iter().map(encode_psbt).collect(),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(SubmitTxResponse {
            signed_ark_tx: decode_psbt(&response.signed_ark_tx)?,
            signed_checkpoint_txs: response
                .signed_checkpoint_txs
                .iter()
                .map(|tx| decode_psbt(tx))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }

    pub async fn submit_intent(&self, intent: &Intent) -> Result<String, Error> {
        let response: SubmitIntentResponse = self
            .http
            .post(format!("{}/v1/intent", self.base_url))
            .json(&SubmitIntentRequest {
                intent: IntentWire {
                    proof: intent.proof.clone(),
                    message: intent.message.clone(),
                },
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(response.signed_proof)
    }

    pub async fn submit_finalization(
        &self,
        signed_intent: &Intent,
        forfeits: &[String],
        connector_tree: &[TxTreeNode],
        commitment_tx: &str,
    ) -> Result<SubmitFinalizationResponse, Error> {
        let response: SubmitFinalizationResponse = self
            .http
            .post(format!("{}/v1/finalization", self.base_url))
            .json(&SubmitFinalizationRequest {
                signed_intent: IntentWire {
                    proof: signed_intent.proof.clone(),
                    message: signed_intent.message.clone(),
                },
                forfeits: forfeits.to_vec(),
                connector_tree: connector_tree
                    .iter()
                    .map(|node| TxTreeNodeWire {
                        txid: node.txid.clone(),
                        tx: node.tx.clone(),
                        children: node.children.clone(),
                    })
                    .collect(),
                commitment_tx: commitment_tx.to_owned(),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(response)
    }

    pub async fn submit_onchain_tx(&self, tx: &Psbt) -> Result<SubmitOnchainTxResponse, Error> {
        let response: SubmitOnchainTxResponseWire = self
            .http
            .post(format!("{}/v1/onchain-tx", self.base_url))
            .json(&SubmitOnchainTxRequest {
                tx: encode_psbt(tx),
            })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(SubmitOnchainTxResponse {
            signed_tx: decode_psbt(&response.signed_tx)?,
        })
    }
}

fn base64_engine() -> base64::engine::GeneralPurpose {
    base64::engine::GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        base64::engine::GeneralPurposeConfig::new(),
    )
}

fn encode_psbt(psbt: &Psbt) -> String {
    base64_engine().encode(psbt.serialize())
}

fn decode_psbt(psbt: &str) -> Result<Psbt, Error> {
    let bytes = base64_engine().decode(psbt)?;
    Ok(Psbt::deserialize(&bytes)?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetInfoResponse {
    version: String,
    signer_pubkey: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitTxRequest {
    ark_tx: String,
    checkpoint_txs: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmitTxResponseWire {
    signed_ark_tx: String,
    signed_checkpoint_txs: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitIntentRequest {
    intent: IntentWire,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IntentWire {
    proof: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmitIntentResponse {
    signed_proof: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitFinalizationRequest {
    signed_intent: IntentWire,
    forfeits: Vec<String>,
    connector_tree: Vec<TxTreeNodeWire>,
    commitment_tx: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TxTreeNodeWire {
    txid: String,
    tx: String,
    children: std::collections::BTreeMap<u32, String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitFinalizationResponse {
    pub signed_forfeits: Vec<String>,
    pub signed_commitment_tx: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SubmitOnchainTxRequest {
    tx: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmitOnchainTxResponseWire {
    signed_tx: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore]
    async fn get_info() {
        let client = IntrospectorClient::new("http://localhost:7073");
        let info = client.get_info().await.unwrap();
    }
}

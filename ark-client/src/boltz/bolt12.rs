use crate::boltz::Asset;
use crate::boltz::CreateSubmarineSwapResponse;
use crate::error::ErrorContext;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use crate::LnInvoice;
use crate::SubmarineSwapData;
use crate::SwapStatus;
use crate::SwapStorage;
use ark_core::send::SendReceiver;
use bitcoin::hashes::ripemd160;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::Amount;
use bitcoin::PublicKey;
use bitcoin::Txid;
use lightning::offers::invoice::Bolt12Invoice;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// A BOLT12 invoice paired with its original encoded string.
#[derive(Debug, Clone)]
pub struct ParsedBolt12Invoice {
    encoded: String,
    invoice: Bolt12Invoice,
}

impl ParsedBolt12Invoice {
    pub fn parse(encoded: impl Into<String>) -> Result<Self, Error> {
        let encoded = encoded.into().trim().to_string();
        let invoice = parse_bolt12_invoice(&encoded)?;

        Ok(Self { encoded, invoice })
    }

    pub fn encoded(&self) -> &str {
        &self.encoded
    }

    pub fn invoice(&self) -> &Bolt12Invoice {
        &self.invoice
    }

    pub fn payment_hash(&self) -> sha256::Hash {
        sha256::Hash::from_byte_array(self.invoice.payment_hash().0)
    }
}

impl Serialize for ParsedBolt12Invoice {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.encoded)
    }
}

impl<'de> Deserialize<'de> for ParsedBolt12Invoice {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        Self::parse(encoded).map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for ParsedBolt12Invoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.encoded)
    }
}

/// Result of a Bolt12 offer payment preparation.
#[derive(Clone, Debug)]
pub struct Bolt12SubmarineSwapResult {
    pub swap_id: String,
    pub txid: Txid,
    pub amount: Amount,
    /// The BOLT12 invoice fetched from the offer and used for this swap.
    pub invoice: String,
}

impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: crate::KeyProvider,
{
    /// Prepare the payment of a BOLT12 offer by fetching an invoice and setting up a submarine
    /// swap via Boltz.
    ///
    /// This function does not execute the payment itself. Once you are ready for payment you
    /// will have to send the required `amount` to the `vhtlc_address` in the returned swap data.
    ///
    /// If you are looking for a function which pays the offer immediately, consider using
    /// [`Client::pay_bolt12_offer`] instead.
    ///
    /// # Trust Model
    ///
    /// This method fetches the BOLT12 invoice via the Boltz API (`bolt12_fetch`). The invoice
    /// is not locally verified against the offer's signing key because full BOLT12 invoice
    /// verification requires a dedicated parsing library (e.g., LDK). This is the same trust
    /// model as BOLT11 submarine swaps, where the VHTLC address and claim keys are provided by
    /// Boltz.
    ///
    /// # Arguments
    ///
    /// - `offer`: a BOLT12 offer string.
    /// - `amount_sats`: optional amount in satoshis. Required if the offer does not specify an
    ///   amount.
    ///
    /// # Returns
    ///
    /// - A [`SubmarineSwapData`] object, including an identifier for the swap.
    pub async fn prepare_bolt12_offer_payment(
        &self,
        offer: &str,
        amount_sats: Option<u64>,
    ) -> Result<SubmarineSwapData, Error> {
        let (data, _invoice_str) = self
            .create_bolt12_submarine_swap(offer, amount_sats)
            .await?;

        tracing::info!(
            swap_id = data.id,
            vhtlc_address = %data.vhtlc_address,
            expected_amount = %data.amount,
            "Prepared BOLT12 offer payment"
        );

        Ok(data)
    }

    /// Pay a BOLT12 offer by performing a submarine swap via Boltz. This fetches the BOLT12
    /// invoice from the offer, creates a submarine swap, and funds the VHTLC.
    ///
    /// # Trust Model
    ///
    /// This method fetches the BOLT12 invoice via the Boltz API (`bolt12_fetch`). The invoice
    /// is not locally verified against the offer's signing key because full BOLT12 invoice
    /// verification requires a dedicated parsing library (e.g., LDK). This is the same trust
    /// model as BOLT11 submarine swaps, where the VHTLC address and claim keys are provided by
    /// Boltz.
    ///
    /// # Arguments
    ///
    /// - `offer`: a BOLT12 offer string.
    /// - `amount_sats`: optional amount in satoshis. Required if the offer does not specify an
    ///   amount.
    ///
    /// # Returns
    ///
    /// - A [`Bolt12SubmarineSwapResult`], including an identifier for the swap and the TXID of the
    ///   Ark transaction that funds the VHTLC.
    pub async fn pay_bolt12_offer(
        &self,
        offer: &str,
        amount_sats: Option<u64>,
    ) -> Result<Bolt12SubmarineSwapResult, Error> {
        let (data, invoice) = self
            .create_bolt12_submarine_swap(offer, amount_sats)
            .await?;

        let vhtlc_address = data.vhtlc_address;
        let amount = data.amount;
        let swap_id = data.id;
        let txid = self
            .send(vec![SendReceiver::bitcoin(vhtlc_address, amount)])
            .await?;

        tracing::info!(%swap_id, %amount, "Funded VHTLC for BOLT12 offer");

        Ok(Bolt12SubmarineSwapResult {
            swap_id,
            txid,
            amount,
            invoice: invoice.encoded().to_string(),
        })
    }

    /// Shared setup for BOLT12 submarine swaps: fetch invoice, create swap, persist data.
    async fn create_bolt12_submarine_swap(
        &self,
        offer: &str,
        amount_sats: Option<u64>,
    ) -> Result<(SubmarineSwapData, ParsedBolt12Invoice), Error> {
        let invoice = self.fetch_bolt12_invoice(offer, amount_sats).await?;

        let payment_hash = invoice.payment_hash();
        let preimage_hash = ripemd160::Hash::hash(payment_hash.as_byte_array());

        let refund_keypair = self.next_keypair(crate::key_provider::KeypairIndex::New)?;
        let refund_public_key = refund_keypair.public_key();
        let key_derivation_index =
            self.derivation_index_for_pk(&refund_keypair.x_only_public_key().0);

        let request = CreateStringInvoiceSubmarineSwapRequest {
            from: Asset::Ark,
            to: Asset::Btc,
            invoice: invoice.encoded().to_string(),
            refund_public_key: refund_public_key.into(),
        };
        let url = format!("{}/v2/swap/submarine", self.inner.boltz_url);

        let client = reqwest::Client::builder()
            .timeout(self.inner.timeout)
            .build()
            .map_err(|e| Error::ad_hoc(e.to_string()))?;
        let response = client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to send submarine swap request")?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))
                .context("failed to read error text")?;

            return Err(Error::ad_hoc(format!(
                "failed to create submarine swap: {error_text}"
            )));
        }

        let swap_response: CreateSubmarineSwapResponse = response
            .json()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to deserialize submarine swap response")?;

        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(Error::ad_hoc)
            .context("failed to compute created_at")?;

        let data = SubmarineSwapData {
            id: swap_response.id.clone(),
            status: SwapStatus::Created,
            preimage: None,
            preimage_hash,
            refund_public_key: refund_public_key.into(),
            claim_public_key: swap_response.claim_public_key,
            vhtlc_address: swap_response.address,
            timeout_block_heights: swap_response.timeout_block_heights,
            amount: swap_response.expected_amount,
            invoice: LnInvoice::Bolt12(invoice.clone()),
            created_at: created_at.as_secs(),
            key_derivation_index,
        };

        self.swap_storage()
            .insert_submarine(swap_response.id.clone(), data.clone())
            .await?;

        Ok((data, invoice))
    }

    /// Fetch a BOLT12 invoice from an offer using the Boltz API.
    ///
    /// This calls `POST /v2/lightning/BTC/bolt12/fetch` to resolve a BOLT12 offer into an invoice
    /// that can be used to create a submarine swap.
    ///
    /// # Security Note
    ///
    /// Callers that use the returned invoice outside of this SDK's submarine swap methods should
    /// verify the invoice's signing key against the offer's signing key (or the public key of
    /// the final hop in one of the offer's message paths).
    ///
    /// # Arguments
    ///
    /// - `offer`: a BOLT12 offer string (typically starts with `lno`).
    /// - `amount_sats`: optional amount in satoshis. Required if the offer does not specify an
    ///   amount.
    ///
    /// # Returns
    ///
    /// The BOLT12 invoice string (typically starts with `lni`).
    async fn fetch_bolt12_invoice(
        &self,
        offer: &str,
        amount_sats: Option<u64>,
    ) -> Result<ParsedBolt12Invoice, Error> {
        let request = Bolt12FetchInvoiceRequest {
            offer: offer.to_string(),
            amount: amount_sats,
        };

        let response: Bolt12FetchInvoiceResponse = self
            .post_bolt12_api(
                "/v2/lightning/BTC/bolt12/fetch",
                &request,
                "failed to fetch bolt12 invoice from offer",
            )
            .await?;
        let invoice = ParsedBolt12Invoice::parse(response.invoice)
            .context("bolt12_fetch returned invalid BOLT12 invoice")?;

        tracing::info!("Fetched BOLT12 invoice from offer");

        Ok(invoice)
    }

    async fn post_bolt12_api<TReq, TResp>(
        &self,
        path: &str,
        request: &TReq,
        error_context: &str,
    ) -> Result<TResp, Error>
    where
        TReq: Serialize + ?Sized,
        TResp: DeserializeOwned,
    {
        let urls = bolt12_api_url_candidates(&self.inner.boltz_url, path);
        let client = reqwest::Client::builder()
            .timeout(self.inner.timeout)
            .build()
            .map_err(|e| Error::ad_hoc(e.to_string()))?;

        let mut last_error = None;
        for (idx, url) in urls.iter().enumerate() {
            let response = client
                .post(url)
                .json(request)
                .send()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))
                .with_context(|| format!("{error_context}: request failed"))?;

            if response.status().is_success() {
                return response
                    .json()
                    .await
                    .map_err(|e| Error::ad_hoc(e.to_string()))
                    .with_context(|| format!("{error_context}: failed to deserialize response"));
            }

            let status = response.status();
            let error_text = response
                .text()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))
                .with_context(|| format!("{error_context}: failed to read error text"))?;

            let route_missing =
                status == reqwest::StatusCode::NOT_FOUND && error_text.contains("Cannot POST");
            last_error = Some(format!("{error_context}: {error_text}"));

            if route_missing && idx + 1 < urls.len() {
                continue;
            }

            break;
        }

        Err(Error::ad_hoc(last_error.unwrap_or_else(|| {
            format!("{error_context}: no BOLT12 API URL candidates")
        })))
    }
}

fn bolt12_api_url_candidates(base_url: &str, path: &str) -> Vec<String> {
    let base_url = base_url.trim_end_matches('/');
    let mut urls = vec![format!("{base_url}{path}")];

    // In the local Boltz regtest setup the BOLT12 API is implemented by Boltz's sidecar HTTP
    // server on port 9005, while regular swap endpoints are served by the main API on port 9001.
    if let Ok(mut url) = reqwest::Url::parse(base_url) {
        if url.port() == Some(9001) && url.set_port(Some(9005)).is_ok() {
            let sidecar_base = url.as_str().trim_end_matches('/');
            let sidecar_url = format!("{sidecar_base}{path}");
            if !urls.contains(&sidecar_url) {
                urls.push(sidecar_url);
            }
        }
    }

    urls
}

fn parse_bolt12_invoice(invoice: &str) -> Result<Bolt12Invoice, Error> {
    let invoice = invoice.trim();
    if let Ok((hrp, data)) = bech32::decode(invoice) {
        if hrp.to_string() != "lni" {
            return Err(Error::ad_hoc(format!(
                "expected bolt12 invoice with 'lni' prefix, got '{hrp}'"
            )));
        }

        return Bolt12Invoice::try_from(data)
            .map_err(|e| Error::ad_hoc(format!("invalid bolt12 invoice: {e:?}")));
    }

    let parsed = bech32::primitives::decode::CheckedHrpstring::new::<bech32::NoChecksum>(invoice)
        .map_err(|e| Error::ad_hoc(format!("invalid bolt12 invoice encoding: {e}")))?;

    if parsed.hrp().lowercase_char_iter().ne("lni".chars()) {
        return Err(Error::ad_hoc(format!(
            "expected bolt12 invoice with 'lni' prefix, got '{}'",
            parsed.hrp()
        )));
    }

    let data = parsed.byte_iter().collect::<Vec<_>>();
    Bolt12Invoice::try_from(data)
        .map_err(|e| Error::ad_hoc(format!("invalid bolt12 invoice: {e:?}")))
}

/// Request to the Boltz API to create a submarine swap with a string invoice (BOLT11 or BOLT12).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CreateStringInvoiceSubmarineSwapRequest {
    from: Asset,
    to: Asset,
    invoice: String,
    #[serde(rename = "refundPublicKey")]
    refund_public_key: PublicKey,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bolt12FetchInvoiceRequest {
    offer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    amount: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bolt12FetchInvoiceResponse {
    invoice: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_BOLT12_INVOICE: &str = "lni1qqgyfz22nen8c8lf89xjzwcd4dd55q3qqc3xu3s3rg94nj40zfsy866mhu5vxne6tcej5878k2mneuvgjy8s5ytpwf4j6unnyppy7nz5xyezqefjv5gwuqmturepmkwfg3x5wqavcls4n563gjrfqh9rjkq336k7ctymg7saa5p547vcr3kax2ylxq0svfy5xym9fkw8tpr0ye9tlky0tddvc7f8jhqzqvh7r68ewg5gsnu9p0nja65vl7k0y9ymgg8lv5hkswq745rhs0s7cqpnxvlsgq65m7zhxpwj24nvq4wptz572erpcncv3csjv2ljrxl75cdrv3vlu256y45a3ntr4yxt8qak8n4v6gpev8qv5c7hszqxaece3hqpnsaej89fcqcf3hvn23jtyjffs88sxnsqxg0vzx6v54mq9xz3tufvyfj5nu4w9ut3nc83ulvxlltuypwt2qvujpmpqyasyrslctv267z2hdpd7kxumutzzq4w0hd2daakzvmkaxf29kkddcrx5nuxw6ccdwh2jpzvugesqc2l09gzqp3zderpzxstt8927ynqg044h0egcd8n5h3n9g0u0v4h8ncc3yg02gp3apyq2sq9sggrkavptf4xkaxr0y64s5v6rln7afe4lqzlgtqunfzj2qmedhka97c6pxqz4e7a4fhhkcfnwm5e9gk6e4hqv6j0semtrp46a2gyfn3rxqrptaus8w9rgyaa0kuutdn9nc2hhlnta5zkkkhmdn46ahrh9rwjkhquryyjqyptpkyl5fs5v6qka8v747nqhmuxadur3r5lgdsruhq6z4w64pq0hwsqxg68q4r8hnhzp3248m2py0h0a58ypqjkshj3mp0txuvahcyjrqm2cy4czqtk34y96hvhsv3lamy5s5za4k3pcqqqqqqqqqqqqqqq5qqqqqqqqqqqqqwjfvkl43fqqqqqqzjqg6s945d6sg9crdtxau5wsdcl9uze6wfg5ltdzaxnljwf45qwrr59m6qgxg0y0z4qx85yszhqxqsqqzczzq4w0hd2daakzvmkaxf29kkddcrx5nuxw6ccdwh2jpzvugesqc2l08cypufuzjvgwtk8zzns3cnap3hsy0dpdku8vfac7rjhh5tsuaff5476hwxu0unzn5q5u3pppfhwtr7fyyfjrmg7xyts6wgk4yne2leul4us";

    #[test]
    fn test_parse_bolt12_invoice_with_ldk() {
        let invoice = ParsedBolt12Invoice::parse(VALID_BOLT12_INVOICE).unwrap();
        assert_eq!(invoice.encoded(), VALID_BOLT12_INVOICE);
        assert_ne!(invoice.payment_hash().as_byte_array(), &[0u8; 32]);
    }

    #[test]
    fn test_parse_bolt12_invoice_wrong_hrp() {
        let invalid_invoice = VALID_BOLT12_INVOICE.replacen("lni", "lno", 1);
        let err = ParsedBolt12Invoice::parse(invalid_invoice).unwrap_err();
        assert!(err.to_string().contains("lni"));
    }

    #[test]
    fn test_ln_invoice_serde_roundtrip_bolt12() {
        let invoice = LnInvoice::Bolt12(ParsedBolt12Invoice::parse(VALID_BOLT12_INVOICE).unwrap());

        let json = serde_json::to_string(&invoice).unwrap();
        let deserialized: LnInvoice = serde_json::from_str(&json).unwrap();

        match deserialized {
            LnInvoice::Bolt12(invoice) => assert_eq!(invoice.encoded(), VALID_BOLT12_INVOICE),
            LnInvoice::Bolt11(_) => panic!("expected Bolt12 variant"),
        }
    }
}

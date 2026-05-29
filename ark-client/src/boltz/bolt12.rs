use crate::boltz::CreateSubmarineSwapParams;
use crate::error::ErrorContext;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use crate::LnInvoice;
use crate::SubmarineSwapData;
use crate::SwapStorage;
use ark_core::send::SendReceiver;
use bitcoin::hashes::ripemd160;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::Amount;
use bitcoin::Txid;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::offer::Offer;
use serde::Deserialize;
use serde::Serialize;
use std::fmt;
use std::str::FromStr;

/// A BOLT12 invoice paired with its original encoded string.
#[derive(Debug, Clone)]
pub struct ParsedBolt12Invoice {
    encoded: String,
    invoice: Bolt12Invoice,
}

impl ParsedBolt12Invoice {
    pub fn parse(encoded: impl Into<String>) -> Result<Self, Error> {
        let encoded = encoded.into().trim().to_string();

        let parsed =
            bech32::primitives::decode::CheckedHrpstring::new::<bech32::NoChecksum>(&encoded)
                .map_err(|e| Error::ad_hoc(format!("invalid bolt12 invoice encoding: {e}")))?;

        if parsed.hrp().lowercase_char_iter().ne("lni".chars()) {
            return Err(Error::ad_hoc(format!(
                "expected bolt12 invoice with 'lni' prefix, got '{}'",
                parsed.hrp()
            )));
        }

        let data = parsed.byte_iter().collect::<Vec<_>>();
        let invoice = Bolt12Invoice::try_from(data)
            .map_err(|e| Error::ad_hoc(format!("invalid bolt12 invoice: {e:?}")))?;

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
    /// is locally verified against the offer: its signing public key must match the offer's
    /// `issuer_signing_pubkey` (if set) or the final hop of one of the offer's blinded message
    /// paths, and its `offer_id` (if present) must match the offer's identifier. The returned
    /// VHTLC address is verified against the VHTLC parameters before it is persisted or funded.
    ///
    /// # Arguments
    ///
    /// - `offer`: a BOLT12 offer string.
    /// - `amount`: optional amount. Required if the offer does not specify an amount.
    ///
    /// # Returns
    ///
    /// - A [`SubmarineSwapData`] object, including an identifier for the swap.
    pub async fn prepare_bolt12_offer_payment(
        &self,
        offer: &str,
        amount: Option<Amount>,
    ) -> Result<SubmarineSwapData, Error> {
        let (data, _invoice_str) = self.create_bolt12_submarine_swap(offer, amount).await?;

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
    /// is locally verified against the offer: its signing public key must match the offer's
    /// `issuer_signing_pubkey` (if set) or the final hop of one of the offer's blinded message
    /// paths, and its `offer_id` (if present) must match the offer's identifier. The returned
    /// VHTLC address is verified against the VHTLC parameters before it is persisted or funded.
    ///
    /// # Arguments
    ///
    /// - `offer`: a BOLT12 offer string.
    /// - `amount`: optional amount. Required if the offer does not specify an amount.
    ///
    /// # Returns
    ///
    /// - A [`Bolt12SubmarineSwapResult`], including an identifier for the swap and the TXID of the
    ///   Ark transaction that funds the VHTLC.
    pub async fn pay_bolt12_offer(
        &self,
        offer: &str,
        amount: Option<Amount>,
    ) -> Result<Bolt12SubmarineSwapResult, Error> {
        let (data, invoice) = self.create_bolt12_submarine_swap(offer, amount).await?;

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
        amount: Option<Amount>,
    ) -> Result<(SubmarineSwapData, ParsedBolt12Invoice), Error> {
        let invoice = self.fetch_bolt12_invoice(offer, amount).await?;

        let payment_hash = invoice.payment_hash();
        let preimage_hash = ripemd160::Hash::hash(payment_hash.as_byte_array());

        let refund_keypair = self.next_keypair(crate::key_provider::KeypairIndex::New)?;
        let refund_public_key = refund_keypair.public_key();
        let key_derivation_index =
            self.derivation_index_for_pk(&refund_keypair.x_only_public_key().0);

        let data = self
            .create_submarine_swap(CreateSubmarineSwapParams {
                invoice: LnInvoice::Bolt12(invoice.clone()),
                refund_public_key: refund_public_key.into(),
                key_derivation_index,
                preimage_hash,
            })
            .await?;

        Ok((data, invoice))
    }

    /// Fetch a BOLT12 invoice from an offer using the Boltz API.
    ///
    /// This calls `POST /v2/lightning/BTC/bolt12/fetch` to resolve a BOLT12 offer into an invoice
    /// that can be used to create a submarine swap.
    ///
    /// The returned invoice is locally verified against the offer: the invoice's signing public key
    /// must match the offer's `issuer_signing_pubkey` (if set) or the final hop of one of the
    /// offer's blinded message paths, and the invoice's `offer_id` (if present) must match the
    /// offer's identifier.
    ///
    /// # Arguments
    ///
    /// - `offer`: a BOLT12 offer string (typically starts with `lno`).
    /// - `amount`: optional amount. Required if the offer does not specify an amount.
    ///
    /// # Returns
    ///
    /// The BOLT12 invoice string (typically starts with `lni`).
    async fn fetch_bolt12_invoice(
        &self,
        offer: &str,
        amount: Option<Amount>,
    ) -> Result<ParsedBolt12Invoice, Error> {
        let request = Bolt12FetchInvoiceRequest {
            offer: offer.to_string(),
            amount: amount.map(|amount| amount.to_sat()),
        };

        let url = format!(
            "{}/v2/lightning/BTC/bolt12/fetch",
            self.inner.boltz_bolt12_url().trim_end_matches('/')
        );
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
            .context("failed to fetch bolt12 invoice from offer")?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))
                .context("failed to read bolt12_fetch error text")?;

            return Err(Error::ad_hoc(format!(
                "failed to fetch bolt12 invoice from offer: {error_text}"
            )));
        }

        let response: Bolt12FetchInvoiceResponse = response
            .json()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to deserialize bolt12_fetch response")?;
        let invoice = ParsedBolt12Invoice::parse(response.invoice)
            .context("bolt12_fetch returned invalid BOLT12 invoice")?;

        // Verify the invoice matches the offer before using it.
        verify_bolt12_invoice_against_offer(offer, &invoice)?;

        tracing::info!("Fetched and verified BOLT12 invoice from offer");

        Ok(invoice)
    }
}

/// Verify that a BOLT12 invoice corresponds to the offer it claims to be for.
///
/// Checks:
///
/// 1. If the offer specifies an explicit `issuer_signing_pubkey`, the invoice's `signing_pubkey`
///    must match it.
/// 2. Otherwise, if the offer has blinded message paths, the invoice's `signing_pubkey` must match
///    the `blinded_node_id` of the final hop in one of those paths.
/// 3. If the invoice carries an `offer_id`, it must match the offer's own identifier.
///
/// This prevents a Boltz endpoint from returning an invoice for a different node than the
/// one the user intended to pay — the central trust gap in the Boltz-resolves-offer flow.
pub fn verify_bolt12_invoice_against_offer(
    offer_str: &str,
    invoice: &ParsedBolt12Invoice,
) -> Result<(), Error> {
    let offer = Offer::from_str(offer_str)
        .map_err(|e| Error::ad_hoc(format!("invalid bolt12 offer: {e:?}")))?;
    let invoice_signing_pk = invoice.invoice().signing_pubkey();

    // Check 1: explicit issuer_signing_pubkey.
    if let Some(issuer_pk) = offer.issuer_signing_pubkey() {
        if invoice_signing_pk != issuer_pk {
            return Err(Error::ad_hoc(format!(
                "BOLT12 invoice signing pubkey ({invoice_signing_pk}) does not match \
                 offer issuer_signing_pubkey ({issuer_pk})",
            )));
        }
    } else {
        // Check 2: offer has blinded paths — verify invoice signing key is the
        // final hop in one of them.
        let paths = offer.paths();
        if !paths.is_empty() {
            let matches = paths
                .iter()
                .filter_map(|path| path.blinded_hops().last())
                .any(|last_hop| invoice_signing_pk == last_hop.blinded_node_id);
            if !matches {
                return Err(Error::ad_hoc(format!(
                    "BOLT12 invoice signing pubkey ({invoice_signing_pk}) does not match \
                     the final hop of any blinded message path in the offer",
                )));
            }
        }
        // If neither issuer_signing_pubkey nor paths are set, we cannot verify the
        // invoice's signing key — the offer is unusually permissive. Accept it.
    }

    // Check 3: offer_id cross-check (if the invoice carries one).
    if let Some(invoice_offer_id) = invoice.invoice().offer_id() {
        if invoice_offer_id != offer.id() {
            return Err(Error::ad_hoc(format!(
                "BOLT12 invoice offer_id ({invoice_offer_id:?}) does not match \
                 offer id ({:?})",
                offer.id()
            )));
        }
    }

    Ok(())
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

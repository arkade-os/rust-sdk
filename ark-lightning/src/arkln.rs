//! Lightning Network Module for the Ark Lightning Swap
//!
//! Vincenzo Palazzo <vincenzopalazzodev@gmail.com>
use crate::ldk::bolt11_invoice as invoice;
use crate::ldk::offers;
use bitcoin::Amount;

#[derive(Debug, Clone)]
pub struct GetPayOptions {
    pub invoice_amount: Amount,
    pub description: Option<String>,
    pub claim_public_key: String,
}

/// A struct representing the Lightning Network functionality.
pub trait Lightning {
    /// Get a Bolt11 invoice!
    fn get_invoice(
        &self,
        opts: GetPayOptions,
    ) -> impl Future<Output = anyhow::Result<invoice::Bolt11Invoice>> + Send;

    /// Get an bolt12 offer!
    fn get_offer(
        &self,
        offer: GetPayOptions,
    ) -> impl Future<Output = anyhow::Result<offers::offer::Offer>> + Send;

    /// Pay a bolt11 invoice!
    fn pay_invoice(
        &self,
        invoice: &invoice::Bolt11Invoice,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Pay a bolt12 offer!
    fn pay_offer(
        &self,
        offer: &offers::offer::Offer,
    ) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Pay a BIP321 payment request!
    fn pay_bip321(&self, bip321: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    // TODO: add the bip 353 support!
}

//! Lightning Network Module for the Ark Lightning Swap
//!
//! Vincenzo Palazzo <vincenzopalazzodev@gmail.com>
use crate::ldk::bolt11_invoice as invoice;
use crate::ldk::offers;
use bitcoin::Amount;
use lightning::bolt11_invoice::Bolt11Invoice;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::offer::Offer;

#[derive(Debug, Clone)]
pub struct RcvOptions {
    pub invoice_amount: Amount,
    pub description: Option<String>,
    pub claim_public_key: String,
}

#[derive(Debug, Clone)]
pub struct SentOptions {
    pub invoice: invoice::Bolt11Invoice,
    pub refund_public_key: String,
}

pub trait EventHandle: Send + Sync {
    fn on_invoice_paid(&self, invoice: Bolt11Invoice, amount: Amount, preimage: Vec<u8>);
    fn on_offer_paid(
        &self,
        offer: Offer,
        invoice: Bolt12Invoice,
        amount: Amount,
        preimage: Vec<u8>,
    );
    fn on_payment_pending(&self, amount: Amount);
    fn on_payment_failed(&self, amount: Amount);
    fn on_payment_received(&self, amount: Amount);
}

pub struct DummyEventHandler;

impl EventHandle for DummyEventHandler {
    fn on_invoice_paid(&self, _invoice: Bolt11Invoice, _amount: Amount, _preimage: Vec<u8>) {}
    fn on_offer_paid(
        &self,
        _offer: Offer,
        _invoice: Bolt12Invoice,
        _amount: Amount,
        _preimage: Vec<u8>,
    ) {
    }
    fn on_payment_pending(&self, _amount: Amount) {}
    fn on_payment_failed(&self, _amount: Amount) {}
    fn on_payment_received(&self, _amount: Amount) {}
}

/// A struct representing the Lightning Network functionality.
pub trait Lightning {
    /// Get a Bolt11 invoice!
    fn get_invoice(
        &self,
        opts: RcvOptions,
    ) -> impl Future<Output = anyhow::Result<invoice::Bolt11Invoice>> + Send;

    /// Get an bolt12 offer!
    fn get_offer(
        &self,
        offer: RcvOptions,
    ) -> impl Future<Output = anyhow::Result<offers::offer::Offer>> + Send;

    /// Pay a bolt11 invoice!
    fn pay_invoice(&self, opts: SentOptions) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Pay a bolt12 offer!
    fn pay_offer(&self, opts: SentOptions) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Pay a BIP321 payment request!
    fn pay_bip321(&self, bip321: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    // TODO: add the bip 353 support!
}

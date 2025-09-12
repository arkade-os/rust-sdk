//! Lightning Network Module for the Ark Lightning Swap
//!
//! Vincenzo Palazzo <vincenzopalazzodev@gmail.com>
use ark_core::ArkAddress;
use bitcoin::Amount;
use bitcoin::Transaction;
use bitcoin::XOnlyPublicKey;
use lightning::bolt11_invoice::Bolt11Invoice;
use lightning::offers::invoice::Bolt12Invoice;
use lightning::offers::offer::Offer;
use std::future::Future;
use std::pin::Pin;

#[derive(Debug, Clone)]
pub struct RcvOptions {
    pub invoice_amount: Amount,
    pub description: Option<String>,
    pub claim_public_key: String,
}

#[derive(Debug, Clone)]
pub struct SentOptions {
    pub invoice: Bolt11Invoice,
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
    ) -> impl Future<Output = anyhow::Result<Bolt11Invoice>> + Send;

    /// Get an bolt12 offer!
    fn get_offer(
        &self,
        offer: RcvOptions,
    ) -> impl Future<Output = anyhow::Result<Offer>> + Send;

    /// Pay a bolt11 invoice!
    fn pay_invoice(&self, opts: SentOptions) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Pay a bolt12 offer!
    fn pay_offer(&self, opts: SentOptions) -> impl Future<Output = anyhow::Result<()>> + Send;

    /// Pay a BIP321 payment request!
    fn pay_bip321(&self, bip321: &str) -> impl Future<Output = anyhow::Result<()>> + Send;
    // TODO: add the bip 353 support!
}

pub trait ArkWallet {
    /// Send funds on a specific address
    fn send_bitcoin(&self, address: ArkAddress, amount: Amount) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>;

    // Extract the xpub from the wallet
    fn get_xpub(&self) -> XOnlyPublicKey;

    // Sign a transaction with the wallet or something that can sign!
    fn sign_tx(&self, tx: &Transaction) -> Pin<Box<dyn Future<Output = anyhow::Result<Transaction>> + Send>>;
}

/// Dummy wallet implementation for testing
pub struct DummyWallet;

impl DummyWallet {
    pub fn new() -> Self {
        Self
    }
}

impl ArkWallet for DummyWallet {
    fn send_bitcoin(&self, _address: ArkAddress, _amount: Amount) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>> {
        Box::pin(async { Ok(()) })
    }

    fn get_xpub(&self) -> XOnlyPublicKey {
        // Return a dummy public key
        XOnlyPublicKey::from_slice(&[0u8; 32]).unwrap()
    }

    fn sign_tx(&self, tx: &Transaction) -> Pin<Box<dyn Future<Output = anyhow::Result<Transaction>> + Send>> {
        let tx = tx.clone();
        Box::pin(async move { Ok(tx) })
    }
}
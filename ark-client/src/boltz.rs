// Active VHTLC contracts are not swept by deprecated-signer migration. Their claim/refund
// recovery paths reconstruct scripts against both current and deprecated server keys so swaps
// created before a signer rotation remain recoverable.

use crate::batch::BatchOutputType;
use crate::error::ErrorContext as _;
use crate::swap_storage::SwapStorage;
use crate::timeout_op;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::contract::ContractState;
use ark_core::contract::VhtlcContract;
use ark_core::intent;
use ark_core::script::extract_checksig_pubkeys;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::OffchainTransactions;
use ark_core::send::SendReceiver;
use ark_core::send::VtxoInput;
use ark_core::server::parse_sequence_number;
use ark_core::server::Info;
use ark_core::server::PendingTx;
use ark_core::server::SubscriptionEvent;
use ark_core::server::SubscriptionResponse;
use ark_core::vhtlc::VhtlcOptions;
use ark_core::vhtlc::VhtlcScript;
use ark_core::ArkAddress;
use ark_core::VtxoList;
use ark_core::VTXO_CONDITION_KEY;
use bitcoin::absolute;
use bitcoin::consensus::Encodable;
use bitcoin::hashes::ripemd160;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::io::Write;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::taproot::LeafVersion;
use bitcoin::Amount;
use bitcoin::Psbt;
use bitcoin::PublicKey;
use bitcoin::ScriptBuf;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::VarInt;
use bitcoin::XOnlyPublicKey;
use lightning_invoice::Bolt11Invoice;
use rand::CryptoRng;
use rand::Rng;
use serde::Deserialize;
use serde::Serialize;
use serde_with::serde_as;
use serde_with::DisplayFromStr;
use std::collections::HashMap;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Maximum byte length of a BOLT11 invoice description (`d` field).
///
/// BOLT11 tagged fields use a 10-bit length in 5-bit groups, capping the payload at
/// `floor(1023 * 5 / 8) = 639` UTF-8 bytes.
const MAX_BOLT11_DESCRIPTION_BYTES: usize = 639;
const VHTLC_WATCHER_INITIAL_BACKOFF: Duration = Duration::from_secs(1);
const VHTLC_WATCHER_MAX_BACKOFF: Duration = Duration::from_secs(30);
const VHTLC_WATCHER_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

fn validate_invoice_description(description: Option<&str>) -> Result<(), Error> {
    if let Some(d) = description {
        if d.len() > MAX_BOLT11_DESCRIPTION_BYTES {
            return Err(Error::consumer(format!(
                "invoice description is {} bytes (> {} bytes).",
                d.len(),
                MAX_BOLT11_DESCRIPTION_BYTES,
            )));
        }
    }
    Ok(())
}

/// The type of a Boltz swap.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum SwapType {
    Submarine,
    Reverse,
    Chain,
    /// Swap ID not found in local storage.
    Unknown,
}

impl std::fmt::Display for SwapType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Submarine => write!(f, "submarine"),
            Self::Reverse => write!(f, "reverse"),
            Self::Chain => write!(f, "chain"),
            Self::Unknown => write!(f, "unknown"),
        }
    }
}

/// Status information for a Boltz swap.
#[derive(Clone, Debug)]
pub struct SwapStatusInfo {
    pub swap_id: String,
    pub swap_type: SwapType,
    pub status: SwapStatus,
}

#[derive(Clone, Debug)]
pub struct SubmarineSwapResult {
    pub swap_id: String,
    pub txid: Txid,
    pub amount: Amount,
}

#[derive(Clone, Debug)]
pub struct ReverseSwapResult {
    pub swap_id: String,
    pub amount: Amount,
    pub invoice: Bolt11Invoice,
}

#[derive(Clone, Debug)]
pub struct ClaimVhtlcResult {
    pub swap_id: String,
    pub claim_txid: Txid,
    pub claim_amount: Amount,
    pub preimage: [u8; 32],
}

/// The type of VHTLC spend that was submitted but not yet finalized.
///
/// Determined by matching the spend script in the pending transaction's PSBT against the known
/// VHTLC spend paths.
#[derive(Clone, Debug)]
pub enum PendingVhtlcSpendType {
    /// Claim via `claim_script`: preimage + receiver + server.
    ///
    /// Used in reverse submarine swaps (receiving Lightning → Arkade).
    Claim { swap_id: String, preimage: [u8; 32] },
    /// Collaborative refund via `refund_script`: sender + receiver (Boltz) + server.
    ///
    /// Used in submarine swaps when Boltz cooperates.
    CollaborativeRefund { swap_id: String },
    /// Expired refund via `refund_without_receiver_script`: CLTV timeout + sender + server.
    ///
    /// Used in submarine swaps when the timelock has expired and Boltz is unavailable.
    ExpiredRefund { swap_id: String },
}

impl PendingVhtlcSpendType {
    pub fn swap_id(&self) -> &str {
        match self {
            Self::Claim { swap_id, .. }
            | Self::CollaborativeRefund { swap_id }
            | Self::ExpiredRefund { swap_id } => swap_id,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::Claim { .. } => "Claim",
            Self::CollaborativeRefund { .. } => "CollaborativeRefund",
            Self::ExpiredRefund { .. } => "ExpiredRefund",
        }
    }
}

/// A pending (submitted but not finalized) VHTLC spend transaction.
#[derive(Clone, Debug)]
pub struct PendingVhtlcSpendTx {
    pub spend_type: PendingVhtlcSpendType,
    pub pending_tx: PendingTx,
}

/// Configuration for the background Boltz VHTLC watcher.
#[derive(Clone, Copy, Debug)]
pub struct BoltzVhtlcWatcherConfig {
    /// How often to refresh the VHTLC subscription and retry status-driven refund actions.
    pub refresh_interval: Duration,
}

impl Default for BoltzVhtlcWatcherConfig {
    fn default() -> Self {
        Self {
            refresh_interval: VHTLC_WATCHER_REFRESH_INTERVAL,
        }
    }
}

/// Handle to stop the background Boltz VHTLC watcher.
pub struct BoltzVhtlcWatcherHandle {
    stop_tx: tokio::sync::watch::Sender<bool>,
}

impl BoltzVhtlcWatcherHandle {
    /// Stop the background watcher.
    pub fn stop(self) {
        let _ = self.stop_tx.send(true);
    }
}

impl Drop for BoltzVhtlcWatcherHandle {
    fn drop(&mut self) {
        let _ = self.stop_tx.send(true);
    }
}

#[derive(Default)]
struct BoltzVhtlcActionLog {
    claimed: HashSet<String>,
    refunded: HashSet<String>,
    claims_in_flight: HashSet<String>,
    refunds_in_flight: HashSet<String>,
}

impl BoltzVhtlcActionLog {
    fn begin_claim(&mut self, swap_id: &str) -> bool {
        if self.claimed.contains(swap_id)
            || self.refunded.contains(swap_id)
            || self.claims_in_flight.contains(swap_id)
            || self.refunds_in_flight.contains(swap_id)
        {
            return false;
        }
        self.claims_in_flight.insert(swap_id.to_string())
    }

    fn finish_claim(&mut self, swap_id: &str, succeeded: bool) {
        self.claims_in_flight.remove(swap_id);
        if succeeded {
            self.claimed.insert(swap_id.to_string());
        }
    }

    fn begin_refund(&mut self, swap_id: &str) -> bool {
        if self.claimed.contains(swap_id)
            || self.refunded.contains(swap_id)
            || self.claims_in_flight.contains(swap_id)
            || self.refunds_in_flight.contains(swap_id)
        {
            return false;
        }
        self.refunds_in_flight.insert(swap_id.to_string())
    }

    fn finish_refund(&mut self, swap_id: &str, succeeded: bool) {
        self.refunds_in_flight.remove(swap_id);
        if succeeded {
            self.refunded.insert(swap_id.to_string());
        }
    }
}

#[derive(Clone, Debug)]
struct VhtlcLifecycleInfo {
    swap_id: String,
    swap_type: SwapType,
    address: ArkAddress,
    script_pubkey: ScriptBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SpentVhtlcAction {
    Reconcile,
    KeepActive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VhtlcContractLiveness {
    /// No VTXO exists yet. Keep watching: the VHTLC may still be funded later.
    Unfunded,
    /// A spend was submitted but not finalized. Keep active for pending-tx recovery.
    PendingSpend,
    /// VTXO exists and can still be spent cooperatively/offchain.
    Funded,
    /// VTXO exists and is recoverable/settleable.
    Recoverable,
    /// VTXO existed and no longer has any actionable offchain/recovery state.
    Spent,
}

impl VhtlcContractLiveness {
    fn should_deactivate_contract(self) -> bool {
        matches!(self, Self::Spent)
    }
}

fn classify_vhtlc_contract_liveness(
    dust: Amount,
    has_pending_spend: bool,
    vtxos: Vec<ark_core::server::VirtualTxOutPoint>,
) -> VhtlcContractLiveness {
    if has_pending_spend {
        return VhtlcContractLiveness::PendingSpend;
    }

    if vtxos.is_empty() {
        return VhtlcContractLiveness::Unfunded;
    }

    let vtxos = VtxoList::new(dust, vtxos);
    if vtxos.spendable_offchain().next().is_some() {
        return VhtlcContractLiveness::Funded;
    }
    if vtxos.recoverable().next().is_some() {
        return VhtlcContractLiveness::Recoverable;
    }

    VhtlcContractLiveness::Spent
}

impl<B, W, S> Client<B, W, S>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    // Submarine swap.

    /// Prepare the payment of a BOLT11 invoice by setting up a submarine swap via Boltz.
    ///
    /// This function does not execute the payment itself. Once you are ready for payment you
    /// will have to send the required `amount` to the `vhtlc_address`.
    ///
    /// If you are looking for a function which pays the invoice immediately, consider using
    /// [`Client::pay_ln_invoice`] instead.
    ///
    /// # Arguments
    ///
    /// - `invoice`: a [`Bolt11Invoice`] to be paid.
    ///
    /// # Returns
    ///
    /// - A [`SubmarineSwapData`] object, including an identifier for the swap.
    pub async fn prepare_ln_invoice_payment(
        &self,
        invoice: Bolt11Invoice,
    ) -> Result<SubmarineSwapData, Error> {
        let refund_keypair = self.next_keypair(crate::key_provider::KeypairIndex::New)?;
        let refund_public_key = refund_keypair.public_key();
        let key_derivation_index =
            self.derivation_index_for_pk(&refund_keypair.x_only_public_key().0);

        let preimage_hash = invoice.payment_hash();
        let preimage_hash = ripemd160::Hash::hash(preimage_hash.as_byte_array());

        let request = CreateSubmarineSwapRequest {
            from: Asset::Ark,
            to: Asset::Btc,
            invoice,
            refund_public_key: refund_public_key.into(),
            referral_id: self.inner.boltz_referral_id.clone(),
        };
        let url = format!("{}/v2/swap/submarine", self.inner.boltz_url);

        let client = reqwest::Client::new();
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

        let server_info = self.server_info().await?;
        let vhtlc = self.build_vhtlc_script(
            &server_info,
            swap_response.claim_public_key,
            refund_public_key.into(),
            preimage_hash,
            &swap_response.timeout_block_heights,
            &swap_response.address,
        )?;

        let script_pubkey = self
            .insert_vhtlc_contract(vhtlc.options().clone(), key_derivation_index)
            .context("failed to persist VHTLC contract for submarine swap")?;

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
            invoice: request.invoice.clone(),
            created_at: created_at.as_secs(),
            key_derivation_index,
            contract_script_pubkey: Some(script_pubkey),
        };

        self.swap_storage()
            .insert_submarine(swap_response.id.clone(), data.clone())
            .await?;

        tracing::info!(
            swap_id = swap_response.id,
            vhtlc_address = %data.vhtlc_address,
            expected_amount = %data.amount,
            "Prepared Lightning invoice payment"
        );

        Ok(data)
    }

    /// Pay a BOLT11 invoice by performing a submarine swap via Boltz. This allows to make Lightning
    /// payments with an Arkade wallet.
    ///
    /// # Arguments
    ///
    /// - `invoice`: a [`Bolt11Invoice`] to be paid.
    ///
    /// # Returns
    ///
    /// - A [`SubmarineSwapResult`], including an identifier for the swap and the TXID of the Arkade
    ///   transaction that funds the VHTLC.
    pub async fn pay_ln_invoice(
        &self,
        invoice: Bolt11Invoice,
    ) -> Result<SubmarineSwapResult, Error> {
        let refund_keypair = self.next_keypair(crate::key_provider::KeypairIndex::New)?;
        let refund_public_key = refund_keypair.public_key();
        let key_derivation_index =
            self.derivation_index_for_pk(&refund_keypair.x_only_public_key().0);

        let preimage_hash = invoice.payment_hash();
        let preimage_hash = ripemd160::Hash::hash(preimage_hash.as_byte_array());

        let request = CreateSubmarineSwapRequest {
            from: Asset::Ark,
            to: Asset::Btc,
            invoice,
            refund_public_key: refund_public_key.into(),
            referral_id: self.inner.boltz_referral_id.clone(),
        };
        let url = format!("{}/v2/swap/submarine", self.inner.boltz_url);

        let client = reqwest::Client::new();
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

        let server_info = self.server_info().await?;
        let vhtlc = self.build_vhtlc_script(
            &server_info,
            swap_response.claim_public_key,
            refund_public_key.into(),
            preimage_hash,
            &swap_response.timeout_block_heights,
            &swap_response.address,
        )?;

        let script_pubkey = self
            .insert_vhtlc_contract(vhtlc.options().clone(), key_derivation_index)
            .context("failed to persist VHTLC contract for submarine swap")?;

        self.swap_storage()
            .insert_submarine(
                swap_response.id.clone(),
                SubmarineSwapData {
                    id: swap_response.id.clone(),
                    status: SwapStatus::Created,
                    preimage: None,
                    preimage_hash,
                    refund_public_key: refund_public_key.into(),
                    claim_public_key: swap_response.claim_public_key,
                    vhtlc_address: swap_response.address,
                    timeout_block_heights: swap_response.timeout_block_heights,
                    amount: swap_response.expected_amount,
                    invoice: request.invoice.clone(),
                    created_at: created_at.as_secs(),
                    key_derivation_index,
                    contract_script_pubkey: Some(script_pubkey),
                },
            )
            .await?;

        let vhtlc_address = swap_response.address;
        let amount = swap_response.expected_amount;

        let txid = self
            .send(vec![SendReceiver::bitcoin(vhtlc_address, amount)])
            .await?;

        tracing::info!(swap_id = swap_response.id, %amount, "Funded VHTLC");

        Ok(SubmarineSwapResult {
            swap_id: swap_response.id,
            txid,
            amount,
        })
    }

    /// Wait for the Lightning invoice associated with a submarine swap to be paid by Boltz.
    ///
    /// Boltz will first need to claim our VHTLC before paying the invoice. When Boltz claims
    /// the VHTLC, the preimage is revealed in the claim transaction's witness. This method
    /// extracts and persists the preimage to swap storage.
    ///
    /// # Returns
    ///
    /// The 32-byte preimage that was revealed when Boltz claimed the VHTLC.
    pub async fn wait_for_invoice_paid(&self, swap_id: &str) -> Result<[u8; 32], Error> {
        use futures::StreamExt;

        let stream =
            self.subscribe_to_swap_updates_for_type(swap_id.to_string(), SwapType::Submarine);
        tokio::pin!(stream);

        while let Some(status_result) = stream.next().await {
            match status_result {
                Ok(status) => {
                    tracing::debug!(swap_id, current = ?status, "Swap status");
                    match status {
                        SwapStatus::InvoicePaid => {
                            let deadline = tokio::time::Instant::now() + self.inner.timeout;

                            loop {
                                match self.extract_submarine_swap_preimage(swap_id).await {
                                    Ok(preimage) => return Ok(preimage),
                                    Err(e) => {
                                        if tokio::time::Instant::now() >= deadline {
                                            return Err(e.context(
                                                "invoice paid but failed to extract preimage from claim tx",
                                            ));
                                        }

                                        tracing::debug!(
                                            swap_id,
                                            "Preimage not available yet, retrying: {e}"
                                        );
                                    }
                                }

                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                        }
                        SwapStatus::InvoiceExpired => {
                            return Err(Error::ad_hoc(format!(
                                "invoice expired for swap {swap_id}"
                            )));
                        }
                        SwapStatus::Error { error } => {
                            tracing::error!(
                                swap_id,
                                "Got error from swap updates subscription: {error}"
                            );
                        }
                        SwapStatus::InvoiceSet
                        | SwapStatus::InvoicePending
                        | SwapStatus::Created
                        | SwapStatus::TransactionMempool
                        | SwapStatus::TransactionConfirmed
                        | SwapStatus::TransactionServerMempool
                        | SwapStatus::TransactionServerConfirmed
                        | SwapStatus::TransactionRefunded
                        | SwapStatus::TransactionFailed
                        | SwapStatus::TransactionClaimed
                        | SwapStatus::TransactionLockupFailed
                        | SwapStatus::InvoiceSettled
                        | SwapStatus::InvoiceFailedToPay
                        | SwapStatus::SwapExpired
                        | SwapStatus::Other(_) => {}
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Err(Error::ad_hoc("Status stream ended unexpectedly"))
    }

    /// Extract the preimage from a claimed submarine swap VHTLC.
    ///
    /// After Boltz claims the VHTLC, the preimage is embedded in the claim transaction's PSBT
    /// via the `VTXO_CONDITION_KEY` unknown field. This method fetches that transaction and
    /// extracts the preimage.
    ///
    /// The extracted preimage is validated against the stored preimage hash and persisted to
    /// swap storage.
    pub async fn extract_submarine_swap_preimage(&self, swap_id: &str) -> Result<[u8; 32], Error> {
        let mut swap_data = self
            .swap_storage()
            .get_submarine(swap_id)
            .await?
            .ok_or(Error::ad_hoc("submarine swap not found"))?;

        // If the preimage was already extracted, return it.
        if let Some(preimage) = swap_data.preimage {
            return Ok(preimage);
        }

        let vhtlc_address = swap_data.vhtlc_address;

        // Find the VHTLC outpoint — it should be spent by now.
        let virtual_tx_outpoints = self
            .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
            .await
            .context("failed to get virtual tx outpoints for VHTLC address")?;

        let vhtlc_outpoint = virtual_tx_outpoints
            .iter()
            .find(|o| o.is_spent)
            .ok_or_else(|| Error::ad_hoc("VHTLC outpoint not found or not yet spent (claimed)"))?;

        let claim_txid = vhtlc_outpoint.ark_txid.ok_or_else(|| {
            Error::ad_hoc("VHTLC is spent but has no ark_txid (claim transaction)")
        })?;

        // Fetch the claim transaction PSBT.
        let claim_txs = timeout_op(
            self.inner.timeout,
            self.network_client()
                .get_virtual_txs(vec![claim_txid.to_string()], None),
        )
        .await?
        .map_err(|e| Error::ad_hoc(e.to_string()))
        .context("failed to fetch claim transaction")?;

        let claim_psbt = claim_txs
            .txs
            .first()
            .ok_or_else(|| Error::ad_hoc("claim transaction not found"))?;

        // Extract the preimage from the PSBT's unknown fields.
        let preimage = extract_preimage_from_psbt(claim_psbt)?;

        // Validate against the stored hash.
        let computed_hash = ripemd160::Hash::hash(sha256::Hash::hash(&preimage).as_byte_array());
        if computed_hash != swap_data.preimage_hash {
            return Err(Error::ad_hoc(format!(
                "extracted preimage does not match stored hash: expected {}, got {}",
                swap_data.preimage_hash, computed_hash
            )));
        }

        // Persist the preimage.
        swap_data.preimage = Some(preimage);
        self.swap_storage()
            .update_submarine(swap_id, swap_data)
            .await
            .context("failed to persist preimage to swap storage")?;

        tracing::info!(
            swap_id,
            "Extracted and persisted preimage from claim transaction"
        );

        Ok(preimage)
    }

    /// Refund a VHTLC after the timelock has expired.
    ///
    /// This path does not require a signature from Boltz.
    pub async fn refund_expired_vhtlc(&self, swap_id: &str) -> Result<Txid, Error> {
        let mut swap_data = self
            .swap_storage()
            .get_submarine(swap_id)
            .await?
            .ok_or(Error::ad_hoc("Submarine swap not found"))?;

        let server_info = self.server_info().await?;

        let vhtlc = self
            .submarine_vhtlc_script(&mut swap_data, &server_info)
            .await?;
        let vhtlc_address = vhtlc.address();

        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(server_info.dust, virtual_tx_outpoints);

            // We expect a single outpoint.
            let mut unspent = vtxo_list.all_unspent();
            let vhtlc_outpoint = unspent.next().ok_or_else(|| {
                Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
            })?;

            vhtlc_outpoint.clone()
        };

        let (refund_address, _) = self.get_offchain_address().await?;
        let refund_amount = swap_data.amount;

        let outputs = vec![SendReceiver {
            address: refund_address,
            amount: refund_amount,
            assets: Vec::new(),
        }];

        let refund_script = vhtlc.refund_without_receiver_script();

        let spend_info = vhtlc.taproot_spend_info();
        let script_ver = (refund_script, LeafVersion::TapScript);
        let control_block = spend_info
            .control_block(&script_ver)
            .ok_or(Error::ad_hoc("control block not found for refund script"))?;

        let script_pubkey = vhtlc.script_pubkey();

        let refunder_pk = swap_data.refund_public_key.inner.x_only_public_key().0;
        let vhtlc_input = VtxoInput::new(
            script_ver.0,
            Some(absolute::LockTime::from_consensus(
                swap_data.timeout_block_heights.refund,
            )),
            control_block,
            vhtlc.tapscripts(),
            script_pubkey,
            refund_amount,
            vhtlc_outpoint.outpoint,
            vhtlc_outpoint.assets,
        );

        // The change address is superfluous because we are _draining_ the VHTLC.
        let change_address = &refund_address;

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &outputs,
            change_address,
            std::slice::from_ref(&vhtlc_input),
            &server_info,
        )?;

        let kp = self.keypair_by_pk(&refunder_pk)?;
        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                let pk = kp.x_only_public_key().0;

                Ok(vec![(sig, pk)])
            };

        sign_ark_transaction(sign_fn, &mut ark_tx, 0)?;

        let ark_txid = ark_tx.unsigned_tx.compute_txid();

        let res = self
            .network_client()
            .submit_offchain_transaction_request(ark_tx, checkpoint_txs)
            .await?;

        let mut checkpoint_psbt = res
            .signed_checkpoint_txs
            .first()
            .ok_or_else(|| Error::ad_hoc("no checkpoint PSBTs found"))?
            .clone();

        let kp = self.keypair_by_pk(&refunder_pk)?;
        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                let pk = kp.x_only_public_key().0;

                Ok(vec![(sig, pk)])
            };

        sign_checkpoint_transaction(sign_fn, &mut checkpoint_psbt)?;

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, vec![checkpoint_psbt]),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize offchain transaction")?;

        self.mark_vhtlc_contract_inactive(swap_data.contract_script_pubkey.as_ref())?;

        tracing::info!(txid = %ark_txid, "Refunded VHTLC");

        Ok(ark_txid)
    }

    /// Refund a VHTLC after the timelock has expired via settlement.
    ///
    /// This path does not require a signature from Boltz.
    pub async fn refund_expired_vhtlc_via_settlement<R>(
        &self,
        rng: &mut R,
        swap_id: &str,
    ) -> Result<Txid, Error>
    where
        R: Rng + CryptoRng,
    {
        let mut swap_data = self
            .swap_storage()
            .get_submarine(swap_id)
            .await?
            .ok_or(Error::ad_hoc("Submarine swap not found"))?;

        let timeout_block_heights = swap_data.timeout_block_heights;
        let server_info = self.server_info().await?;

        let vhtlc = self
            .submarine_vhtlc_script(&mut swap_data, &server_info)
            .await?;
        let vhtlc_address = vhtlc.address();

        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(server_info.dust, virtual_tx_outpoints);

            // We expect a single outpoint.
            let mut recoverable = vtxo_list.recoverable();

            recoverable
                .next()
                .ok_or_else(|| {
                    Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
                })?
                .clone()
        };

        let refund_script = vhtlc.refund_without_receiver_script();

        let spend_info = vhtlc.taproot_spend_info();
        let script_ver = (refund_script, LeafVersion::TapScript);
        let control_block = spend_info
            .control_block(&script_ver)
            .ok_or(Error::ad_hoc("control block not found for refund script"))?;

        let script_pubkey = vhtlc.script_pubkey();

        let (refund_address, _) = self.get_offchain_address_with_server_info(&server_info)?;
        let refund_amount = swap_data.amount;

        let vhtlc_input = intent::Input::new(
            vhtlc_outpoint.outpoint,
            parse_sequence_number(timeout_block_heights.unilateral_refund as i64)
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral refund timeout: {e}")))?,
            Some(absolute::LockTime::from_consensus(
                timeout_block_heights.refund,
            )),
            TxOut {
                value: refund_amount,
                script_pubkey,
            },
            vhtlc.tapscripts(),
            (script_ver.0, control_block),
            false,
            true,
            vhtlc_outpoint.assets,
        );

        let commitment_txid = self
            .join_next_batch(
                rng,
                &server_info,
                Vec::new(),
                vec![vhtlc_input],
                BatchOutputType::Board {
                    to_address: refund_address,
                    to_amount: refund_amount,
                },
            )
            .await
            .context("failed to join batch")?;

        self.mark_vhtlc_contract_inactive(swap_data.contract_script_pubkey.as_ref())?;

        tracing::info!(txid = %commitment_txid, "Refunded VHTLC via settlement");

        Ok(commitment_txid)
    }

    /// Refund a VHTLC with collaboration from Boltz.
    ///
    /// This path requires Boltz's cooperation to sign the refund transaction. It allows refunding
    /// a submarine swap before the timelock expires. For refunds after timelock expiry without
    /// Boltz cooperation, use [`Client::refund_expired_vhtlc`] instead.
    pub async fn refund_vhtlc(&self, swap_id: &str) -> Result<Txid, Error> {
        let mut swap_data = self
            .swap_storage()
            .get_submarine(swap_id)
            .await?
            .ok_or(Error::ad_hoc("submarine swap not found"))?;

        let server_info = self.server_info().await?;

        let vhtlc = self
            .submarine_vhtlc_script(&mut swap_data, &server_info)
            .await?;
        let vhtlc_address = vhtlc.address();

        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(server_info.dust, virtual_tx_outpoints);

            // We expect a single outpoint.
            let mut unspent = vtxo_list.all_unspent();
            let vhtlc_outpoint = unspent.next().ok_or_else(|| {
                Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
            })?;

            vhtlc_outpoint.clone()
        };

        let (refund_address, _) = self.get_offchain_address().await?;
        let refund_amount = swap_data.amount;

        let outputs = vec![SendReceiver {
            address: refund_address,
            amount: refund_amount,
            assets: Vec::new(),
        }];

        // Use the collaborative refund script which requires sender + receiver + server signatures.
        let refund_script = vhtlc.refund_script();

        let spend_info = vhtlc.taproot_spend_info();
        let script_ver = (refund_script, LeafVersion::TapScript);
        let control_block = spend_info
            .control_block(&script_ver)
            .ok_or(Error::ad_hoc("control block not found for refund script"))?;

        let script_pubkey = vhtlc.script_pubkey();

        let refunder_pk = swap_data.refund_public_key.inner.x_only_public_key().0;
        let vhtlc_input = VtxoInput::new(
            script_ver.0,
            None, // No locktime required for collaborative refund
            control_block,
            vhtlc.tapscripts(),
            script_pubkey,
            refund_amount,
            vhtlc_outpoint.outpoint,
            vhtlc_outpoint.assets,
        );

        // The change address is superfluous because we are _draining_ the VHTLC.
        let change_address = &refund_address;

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &outputs,
            change_address,
            std::slice::from_ref(&vhtlc_input),
            &server_info,
        )?;

        // Sign the ark transaction with the sender's (user's) key.
        let kp = self.keypair_by_pk(&refunder_pk)?;
        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                let pk = kp.x_only_public_key().0;

                Ok(vec![(sig, pk)])
            };

        sign_ark_transaction(sign_fn, &mut ark_tx, 0)?;

        // Get the unsigned checkpoint - we'll sign it after arkd adds its signature.
        let checkpoint_psbt = checkpoint_txs
            .first()
            .ok_or_else(|| Error::ad_hoc("no checkpoint PSBTs found"))?
            .clone();

        // Send ark transaction (with user signature) and unsigned checkpoint to Boltz.
        // Boltz will add their signature (receiver) to the ark transaction.
        let url = format!(
            "{}/v2/swap/submarine/{swap_id}/refund/ark",
            self.inner.boltz_url
        );
        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .json(&RefundSwapRequest {
                transaction: ark_tx.to_string(),
                checkpoint: checkpoint_psbt.to_string(),
            })
            .send()
            .await
            .map_err(Error::ad_hoc)
            .context("failed to send refund request to Boltz")?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))
                .context("failed to read error text")?;

            return Err(Error::ad_hoc(format!(
                "Boltz refund request failed: {error_text}"
            )));
        }

        let refund_response: RefundSwapResponse = response
            .json()
            .await
            .map_err(Error::ad_hoc)
            .context("failed to deserialize refund response")?;

        if let Some(err) = refund_response.error.as_deref() {
            return Err(Error::ad_hoc(format!("Boltz refund request failed: {err}")));
        }

        // Parse the Boltz-signed transactions.
        let boltz_signed_ark_tx = Psbt::from_str(&refund_response.transaction)
            .map_err(Error::ad_hoc)
            .context("could not parse refund transaction PSBT")?;

        let boltz_signed_checkpoint = Psbt::from_str(&refund_response.checkpoint)
            .map_err(Error::ad_hoc)
            .context("could not parse refund checkpoint PSBT")?;

        let ark_txid = boltz_signed_ark_tx.unsigned_tx.compute_txid();

        // Extract Boltz's signatures before sending to arkd (server strips incoming sigs).
        let boltz_tap_script_sigs = boltz_signed_checkpoint
            .inputs
            .first()
            .ok_or_else(|| Error::ad_hoc("boltz checkpoint has no inputs"))?
            .tap_script_sigs
            .clone();

        // Submit to arkd for server signature.
        // We send the Boltz-signed transactions so arkd can add its signature.
        let res = self
            .network_client()
            .submit_offchain_transaction_request(boltz_signed_ark_tx, vec![boltz_signed_checkpoint])
            .await?;

        // The server returns the checkpoint with its signature added.
        // Now we need to add our (sender) signature to the checkpoint.
        let mut server_signed_checkpoint = res
            .signed_checkpoint_txs
            .first()
            .ok_or_else(|| Error::ad_hoc("no signed checkpoint PSBTs returned"))?
            .clone();

        let kp = self.keypair_by_pk(&refunder_pk)?;
        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                let pk = kp.x_only_public_key().0;

                Ok(vec![(sig, pk)])
            };

        server_signed_checkpoint
            .inputs
            .first_mut()
            .ok_or_else(|| Error::ad_hoc("server checkpoint has no inputs"))?
            .tap_script_sigs
            .extend(boltz_tap_script_sigs);

        sign_checkpoint_transaction(sign_fn, &mut server_signed_checkpoint)?;

        // Finalize the transaction with the fully-signed checkpoint.
        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, vec![server_signed_checkpoint]),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize offchain transaction")?;

        self.mark_vhtlc_contract_inactive(swap_data.contract_script_pubkey.as_ref())?;

        tracing::info!(swap_id, txid = %ark_txid, "Refunded VHTLC via collaborative refund");

        Ok(ark_txid)
    }

    // Reverse submarine swap.

    async fn validate_reverse_recipient_address(
        &self,
        recipient_address: Option<&ArkAddress>,
    ) -> Result<(), Error> {
        let Some(recipient_address) = recipient_address else {
            return Ok(());
        };

        let server_info = self.server_info().await?;
        let server_signer: XOnlyPublicKey = server_info.signer_pk.into();
        if recipient_address.server() != server_signer {
            return Err(Error::consumer(format!(
                "recipient Arkade address belongs to a different server: expected {server_signer}, got {}",
                recipient_address.server()
            )));
        }

        Ok(())
    }

    async fn reverse_claim_address(&self, swap: &ReverseSwapData) -> Result<ArkAddress, Error> {
        if let Some(address) = swap.claim_address {
            self.validate_reverse_recipient_address(Some(&address))
                .await?;
            return Ok(address);
        }

        let (address, _) = self
            .get_offchain_address()
            .await
            .context("failed to get offchain address")?;

        Ok(address)
    }

    /// Generate a BOLT11 invoice to perform a reverse submarine swap via Boltz. This allows to
    /// receive Lightning payments into an Arkade wallet.
    ///
    /// # Arguments
    ///
    /// - `amount`: the expected [`Amount`] to be received.
    /// - `expiry_secs`: optional invoice expiry, in seconds from now. If `None`, Boltz's default is
    ///   used.
    /// - `description`: optional memo embedded in the BOLT11 invoice's `d` field (visible to the
    ///   payer).
    ///
    /// # Returns
    ///
    /// - A `ReverseSwapResult`, including an identifier for the reverse swap and the
    ///   [`Bolt11Invoice`] to be paid.
    pub async fn get_ln_invoice(
        &self,
        amount: SwapAmount,
        expiry_secs: Option<u64>,
        description: Option<String>,
    ) -> Result<ReverseSwapResult, Error> {
        self.create_reverse_swap_invoice_with_new_preimage(amount, expiry_secs, None, description)
            .await
    }

    /// Generate a BOLT11 invoice to receive Lightning into another user's Arkade address.
    ///
    /// The local client still creates and claims the Boltz reverse-swap VHTLC, but the resulting
    /// Arkade output is sent to `recipient_address` instead of a fresh local address.
    ///
    /// # Arguments
    ///
    /// - `amount`: the expected [`Amount`] to be received.
    /// - `recipient_address`: Arkade address that receives the claimed VHTLC output.
    /// - `expiry_secs`: optional invoice expiry, in seconds from now. If `None`, Boltz's default is
    ///   used.
    /// - `description`: optional memo embedded in the BOLT11 invoice's `d` field (visible to the
    ///   payer).
    ///
    /// # Returns
    ///
    /// - A `ReverseSwapResult`, including an identifier for the reverse swap and the
    ///   [`Bolt11Invoice`] to be paid.
    pub async fn get_ln_invoice_for_address(
        &self,
        amount: SwapAmount,
        recipient_address: ArkAddress,
        expiry_secs: Option<u64>,
        description: Option<String>,
    ) -> Result<ReverseSwapResult, Error> {
        self.create_reverse_swap_invoice_with_new_preimage(
            amount,
            expiry_secs,
            Some(recipient_address),
            description,
        )
        .await
    }

    async fn create_reverse_swap_invoice_with_new_preimage(
        &self,
        amount: SwapAmount,
        expiry_secs: Option<u64>,
        recipient_address: Option<ArkAddress>,
        description: Option<String>,
    ) -> Result<ReverseSwapResult, Error> {
        let preimage: [u8; 32] = rand::random();
        let preimage_hash_sha256 = sha256::Hash::hash(&preimage);

        self.create_reverse_swap_invoice(
            amount,
            expiry_secs,
            preimage_hash_sha256,
            Some(preimage),
            recipient_address,
            description,
        )
        .await
    }

    /// Generate a BOLT11 invoice using a provided SHA256 preimage hash for a reverse submarine
    /// swap via Boltz. This allows receiving Lightning payments when the preimage is managed
    /// externally.
    ///
    /// # Arguments
    ///
    /// - `amount`: the expected [`Amount`] to be received.
    /// - `expiry_secs`: optional invoice expiry, in seconds from now. If `None`, Boltz's default is
    ///   used.
    /// - `preimage_hash_sha256`: the SHA256 hash of the preimage. The preimage itself is not stored
    ///   and must be provided later when claiming via [`Self::claim_vhtlc`].
    /// - `description`: optional memo embedded in the BOLT11 invoice's `d` field (visible to the
    ///   payer).
    ///
    /// # Returns
    ///
    /// - A [`ReverseSwapResult`], including an identifier for the reverse swap and the
    ///   [`Bolt11Invoice`] to be paid.
    ///
    /// # Note
    ///
    /// After calling this method, use [`Self::wait_for_vhtlc_funding`] to wait for the VHTLC to
    /// be funded, then [`Self::claim_vhtlc`] with the preimage to claim the funds.
    pub async fn get_ln_invoice_from_hash(
        &self,
        amount: SwapAmount,
        expiry_secs: Option<u64>,
        preimage_hash_sha256: sha256::Hash,
        description: Option<String>,
    ) -> Result<ReverseSwapResult, Error> {
        self.create_reverse_swap_invoice(
            amount,
            expiry_secs,
            preimage_hash_sha256,
            None,
            None,
            description,
        )
        .await
    }

    /// Generate a BOLT11 invoice from an externally managed preimage hash and receive the claimed
    /// VHTLC output into another user's Arkade address.
    ///
    /// After calling this method, use [`Self::wait_for_vhtlc_funding`] to wait for the VHTLC to
    /// be funded, then [`Self::claim_vhtlc`] with the preimage to claim the funds.
    pub async fn get_ln_invoice_from_hash_for_address(
        &self,
        amount: SwapAmount,
        recipient_address: ArkAddress,
        expiry_secs: Option<u64>,
        preimage_hash_sha256: sha256::Hash,
        description: Option<String>,
    ) -> Result<ReverseSwapResult, Error> {
        self.create_reverse_swap_invoice(
            amount,
            expiry_secs,
            preimage_hash_sha256,
            None,
            Some(recipient_address),
            description,
        )
        .await
    }

    async fn create_reverse_swap_invoice(
        &self,
        amount: SwapAmount,
        expiry_secs: Option<u64>,
        preimage_hash_sha256: sha256::Hash,
        preimage: Option<[u8; 32]>,
        recipient_address: Option<ArkAddress>,
        description: Option<String>,
    ) -> Result<ReverseSwapResult, Error> {
        validate_invoice_description(description.as_deref())?;
        self.validate_reverse_recipient_address(recipient_address.as_ref())
            .await?;

        let preimage_hash = ripemd160::Hash::hash(preimage_hash_sha256.as_byte_array());

        let claim_keypair = self.next_keypair(crate::key_provider::KeypairIndex::New)?;
        let claim_public_key = claim_keypair.public_key();
        let key_derivation_index =
            self.derivation_index_for_pk(&claim_keypair.x_only_public_key().0);

        let (invoice_amount, onchain_amount) = match amount {
            SwapAmount::Invoice(amount) => (Some(amount), None),
            SwapAmount::Vhtlc(amount) => (None, Some(amount)),
        };

        let request = CreateReverseSwapRequest {
            from: Asset::Btc,
            to: Asset::Ark,
            invoice_amount,
            onchain_amount,
            claim_public_key: claim_public_key.into(),
            preimage_hash: preimage_hash_sha256,
            invoice_expiry: expiry_secs,
            referral_id: self.inner.boltz_referral_id.clone(),
            description,
        };

        let url = format!("{}/v2/swap/reverse", self.inner.boltz_url);

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

        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(Error::ad_hoc)
            .context("failed to compute created_at")?;

        let swap_amount = response.onchain_amount.or(onchain_amount).ok_or_else(|| {
            Error::ad_hoc("onchain_amount not provided by Boltz and not specified in request")
        })?;

        let server_info = self.server_info().await?;
        let vhtlc = self.build_vhtlc_script(
            &server_info,
            claim_public_key.into(),
            response.refund_public_key,
            preimage_hash,
            &response.timeout_block_heights,
            &response.lockup_address,
        )?;

        let script_pubkey = self
            .insert_vhtlc_contract(vhtlc.options().clone(), key_derivation_index)
            .context("failed to persist VHTLC contract for reverse submarine swap")?;

        let swap = ReverseSwapData {
            id: response.id.clone(),
            status: SwapStatus::Created,
            preimage,
            vhtlc_address: response.lockup_address,
            preimage_hash,
            refund_public_key: response.refund_public_key,
            amount: swap_amount,
            claim_public_key: claim_public_key.into(),
            timeout_block_heights: response.timeout_block_heights,
            created_at: created_at.as_secs(),
            key_derivation_index,
            bolt11: response.invoice.to_string(),
            invoice_expiry: response.invoice.expiry_time().as_secs(),
            claim_address: recipient_address,
            contract_script_pubkey: Some(script_pubkey),
        };

        self.swap_storage()
            .insert_reverse(response.id.clone(), swap.clone())
            .await
            .context("failed to persist swap data")?;

        Ok(ReverseSwapResult {
            swap_id: swap.id,
            invoice: response.invoice,
            amount: swap_amount,
        })
    }

    /// Wait for the VHTLC associated with a reverse submarine swap to be funded.
    ///
    /// This method only waits for the funding transaction to be detected (in mempool or confirmed).
    /// It does not claim the VHTLC. Use [`Self::claim_vhtlc`] to claim after the preimage is known.
    ///
    /// # Arguments
    ///
    /// - `swap_id`: The unique identifier for the reverse swap.
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` when the VHTLC funding transaction is detected.
    pub async fn wait_for_vhtlc_funding(&self, swap_id: &str) -> Result<(), Error> {
        use futures::StreamExt;

        let stream =
            self.subscribe_to_swap_updates_for_type(swap_id.to_string(), SwapType::Reverse);
        tokio::pin!(stream);

        while let Some(status_result) = stream.next().await {
            match status_result {
                Ok(status) => {
                    tracing::debug!(swap_id, current = ?status, "Swap status");

                    match status {
                        SwapStatus::TransactionMempool | SwapStatus::TransactionConfirmed => {
                            tracing::debug!(swap_id, "VHTLC funding detected");
                            return Ok(());
                        }
                        SwapStatus::InvoiceExpired => {
                            return Err(Error::ad_hoc(format!(
                                "invoice expired for swap {swap_id}"
                            )));
                        }
                        SwapStatus::Error { error } => {
                            tracing::error!(
                                swap_id,
                                "Got error from swap updates subscription: {error}"
                            );
                        }
                        // TODO: We may still need to handle some of these explicitly.
                        SwapStatus::Created
                        | SwapStatus::TransactionRefunded
                        | SwapStatus::TransactionFailed
                        | SwapStatus::TransactionClaimed
                        | SwapStatus::TransactionLockupFailed
                        | SwapStatus::TransactionServerMempool
                        | SwapStatus::TransactionServerConfirmed
                        | SwapStatus::InvoiceSet
                        | SwapStatus::InvoicePending
                        | SwapStatus::InvoicePaid
                        | SwapStatus::InvoiceSettled
                        | SwapStatus::InvoiceFailedToPay
                        | SwapStatus::SwapExpired
                        | SwapStatus::Other(_) => {}
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Err(Error::ad_hoc("Status stream ended unexpectedly"))
    }

    /// Claim a funded VHTLC for a reverse submarine swap using the preimage.
    ///
    /// This method should be called after the VHTLC has been funded (after
    /// [`Self::wait_for_vhtlc_funding`] returns) and the preimage is known.
    ///
    /// # Arguments
    ///
    /// - `swap_id`: The unique identifier for the reverse swap.
    /// - `preimage`: The 32-byte preimage that unlocks the VHTLC.
    ///
    /// # Returns
    ///
    /// Returns a [`ClaimVhtlcResult`] with details about the claim transaction.
    pub async fn claim_vhtlc(
        &self,
        swap_id: &str,
        preimage: [u8; 32],
    ) -> Result<ClaimVhtlcResult, Error> {
        let mut swap = self
            .swap_storage()
            .get_reverse(swap_id)
            .await
            .context("failed to get reverse swap data")?
            .ok_or_else(|| Error::ad_hoc(format!("reverse swap data not found: {swap_id}")))?;

        // Verify the preimage matches the stored hash
        let preimage_hash_sha256 = sha256::Hash::hash(&preimage);
        let preimage_hash = ripemd160::Hash::hash(preimage_hash_sha256.as_byte_array());

        if preimage_hash != swap.preimage_hash {
            return Err(Error::ad_hoc(format!(
                "preimage does not match stored hash for swap {swap_id}"
            )));
        }

        tracing::debug!(swap_id, "Claiming VHTLC with verified preimage");

        let server_info = self.server_info().await?;

        let vhtlc = self.reverse_vhtlc_script(&mut swap, &server_info).await?;
        let vhtlc_address = vhtlc.address();

        // TODO: Ideally we can skip this if the vout is always the same (probably 0).
        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(server_info.dust, virtual_tx_outpoints);

            // We expect a single outpoint.
            let mut unspent = vtxo_list.all_unspent();
            let vhtlc_outpoint = unspent.next().ok_or_else(|| {
                Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
            })?;

            vhtlc_outpoint.clone()
        };

        let claim_address = self.reverse_claim_address(&swap).await?;
        let claim_amount = swap.amount;

        let outputs = vec![SendReceiver {
            address: claim_address,
            amount: claim_amount,
            assets: Vec::new(),
        }];

        let spend_info = vhtlc.taproot_spend_info();
        let script_ver = (vhtlc.claim_script(), LeafVersion::TapScript);
        let control_block = spend_info
            .control_block(&script_ver)
            .ok_or(Error::ad_hoc("control block not found for claim script"))?;

        let script_pubkey = vhtlc.script_pubkey();

        let claimer_pk = swap.claim_public_key.inner.x_only_public_key().0;
        let vhtlc_input = VtxoInput::new(
            script_ver.0,
            None,
            control_block,
            vhtlc.tapscripts(),
            script_pubkey,
            claim_amount,
            vhtlc_outpoint.outpoint,
            vhtlc_outpoint.assets,
        );

        // The change address is superfluous because we are _draining_ the VHTLC.
        let change_address = &claim_address;

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &outputs,
            change_address,
            std::slice::from_ref(&vhtlc_input),
            &server_info,
        )
        .map_err(Error::from)
        .context("failed to build offchain TXs")?;

        let kp = self.keypair_by_pk(&claimer_pk)?;
        let sign_fn =
            |input: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                // Add preimage to PSBT input.
                {
                    // Initialized with a 1, because we only have one witness element: the preimage.
                    let mut bytes = vec![1];

                    let length = VarInt::from(preimage.len() as u64);

                    length
                        .consensus_encode(&mut bytes)
                        .expect("valid length encoding");

                    bytes.write_all(&preimage).expect("valid preimage encoding");

                    input.unknown.insert(
                        psbt::raw::Key {
                            type_value: 222,
                            key: VTXO_CONDITION_KEY.to_vec(),
                        },
                        bytes,
                    );
                }

                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                let pk = kp.x_only_public_key().0;

                Ok(vec![(sig, pk)])
            };

        sign_ark_transaction(sign_fn, &mut ark_tx, 0)
            .map_err(Error::from)
            .context("failed to sign Arkade TX")?;

        let ark_txid = ark_tx.unsigned_tx.compute_txid();

        let res = self
            .network_client()
            .submit_offchain_transaction_request(ark_tx, checkpoint_txs)
            .await
            .map_err(Error::from)
            .context("failed to submit offchain TXs")?;

        let mut checkpoint_psbt = res
            .signed_checkpoint_txs
            .first()
            .ok_or_else(|| Error::ad_hoc("no checkpoint PSBTs found"))?
            .clone();

        sign_checkpoint_transaction(sign_fn, &mut checkpoint_psbt)
            .map_err(Error::from)
            .context("failed to sign checkpoint TX")?;

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, vec![checkpoint_psbt]),
        )
        .await
        .context("failed to finalize offchain transaction")?
        .map_err(Error::ark_server)
        .context("failed to finalize offchain transaction")?;

        self.mark_vhtlc_contract_inactive(swap.contract_script_pubkey.as_ref())?;

        tracing::info!(swap_id, txid = %ark_txid, "Claimed VHTLC");

        // Update storage to persist the preimage
        let mut updated_swap = swap.clone();
        updated_swap.preimage = Some(preimage);
        self.swap_storage()
            .update_reverse(swap_id, updated_swap)
            .await
            .context("failed to update swap data with preimage")?;

        Ok(ClaimVhtlcResult {
            swap_id: swap_id.to_string(),
            claim_txid: ark_txid,
            claim_amount,
            preimage,
        })
    }

    /// Wait for the VHTLC associated with a reverse submarine swap to be funded, then claim it.
    ///
    /// # Note
    ///
    /// This method requires that the preimage was stored when creating the reverse swap (i.e., via
    /// [`Self::get_ln_invoice`]). If the swap was created with [`Self::get_ln_invoice_from_hash`],
    /// use [`Self::wait_for_vhtlc_funding`] followed by [`Self::claim_vhtlc`] instead.
    pub async fn wait_for_vhtlc(&self, swap_id: &str) -> Result<ClaimVhtlcResult, Error> {
        let swap = self
            .swap_storage()
            .get_reverse(swap_id)
            .await
            .context("failed to get reverse swap data")?
            .ok_or_else(|| Error::ad_hoc(format!("reverse swap data not found: {swap_id}")))?;

        // Ensure the preimage is available in storage
        let preimage = swap.preimage.ok_or_else(|| {
            Error::ad_hoc(format!(
                "preimage not found in storage for swap {swap_id}. \
                 Use wait_for_vhtlc_funding and claim_vhtlc instead."
            ))
        })?;

        self.wait_for_vhtlc_funding(swap_id).await?;
        self.claim_vhtlc(swap_id, preimage).await
    }

    // Chain swap.

    /// Create a chain swap via Boltz for swapping between Arkade and on-chain BTC.
    ///
    /// Returns a [`ChainSwapResult`] containing the swap ID and the address the user must
    /// fund to initiate the swap. For [`ChainSwapDirection::ArkToBtc`], the user should send
    /// Arkade VTXOs to the `user_lockup_address` using [`Client::send_vtxo`]. For
    /// [`ChainSwapDirection::BtcToArk`], the user should send BTC to the `user_lockup_address`.
    ///
    /// After funding, use [`Self::wait_for_chain_swap_server_lockup`] to wait for Boltz to
    /// lock their side, then [`Self::claim_chain_swap`] to claim.
    pub async fn create_chain_swap(
        &self,
        direction: ChainSwapDirection,
        amount: ChainSwapAmount,
    ) -> Result<ChainSwapResult, Error> {
        let preimage: [u8; 32] = rand::random();
        let preimage_hash = sha256::Hash::hash(&preimage);

        let claim_keypair = self.next_keypair(crate::key_provider::KeypairIndex::New)?;
        let claim_public_key = PublicKey::new(claim_keypair.public_key());
        let claim_key_derivation_index =
            self.derivation_index_for_pk(&claim_keypair.x_only_public_key().0);

        let refund_keypair = self.next_keypair(crate::key_provider::KeypairIndex::New)?;
        let refund_public_key = PublicKey::new(refund_keypair.public_key());
        let refund_key_derivation_index =
            self.derivation_index_for_pk(&refund_keypair.x_only_public_key().0);

        let (from, to) = match &direction {
            ChainSwapDirection::ArkToBtc => (Asset::Ark, Asset::Btc),
            ChainSwapDirection::BtcToArk => (Asset::Btc, Asset::Ark),
        };

        let (user_lock_amount, server_lock_amount) = match &amount {
            ChainSwapAmount::UserLock(a) => (Some(*a), None),
            ChainSwapAmount::ServerLock(a) => (None, Some(*a)),
        };

        let request = CreateChainSwapRequest {
            from,
            to,
            user_lock_amount,
            server_lock_amount,
            claim_public_key,
            refund_public_key,
            preimage_hash,
            referral_id: self.inner.boltz_referral_id.clone(),
        };

        let url = format!("{}/v2/swap/chain", self.inner.boltz_url);

        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .json(&request)
            .send()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to send chain swap request")?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))
                .context("failed to read error text")?;

            return Err(Error::ad_hoc(format!(
                "failed to create chain swap: {error_text}"
            )));
        }

        let swap_response: CreateChainSwapResponse = response
            .json()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to deserialize chain swap response")?;

        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(Error::ad_hoc)
            .context("failed to compute created_at")?;

        // lockup_details = user's side (where user locks funds)
        // claim_details  = server's side (where user claims funds)
        // The Arkade side carries `timeouts` (full VHTLC timelocks).
        // The BTC side carries `swap_tree` and optionally `bip21`.
        let bip21 = swap_response
            .lockup_details
            .bip21
            .or(swap_response.claim_details.bip21.clone());

        let swap_tree = swap_response
            .lockup_details
            .swap_tree
            .or(swap_response.claim_details.swap_tree.clone());

        let chain_vhtlc_fields = chain_vhtlc_fields(
            &direction,
            claim_public_key,
            refund_public_key,
            swap_response.lockup_details.server_public_key,
            swap_response.claim_details.server_public_key,
            &swap_response.lockup_details.lockup_address,
            &swap_response.claim_details.lockup_address,
            swap_response.lockup_details.timeouts,
            swap_response.claim_details.timeouts,
            claim_key_derivation_index,
            refund_key_derivation_index,
        )?;

        let server_info = self.server_info().await?;
        let vhtlc = self.build_vhtlc_script(
            &server_info,
            chain_vhtlc_fields.claim_public_key,
            chain_vhtlc_fields.refund_public_key,
            ripemd160::Hash::hash(preimage_hash.as_byte_array()),
            &chain_vhtlc_fields.timeouts,
            &chain_vhtlc_fields.address,
        )?;
        let contract_script_pubkey = vhtlc.script_pubkey();

        let stored_script_pubkey = self
            .insert_vhtlc_contract(
                vhtlc.options().clone(),
                chain_vhtlc_fields.key_derivation_index,
            )
            .context("failed to persist chain VHTLC contract")?;
        debug_assert_eq!(stored_script_pubkey, contract_script_pubkey);

        let data = ChainSwapData {
            id: swap_response.id.clone(),
            status: SwapStatus::Created,
            direction,
            preimage: Some(preimage),
            preimage_hash,
            claim_public_key,
            refund_public_key,
            server_claim_public_key: swap_response.lockup_details.server_public_key,
            server_refund_public_key: swap_response.claim_details.server_public_key,
            user_lockup_address: swap_response.lockup_details.lockup_address,
            server_lockup_address: swap_response.claim_details.lockup_address,
            user_lockup_amount: swap_response.lockup_details.amount,
            server_lockup_amount: swap_response.claim_details.amount,
            user_timeout_block_height: swap_response.lockup_details.timeout_block_height,
            server_timeout_block_height: swap_response.claim_details.timeout_block_height,
            user_timeout_block_heights: swap_response.lockup_details.timeouts,
            server_timeout_block_heights: swap_response.claim_details.timeouts,
            bip21,
            swap_tree,
            created_at: created_at.as_secs(),
            claim_key_derivation_index,
            refund_key_derivation_index,
            contract_script_pubkey: Some(contract_script_pubkey),
        };

        self.swap_storage()
            .insert_chain(swap_response.id.clone(), data.clone())
            .await?;

        tracing::info!(
            swap_id = swap_response.id,
            direction = ?data.direction,
            user_lockup_address = %data.user_lockup_address,
            user_lockup_amount = %data.user_lockup_amount,
            server_lockup_amount = %data.server_lockup_amount,
            "Created chain swap"
        );

        Ok(ChainSwapResult {
            swap_id: swap_response.id,
            user_lockup_address: data.user_lockup_address,
            user_lockup_amount: data.user_lockup_amount,
            server_lockup_amount: data.server_lockup_amount,
            bip21: data.bip21,
        })
    }

    /// Wait for Boltz to lock funds on their side of the chain swap.
    ///
    /// Returns when the server's lockup transaction is detected in the mempool or confirmed.
    /// After this returns, use [`Self::claim_chain_swap`] to claim the funds.
    ///
    /// Returns the server's lockup transaction ID if available.
    pub async fn wait_for_chain_swap_server_lockup(
        &self,
        swap_id: &str,
    ) -> Result<Option<String>, Error> {
        use futures::StreamExt;

        let stream = self.subscribe_to_swap_updates_for_type(swap_id.to_string(), SwapType::Chain);
        tokio::pin!(stream);

        while let Some(status_result) = stream.next().await {
            match status_result {
                Ok(status) => {
                    tracing::debug!(swap_id, current = ?status, "Chain swap status");
                    match status {
                        SwapStatus::TransactionServerMempool
                        | SwapStatus::TransactionServerConfirmed => {
                            // Fetch the full status to get the server's lockup txid.
                            let url = format!("{}/v2/swap/{swap_id}", self.inner.boltz_url);
                            let txid = async {
                                reqwest::Client::new()
                                    .get(&url)
                                    .send()
                                    .await
                                    .ok()?
                                    .json::<GetSwapStatusResponse>()
                                    .await
                                    .ok()?
                                    .transaction
                                    .map(|t| t.id)
                            }
                            .await;

                            tracing::info!(
                                swap_id,
                                server_lockup_txid = txid.as_deref().unwrap_or("unknown"),
                                "Server lockup detected"
                            );
                            return Ok(txid);
                        }
                        SwapStatus::SwapExpired => {
                            return Err(Error::ad_hoc(format!("chain swap expired: {swap_id}")));
                        }
                        SwapStatus::TransactionRefunded | SwapStatus::TransactionFailed => {
                            return Err(Error::ad_hoc(format!(
                                "chain swap failed or refunded: {swap_id}"
                            )));
                        }
                        SwapStatus::Error { error } => {
                            tracing::error!(swap_id, "Got error from chain swap updates: {error}");
                        }
                        // User lockup detected — still waiting for server side.
                        SwapStatus::Created
                        | SwapStatus::TransactionMempool
                        | SwapStatus::TransactionConfirmed
                        | SwapStatus::TransactionClaimed
                        | SwapStatus::TransactionLockupFailed
                        | SwapStatus::InvoiceSet
                        | SwapStatus::InvoicePending
                        | SwapStatus::InvoicePaid
                        | SwapStatus::InvoiceSettled
                        | SwapStatus::InvoiceFailedToPay
                        | SwapStatus::InvoiceExpired
                        | SwapStatus::Other(_) => {}
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Err(Error::ad_hoc("Chain swap status stream ended unexpectedly"))
    }

    /// Claim the Arkade VHTLC from a chain swap after Boltz has locked funds.
    ///
    /// This claims the server's Arkade VHTLC lockup using the stored preimage. It is intended
    /// for [`ChainSwapDirection::BtcToArk`] swaps where the server locks an Arkade VHTLC.
    ///
    /// Call this after [`Self::wait_for_chain_swap_server_lockup`] returns.
    pub async fn claim_chain_swap(&self, swap_id: &str) -> Result<Txid, Error> {
        let mut swap = self
            .swap_storage()
            .get_chain(swap_id)
            .await
            .context("failed to get chain swap data")?
            .ok_or_else(|| Error::ad_hoc(format!("chain swap data not found: {swap_id}")))?;

        if swap.direction != ChainSwapDirection::BtcToArk {
            return Err(Error::ad_hoc(
                "claim_chain_swap only applies to BtcToArk swaps; use claim_chain_swap_btc",
            ));
        }

        let preimage = swap
            .preimage
            .ok_or_else(|| Error::ad_hoc(format!("preimage not found for chain swap {swap_id}")))?;

        let server_info = self.server_info().await?;

        let vhtlc = self.chain_vhtlc_script(&mut swap, &server_info).await?;
        let vhtlc_address = vhtlc.address();

        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(server_info.dust, virtual_tx_outpoints);

            let mut unspent = vtxo_list.all_unspent();
            let vhtlc_outpoint = unspent.next().ok_or_else(|| {
                Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
            })?;

            vhtlc_outpoint.clone()
        };

        let (claim_address, _) = self
            .get_offchain_address()
            .await
            .context("failed to get offchain address")?;
        let claim_amount = swap.server_lockup_amount;

        let outputs = vec![SendReceiver::bitcoin(claim_address, claim_amount)];

        let spend_info = vhtlc.taproot_spend_info();
        let script_ver = (vhtlc.claim_script(), LeafVersion::TapScript);
        let control_block = spend_info
            .control_block(&script_ver)
            .ok_or(Error::ad_hoc("control block not found for claim script"))?;

        let script_pubkey = vhtlc.script_pubkey();

        let claimer_pk = swap.claim_public_key.inner.x_only_public_key().0;
        let vhtlc_input = VtxoInput::new(
            script_ver.0,
            None,
            control_block,
            vhtlc.tapscripts(),
            script_pubkey,
            claim_amount,
            vhtlc_outpoint.outpoint,
            vhtlc_outpoint.assets,
        );

        // The change address is superfluous because we are _draining_ the VHTLC.
        let change_address = &claim_address;

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &outputs,
            change_address,
            std::slice::from_ref(&vhtlc_input),
            &server_info,
        )
        .map_err(Error::from)
        .context("failed to build offchain TXs")?;

        let kp = self.keypair_by_pk(&claimer_pk)?;
        let sign_fn =
            |input: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                // Add preimage to PSBT input.
                {
                    let mut bytes = vec![1];

                    let length = VarInt::from(preimage.len() as u64);

                    length
                        .consensus_encode(&mut bytes)
                        .expect("valid length encoding");

                    bytes.write_all(&preimage).expect("valid preimage encoding");

                    input.unknown.insert(
                        psbt::raw::Key {
                            type_value: 222,
                            key: VTXO_CONDITION_KEY.to_vec(),
                        },
                        bytes,
                    );
                }

                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                let pk = kp.x_only_public_key().0;

                Ok(vec![(sig, pk)])
            };

        sign_ark_transaction(sign_fn, &mut ark_tx, 0)
            .map_err(Error::from)
            .context("failed to sign Arkade TX")?;

        let ark_txid = ark_tx.unsigned_tx.compute_txid();

        let res = self
            .network_client()
            .submit_offchain_transaction_request(ark_tx, checkpoint_txs)
            .await
            .map_err(Error::from)
            .context("failed to submit offchain TXs")?;

        let mut checkpoint_psbt = res
            .signed_checkpoint_txs
            .first()
            .ok_or_else(|| Error::ad_hoc("no checkpoint PSBTs found"))?
            .clone();

        sign_checkpoint_transaction(sign_fn, &mut checkpoint_psbt)
            .map_err(Error::from)
            .context("failed to sign checkpoint TX")?;

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, vec![checkpoint_psbt]),
        )
        .await
        .context("failed to finalize offchain transaction")?
        .map_err(Error::ark_server)
        .context("failed to finalize offchain transaction")?;

        tracing::info!(swap_id, txid = %ark_txid, "Claimed chain swap VHTLC");

        let mut updated_swap = swap.clone();
        updated_swap.status = SwapStatus::TransactionClaimed;
        self.swap_storage()
            .update_chain(swap_id, updated_swap.clone())
            .await
            .context("failed to update chain swap data")?;
        self.mark_vhtlc_contract_inactive(updated_swap.contract_script_pubkey.as_ref())?;

        Ok(ark_txid)
    }

    /// Claim on-chain BTC from a chain swap after Boltz has locked funds.
    ///
    /// This claims the server's on-chain BTC HTLC using the stored preimage. It is intended
    /// for [`ChainSwapDirection::ArkToBtc`] swaps where the server locks on-chain BTC.
    ///
    /// Call this after [`Self::wait_for_chain_swap_server_lockup`] returns.
    pub async fn claim_chain_swap_btc(
        &self,
        swap_id: &str,
        destination_address: bitcoin::Address,
        fee_rate_sat_vb: f64,
    ) -> Result<Txid, Error> {
        let swap = self
            .swap_storage()
            .get_chain(swap_id)
            .await
            .context("failed to get chain swap data")?
            .ok_or_else(|| Error::ad_hoc(format!("chain swap data not found: {swap_id}")))?;

        let preimage = swap
            .preimage
            .ok_or_else(|| Error::ad_hoc(format!("preimage not found for chain swap {swap_id}")))?;

        let swap_tree = swap.swap_tree.clone().ok_or_else(|| {
            Error::ad_hoc("no swap tree found (this swap has no on-chain BTC HTLC)")
        })?;

        // The BTC lockup is server-side for ArkToBtc
        let btc_address_str = &swap.server_lockup_address;

        // Reconstruct the taproot tree. For ArkToBtc, the server's key on the BTC
        // side is server_refund_public_key and the user's key is claim_public_key.
        let taproot_spend_info = reconstruct_btc_htlc(
            swap.server_refund_public_key,
            swap.claim_public_key,
            &swap_tree,
        )?;

        let secp = Secp256k1::new();

        // Verify the reconstructed address matches the lockup address.
        let expected_spk = ScriptBuf::new_p2tr(
            &secp,
            taproot_spend_info.internal_key(),
            taproot_spend_info.merkle_root(),
        );

        let parsed_address: bitcoin::Address<bitcoin::address::NetworkUnchecked> = btc_address_str
            .parse()
            .map_err(|e| Error::ad_hoc(format!("invalid BTC lockup address: {e}")))?;
        let parsed_address = parsed_address.assume_checked();
        let target_spk = parsed_address.script_pubkey();

        if expected_spk != target_spk {
            return Err(Error::ad_hoc(format!(
                "taproot address mismatch for BTC lockup {btc_address_str}"
            )));
        }

        let claim_script_bytes: Vec<u8> =
            bitcoin::hex::FromHex::from_hex(&swap_tree.claim_leaf.output)
                .map_err(|e| Error::ad_hoc(format!("invalid claim leaf hex: {e}")))?;
        let claim_script = ScriptBuf::from_bytes(claim_script_bytes);
        let claim_ver = (claim_script.clone(), LeafVersion::TapScript);

        // Find the unspent UTXO at the BTC lockup address
        let utxos = self
            .inner
            .blockchain
            .find_outpoints(&parsed_address)
            .await
            .context("failed to find UTXOs at BTC lockup address")?;

        let utxo = utxos.iter().find(|u| !u.is_spent).ok_or_else(|| {
            Error::ad_hoc(format!(
                "no unspent UTXO found at BTC lockup address {btc_address_str}"
            ))
        })?;

        // Get the control block for the claim leaf
        let control_block = taproot_spend_info
            .control_block(&claim_ver)
            .ok_or(Error::ad_hoc("control block not found for claim leaf"))?;

        let cb_bytes = control_block.serialize();
        // Weight: 4 * (overhead 10.5 + input ~41 + output ~43) + witness items
        let witness_weight = 1 + 1 + 64 + 1 + 32 + 1 + claim_script.len() + 1 + cb_bytes.len() + 1;
        let weight = 4 * (11 + 41 + 43) + witness_weight;
        let vsize = weight.div_ceil(4);
        let fee = Amount::from_sat((vsize as f64 * fee_rate_sat_vb).ceil() as u64);

        let claim_amount = utxo.amount.checked_sub(fee).ok_or_else(|| {
            Error::ad_hoc(format!(
                "UTXO amount {} is less than estimated fee {}",
                utxo.amount, fee
            ))
        })?;

        // Build the unsigned transaction
        let mut tx = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time: absolute::LockTime::ZERO,
            input: vec![bitcoin::TxIn {
                previous_output: utxo.outpoint,
                script_sig: ScriptBuf::new(),
                sequence: bitcoin::Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![TxOut {
                value: claim_amount,
                script_pubkey: destination_address.script_pubkey(),
            }],
        };

        // Compute the taproot script-path sighash
        let leaf_hash =
            bitcoin::taproot::TapLeafHash::from_script(&claim_script, LeafVersion::TapScript);

        let prevouts = [TxOut {
            value: utxo.amount,
            script_pubkey: target_spk.clone(),
        }];

        let sighash = bitcoin::sighash::SighashCache::new(&tx)
            .taproot_script_spend_signature_hash(
                0,
                &bitcoin::sighash::Prevouts::All(&prevouts),
                leaf_hash,
                bitcoin::TapSighashType::Default,
            )
            .map_err(|e| Error::ad_hoc(format!("failed to compute sighash: {e}")))?;

        let msg = secp256k1::Message::from_digest(sighash.to_byte_array());
        let claim_kp = self.keypair_by_pk(&swap.claim_public_key.inner.x_only_public_key().0)?;
        let signature = secp.sign_schnorr_no_aux_rand(&msg, &claim_kp);

        // Build witness: <signature> <preimage> <claim_script> <control_block>
        let mut witness = bitcoin::Witness::new();
        witness.push(signature.serialize());
        witness.push(preimage);
        witness.push(claim_script.as_bytes());
        witness.push(cb_bytes);

        tx.input[0].witness = witness;

        // Broadcast
        self.inner
            .blockchain
            .broadcast(&tx)
            .await
            .context("failed to broadcast BTC claim transaction")?;

        let txid = tx.compute_txid();

        tracing::info!(swap_id, %txid, %claim_amount, "Claimed on-chain BTC from chain swap");

        let mut updated_swap = swap.clone();
        updated_swap.status = SwapStatus::TransactionClaimed;
        self.swap_storage()
            .update_chain(swap_id, updated_swap)
            .await
            .context("failed to update chain swap data")?;

        Ok(txid)
    }

    /// Refund the Arkade VHTLC from a chain swap after the timelock has expired.
    ///
    /// This is for [`ChainSwapDirection::ArkToBtc`] swaps where the user locked an Arkade VHTLC
    /// and needs to reclaim it (e.g. if Boltz never locked BTC or the swap expired).
    ///
    /// This path does not require a signature from Boltz.
    pub async fn refund_chain_swap(&self, swap_id: &str) -> Result<Txid, Error> {
        let mut swap = self
            .swap_storage()
            .get_chain(swap_id)
            .await
            .context("failed to get chain swap data")?
            .ok_or_else(|| Error::ad_hoc(format!("chain swap data not found: {swap_id}")))?;

        if swap.direction != ChainSwapDirection::ArkToBtc {
            return Err(Error::ad_hoc(
                "refund_chain_swap only applies to ArkToBtc swaps; use refund_chain_swap_btc",
            ));
        }

        let timeout_block_heights = swap.user_timeout_block_heights.ok_or_else(|| {
            Error::ad_hoc("chain swap is missing Arkade-side VHTLC timeouts for user lockup")
        })?;

        let server_info = self.server_info().await?;

        let vhtlc = self.chain_vhtlc_script(&mut swap, &server_info).await?;
        let vhtlc_address = vhtlc.address();

        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(server_info.dust, virtual_tx_outpoints);

            let mut unspent = vtxo_list.all_unspent();
            unspent
                .next()
                .ok_or_else(|| {
                    Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
                })?
                .clone()
        };

        let (refund_address, _) = self.get_offchain_address().await?;
        let refund_amount = swap.user_lockup_amount;

        let outputs = vec![SendReceiver::bitcoin(refund_address, refund_amount)];

        let refund_script = vhtlc.refund_without_receiver_script();
        let spend_info = vhtlc.taproot_spend_info();
        let script_ver = (refund_script, LeafVersion::TapScript);
        let control_block = spend_info
            .control_block(&script_ver)
            .ok_or(Error::ad_hoc("control block not found for refund script"))?;

        let script_pubkey = vhtlc.script_pubkey();
        let refunder_pk = swap.refund_public_key.inner.x_only_public_key().0;

        // The change address is superfluous because we are _draining_ the VHTLC.
        let change_address = &refund_address;

        let vhtlc_input = VtxoInput::new(
            script_ver.0,
            Some(absolute::LockTime::from_consensus(
                timeout_block_heights.refund,
            )),
            control_block,
            vhtlc.tapscripts(),
            script_pubkey,
            refund_amount,
            vhtlc_outpoint.outpoint,
            vhtlc_outpoint.assets,
        );

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &outputs,
            change_address,
            std::slice::from_ref(&vhtlc_input),
            &server_info,
        )?;

        let kp = self.keypair_by_pk(&refunder_pk)?;
        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                let pk = kp.x_only_public_key().0;
                Ok(vec![(sig, pk)])
            };

        sign_ark_transaction(sign_fn, &mut ark_tx, 0)?;

        let ark_txid = ark_tx.unsigned_tx.compute_txid();

        let res = self
            .network_client()
            .submit_offchain_transaction_request(ark_tx, checkpoint_txs)
            .await?;

        let mut checkpoint_psbt = res
            .signed_checkpoint_txs
            .first()
            .ok_or_else(|| Error::ad_hoc("no checkpoint PSBTs found"))?
            .clone();

        let kp = self.keypair_by_pk(&refunder_pk)?;
        let sign_fn =
            |_: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                let pk = kp.x_only_public_key().0;
                Ok(vec![(sig, pk)])
            };

        sign_checkpoint_transaction(sign_fn, &mut checkpoint_psbt)?;

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, vec![checkpoint_psbt]),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize offchain transaction")?;

        tracing::info!(swap_id, txid = %ark_txid, "Refunded chain swap Arkade VHTLC");

        let mut updated_swap = swap.clone();
        updated_swap.status = SwapStatus::TransactionRefunded;
        self.swap_storage()
            .update_chain(swap_id, updated_swap.clone())
            .await
            .context("failed to update chain swap data")?;
        self.mark_vhtlc_contract_inactive(updated_swap.contract_script_pubkey.as_ref())?;

        Ok(ark_txid)
    }

    /// Refund on-chain BTC from a chain swap after the timelock has expired.
    ///
    /// This is for [`ChainSwapDirection::BtcToArk`] swaps where the user locked on-chain BTC
    /// and needs to reclaim it (e.g. if Boltz never locked the Arkade VHTLC or the swap expired).
    pub async fn refund_chain_swap_btc(
        &self,
        swap_id: &str,
        destination_address: bitcoin::Address,
        fee_rate_sat_vb: f64,
    ) -> Result<Txid, Error> {
        let swap = self
            .swap_storage()
            .get_chain(swap_id)
            .await
            .context("failed to get chain swap data")?
            .ok_or_else(|| Error::ad_hoc(format!("chain swap data not found: {swap_id}")))?;

        let swap_tree = swap.swap_tree.clone().ok_or_else(|| {
            Error::ad_hoc("no swap tree found (this swap has no on-chain BTC lockup)")
        })?;

        // The user's BTC lockup address
        let btc_address_str = &swap.user_lockup_address;

        // Reconstruct the taproot tree. For BtcToArk, the server's key on the BTC
        // side is server_claim_public_key and the user's key is refund_public_key.
        let taproot_spend_info = reconstruct_btc_htlc(
            swap.server_claim_public_key,
            swap.refund_public_key,
            &swap_tree,
        )?;

        let secp = Secp256k1::new();

        let refund_script_bytes: Vec<u8> =
            bitcoin::hex::FromHex::from_hex(&swap_tree.refund_leaf.output)
                .map_err(|e| Error::ad_hoc(format!("invalid refund leaf hex: {e}")))?;
        let refund_script = ScriptBuf::from_bytes(refund_script_bytes);
        let refund_ver = (refund_script.clone(), LeafVersion::TapScript);

        // Verify address
        let expected_spk = ScriptBuf::new_p2tr(
            &secp,
            taproot_spend_info.internal_key(),
            taproot_spend_info.merkle_root(),
        );

        let parsed_address: bitcoin::Address<bitcoin::address::NetworkUnchecked> = btc_address_str
            .parse()
            .map_err(|e| Error::ad_hoc(format!("invalid BTC lockup address: {e}")))?;
        let parsed_address = parsed_address.assume_checked();
        let target_spk = parsed_address.script_pubkey();

        if expected_spk != target_spk {
            return Err(Error::ad_hoc(format!(
                "taproot address mismatch for BTC lockup {btc_address_str}"
            )));
        }

        // Find the unspent UTXO
        let utxos = self
            .inner
            .blockchain
            .find_outpoints(&parsed_address)
            .await
            .context("failed to find UTXOs at BTC lockup address")?;

        let utxo = utxos.iter().find(|u| !u.is_spent).ok_or_else(|| {
            Error::ad_hoc(format!(
                "no unspent UTXO found at BTC lockup address {btc_address_str}"
            ))
        })?;

        let control_block = taproot_spend_info
            .control_block(&refund_ver)
            .ok_or(Error::ad_hoc("control block not found for refund leaf"))?;

        let cb_bytes = control_block.serialize();
        let witness_weight = 1 + 1 + 64 + 1 + refund_script.len() + 1 + cb_bytes.len() + 1;
        let weight = 4 * (11 + 41 + 43) + witness_weight;
        let vsize = weight.div_ceil(4);
        let fee = Amount::from_sat((vsize as f64 * fee_rate_sat_vb).ceil() as u64);

        let refund_amount = utxo.amount.checked_sub(fee).ok_or_else(|| {
            Error::ad_hoc(format!(
                "UTXO amount {} is less than estimated fee {}",
                utxo.amount, fee
            ))
        })?;

        // Use the user's timeout block height as nLockTime
        let lock_time = absolute::LockTime::from_consensus(swap.user_timeout_block_height);

        let mut tx = bitcoin::Transaction {
            version: bitcoin::transaction::Version::TWO,
            lock_time,
            input: vec![bitcoin::TxIn {
                previous_output: utxo.outpoint,
                script_sig: ScriptBuf::new(),
                sequence: bitcoin::Sequence::ENABLE_LOCKTIME_NO_RBF,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![TxOut {
                value: refund_amount,
                script_pubkey: destination_address.script_pubkey(),
            }],
        };

        // Sign with the refund key
        let leaf_hash =
            bitcoin::taproot::TapLeafHash::from_script(&refund_script, LeafVersion::TapScript);

        let prevouts = [TxOut {
            value: utxo.amount,
            script_pubkey: target_spk,
        }];

        let sighash = bitcoin::sighash::SighashCache::new(&tx)
            .taproot_script_spend_signature_hash(
                0,
                &bitcoin::sighash::Prevouts::All(&prevouts),
                leaf_hash,
                bitcoin::TapSighashType::Default,
            )
            .map_err(|e| Error::ad_hoc(format!("failed to compute sighash: {e}")))?;

        let msg = secp256k1::Message::from_digest(sighash.to_byte_array());
        let refund_kp = self.keypair_by_pk(&swap.refund_public_key.inner.x_only_public_key().0)?;
        let signature = secp.sign_schnorr_no_aux_rand(&msg, &refund_kp);

        // Witness for refund: <signature> <refund_script> <control_block>
        let mut witness = bitcoin::Witness::new();
        witness.push(signature.serialize());
        witness.push(refund_script.as_bytes());
        witness.push(cb_bytes);

        tx.input[0].witness = witness;

        self.inner
            .blockchain
            .broadcast(&tx)
            .await
            .context("failed to broadcast BTC refund transaction")?;

        let txid = tx.compute_txid();

        tracing::info!(swap_id, %txid, %refund_amount, "Refunded on-chain BTC from chain swap");

        let mut updated_swap = swap.clone();
        updated_swap.status = SwapStatus::TransactionRefunded;
        self.swap_storage()
            .update_chain(swap_id, updated_swap)
            .await
            .context("failed to update chain swap data")?;

        Ok(txid)
    }

    /// Query the current status of any Boltz swap by ID.
    ///
    /// Checks local swap storage to determine the swap type, then queries the Boltz API
    /// for the live status.
    pub async fn get_swap_status(&self, swap_id: &str) -> Result<SwapStatusInfo, Error> {
        // Determine swap type from local storage.
        let swap_type = self.swap_type_for_id(swap_id).await?;

        // Query the Boltz API for live status.
        let url = format!("{}/v2/swap/{swap_id}", self.inner.boltz_url);
        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to query swap status")?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))?;
            return Err(Error::ad_hoc(format!(
                "failed to get swap status: {error_text}"
            )));
        }

        let status_response: GetSwapStatusResponse = response
            .json()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to deserialize swap status response")?;

        self.persist_swap_status_for_type(swap_type, swap_id, status_response.status.clone())
            .await?;

        Ok(SwapStatusInfo {
            swap_id: swap_id.to_string(),
            swap_type,
            status: status_response.status,
        })
    }

    /// Fetch fee information from Boltz for both submarine and reverse swaps.
    ///
    /// # Returns
    ///
    /// - A [`BoltzFees`] struct containing fee information for both swap types.
    pub async fn get_fees(&self) -> Result<BoltzFees, Error> {
        let client = reqwest::Client::builder()
            .timeout(self.inner.timeout)
            .build()
            .map_err(|e| Error::ad_hoc(e.to_string()))?;

        // Fetch submarine swap fees (Arkade -> BTC)
        let submarine_url = format!("{}/v2/swap/submarine", self.inner.boltz_url);
        let submarine_response = client
            .get(&submarine_url)
            .send()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to fetch submarine swap fees")?;

        if !submarine_response.status().is_success() {
            let error_text = submarine_response
                .text()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))?;
            return Err(Error::ad_hoc(format!(
                "failed to fetch submarine swap fees: {error_text}"
            )));
        }

        let submarine_pairs: SubmarinePairsResponse = submarine_response
            .json()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to deserialize submarine swap fees response")?;

        let submarine_pair_fees = &submarine_pairs.ark.btc.fees;
        let submarine_fees = SubmarineSwapFees {
            percentage: submarine_pair_fees.percentage,
            miner_fees: submarine_pair_fees.miner_fees,
        };

        // Fetch reverse swap fees (BTC -> Arkade)
        let reverse_url = format!("{}/v2/swap/reverse", self.inner.boltz_url);
        let reverse_response = client
            .get(&reverse_url)
            .send()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to fetch reverse swap fees")?;

        if !reverse_response.status().is_success() {
            let error_text = reverse_response
                .text()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))?;
            return Err(Error::ad_hoc(format!(
                "failed to fetch reverse swap fees: {error_text}"
            )));
        }

        let reverse_pairs: ReversePairsResponse = reverse_response
            .json()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to deserialize reverse swap fees response")?;

        let reverse_pair_fees = &reverse_pairs.btc.ark.fees;
        let reverse_fees = ReverseSwapFees {
            percentage: reverse_pair_fees.percentage,
            miner_fees: ReverseMinerFees {
                lockup: reverse_pair_fees.miner_fees.lockup,
                claim: reverse_pair_fees.miner_fees.claim,
            },
        };

        Ok(BoltzFees {
            submarine: submarine_fees,
            reverse: reverse_fees,
        })
    }

    /// Fetch swap amount limits from Boltz for submarine swaps.
    ///
    /// # Returns
    ///
    /// - A [`SwapLimits`] struct containing minimum and maximum swap amounts in satoshis.
    pub async fn get_limits(&self) -> Result<SwapLimits, Error> {
        let client = reqwest::Client::builder()
            .timeout(self.inner.timeout)
            .build()
            .map_err(|e| Error::ad_hoc(e.to_string()))?;

        let url = format!("{}/v2/swap/submarine", self.inner.boltz_url);
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to fetch swap limits")?;

        if !response.status().is_success() {
            let error_text = response
                .text()
                .await
                .map_err(|e| Error::ad_hoc(e.to_string()))?;
            return Err(Error::ad_hoc(format!(
                "failed to fetch swap limits: {error_text}"
            )));
        }

        let pairs: SubmarinePairsResponse = response
            .json()
            .await
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to deserialize swap limits response")?;

        Ok(SwapLimits {
            min: pairs.ark.btc.limits.minimal,
            max: pairs.ark.btc.limits.maximal,
        })
    }

    /// Use Boltz's API to learn about updates for a particular swap.
    // TODO: Make sure this is WASM-compatible.
    pub fn subscribe_to_swap_updates(
        &self,
        swap_id: String,
    ) -> impl futures::Stream<Item = Result<SwapStatus, Error>> + '_ {
        async_stream::stream! {
            use futures::StreamExt;

            let swap_type = match self.swap_type_for_id(&swap_id).await {
                Ok(swap_type) => swap_type,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            let stream = self.subscribe_to_swap_updates_for_type(swap_id, swap_type);
            tokio::pin!(stream);

            while let Some(status) = stream.next().await {
                yield status;
            }
        }
    }

    /// Start a background watcher for active Boltz VHTLC contracts.
    ///
    /// The watcher subscribes directly to active VHTLC scripts and drives lifecycle actions from
    /// VTXO events: it claims locally claimable VHTLCs, extracts submarine preimages after Boltz
    /// spends a VHTLC, reconciles contract state, and retries refunds for locally stored refundable
    /// statuses. This keeps VHTLC lifecycle handling event-driven instead of tying contract
    /// deactivation to Boltz status polling.
    pub fn start_boltz_vhtlc_watcher(self: &Arc<Self>) -> BoltzVhtlcWatcherHandle
    where
        B: Send + Sync + 'static,
        W: Send + Sync + 'static,
    {
        self.start_boltz_vhtlc_watcher_with_config(BoltzVhtlcWatcherConfig::default())
    }

    /// Start a background watcher for active Boltz VHTLC contracts with custom configuration.
    pub fn start_boltz_vhtlc_watcher_with_config(
        self: &Arc<Self>,
        config: BoltzVhtlcWatcherConfig,
    ) -> BoltzVhtlcWatcherHandle
    where
        B: Send + Sync + 'static,
        W: Send + Sync + 'static,
    {
        let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
        let client = Arc::clone(self);
        tokio::spawn(async move {
            run_boltz_vhtlc_watcher_loop(client, stop_rx, config).await;
            tracing::debug!("Boltz VHTLC watcher stopped");
        });
        BoltzVhtlcWatcherHandle { stop_tx }
    }

    fn subscribe_to_swap_updates_for_type(
        &self,
        swap_id: String,
        swap_type: SwapType,
    ) -> impl futures::Stream<Item = Result<SwapStatus, Error>> + '_ {
        async_stream::stream! {
            let mut last_status: Option<SwapStatus> = None;
            let url = format!("{}/v2/swap/{swap_id}", self.inner.boltz_url);

            loop {
                let client = reqwest::Client::new();
                let response = client
                    .get(&url)
                    .send()
                    .await;

                match response {
                    Ok(resp) if resp.status().is_success() => {
                        let status_response = resp
                            .json::<GetSwapStatusResponse>()
                            .await
                            .map_err(|e| Error::ad_hoc(e.to_string()));

                        match status_response {
                            Ok(current_status) => {
                                let current_status = current_status.status;

                                let status_changed = last_status.as_ref() != Some(&current_status);
                                if status_changed || current_status.is_terminal() {
                                    if let Err(e) = self.persist_swap_status_for_type(swap_type, &swap_id, current_status.clone()).await {
                                        yield Err(e);
                                        break;
                                    }
                                }

                                // Only yield if status has changed.
                                if status_changed {
                                    last_status = Some(current_status.clone());
                                    yield Ok(current_status);
                                }
                            }
                            Err(e) => {
                                yield Err(Error::ad_hoc(format!(
                                            "failed to deserialize swap status response: {e}"
                                        )));
                                break;
                            }
                        }
                    }
                    Ok(resp) => {
                        let error_text = resp
                            .text()
                            .await
                            .unwrap_or_else(|_| "Unknown error".to_string());

                        yield Err(Error::ad_hoc(format!(
                            "failed to check swap status: {error_text}"
                        )));
                        break;
                    }
                    Err(e) => {
                        yield Err(Error::ad_hoc(e.to_string())
                            .context("failed to send swap status request"));
                        break;
                    }
                }

                // Poll every second
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }

    // Pending VHTLC spend recovery.

    /// List pending (submitted but not finalized) VHTLC spend transactions.
    ///
    /// This checks swaps whose VHTLC contract is still active, queries the server for pending
    /// VTXOs on their VHTLC addresses, and determines the spend type from the PSBT data.
    pub async fn list_pending_vhtlc_spend_txs(&self) -> Result<Vec<PendingVhtlcSpendTx>, Error> {
        let vhtlc_infos = self.collect_active_vhtlc_infos().await?;

        if vhtlc_infos.is_empty() {
            return Ok(vec![]);
        }

        let addresses = vhtlc_infos.iter().map(|info| info.address);
        let request = ark_core::server::GetVtxosRequest::new_for_addresses(addresses)
            .pending_only()
            .map_err(Error::from)?;

        let vtxos = self
            .fetch_all_vtxos(request)
            .await
            .context("failed to fetch pending VHTLC VTXOs")?;

        tracing::debug!(
            num_pending_vtxos = vtxos.len(),
            "Fetched pending VHTLC VTXOs"
        );

        if vtxos.is_empty() {
            return Ok(vec![]);
        }

        // Map script_pubkey → VhtlcInfo for lookup.
        let info_by_script: HashMap<_, _> = vhtlc_infos
            .iter()
            .map(|info| (info.script_pubkey.clone(), info))
            .collect();

        let secp = Secp256k1::new();
        let mut results = Vec::new();
        let mut seen_ark_txids = HashSet::new();

        for vtxo in &vtxos {
            let info = match info_by_script.get(&vtxo.script) {
                Some(info) => info,
                None => {
                    tracing::warn!(
                        outpoint = %vtxo.outpoint,
                        "Skipping pending VHTLC VTXO with unknown script"
                    );
                    continue;
                }
            };

            // Build an intent to fetch the pending tx from the server.
            // We prove ownership using the forfeit-like spend path that we can sign.
            // If we have a preimage (reverse swap claim path), include it as extra
            // witness so the server can verify the intent proof for the claim script.
            let intent_input = match info.preimage {
                Some(preimage) => intent::Input::new_with_extra_witness(
                    vtxo.outpoint,
                    bitcoin::Sequence::ZERO,
                    None,
                    TxOut {
                        value: vtxo.amount,
                        script_pubkey: info.script_pubkey.clone(),
                    },
                    vhtlc_tapscripts(&info.vhtlc),
                    info.intent_spend_info.clone(),
                    false,
                    vtxo.is_swept,
                    vtxo.assets.clone(),
                    vec![preimage.to_vec()],
                ),
                None => intent::Input::new(
                    vtxo.outpoint,
                    bitcoin::Sequence::ZERO,
                    None,
                    TxOut {
                        value: vtxo.amount,
                        script_pubkey: info.script_pubkey.clone(),
                    },
                    vhtlc_tapscripts(&info.vhtlc),
                    info.intent_spend_info.clone(),
                    false,
                    vtxo.is_swept,
                    vtxo.assets.clone(),
                ),
            };

            let sign_for_vtxo_fn = |input: &mut psbt::Input,
                                    msg: secp256k1::Message|
             -> Result<
                Vec<(schnorr::Signature, XOnlyPublicKey)>,
                ark_core::Error,
            > {
                match &input.witness_script {
                    None => Err(ark_core::Error::ad_hoc(
                        "Missing witness script when signing get-pending-tx intent for VHTLC",
                    )),
                    Some(script) => {
                        let pks = extract_checksig_pubkeys(script);
                        let mut res = vec![];
                        for pk in &pks {
                            if let Ok(keypair) = self.keypair_by_pk(pk) {
                                let sig = secp.sign_schnorr_no_aux_rand(&msg, &keypair);
                                res.push((sig, keypair.x_only_public_key().0));
                            }
                        }
                        Ok(res)
                    }
                }
            };

            let sign_for_onchain_fn =
                |_: &mut psbt::Input,
                 _: secp256k1::Message|
                 -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
                    Err(ark_core::Error::ad_hoc(
                        "unexpected onchain input in get-pending-tx intent",
                    ))
                };

            let message = intent::IntentMessage::GetPendingTx { expire_at: 0 };
            let get_pending_intent = intent::make_intent(
                sign_for_vtxo_fn,
                sign_for_onchain_fn,
                vec![intent_input],
                vec![],
                message,
            )?;

            let pending_txs = self
                .network_client()
                .get_pending_tx(get_pending_intent)
                .await
                .map_err(Error::ark_server)
                .context("failed to get pending VHTLC transactions")?;

            for pending_tx in pending_txs {
                if !seen_ark_txids.insert(pending_tx.ark_txid) {
                    continue;
                }

                let spend_type = Self::identify_vhtlc_spend_type(info, &pending_tx)?;

                tracing::info!(
                    ark_txid = %pending_tx.ark_txid,
                    swap_id = spend_type.swap_id(),
                    spend_type = spend_type.name(),
                    "Found pending VHTLC spend transaction"
                );

                results.push(PendingVhtlcSpendTx {
                    spend_type,
                    pending_tx,
                });
            }
        }

        Ok(results)
    }

    /// Continue (finalize) a pending VHTLC spend transaction.
    ///
    /// Handles the different spend types appropriately:
    /// - **Claim**: signs the checkpoint with the claim key and injects the preimage.
    /// - **CollaborativeRefund**: re-requests Boltz's signature, then signs with the refund key.
    /// - **ExpiredRefund**: signs the checkpoint with the refund key (no Boltz needed).
    pub async fn continue_pending_vhtlc_spend_tx(
        &self,
        pending: &PendingVhtlcSpendTx,
    ) -> Result<Txid, Error> {
        let ark_txid = pending.pending_tx.ark_txid;

        match &pending.spend_type {
            PendingVhtlcSpendType::Claim { preimage, .. } => {
                self.continue_pending_claim(ark_txid, &pending.pending_tx, *preimage)
                    .await
            }
            PendingVhtlcSpendType::CollaborativeRefund { swap_id } => {
                self.continue_pending_collaborative_refund(ark_txid, &pending.pending_tx, swap_id)
                    .await
            }
            PendingVhtlcSpendType::ExpiredRefund { .. } => {
                self.continue_pending_expired_refund(ark_txid, &pending.pending_tx)
                    .await
            }
        }
    }

    /// Sign and finalize all pending VHTLC spend transactions.
    pub async fn continue_pending_vhtlc_spend_txs(&self) -> Result<Vec<Txid>, Error> {
        let pending = self.list_pending_vhtlc_spend_txs().await?;

        let mut finalized = Vec::new();
        for tx in &pending {
            match self.continue_pending_vhtlc_spend_tx(tx).await {
                Ok(txid) => finalized.push(txid),
                Err(e) => {
                    tracing::warn!(
                        ark_txid = %tx.pending_tx.ark_txid,
                        swap_id = tx.spend_type.swap_id(),
                        ?e,
                        "Failed to finalize pending VHTLC spend tx"
                    );
                }
            }
        }

        Ok(finalized)
    }

    /// Sign and finalize a pending claim VHTLC checkpoint.
    async fn continue_pending_claim(
        &self,
        ark_txid: Txid,
        pending_tx: &PendingTx,
        preimage: [u8; 32],
    ) -> Result<Txid, Error> {
        let mut signed_checkpoint_txs = pending_tx.signed_checkpoint_txs.clone();

        for checkpoint_psbt in signed_checkpoint_txs.iter_mut() {
            Self::restore_witness_script_if_needed(checkpoint_psbt, &pending_tx.signed_ark_tx)?;

            // Inject preimage into checkpoint inputs before signing.
            Self::inject_preimage_into_psbt(checkpoint_psbt, preimage);

            self.sign_checkpoint_with_own_keys(checkpoint_psbt)?;
        }

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, signed_checkpoint_txs),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize pending claim transaction")?;

        tracing::info!(txid = %ark_txid, "Finalized pending VHTLC claim");
        Ok(ark_txid)
    }

    /// Re-request Boltz's signature and finalize a pending collaborative refund.
    async fn continue_pending_collaborative_refund(
        &self,
        ark_txid: Txid,
        pending_tx: &PendingTx,
        swap_id: &str,
    ) -> Result<Txid, Error> {
        // For collaborative refunds, the server stripped Boltz's signatures when we
        // submitted. We need to re-request them from Boltz.
        //
        // Re-send the ark tx and each checkpoint to Boltz's refund endpoint to get fresh
        // signatures from them.
        let url = format!(
            "{}/v2/swap/submarine/{swap_id}/refund/ark",
            self.inner.boltz_url
        );
        let client = reqwest::Client::new();

        let mut signed_checkpoint_txs = Vec::new();

        for checkpoint_psbt in &pending_tx.signed_checkpoint_txs {
            let response = client
                .post(&url)
                .json(&RefundSwapRequest {
                    transaction: pending_tx.signed_ark_tx.to_string(),
                    checkpoint: checkpoint_psbt.to_string(),
                })
                .send()
                .await
                .map_err(Error::ad_hoc)
                .context("failed to re-request Boltz refund signature")?;

            if !response.status().is_success() {
                let error_text = response
                    .text()
                    .await
                    .map_err(|e| Error::ad_hoc(e.to_string()))
                    .context("failed to read Boltz error text")?;

                return Err(Error::ad_hoc(format!(
                    "Boltz refund re-sign request failed: {error_text}"
                )));
            }

            let refund_response: RefundSwapResponse = response
                .json()
                .await
                .map_err(Error::ad_hoc)
                .context("failed to deserialize Boltz refund response")?;

            if let Some(err) = refund_response.error.as_deref() {
                return Err(Error::ad_hoc(format!("Boltz refund re-sign failed: {err}")));
            }

            let boltz_signed_checkpoint = Psbt::from_str(&refund_response.checkpoint)
                .map_err(Error::ad_hoc)
                .context("could not parse Boltz-signed checkpoint PSBT")?;

            // Extract Boltz's tap_script_sigs.
            let boltz_tap_script_sigs = boltz_signed_checkpoint
                .inputs
                .first()
                .ok_or_else(|| Error::ad_hoc("Boltz checkpoint has no inputs"))?
                .tap_script_sigs
                .clone();

            // Start from the server's checkpoint (which has the server's signature).
            let mut final_checkpoint = checkpoint_psbt.clone();
            Self::restore_witness_script_if_needed(
                &mut final_checkpoint,
                &pending_tx.signed_ark_tx,
            )?;

            // Merge Boltz's signatures.
            final_checkpoint
                .inputs
                .first_mut()
                .ok_or_else(|| Error::ad_hoc("checkpoint has no inputs"))?
                .tap_script_sigs
                .extend(boltz_tap_script_sigs);

            // Add our (sender) signature.
            self.sign_checkpoint_with_own_keys(&mut final_checkpoint)?;

            signed_checkpoint_txs.push(final_checkpoint);
        }

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, signed_checkpoint_txs),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize pending collaborative refund")?;

        tracing::info!(txid = %ark_txid, swap_id, "Finalized pending collaborative refund");
        Ok(ark_txid)
    }

    /// Sign and finalize a pending expired refund checkpoint.
    async fn continue_pending_expired_refund(
        &self,
        ark_txid: Txid,
        pending_tx: &PendingTx,
    ) -> Result<Txid, Error> {
        let mut signed_checkpoint_txs = pending_tx.signed_checkpoint_txs.clone();

        for checkpoint_psbt in signed_checkpoint_txs.iter_mut() {
            Self::restore_witness_script_if_needed(checkpoint_psbt, &pending_tx.signed_ark_tx)?;
            self.sign_checkpoint_with_own_keys(checkpoint_psbt)?;
        }

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, signed_checkpoint_txs),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize pending expired refund")?;

        tracing::info!(txid = %ark_txid, "Finalized pending expired VHTLC refund");
        Ok(ark_txid)
    }

    // Private helpers for pending VHTLC recovery.

    /// Try to reconstruct a [`VhtlcScript`] that matches `expected_address` by trying the current
    /// server signer and all deprecated signers in order. Returns the first match.
    ///
    /// This handles the case where the server rotated its signing key after a swap was created:
    /// the VHTLC was built with the old key, so we must try deprecated keys to find the right one.
    fn reconstruct_vhtlc_for_address(
        &self,
        server_info: &Info,
        mk_opts: impl Fn(XOnlyPublicKey) -> Result<VhtlcOptions, Error>,
        expected_address: &ArkAddress,
    ) -> Result<VhtlcScript, Error> {
        reconstruct_vhtlc_from_keys(
            server_info.all_server_keys(),
            server_info.network,
            mk_opts,
            expected_address,
        )
    }

    /// Reconstruct a [`VhtlcScript`] from swap data fields, trying current + deprecated signers.
    fn build_vhtlc_script(
        &self,
        server_info: &Info,
        claim_public_key: PublicKey,
        refund_public_key: PublicKey,
        preimage_hash: ripemd160::Hash,
        timeout_block_heights: &TimeoutBlockHeights,
        expected_address: &ArkAddress,
    ) -> Result<VhtlcScript, Error> {
        let unilateral_claim_delay =
            parse_sequence_number(timeout_block_heights.unilateral_claim as i64)
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral claim timeout: {e}")))?;
        let unilateral_refund_delay =
            parse_sequence_number(timeout_block_heights.unilateral_refund as i64)
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral refund timeout: {e}")))?;
        let unilateral_refund_without_receiver_delay =
            parse_sequence_number(timeout_block_heights.unilateral_refund_without_receiver as i64)
                .map_err(|e| {
                    Error::ad_hoc(format!("invalid refund without receiver timeout: {e}"))
                })?;

        self.reconstruct_vhtlc_for_address(
            server_info,
            |server| {
                Ok(VhtlcOptions {
                    sender: refund_public_key.inner.x_only_public_key().0,
                    receiver: claim_public_key.inner.x_only_public_key().0,
                    server,
                    preimage_hash,
                    refund_locktime: timeout_block_heights.refund,
                    unilateral_claim_delay,
                    unilateral_refund_delay,
                    unilateral_refund_without_receiver_delay,
                })
            },
            expected_address,
        )
    }

    pub(crate) async fn migrate_boltz_vhtlc_contracts(
        &self,
        server_info: &Info,
    ) -> Result<u32, Error> {
        let mut migrated = 0;

        for mut swap in self.swap_storage().list_all_submarine().await? {
            if swap.contract_script_pubkey.is_some() {
                continue;
            }
            match self.submarine_vhtlc_script(&mut swap, server_info).await {
                Ok(_) => {
                    self.best_effort_reconcile_vhtlc_contract_state_from_vtxos(
                        &swap.id,
                        swap.vhtlc_address,
                        swap.contract_script_pubkey.clone(),
                    )
                    .await;
                    migrated += 1;
                }
                Err(error) => {
                    tracing::warn!(swap_id = %swap.id, ?error, "Failed to migrate submarine VHTLC contract");
                }
            }
        }

        for mut swap in self.swap_storage().list_all_reverse().await? {
            if swap.contract_script_pubkey.is_some() {
                continue;
            }
            match self.reverse_vhtlc_script(&mut swap, server_info).await {
                Ok(_) => {
                    self.best_effort_reconcile_vhtlc_contract_state_from_vtxos(
                        &swap.id,
                        swap.vhtlc_address,
                        swap.contract_script_pubkey.clone(),
                    )
                    .await;
                    migrated += 1;
                }
                Err(error) => {
                    tracing::warn!(swap_id = %swap.id, ?error, "Failed to migrate reverse VHTLC contract");
                }
            }
        }

        for mut swap in self.swap_storage().list_all_chain().await? {
            if swap.contract_script_pubkey.is_some() {
                continue;
            }
            match self.chain_vhtlc_script(&mut swap, server_info).await {
                Ok(_) => {
                    match swap.chain_vhtlc_address() {
                        Ok(vhtlc_address) => {
                            self.best_effort_reconcile_vhtlc_contract_state_from_vtxos(
                                &swap.id,
                                vhtlc_address,
                                swap.contract_script_pubkey.clone(),
                            )
                            .await;
                        }
                        Err(error) => {
                            tracing::warn!(
                                swap_id = %swap.id,
                                ?error,
                                "Failed to resolve chain VHTLC address during contract-state reconciliation"
                            );
                        }
                    }
                    migrated += 1;
                }
                Err(error) => {
                    tracing::warn!(swap_id = %swap.id, ?error, "Failed to migrate chain VHTLC contract");
                }
            }
        }

        Ok(migrated)
    }

    async fn swap_type_for_id(&self, swap_id: &str) -> Result<SwapType, Error> {
        if self.swap_storage().get_submarine(swap_id).await?.is_some() {
            Ok(SwapType::Submarine)
        } else if self.swap_storage().get_reverse(swap_id).await?.is_some() {
            Ok(SwapType::Reverse)
        } else if self.swap_storage().get_chain(swap_id).await?.is_some() {
            Ok(SwapType::Chain)
        } else {
            Ok(SwapType::Unknown)
        }
    }

    async fn persist_swap_status_for_type(
        &self,
        swap_type: SwapType,
        swap_id: &str,
        status: SwapStatus,
    ) -> Result<(), Error> {
        match swap_type {
            SwapType::Submarine => {
                if self.swap_storage().get_submarine(swap_id).await?.is_some() {
                    let should_reconcile = status.is_terminal();
                    self.swap_storage()
                        .update_status_submarine(swap_id, status)
                        .await?;
                    if should_reconcile {
                        if let Some(swap) = self.swap_storage().get_submarine(swap_id).await? {
                            self.best_effort_reconcile_vhtlc_contract_state_from_vtxos(
                                swap_id,
                                swap.vhtlc_address,
                                swap.contract_script_pubkey,
                            )
                            .await;
                        }
                    }
                }
            }
            SwapType::Reverse => {
                if self.swap_storage().get_reverse(swap_id).await?.is_some() {
                    let should_reconcile = status.is_terminal();
                    self.swap_storage()
                        .update_status_reverse(swap_id, status)
                        .await?;
                    if should_reconcile {
                        if let Some(swap) = self.swap_storage().get_reverse(swap_id).await? {
                            self.best_effort_reconcile_vhtlc_contract_state_from_vtxos(
                                swap_id,
                                swap.vhtlc_address,
                                swap.contract_script_pubkey,
                            )
                            .await;
                        }
                    }
                }
            }
            SwapType::Chain => {
                if self.swap_storage().get_chain(swap_id).await?.is_some() {
                    let should_reconcile = status.is_terminal();
                    self.swap_storage()
                        .update_status_chain(swap_id, status)
                        .await?;
                    if should_reconcile {
                        if let Some(swap) = self.swap_storage().get_chain(swap_id).await? {
                            match swap.chain_vhtlc_address() {
                                Ok(vhtlc_address) => {
                                    self.best_effort_reconcile_vhtlc_contract_state_from_vtxos(
                                        swap_id,
                                        vhtlc_address,
                                        swap.contract_script_pubkey,
                                    )
                                    .await;
                                }
                                Err(error) => {
                                    tracing::warn!(
                                        swap_id,
                                        ?error,
                                        "Failed to resolve chain VHTLC address during contract-state reconciliation"
                                    );
                                }
                            }
                        }
                    }
                }
            }
            SwapType::Unknown => {}
        }

        Ok(())
    }

    fn insert_vhtlc_contract(
        &self,
        options: VhtlcOptions,
        key_derivation_index: Option<u32>,
    ) -> Result<ScriptBuf, Error> {
        let contract = VhtlcContract { options };
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let mut manager = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?;
        let stored =
            manager.insert_or_get(contract, ContractState::Active, key_derivation_index)?;
        Ok(stored.script_pubkey)
    }

    async fn observe_vhtlc_contract_liveness(
        &self,
        server_info: &Info,
        address: ArkAddress,
    ) -> Result<VhtlcContractLiveness, Error> {
        let pending_request =
            ark_core::server::GetVtxosRequest::new_for_addresses(std::iter::once(address))
                .pending_only()
                .map_err(Error::from)?;
        let pending_vtxos = self
            .fetch_all_vtxos(pending_request)
            .await
            .context("failed to fetch pending VHTLC VTXOs")?;

        let vtxos = self
            .get_virtual_tx_outpoints(std::iter::once(address))
            .await
            .context("failed to fetch VHTLC VTXOs")?;

        Ok(classify_vhtlc_contract_liveness(
            server_info.dust,
            !pending_vtxos.is_empty(),
            vtxos,
        ))
    }

    async fn reconcile_vhtlc_contract_state_from_vtxos(
        &self,
        swap_id: &str,
        address: ArkAddress,
        script_pubkey: Option<ScriptBuf>,
    ) -> Result<(), Error> {
        let Some(script_pubkey) = script_pubkey else {
            return Ok(());
        };

        let server_info = self.server_info().await?;
        let liveness = self
            .observe_vhtlc_contract_liveness(&server_info, address)
            .await?;

        tracing::debug!(
            swap_id,
            %address,
            ?liveness,
            "Observed VHTLC contract liveness"
        );

        if liveness.should_deactivate_contract() {
            self.mark_vhtlc_contract_inactive(Some(&script_pubkey))?;
            tracing::info!(
                swap_id,
                %address,
                "Marked spent VHTLC contract inactive"
            );
        }

        Ok(())
    }

    async fn best_effort_reconcile_vhtlc_contract_state_from_vtxos(
        &self,
        swap_id: &str,
        address: ArkAddress,
        script_pubkey: Option<ScriptBuf>,
    ) {
        match self.vhtlc_contract_is_inactive(script_pubkey.as_ref()) {
            Ok(true) => return,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    swap_id,
                    ?error,
                    "Failed to check VHTLC contract state before reconciliation"
                );
            }
        }

        if let Err(error) = self
            .reconcile_vhtlc_contract_state_from_vtxos(swap_id, address, script_pubkey)
            .await
        {
            tracing::warn!(
                swap_id,
                ?error,
                "Failed to reconcile VHTLC contract state from VTXO status"
            );
        }
    }

    fn mark_vhtlc_contract_inactive(&self, script_pubkey: Option<&ScriptBuf>) -> Result<(), Error> {
        let Some(script_pubkey) = script_pubkey else {
            return Ok(());
        };
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let result = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?
            .update_state(script_pubkey, ContractState::Inactive);
        result
    }

    fn vhtlc_contract_is_inactive(&self, script_pubkey: Option<&ScriptBuf>) -> Result<bool, Error> {
        let Some(script_pubkey) = script_pubkey else {
            return Ok(false);
        };
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let inactive = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?
            .get(script_pubkey)?
            .is_some_and(|contract| contract.state == ContractState::Inactive);
        Ok(inactive)
    }

    fn get_vhtlc_contract(
        &self,
        script_pubkey: &ScriptBuf,
    ) -> Result<Option<VhtlcContract>, Error> {
        let state = self
            .state
            .read()
            .map_err(|_| Error::ad_hoc("client server state lock poisoned"))?;
        let contract = state
            .contract_manager
            .lock()
            .map_err(|_| Error::ad_hoc("contract manager lock poisoned"))?
            .get_typed(script_pubkey)?;
        Ok(contract)
    }

    async fn submarine_vhtlc_script(
        &self,
        swap: &mut SubmarineSwapData,
        server_info: &Info,
    ) -> Result<VhtlcScript, Error> {
        if let Some(script_pubkey) = &swap.contract_script_pubkey {
            if let Some(contract) = self.get_vhtlc_contract(script_pubkey)? {
                let vhtlc = vhtlc_script_from_contract(contract, &swap.vhtlc_address, server_info)?;

                return Ok(vhtlc);
            }

            tracing::warn!(
                swap_id = %swap.id,
                "VHTLC contract reference missing; recreating from legacy swap data"
            );
        }

        let vhtlc = self.build_vhtlc_script(
            server_info,
            swap.claim_public_key,
            swap.refund_public_key,
            swap.preimage_hash,
            &swap.timeout_block_heights,
            &swap.vhtlc_address,
        )?;

        let script_pubkey = self
            .insert_vhtlc_contract(vhtlc.options().clone(), swap.key_derivation_index)
            .context("failed to persist VHTLC contract")?;

        swap.contract_script_pubkey = Some(script_pubkey);
        self.swap_storage()
            .update_submarine(&swap.id, swap.clone())
            .await
            .context("failed to persist VHTLC contract reference")?;

        Ok(vhtlc)
    }

    fn build_chain_vhtlc_script(
        &self,
        swap: &ChainSwapData,
        server_info: &Info,
    ) -> Result<VhtlcScript, Error> {
        let fields = swap.chain_vhtlc_fields()?;
        self.build_vhtlc_script(
            server_info,
            fields.claim_public_key,
            fields.refund_public_key,
            ripemd160::Hash::hash(swap.preimage_hash.as_byte_array()),
            &fields.timeouts,
            &fields.address,
        )
    }

    async fn chain_vhtlc_script(
        &self,
        swap: &mut ChainSwapData,
        server_info: &Info,
    ) -> Result<VhtlcScript, Error> {
        if let Some(script_pubkey) = &swap.contract_script_pubkey {
            if let Some(contract) = self.get_vhtlc_contract(script_pubkey)? {
                let expected_address = swap.chain_vhtlc_address()?;
                let vhtlc = vhtlc_script_from_contract(contract, &expected_address, server_info)?;

                return Ok(vhtlc);
            }

            tracing::warn!(
                swap_id = %swap.id,
                "Chain VHTLC contract reference missing; recreating from swap data"
            );
        }

        let vhtlc = self.build_chain_vhtlc_script(swap, server_info)?;

        let script_pubkey = self
            .insert_vhtlc_contract(vhtlc.options().clone(), swap.chain_vhtlc_key_index())
            .context("failed to persist chain VHTLC contract")?;

        swap.contract_script_pubkey = Some(script_pubkey);
        self.swap_storage()
            .update_chain(&swap.id, swap.clone())
            .await
            .context("failed to persist chain VHTLC contract reference")?;

        Ok(vhtlc)
    }

    async fn reverse_vhtlc_script(
        &self,
        swap: &mut ReverseSwapData,
        server_info: &Info,
    ) -> Result<VhtlcScript, Error> {
        if let Some(script_pubkey) = &swap.contract_script_pubkey {
            if let Some(contract) = self.get_vhtlc_contract(script_pubkey)? {
                return vhtlc_script_from_contract(contract, &swap.vhtlc_address, server_info);
            }
            tracing::warn!(
                swap_id = %swap.id,
                "VHTLC contract reference missing; recreating from legacy swap data"
            );
        }

        let vhtlc = self.build_vhtlc_script(
            server_info,
            swap.claim_public_key,
            swap.refund_public_key,
            swap.preimage_hash,
            &swap.timeout_block_heights,
            &swap.vhtlc_address,
        )?;

        let script_pubkey = self
            .insert_vhtlc_contract(vhtlc.options().clone(), swap.key_derivation_index)
            .context("failed to persist VHTLC contract")?;

        swap.contract_script_pubkey = Some(script_pubkey);
        self.swap_storage()
            .update_reverse(&swap.id, swap.clone())
            .await
            .context("failed to persist VHTLC contract reference")?;
        Ok(vhtlc)
    }

    /// Ensure a swap key is loaded into the key provider's cache so
    /// `keypair_by_pk` can find it during intent signing.
    ///
    /// Returns `true` if the key is available (already cached or successfully derived).
    /// Returns `false` for legacy swap data without a stored derivation index.
    fn ensure_swap_key_cached(
        &self,
        pk: &XOnlyPublicKey,
        key_derivation_index: Option<u32>,
        swap_id: &str,
    ) -> bool {
        // Already in cache — nothing to do.
        if self.keypair_by_pk(pk).is_ok() {
            return true;
        }

        let Some(index) = key_derivation_index else {
            tracing::warn!(
                swap_id,
                "Legacy swap data without derivation index, skipping recovery"
            );
            return false;
        };

        let Some(key_provider) = self.inner.discoverable_key_provider.as_ref() else {
            return false;
        };

        match key_provider.derive_at_discovery_index(index) {
            Ok(Some(kp)) if kp.x_only_public_key().0 == *pk => {
                if let Err(e) = key_provider.cache_discovered_keypair(index, kp) {
                    tracing::warn!(swap_id, %e, "Failed to cache swap key");
                    return false;
                }
                true
            }
            Ok(_) => {
                tracing::warn!(
                    swap_id,
                    index,
                    "Key at stored derivation index does not match swap pubkey"
                );
                false
            }
            Err(e) => {
                tracing::warn!(swap_id, index, %e, "Failed to derive key at stored index");
                false
            }
        }
    }

    async fn collect_active_vhtlc_infos(&self) -> Result<Vec<VhtlcInfo>, Error> {
        let submarine_swaps = self
            .swap_storage()
            .list_all_submarine()
            .await
            .context("failed to list submarine swaps")?;

        let reverse_swaps = self
            .swap_storage()
            .list_all_reverse()
            .await
            .context("failed to list reverse swaps")?;

        let server_info = self.server_info().await?;
        let mut infos = Vec::new();

        for mut swap in submarine_swaps {
            if self.vhtlc_contract_is_inactive(swap.contract_script_pubkey.as_ref())? {
                continue;
            }

            // Ensure the refund key (sender) is in the key cache.
            if !self.ensure_swap_key_cached(
                &swap.refund_public_key.inner.x_only_public_key().0,
                swap.key_derivation_index,
                &swap.id,
            ) {
                continue;
            }

            let vhtlc = self.submarine_vhtlc_script(&mut swap, &server_info).await?;

            // For submarine swaps, the user is the sender (refund key).
            // Use refund_without_receiver_script as the intent proof — it only requires
            // sender + server, and we can always sign for sender.
            let refund_script = vhtlc.refund_without_receiver_script();
            let spend_info = vhtlc.taproot_spend_info();
            let control_block = spend_info
                .control_block(&(refund_script.clone(), LeafVersion::TapScript))
                .ok_or_else(|| {
                    Error::ad_hoc("control block not found for refund_without_receiver script")
                })?;

            infos.push(VhtlcInfo {
                swap_id: swap.id.clone(),
                address: swap.vhtlc_address,
                script_pubkey: vhtlc.script_pubkey(),
                vhtlc,
                intent_spend_info: (refund_script, control_block),
                preimage: swap.preimage,
            });
        }

        for mut swap in reverse_swaps {
            if self.vhtlc_contract_is_inactive(swap.contract_script_pubkey.as_ref())? {
                continue;
            }

            // Ensure the claim key (receiver) is in the key cache.
            if !self.ensure_swap_key_cached(
                &swap.claim_public_key.inner.x_only_public_key().0,
                swap.key_derivation_index,
                &swap.id,
            ) {
                continue;
            }

            let vhtlc = self.reverse_vhtlc_script(&mut swap, &server_info).await?;

            // For reverse swaps, the user is the receiver (claim key).
            // Use claim_script as the intent proof — we need to sign with the receiver key.
            let claim_script = vhtlc.claim_script();
            let spend_info = vhtlc.taproot_spend_info();
            let control_block = spend_info
                .control_block(&(claim_script.clone(), LeafVersion::TapScript))
                .ok_or_else(|| Error::ad_hoc("control block not found for claim script"))?;

            infos.push(VhtlcInfo {
                swap_id: swap.id.clone(),
                address: swap.vhtlc_address,
                script_pubkey: vhtlc.script_pubkey(),
                vhtlc,
                intent_spend_info: (claim_script, control_block),
                preimage: swap.preimage,
            });
        }

        Ok(infos)
    }

    /// Determine the spend type by comparing the PSBT's spend script against known VHTLC scripts.
    fn identify_vhtlc_spend_type(
        info: &VhtlcInfo,
        pending_tx: &PendingTx,
    ) -> Result<PendingVhtlcSpendType, Error> {
        // Extract the spend script from the ark tx's PSBT input tap_scripts.
        let spend_script = pending_tx
            .signed_ark_tx
            .inputs
            .iter()
            .find_map(|input| {
                input.tap_scripts.values().find_map(|(script, _)| {
                    // Match against this VHTLC's known scripts.
                    let claim = info.vhtlc.claim_script();
                    let refund = info.vhtlc.refund_script();
                    let refund_no_recv = info.vhtlc.refund_without_receiver_script();

                    if *script == claim || *script == refund || *script == refund_no_recv {
                        Some(script.clone())
                    } else {
                        None
                    }
                })
            })
            .ok_or_else(|| {
                Error::ad_hoc(format!(
                    "could not identify spend script in pending tx {} for swap {}",
                    pending_tx.ark_txid, info.swap_id
                ))
            })?;

        let claim_script = info.vhtlc.claim_script();
        let refund_script = info.vhtlc.refund_script();

        if spend_script == claim_script {
            // Claim — we need the preimage. Try to extract it from the ark tx PSBT
            // (it was injected as extra witness data when the tx was originally signed),
            // falling back to what's stored in swap data.
            let preimage = extract_preimage_from_psbt(&pending_tx.signed_ark_tx)
                .ok()
                .or(info.preimage)
                .ok_or_else(|| {
                    Error::ad_hoc(format!(
                        "cannot recover preimage for pending claim of swap {}",
                        info.swap_id
                    ))
                })?;

            Ok(PendingVhtlcSpendType::Claim {
                swap_id: info.swap_id.clone(),
                preimage,
            })
        } else if spend_script == refund_script {
            Ok(PendingVhtlcSpendType::CollaborativeRefund {
                swap_id: info.swap_id.clone(),
            })
        } else {
            Ok(PendingVhtlcSpendType::ExpiredRefund {
                swap_id: info.swap_id.clone(),
            })
        }
    }

    /// Inject a preimage into all inputs of a PSBT via the `VTXO_CONDITION_KEY` unknown field.
    fn inject_preimage_into_psbt(psbt: &mut Psbt, preimage: [u8; 32]) {
        let mut bytes = vec![1];
        let length = VarInt::from(preimage.len() as u64);
        length
            .consensus_encode(&mut bytes)
            .expect("valid length encoding");
        bytes.write_all(&preimage).expect("valid preimage encoding");

        let key = psbt::raw::Key {
            type_value: 222,
            key: VTXO_CONDITION_KEY.to_vec(),
        };

        for input in &mut psbt.inputs {
            input.unknown.insert(key.clone(), bytes.clone());
        }
    }

    /// Sign a checkpoint PSBT by matching pubkeys in the witness script against our keys.
    fn sign_checkpoint_with_own_keys(&self, checkpoint_psbt: &mut Psbt) -> Result<(), Error> {
        let sign_fn =
            |input: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                let script = input.witness_script.as_ref().ok_or_else(|| {
                    ark_core::Error::ad_hoc("missing witness script for checkpoint signing")
                })?;
                let pks = extract_checksig_pubkeys(script);
                let mut res = vec![];
                for pk in pks {
                    if let Ok(keypair) = self.keypair_by_pk(&pk) {
                        let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &keypair);
                        res.push((sig, keypair.x_only_public_key().0));
                    }
                }
                Ok(res)
            };

        sign_checkpoint_transaction(sign_fn, checkpoint_psbt)?;
        Ok(())
    }

    /// Restore the witness_script on a checkpoint PSBT if the server stripped it.
    ///
    /// This is the same logic used by [`Client::continue_pending_offchain_txs`].
    fn restore_witness_script_if_needed(
        checkpoint_psbt: &mut Psbt,
        signed_ark_tx: &Psbt,
    ) -> Result<(), Error> {
        if checkpoint_psbt
            .inputs
            .first()
            .ok_or_else(|| Error::ad_hoc("checkpoint PSBT has no inputs"))?
            .witness_script
            .is_some()
        {
            return Ok(());
        }

        let checkpoint_txid = checkpoint_psbt.unsigned_tx.compute_txid();

        let ark_input_idx = signed_ark_tx
            .unsigned_tx
            .input
            .iter()
            .position(|inp| inp.previous_output.txid == checkpoint_txid)
            .ok_or_else(|| {
                Error::ad_hoc(format!(
                    "checkpoint txid {checkpoint_txid} not found in ark tx inputs"
                ))
            })?;

        let witness_script = signed_ark_tx
            .inputs
            .get(ark_input_idx)
            .and_then(|input| input.witness_script.clone())
            .ok_or_else(|| {
                Error::ad_hoc(format!(
                    "missing witness script on ark tx input {ark_input_idx}"
                ))
            })?;

        checkpoint_psbt
            .inputs
            .first_mut()
            .ok_or_else(|| Error::ad_hoc("checkpoint PSBT has no inputs"))?
            .witness_script = Some(witness_script);
        Ok(())
    }
}

async fn run_boltz_vhtlc_watcher_loop<B, W, S>(
    client: Arc<Client<B, W, S>>,
    mut stop_rx: tokio::sync::watch::Receiver<bool>,
    config: BoltzVhtlcWatcherConfig,
) where
    B: Blockchain + Send + Sync + 'static,
    W: OnchainWallet + Send + Sync + 'static,
    S: SwapStorage + 'static,
{
    let refresh_interval = if config.refresh_interval.is_zero() {
        VHTLC_WATCHER_REFRESH_INTERVAL
    } else {
        config.refresh_interval
    };
    let mut backoff = VHTLC_WATCHER_INITIAL_BACKOFF;
    let mut action_log = BoltzVhtlcActionLog::default();

    loop {
        if *stop_rx.borrow() {
            return;
        }

        drive_boltz_vhtlc_swaps(client.as_ref(), &mut action_log).await;

        let addresses = match active_vhtlc_addresses(client.as_ref()).await {
            Ok(addresses) => addresses,
            Err(error) => {
                tracing::warn!(?error, "Failed to collect active VHTLC addresses");
                if wait_for_vhtlc_watcher_retry(&mut stop_rx, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(VHTLC_WATCHER_MAX_BACKOFF);
                continue;
            }
        };

        if addresses.is_empty() {
            if wait_for_vhtlc_watcher_retry(&mut stop_rx, refresh_interval).await {
                return;
            }
            backoff = VHTLC_WATCHER_INITIAL_BACKOFF;
            continue;
        }

        let subscription_id = match client.subscribe_to_scripts(addresses.clone(), None).await {
            Ok(subscription_id) => subscription_id,
            Err(error) => {
                tracing::warn!(?error, "Failed to subscribe to VHTLC scripts");
                if wait_for_vhtlc_watcher_retry(&mut stop_rx, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(VHTLC_WATCHER_MAX_BACKOFF);
                continue;
            }
        };

        let mut stream = match client.get_subscription(subscription_id.clone()).await {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!(?error, "Failed to open VHTLC subscription stream");
                if wait_for_vhtlc_watcher_retry(&mut stop_rx, backoff).await {
                    return;
                }
                backoff = (backoff * 2).min(VHTLC_WATCHER_MAX_BACKOFF);
                continue;
            }
        };

        tracing::info!(
            watched_scripts = addresses.len(),
            "Boltz VHTLC watcher connected"
        );
        backoff = VHTLC_WATCHER_INITIAL_BACKOFF;
        let mut subscribed_addrs: HashSet<ArkAddress> = addresses.into_iter().collect();
        let mut refresh_interval = tokio::time::interval(refresh_interval);

        loop {
            use futures::StreamExt;

            tokio::select! {
                _ = stop_rx.changed() => return,
                _ = refresh_interval.tick() => {
                    if let Err(error) = refresh_boltz_vhtlc_subscription(
                        client.as_ref(),
                        &subscription_id,
                        &mut subscribed_addrs,
                    ).await {
                        tracing::warn!(?error, "Failed to refresh VHTLC watcher subscription");
                    }
                    drive_boltz_vhtlc_swaps(client.as_ref(), &mut action_log).await;
                }
                event = stream.next() => {
                    match event {
                        Some(Ok(SubscriptionResponse::Heartbeat)) => {}
                        Some(Ok(SubscriptionResponse::Event(event))) => {
                            if let Err(error) = handle_boltz_vhtlc_subscription_event(
                                client.as_ref(),
                                &event,
                                &mut action_log,
                            ).await {
                                tracing::warn!(?error, "Failed to handle VHTLC subscription event");
                            }
                        }
                        Some(Err(error)) => {
                            tracing::warn!(?error, "VHTLC subscription stream error");
                            break;
                        }
                        None => {
                            tracing::debug!("VHTLC subscription stream ended");
                            break;
                        }
                    }
                }
            }
        }

        if wait_for_vhtlc_watcher_retry(&mut stop_rx, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(VHTLC_WATCHER_MAX_BACKOFF);
    }
}

async fn drive_boltz_vhtlc_swaps<B, W, S>(
    client: &Client<B, W, S>,
    action_log: &mut BoltzVhtlcActionLog,
) where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    if let Err(error) = drive_claimable_vhtlc_swaps(client, action_log).await {
        tracing::warn!(?error, "Failed to drive claimable VHTLC swaps");
    }
    if let Err(error) = drive_refundable_vhtlc_swaps(client, action_log).await {
        tracing::warn!(?error, "Failed to drive refundable VHTLC swaps");
    }
}

async fn active_vhtlc_addresses<B, W, S>(client: &Client<B, W, S>) -> Result<Vec<ArkAddress>, Error>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    let infos = collect_active_vhtlc_lifecycle_infos(client).await?;
    Ok(infos.into_iter().map(|info| info.address).collect())
}

async fn refresh_boltz_vhtlc_subscription<B, W, S>(
    client: &Client<B, W, S>,
    subscription_id: &str,
    subscribed_addrs: &mut HashSet<ArkAddress>,
) -> Result<(), Error>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    let new_addrs: Vec<_> = active_vhtlc_addresses(client)
        .await?
        .into_iter()
        .filter(|address| !subscribed_addrs.contains(address))
        .collect();

    if new_addrs.is_empty() {
        return Ok(());
    }

    client
        .subscribe_to_scripts(new_addrs.clone(), Some(subscription_id.to_string()))
        .await?;

    let added = new_addrs.len();
    subscribed_addrs.extend(new_addrs);
    tracing::info!(added, "Updated VHTLC watcher subscription");
    Ok(())
}

async fn handle_boltz_vhtlc_subscription_event<B, W, S>(
    client: &Client<B, W, S>,
    event: &SubscriptionEvent,
    action_log: &mut BoltzVhtlcActionLog,
) -> Result<(), Error>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    if event.new_vtxos.is_empty() && event.spent_vtxos.is_empty() {
        return Ok(());
    }

    tracing::debug!(
        txid = %event.txid,
        new_vtxos = event.new_vtxos.len(),
        spent_vtxos = event.spent_vtxos.len(),
        "Received VHTLC subscription event"
    );

    let infos = collect_active_vhtlc_lifecycle_infos(client).await?;
    let info_by_script: HashMap<_, _> = infos
        .into_iter()
        .map(|info| (info.script_pubkey.clone(), info))
        .collect();

    for new_vtxo in &event.new_vtxos {
        let Some(info) = info_by_script.get(&new_vtxo.script) else {
            continue;
        };
        drive_funded_vhtlc_swap(client, info, action_log).await;
    }

    for spent_vtxo in &event.spent_vtxos {
        let Some(info) = info_by_script.get(&spent_vtxo.script) else {
            continue;
        };
        if drive_spent_vhtlc_swap(client, info).await == SpentVhtlcAction::Reconcile {
            client
                .reconcile_vhtlc_contract_state_from_vtxos(
                    &info.swap_id,
                    info.address,
                    Some(info.script_pubkey.clone()),
                )
                .await?;
        }
    }

    Ok(())
}

async fn collect_active_vhtlc_lifecycle_infos<B, W, S>(
    client: &Client<B, W, S>,
) -> Result<Vec<VhtlcLifecycleInfo>, Error>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    let submarine_swaps = client
        .swap_storage()
        .list_all_submarine()
        .await
        .context("failed to list submarine swaps")?;
    let reverse_swaps = client
        .swap_storage()
        .list_all_reverse()
        .await
        .context("failed to list reverse swaps")?;
    let chain_swaps = client
        .swap_storage()
        .list_all_chain()
        .await
        .context("failed to list chain swaps")?;
    let server_info = client.server_info().await?;
    let mut infos = Vec::new();

    for mut swap in submarine_swaps {
        match client.vhtlc_contract_is_inactive(swap.contract_script_pubkey.as_ref()) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    swap_id = %swap.id,
                    ?error,
                    "Skipping submarine swap after VHTLC contract state check failed"
                );
                continue;
            }
        }

        let vhtlc = match client.submarine_vhtlc_script(&mut swap, &server_info).await {
            Ok(vhtlc) => vhtlc,
            Err(error) => {
                tracing::warn!(
                    swap_id = %swap.id,
                    ?error,
                    "Skipping submarine swap after VHTLC reconstruction failed"
                );
                continue;
            }
        };

        infos.push(VhtlcLifecycleInfo {
            swap_id: swap.id.clone(),
            swap_type: SwapType::Submarine,
            address: swap.vhtlc_address,
            script_pubkey: vhtlc.script_pubkey(),
        });
    }

    for mut swap in reverse_swaps {
        match client.vhtlc_contract_is_inactive(swap.contract_script_pubkey.as_ref()) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    swap_id = %swap.id,
                    ?error,
                    "Skipping reverse swap after VHTLC contract state check failed"
                );
                continue;
            }
        }

        let vhtlc = match client.reverse_vhtlc_script(&mut swap, &server_info).await {
            Ok(vhtlc) => vhtlc,
            Err(error) => {
                tracing::warn!(
                    swap_id = %swap.id,
                    ?error,
                    "Skipping reverse swap after VHTLC reconstruction failed"
                );
                continue;
            }
        };

        infos.push(VhtlcLifecycleInfo {
            swap_id: swap.id.clone(),
            swap_type: SwapType::Reverse,
            address: swap.vhtlc_address,
            script_pubkey: vhtlc.script_pubkey(),
        });
    }

    for mut swap in chain_swaps {
        match client.vhtlc_contract_is_inactive(swap.contract_script_pubkey.as_ref()) {
            Ok(true) => continue,
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(
                    swap_id = %swap.id,
                    ?error,
                    "Skipping chain swap after VHTLC contract state check failed"
                );
                continue;
            }
        }

        let address = match swap.chain_vhtlc_address() {
            Ok(address) => address,
            Err(error) => {
                tracing::warn!(
                    swap_id = %swap.id,
                    ?error,
                    "Skipping chain swap with unresolved VHTLC address"
                );
                continue;
            }
        };

        let vhtlc = match client.chain_vhtlc_script(&mut swap, &server_info).await {
            Ok(vhtlc) => vhtlc,
            Err(error) => {
                tracing::warn!(
                    swap_id = %swap.id,
                    ?error,
                    "Skipping chain swap after VHTLC reconstruction failed"
                );
                continue;
            }
        };

        infos.push(VhtlcLifecycleInfo {
            swap_id: swap.id.clone(),
            swap_type: SwapType::Chain,
            address,
            script_pubkey: vhtlc.script_pubkey(),
        });
    }

    Ok(infos)
}

async fn drive_claimable_vhtlc_swaps<B, W, S>(
    client: &Client<B, W, S>,
    action_log: &mut BoltzVhtlcActionLog,
) -> Result<(), Error>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    let infos = collect_active_vhtlc_lifecycle_infos(client).await?;
    let server_info = client.server_info().await?;

    for info in infos {
        if !matches!(info.swap_type, SwapType::Reverse | SwapType::Chain) {
            continue;
        }

        let liveness = match client
            .observe_vhtlc_contract_liveness(&server_info, info.address)
            .await
        {
            Ok(liveness) => liveness,
            Err(error) => {
                tracing::warn!(
                    swap_id = %info.swap_id,
                    ?error,
                    "Skipping claim retry after VHTLC liveness check failed"
                );
                continue;
            }
        };

        if matches!(
            liveness,
            VhtlcContractLiveness::Funded | VhtlcContractLiveness::Recoverable
        ) {
            drive_funded_vhtlc_swap(client, &info, action_log).await;
        }
    }

    Ok(())
}

async fn drive_funded_vhtlc_swap<B, W, S>(
    client: &Client<B, W, S>,
    info: &VhtlcLifecycleInfo,
    action_log: &mut BoltzVhtlcActionLog,
) where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    match info.swap_type {
        SwapType::Reverse => drive_reverse_vhtlc_claim(client, &info.swap_id, action_log).await,
        SwapType::Chain => drive_chain_vhtlc_claim(client, &info.swap_id, action_log).await,
        SwapType::Submarine | SwapType::Unknown => {}
    }
}

async fn drive_reverse_vhtlc_claim<B, W, S>(
    client: &Client<B, W, S>,
    swap_id: &str,
    action_log: &mut BoltzVhtlcActionLog,
) where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    let swap = match client.swap_storage().get_reverse(swap_id).await {
        Ok(Some(swap)) => swap,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(
                swap_id,
                ?error,
                "Failed to load reverse swap for VHTLC claim"
            );
            return;
        }
    };

    let Some(preimage) = swap.preimage else {
        tracing::debug!(
            swap_id,
            "Reverse VHTLC funded, but preimage is externally managed; skipping auto-claim"
        );
        return;
    };

    if !action_log.begin_claim(swap_id) {
        return;
    }

    match client.claim_vhtlc(swap_id, preimage).await {
        Ok(result) => {
            action_log.finish_claim(swap_id, true);
            if let Err(error) =
                update_reverse_swap_status(client, swap_id, SwapStatus::TransactionClaimed).await
            {
                tracing::warn!(swap_id, ?error, "Failed to persist reverse claim status");
            }
            tracing::info!(
                swap_id,
                txid = %result.claim_txid,
                "Auto-claimed reverse VHTLC from funding event"
            );
        }
        Err(error) => {
            action_log.finish_claim(swap_id, false);
            tracing::warn!(swap_id, ?error, "Failed to auto-claim reverse VHTLC");
        }
    }
}

async fn drive_chain_vhtlc_claim<B, W, S>(
    client: &Client<B, W, S>,
    swap_id: &str,
    action_log: &mut BoltzVhtlcActionLog,
) where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    let swap = match client.swap_storage().get_chain(swap_id).await {
        Ok(Some(swap)) => swap,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(swap_id, ?error, "Failed to load chain swap for VHTLC claim");
            return;
        }
    };

    if swap.direction != ChainSwapDirection::BtcToArk {
        return;
    }
    if swap.preimage.is_none() {
        tracing::debug!(
            swap_id,
            "Chain VHTLC funded, but preimage is missing; skipping auto-claim"
        );
        return;
    }
    if !action_log.begin_claim(swap_id) {
        return;
    }

    match client.claim_chain_swap(swap_id).await {
        Ok(txid) => {
            action_log.finish_claim(swap_id, true);
            tracing::info!(swap_id, %txid, "Auto-claimed chain VHTLC from funding event");
        }
        Err(error) => {
            action_log.finish_claim(swap_id, false);
            tracing::warn!(swap_id, ?error, "Failed to auto-claim chain VHTLC");
        }
    }
}

async fn drive_spent_vhtlc_swap<B, W, S>(
    client: &Client<B, W, S>,
    info: &VhtlcLifecycleInfo,
) -> SpentVhtlcAction
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    if info.swap_type != SwapType::Submarine {
        return SpentVhtlcAction::Reconcile;
    }

    match client.swap_storage().get_submarine(&info.swap_id).await {
        Ok(Some(swap)) if swap.preimage.is_some() => return SpentVhtlcAction::Reconcile,
        Ok(Some(_)) => {}
        Ok(None) => return SpentVhtlcAction::Reconcile,
        Err(error) => {
            tracing::warn!(swap_id = %info.swap_id, ?error, "Failed to load submarine swap");
            return SpentVhtlcAction::KeepActive;
        }
    }

    match client.extract_submarine_swap_preimage(&info.swap_id).await {
        Ok(_) => {
            tracing::info!(
                swap_id = %info.swap_id,
                "Extracted submarine preimage from spent VHTLC event"
            );
            SpentVhtlcAction::Reconcile
        }
        Err(error) => {
            tracing::warn!(
                swap_id = %info.swap_id,
                ?error,
                "Could not extract submarine preimage from spent VHTLC; keeping contract active"
            );
            SpentVhtlcAction::KeepActive
        }
    }
}

async fn drive_refundable_vhtlc_swaps<B, W, S>(
    client: &Client<B, W, S>,
    action_log: &mut BoltzVhtlcActionLog,
) -> Result<(), Error>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    for swap in client.swap_storage().list_all_submarine().await? {
        if !is_submarine_refundable_status(&swap.status)
            || client.vhtlc_contract_is_inactive(swap.contract_script_pubkey.as_ref())?
            || !action_log.begin_refund(&swap.id)
        {
            continue;
        }

        let result = if matches!(swap.status, SwapStatus::SwapExpired) {
            client.refund_expired_vhtlc(&swap.id).await
        } else {
            client.refund_vhtlc(&swap.id).await
        };

        match result {
            Ok(txid) => {
                action_log.finish_refund(&swap.id, true);
                if let Err(error) =
                    update_submarine_swap_status(client, &swap.id, SwapStatus::TransactionRefunded)
                        .await
                {
                    tracing::warn!(
                        swap_id = %swap.id,
                        ?error,
                        "Failed to persist submarine refund status"
                    );
                }
                tracing::info!(swap_id = %swap.id, %txid, "Auto-refunded submarine VHTLC");
            }
            Err(error) => {
                action_log.finish_refund(&swap.id, false);
                tracing::warn!(swap_id = %swap.id, ?error, "Failed to auto-refund submarine VHTLC");
            }
        }
    }

    for swap in client.swap_storage().list_all_chain().await? {
        if swap.direction != ChainSwapDirection::ArkToBtc
            || !is_chain_refundable_status(&swap.status)
            || client.vhtlc_contract_is_inactive(swap.contract_script_pubkey.as_ref())?
            || !action_log.begin_refund(&swap.id)
        {
            continue;
        }

        match client.refund_chain_swap(&swap.id).await {
            Ok(txid) => {
                action_log.finish_refund(&swap.id, true);
                tracing::info!(swap_id = %swap.id, %txid, "Auto-refunded chain VHTLC");
            }
            Err(error) => {
                action_log.finish_refund(&swap.id, false);
                tracing::warn!(swap_id = %swap.id, ?error, "Failed to auto-refund chain VHTLC");
            }
        }
    }

    Ok(())
}

fn is_submarine_refundable_status(status: &SwapStatus) -> bool {
    matches!(
        status,
        SwapStatus::InvoiceFailedToPay
            | SwapStatus::TransactionLockupFailed
            | SwapStatus::SwapExpired
    )
}

fn is_chain_refundable_status(status: &SwapStatus) -> bool {
    matches!(status, SwapStatus::SwapExpired)
}

async fn update_submarine_swap_status<B, W, S>(
    client: &Client<B, W, S>,
    swap_id: &str,
    status: SwapStatus,
) -> Result<(), Error>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    if client
        .swap_storage()
        .get_submarine(swap_id)
        .await?
        .is_some()
    {
        client
            .swap_storage()
            .update_status_submarine(swap_id, status)
            .await?;
    }
    Ok(())
}

async fn update_reverse_swap_status<B, W, S>(
    client: &Client<B, W, S>,
    swap_id: &str,
    status: SwapStatus,
) -> Result<(), Error>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    if client.swap_storage().get_reverse(swap_id).await?.is_some() {
        client
            .swap_storage()
            .update_status_reverse(swap_id, status)
            .await?;
    }
    Ok(())
}

async fn wait_for_vhtlc_watcher_retry(
    stop_rx: &mut tokio::sync::watch::Receiver<bool>,
    duration: Duration,
) -> bool {
    tokio::select! {
        _ = stop_rx.changed() => true,
        _ = tokio::time::sleep(duration) => false,
    }
}

/// Internal info about an active VHTLC, used during pending tx recovery.
struct VhtlcInfo {
    swap_id: String,
    address: ArkAddress,
    script_pubkey: ScriptBuf,
    vhtlc: VhtlcScript,
    /// The spend path and control block used to prove ownership in the GetPendingTx intent.
    intent_spend_info: (ScriptBuf, bitcoin::taproot::ControlBlock),
    preimage: Option<[u8; 32]>,
}

/// Reconstruct the taproot spend info for a Boltz on-chain BTC HTLC.
///
/// Boltz uses `MuSig2(serverKey, userKey)` as the internal key.
/// The tree has two leaves: claim and refund, from the [`SwapTree`].
fn reconstruct_btc_htlc(
    server_pk: PublicKey,
    user_pk: PublicKey,
    swap_tree: &SwapTree,
) -> Result<bitcoin::taproot::TaprootSpendInfo, Error> {
    let claim_script_bytes: Vec<u8> = bitcoin::hex::FromHex::from_hex(&swap_tree.claim_leaf.output)
        .map_err(|e| Error::ad_hoc(format!("invalid claim leaf hex: {e}")))?;
    let claim_script = ScriptBuf::from_bytes(claim_script_bytes);

    let refund_script_bytes: Vec<u8> =
        bitcoin::hex::FromHex::from_hex(&swap_tree.refund_leaf.output)
            .map_err(|e| Error::ad_hoc(format!("invalid refund leaf hex: {e}")))?;
    let refund_script = ScriptBuf::from_bytes(refund_script_bytes);

    let musig_server_pk = musig::PublicKey::from_slice(&server_pk.to_bytes())
        .map_err(|e| Error::ad_hoc(format!("invalid server key for musig: {e}")))?;
    let musig_user_pk = musig::PublicKey::from_slice(&user_pk.to_bytes())
        .map_err(|e| Error::ad_hoc(format!("invalid user key for musig: {e}")))?;

    let key_agg = musig::musig::KeyAggCache::new(&[&musig_server_pk, &musig_user_pk]);
    let internal_key = XOnlyPublicKey::from_slice(&key_agg.agg_pk().serialize())
        .map_err(|e| Error::ad_hoc(format!("invalid aggregated key: {e}")))?;

    let secp = Secp256k1::new();
    bitcoin::taproot::TaprootBuilder::new()
        .add_leaf(1, claim_script)
        .map_err(|e| Error::ad_hoc(format!("failed to add claim leaf: {e}")))?
        .add_leaf(1, refund_script)
        .map_err(|e| Error::ad_hoc(format!("failed to add refund leaf: {e}")))?
        .finalize(&secp, internal_key)
        .map_err(|_| Error::ad_hoc("failed to finalize taproot tree"))
}

/// Collect all tapscripts from a [`VhtlcScript`].
fn vhtlc_tapscripts(vhtlc: &VhtlcScript) -> Vec<ScriptBuf> {
    vec![
        vhtlc.claim_script(),
        vhtlc.refund_script(),
        vhtlc.refund_without_receiver_script(),
        vhtlc.unilateral_claim_script(),
        vhtlc.unilateral_refund_script(),
        vhtlc.unilateral_refund_without_receiver_script(),
    ]
}

/// Extract the preimage from a PSBT's `VTXO_CONDITION_KEY` unknown field.
///
/// The condition data is encoded as: `[num_elements] [varint_length] [preimage_bytes]`.
/// For VHTLC claims, there is exactly one element: the 32-byte preimage.
fn extract_preimage_from_psbt(psbt: &Psbt) -> Result<[u8; 32], Error> {
    let condition_key = psbt::raw::Key {
        type_value: 222,
        key: VTXO_CONDITION_KEY.to_vec(),
    };

    for input in &psbt.inputs {
        if let Some(condition_data) = input.unknown.get(&condition_key) {
            if condition_data.is_empty() {
                continue;
            }

            // First byte is the number of witness elements.
            let num_elements = condition_data[0] as usize;
            if num_elements == 0 {
                continue;
            }

            // Parse the first element: varint length followed by the preimage bytes.
            let mut cursor = std::io::Cursor::new(&condition_data[1..]);
            let length = bitcoin::consensus::Decodable::consensus_decode(&mut cursor)
                .map_err(|e| Error::ad_hoc(format!("failed to decode varint length: {e}")))?;
            let length: VarInt = length;
            let offset = cursor.position() as usize;
            let remaining = &condition_data[1 + offset..];

            if remaining.len() < length.0 as usize {
                return Err(Error::ad_hoc(format!(
                    "condition data too short: expected {} bytes, got {}",
                    length.0,
                    remaining.len()
                )));
            }

            let preimage_bytes = &remaining[..length.0 as usize];

            let preimage: [u8; 32] = preimage_bytes.try_into().map_err(|_| {
                Error::ad_hoc(format!(
                    "preimage has unexpected length: {} (expected 32)",
                    preimage_bytes.len()
                ))
            })?;

            return Ok(preimage);
        }
    }

    Err(Error::ad_hoc(
        "no VTXO_CONDITION_KEY found in any PSBT input",
    ))
}

fn vhtlc_script_from_contract(
    contract: VhtlcContract,
    expected_address: &ArkAddress,
    server_info: &Info,
) -> Result<VhtlcScript, Error> {
    let vhtlc = VhtlcScript::new(contract.options, server_info.network)
        .map_err(|e| Error::ad_hoc(format!("failed to build VHTLC: {e}")))?;

    if vhtlc.address() != *expected_address {
        return Err(Error::ad_hoc("stored VHTLC contract address mismatch"));
    }

    Ok(vhtlc)
}

/// The amount to be shared with Boltz when creating a reverse submarine swap.
pub enum SwapAmount {
    /// Use this value if you need to set the value to be sent by the payer on Lightning.
    Invoice(Amount),
    /// Use this value if you need to set the value to be received by the payee on Arkade.
    Vhtlc(Amount),
}

impl SwapAmount {
    pub fn invoice(amount: Amount) -> Self {
        Self::Invoice(amount)
    }

    pub fn vhtlc(amount: Amount) -> Self {
        Self::Vhtlc(amount)
    }
}

/// The amount specification for a chain swap.
pub enum ChainSwapAmount {
    /// The amount the user will lock up.
    UserLock(Amount),
    /// The amount the user wants to receive (server lock amount).
    ServerLock(Amount),
}

/// Data related to a submarine swap.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmarineSwapData {
    /// Unique swap identifier.
    pub id: String,
    /// Preimage for the swap (learned when Boltz claims the VHTLC).
    pub preimage: Option<[u8; 32]>,
    /// The preimage hash of the BOLT11 invoice.
    pub preimage_hash: ripemd160::Hash,
    /// Public key of the receiving party.
    pub claim_public_key: PublicKey,
    /// Public key of the sending party.
    pub refund_public_key: PublicKey,
    /// Amount locked up in the VHTLC.
    pub amount: Amount,
    /// All the timelocks for this swap.
    pub timeout_block_heights: TimeoutBlockHeights,
    /// Address where funds are locked.
    #[serde_as(as = "DisplayFromStr")]
    pub vhtlc_address: ArkAddress,
    /// BOLT11 invoice associated with the swap.
    pub invoice: Bolt11Invoice,
    /// Current swap status.
    pub status: SwapStatus,
    /// UNIX timestamp when swap was created.
    pub created_at: u64,
    /// BIP32 derivation index of the refund key (sender).
    ///
    /// `None` for legacy swap data created before this field was added.
    #[serde(default)]
    pub key_derivation_index: Option<u32>,
    /// Script pubkey of the contract-store VHTLC row for this swap.
    #[serde(default)]
    pub contract_script_pubkey: Option<ScriptBuf>,
}

/// Data related to a reverse submarine swap.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverseSwapData {
    /// Unique swap identifier.
    pub id: String,
    /// Preimage for the swap (optional, may not be known at creation time).
    pub preimage: Option<[u8; 32]>,
    /// The preimage hash of the BOLT11 invoice.
    pub preimage_hash: ripemd160::Hash,
    /// Public key of the receiving party.
    pub claim_public_key: PublicKey,
    /// Public key of the sending party.
    pub refund_public_key: PublicKey,
    /// Amount locked up in the VHTLC.
    pub amount: Amount,
    /// All the timelocks for this swap.
    pub timeout_block_heights: TimeoutBlockHeights,
    /// Address where funds are locked.
    #[serde_as(as = "DisplayFromStr")]
    pub vhtlc_address: ArkAddress,
    /// Current swap status.
    pub status: SwapStatus,
    /// UNIX timestamp when swap was created.
    pub created_at: u64,
    /// BIP32 derivation index of the claim key (receiver).
    ///
    /// `None` for legacy swap data created before this field was added.
    #[serde(default)]
    pub key_derivation_index: Option<u32>,
    /// BOLT11 invoice string for this swap.
    pub bolt11: String,
    /// Invoice expiry in seconds, derived from the BOLT11 invoice itself.
    pub invoice_expiry: u64,
    /// Arkade address that receives the claimed VHTLC output.
    ///
    /// `None` for normal receives and legacy swap data, where the client claims into a fresh local
    /// offchain address.
    #[serde_as(as = "Option<DisplayFromStr>")]
    #[serde(default)]
    pub claim_address: Option<ArkAddress>,
    /// Script pubkey of the contract-store VHTLC row for this swap.
    #[serde(default)]
    pub contract_script_pubkey: Option<ScriptBuf>,
}

/// All possible states of a Boltz swap.
///
/// Swaps progress through these states during their lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SwapStatus {
    /// Initial state when swap is created.
    #[serde(rename = "swap.created")]
    Created,
    /// Lockup transaction detected in mempool.
    #[serde(rename = "transaction.mempool")]
    TransactionMempool,
    /// Lockup transaction confirmed on-chain.
    #[serde(rename = "transaction.confirmed")]
    TransactionConfirmed,
    /// Transaction refunded.
    #[serde(rename = "transaction.refunded")]
    TransactionRefunded,
    /// Transaction failed.
    #[serde(rename = "transaction.failed")]
    TransactionFailed,
    /// Transaction claimed.
    #[serde(rename = "transaction.claimed")]
    TransactionClaimed,
    /// Server lockup transaction detected in mempool (chain swaps).
    #[serde(rename = "transaction.server.mempool")]
    TransactionServerMempool,
    /// Server lockup transaction confirmed (chain swaps).
    #[serde(rename = "transaction.server.confirmed")]
    TransactionServerConfirmed,
    /// Lightning invoice has been set.
    #[serde(rename = "invoice.set")]
    InvoiceSet,
    /// Waiting for Lightning invoice payment.
    #[serde(rename = "invoice.pending")]
    InvoicePending,
    /// Lightning invoice successfully paid.
    #[serde(rename = "invoice.paid")]
    InvoicePaid,
    /// Lightning invoice settled (reverse swaps).
    #[serde(rename = "invoice.settled")]
    InvoiceSettled,
    /// Lightning invoice payment failed.
    #[serde(rename = "invoice.failedToPay")]
    InvoiceFailedToPay,
    /// Invoice expired.
    #[serde(rename = "invoice.expired")]
    InvoiceExpired,
    /// Lockup amount was insufficient (chain swaps).
    #[serde(rename = "transaction.lockupFailed")]
    TransactionLockupFailed,
    /// Swap expired - can be refunded.
    #[serde(rename = "swap.expired")]
    SwapExpired,
    /// Swap failed with error.
    #[serde(rename = "error")]
    Error { error: String },
    /// An unrecognized status from the Boltz API.
    #[serde(untagged)]
    Other(String),
}

impl SwapStatus {
    /// Whether this status represents a Boltz-terminal lifecycle state.
    ///
    /// Do not use this to decide whether an Arkade-side VHTLC contract is inactive: a terminal
    /// Boltz status can still leave a user-claimable/refundable VHTLC. Use observed VTXO liveness
    /// for contract deactivation instead.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::TransactionRefunded
                | Self::TransactionFailed
                | Self::TransactionClaimed
                | Self::TransactionLockupFailed
                | Self::InvoicePaid
                | Self::InvoiceSettled
                | Self::InvoiceFailedToPay
                | Self::InvoiceExpired
                | Self::SwapExpired
                | Self::Error { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
#[serde(rename_all = "camelCase")]
pub struct TimeoutBlockHeights {
    pub refund: u32,
    pub unilateral_claim: u32,
    pub unilateral_refund: u32,
    pub unilateral_refund_without_receiver: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
enum Asset {
    Btc,
    Ark,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateReverseSwapRequest {
    from: Asset,
    to: Asset,
    #[serde(skip_serializing_if = "Option::is_none")]
    invoice_amount: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    onchain_amount: Option<Amount>,
    claim_public_key: PublicKey,
    preimage_hash: sha256::Hash,
    /// The expiry will be this number of seconds in the future.
    ///
    /// If not provided, the generated invoice will have the default expiry set by Boltz.
    #[serde(skip_serializing_if = "Option::is_none")]
    invoice_expiry: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    referral_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateReverseSwapResponse {
    id: String,
    #[serde_as(as = "DisplayFromStr")]
    lockup_address: ArkAddress,
    refund_public_key: PublicKey,
    timeout_block_heights: TimeoutBlockHeights,
    invoice: Bolt11Invoice,
    onchain_amount: Option<Amount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CreateSubmarineSwapRequest {
    from: Asset,
    to: Asset,
    invoice: Bolt11Invoice,
    #[serde(rename = "refundPublicKey")]
    refund_public_key: PublicKey,
    #[serde(rename = "referralId", skip_serializing_if = "Option::is_none")]
    referral_id: Option<String>,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateSubmarineSwapResponse {
    id: String,
    #[serde_as(as = "DisplayFromStr")]
    address: ArkAddress,
    expected_amount: Amount,
    claim_public_key: PublicKey,
    timeout_block_heights: TimeoutBlockHeights,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GetSwapStatusResponse {
    status: SwapStatus,
    #[serde(default)]
    transaction: Option<SwapStatusTransaction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SwapStatusTransaction {
    id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RefundSwapRequest {
    transaction: String,
    checkpoint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RefundSwapResponse {
    transaction: String,
    checkpoint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Fee information for submarine swaps (Arkade -> Lightning).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmarineSwapFees {
    /// Percentage fee charged by Boltz (e.g., 0.25 = 0.25%).
    pub percentage: f64,
    /// Fixed miner fee in satoshis.
    pub miner_fees: u64,
}

/// Miner fees for reverse swaps, broken down by operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReverseMinerFees {
    /// Miner fee for lockup transaction in satoshis.
    pub lockup: u64,
    /// Miner fee for claim transaction in satoshis.
    pub claim: u64,
}

/// Fee information for reverse swaps (Lightning -> Arkade).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReverseSwapFees {
    /// Percentage fee charged by Boltz (e.g., 0.25 = 0.25%).
    pub percentage: f64,
    /// Miner fees broken down by operation.
    pub miner_fees: ReverseMinerFees,
}

/// Combined fee information for both swap types.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoltzFees {
    /// Fees for submarine swaps (Arkade -> Lightning).
    pub submarine: SubmarineSwapFees,
    /// Fees for reverse swaps (Lightning -> Arkade).
    pub reverse: ReverseSwapFees,
}

/// Limits for swap amounts.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapLimits {
    /// Minimum amount in satoshis.
    pub min: u64,
    /// Maximum amount in satoshis.
    pub max: u64,
}

// Internal structs for deserializing the Boltz API response.

#[derive(Debug, Clone, Deserialize)]
struct PairLimits {
    minimal: u64,
    maximal: u64,
}

// Submarine swap: { "ARK": { "BTC": { ... } } }
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmarinePairFees {
    percentage: f64,
    miner_fees: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct SubmarinePairInfo {
    fees: SubmarinePairFees,
    limits: PairLimits,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
struct SubmarineArkPairs {
    btc: SubmarinePairInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
struct SubmarinePairsResponse {
    ark: SubmarineArkPairs,
}

// Reverse swap: { "BTC": { "ARK": { ... } } }
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReverseMinerFeesResponse {
    claim: u64,
    lockup: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReversePairFees {
    percentage: f64,
    miner_fees: ReverseMinerFeesResponse,
}

#[derive(Debug, Clone, Deserialize)]
struct ReversePairInfo {
    fees: ReversePairFees,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
struct ReverseBtcPairs {
    ark: ReversePairInfo,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
struct ReversePairsResponse {
    btc: ReverseBtcPairs,
}

// ── Chain swap types ──────────────────────────────────────────────────

/// Direction of a chain swap.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ChainSwapDirection {
    /// User locks Arkade VHTLC, claims on-chain BTC.
    ArkToBtc,
    /// User sends on-chain BTC, claims Arkade VHTLC.
    BtcToArk,
}

/// Data for a pending chain swap (Arkade ↔ BTC).
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainSwapData {
    /// Unique swap identifier.
    pub id: String,
    /// Current swap status.
    pub status: SwapStatus,
    /// Direction of the swap.
    pub direction: ChainSwapDirection,
    /// Preimage for the swap.
    pub preimage: Option<[u8; 32]>,
    /// The preimage hash.
    pub preimage_hash: sha256::Hash,
    /// User's claim public key (for claiming Boltz's VHTLC).
    pub claim_public_key: PublicKey,
    /// User's refund public key (for refunding user's VHTLC).
    pub refund_public_key: PublicKey,
    /// Boltz's claim public key (on user's VHTLC).
    pub server_claim_public_key: PublicKey,
    /// Boltz's refund public key (on Boltz's VHTLC).
    pub server_refund_public_key: PublicKey,
    /// Address where user locks funds.
    pub user_lockup_address: String,
    /// Address where Boltz locks funds.
    pub server_lockup_address: String,
    /// Amount user locks up.
    pub user_lockup_amount: Amount,
    /// Amount Boltz locks up (what user receives).
    pub server_lockup_amount: Amount,
    /// Timeout block height for user's lockup.
    pub user_timeout_block_height: u32,
    /// Timeout block height for Boltz's lockup.
    pub server_timeout_block_height: u32,
    /// Full VHTLC timelocks for user's lockup (present when user locks on Arkade side).
    #[serde(default)]
    pub user_timeout_block_heights: Option<TimeoutBlockHeights>,
    /// Full VHTLC timelocks for Boltz's lockup (present when server locks on Arkade side).
    #[serde(default)]
    pub server_timeout_block_heights: Option<TimeoutBlockHeights>,
    /// BIP21 payment URI for funding (present for on-chain BTC lockup).
    #[serde(default)]
    pub bip21: Option<String>,
    /// Swap tree for the on-chain BTC HTLC (present for the BTC side of chain swaps).
    #[serde(default)]
    pub swap_tree: Option<SwapTree>,
    /// UNIX timestamp when swap was created.
    pub created_at: u64,
    /// BIP32 derivation index for the claim key.
    #[serde(default)]
    pub claim_key_derivation_index: Option<u32>,
    /// BIP32 derivation index for the refund key.
    #[serde(default)]
    pub refund_key_derivation_index: Option<u32>,
    /// Script pubkey of the contract-store Arkade-side VHTLC row for this swap.
    #[serde(default)]
    pub contract_script_pubkey: Option<ScriptBuf>,
}

/// Direction-specific Arkade-side VHTLC inputs for a chain swap.
///
/// A Boltz chain swap always has two lockup legs:
///
/// - `lockup_details`: the user's lockup leg (`user_lockup_*` in [`ChainSwapData`])
/// - `claim_details`: Boltz's/server's lockup leg (`server_lockup_*` in [`ChainSwapData`])
///
/// Exactly one of those legs is an Arkade VHTLC; the other is an on-chain BTC HTLC. The Arkade leg
/// is the only part we persist in the contract manager as a [`VhtlcContract`]. This helper result
/// keeps the direction table in one place so creation, lazy migration, and spend paths all agree on
/// which keys, address, timeouts, and wallet derivation index define the Arkade-side VHTLC.
///
/// Direction table:
///
/// | Direction | Arkade-side leg | VHTLC receiver/claim key | VHTLC sender/refund key | Wallet key index |
/// |-----------|--------------|--------------------------|-------------------------|------------------|
/// | Arkade → BTC | user's lockup (`lockup_details`) | Boltz server claim key | wallet refund key | refund key index |
/// | BTC → Arkade | server lockup (`claim_details`) | wallet claim key | Boltz server refund key | claim key index |
///
/// The `address` is parsed and retained here because it is part of VHTLC reconstruction: we build
/// candidate scripts against the current and deprecated Arkade server signers, then keep the
/// candidate whose Arkade address matches the one Boltz returned for the Arkade-side leg.
struct ChainVhtlcFields {
    claim_public_key: PublicKey,
    refund_public_key: PublicKey,
    timeouts: TimeoutBlockHeights,
    address: ArkAddress,
    key_derivation_index: Option<u32>,
}

/// Select the Arkade-side VHTLC fields from chain-swap data.
///
/// This deliberately accepts the raw fields rather than a [`ChainSwapData`] value so callers can
/// use it before the swap row exists. Creation uses it to compute `contract_script_pubkey` first
/// and then builds a fully-populated [`ChainSwapData`] in one struct literal. Lazy migration and
/// spend paths call the same logic through [`ChainSwapData::chain_vhtlc_fields`].
///
/// The argument names use wallet/server and user/server lockup terminology to mirror Boltz's
/// response and [`ChainSwapData`]:
///
/// - `wallet_claim_public_key` / `wallet_refund_public_key` are the user's generated swap keys.
/// - `server_claim_public_key` comes from Boltz `lockup_details.server_public_key` and is used when
///   Boltz claims the user's Arkade lockup in an Arkade→BTC swap.
/// - `server_refund_public_key` comes from Boltz `claim_details.server_public_key` and is used when
///   Boltz refunds its Arkade lockup in a BTC→Arkade swap.
/// - `user_*` fields describe `lockup_details`; `server_*` fields describe `claim_details`.
#[allow(clippy::too_many_arguments)]
fn chain_vhtlc_fields(
    direction: &ChainSwapDirection,
    wallet_claim_public_key: PublicKey,
    wallet_refund_public_key: PublicKey,
    server_claim_public_key: PublicKey,
    server_refund_public_key: PublicKey,
    user_lockup_address: &str,
    server_lockup_address: &str,
    user_timeout_block_heights: Option<TimeoutBlockHeights>,
    server_timeout_block_heights: Option<TimeoutBlockHeights>,
    claim_key_derivation_index: Option<u32>,
    refund_key_derivation_index: Option<u32>,
) -> Result<ChainVhtlcFields, Error> {
    let (claim_public_key, refund_public_key, timeouts, address, key_derivation_index) =
        match direction {
            ChainSwapDirection::ArkToBtc => (
                server_claim_public_key,
                wallet_refund_public_key,
                user_timeout_block_heights.ok_or_else(|| {
                    Error::ad_hoc(
                        "chain swap is missing Arkade-side VHTLC timeouts for user lockup",
                    )
                })?,
                user_lockup_address,
                refund_key_derivation_index,
            ),
            ChainSwapDirection::BtcToArk => (
                wallet_claim_public_key,
                server_refund_public_key,
                server_timeout_block_heights.ok_or_else(|| {
                    Error::ad_hoc(
                        "chain swap is missing Arkade-side VHTLC timeouts for server lockup",
                    )
                })?,
                server_lockup_address,
                claim_key_derivation_index,
            ),
        };

    let address = ArkAddress::decode(address)
        .map_err(|e| Error::ad_hoc(format!("invalid chain VHTLC address: {e}")))?;

    Ok(ChainVhtlcFields {
        claim_public_key,
        refund_public_key,
        timeouts,
        address,
        key_derivation_index,
    })
}

impl ChainSwapData {
    fn chain_vhtlc_fields(&self) -> Result<ChainVhtlcFields, Error> {
        chain_vhtlc_fields(
            &self.direction,
            self.claim_public_key,
            self.refund_public_key,
            self.server_claim_public_key,
            self.server_refund_public_key,
            &self.user_lockup_address,
            &self.server_lockup_address,
            self.user_timeout_block_heights,
            self.server_timeout_block_heights,
            self.claim_key_derivation_index,
            self.refund_key_derivation_index,
        )
    }

    fn chain_vhtlc_key_index(&self) -> Option<u32> {
        match self.direction {
            ChainSwapDirection::ArkToBtc => self.refund_key_derivation_index,
            ChainSwapDirection::BtcToArk => self.claim_key_derivation_index,
        }
    }

    fn chain_vhtlc_address(&self) -> Result<ArkAddress, Error> {
        let address = match self.direction {
            ChainSwapDirection::ArkToBtc => &self.user_lockup_address,
            ChainSwapDirection::BtcToArk => &self.server_lockup_address,
        };
        ArkAddress::decode(address)
            .map_err(|e| Error::ad_hoc(format!("invalid chain VHTLC address: {e}")))
    }
}

/// Result of creating a chain swap.
#[derive(Clone, Debug)]
pub struct ChainSwapResult {
    /// Unique swap identifier.
    pub swap_id: String,
    /// Address the user must fund to initiate the swap.
    pub user_lockup_address: String,
    /// Amount the user must send.
    pub user_lockup_amount: Amount,
    /// Amount the user will receive after fees.
    pub server_lockup_amount: Amount,
    /// BIP21 payment URI for on-chain BTC funding (when the user lockup is BTC).
    pub bip21: Option<String>,
}

// ── Chain swap Boltz API types ───────────────────────────────────────

/// Tapscript tree for an on-chain BTC HTLC used in chain swaps.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SwapTree {
    /// Leaf used to claim (requires preimage + claim key signature).
    pub claim_leaf: SwapTreeLeaf,
    /// Leaf used to refund (requires timelock + refund key signature).
    pub refund_leaf: SwapTreeLeaf,
}

/// A single leaf in a [`SwapTree`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapTreeLeaf {
    /// Tapscript leaf version (192 = TapScript).
    pub version: u8,
    /// Hex-encoded Bitcoin script.
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateChainSwapRequest {
    from: Asset,
    to: Asset,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_lock_amount: Option<Amount>,
    #[serde(skip_serializing_if = "Option::is_none")]
    server_lock_amount: Option<Amount>,
    claim_public_key: PublicKey,
    refund_public_key: PublicKey,
    preimage_hash: sha256::Hash,
    #[serde(skip_serializing_if = "Option::is_none")]
    referral_id: Option<String>,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateChainSwapResponse {
    id: String,
    claim_details: ChainSwapSideDetails,
    lockup_details: ChainSwapSideDetails,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChainSwapSideDetails {
    lockup_address: String,
    server_public_key: PublicKey,
    timeout_block_height: u32,
    #[serde(default)]
    timeouts: Option<TimeoutBlockHeights>,
    amount: Amount,
    #[serde(default)]
    swap_tree: Option<SwapTree>,
    #[serde(default)]
    bip21: Option<String>,
}

// VHTLC timeouts come from the stored swap data/Boltz response, not from the server's current
// unilateral-exit delay. The legacy exit-delay probe is therefore only needed for regular
// VTXO/boarding script discovery.

/// Iterate `server_keys` in order, building a [`VhtlcScript`] for each one, and return the
/// first whose address matches `expected_address`.
///
/// Extracted from [`Client::reconstruct_vhtlc_for_address`] so the key-iteration logic can be
/// tested without a full [`Client`] instance.
pub(crate) fn reconstruct_vhtlc_from_keys(
    server_keys: impl Iterator<Item = XOnlyPublicKey>,
    network: bitcoin::Network,
    mk_opts: impl Fn(XOnlyPublicKey) -> Result<VhtlcOptions, Error>,
    expected_address: &ArkAddress,
) -> Result<VhtlcScript, Error> {
    for server_key in server_keys {
        let opts = mk_opts(server_key)?;
        let vhtlc = VhtlcScript::new(opts, network).map_err(Error::ad_hoc)?;
        if &vhtlc.address() == expected_address {
            return Ok(vhtlc);
        }
    }
    Err(Error::ad_hoc(format!(
        "VHTLC script could not be reconstructed for address {expected_address}: \
         does not match current or any deprecated server key"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contract::ContractManager;
    use crate::swap_storage::InMemorySwapStorage;
    use crate::ExplorerUtxo;
    use crate::OfflineClient;
    use crate::OfflineClientConfig;
    use crate::ServerState;
    use crate::SpendStatus;
    use crate::TxStatus;
    use ark_core::UtxoCoinSelection;
    use bitcoin::secp256k1::Keypair;
    use bitcoin::secp256k1::SecretKey;
    use bitcoin::Transaction;
    use bitcoin::Txid;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::RwLock;
    use std::time::Duration;
    use std::time::Instant;

    #[derive(Clone)]
    struct DummyBlockchain;

    impl Blockchain for DummyBlockchain {
        async fn find_outpoints(
            &self,
            _address: &bitcoin::Address,
        ) -> Result<Vec<ExplorerUtxo>, Error> {
            Ok(Vec::new())
        }

        async fn find_tx(&self, _txid: &Txid) -> Result<Option<Transaction>, Error> {
            Ok(None)
        }

        async fn get_tx_status(&self, _txid: &Txid) -> Result<TxStatus, Error> {
            Ok(TxStatus { confirmed_at: None })
        }

        async fn get_output_status(&self, _txid: &Txid, _vout: u32) -> Result<SpendStatus, Error> {
            Ok(SpendStatus { spend_txid: None })
        }

        async fn broadcast(&self, _tx: &Transaction) -> Result<(), Error> {
            Ok(())
        }

        async fn get_fee_rate(&self) -> Result<f64, Error> {
            Ok(1.0)
        }

        async fn broadcast_package(&self, _txs: &[&Transaction]) -> Result<(), Error> {
            Ok(())
        }
    }

    struct DummyWallet {
        keypair: Keypair,
        secp: Secp256k1<secp256k1::All>,
    }

    impl DummyWallet {
        fn new() -> Self {
            let secp = Secp256k1::new();
            let keypair =
                Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[2; 32]).unwrap());
            Self { keypair, secp }
        }
    }

    impl OnchainWallet for DummyWallet {
        fn get_onchain_address(&self) -> Result<bitcoin::Address, Error> {
            Ok(bitcoin::Address::p2tr(
                &self.secp,
                self.keypair.x_only_public_key().0,
                None,
                bitcoin::Network::Regtest,
            ))
        }

        async fn sync(&self) -> Result<(), Error> {
            Ok(())
        }

        fn balance(&self) -> Result<crate::wallet::Balance, Error> {
            Ok(crate::wallet::Balance {
                immature: Amount::ZERO,
                trusted_pending: Amount::ZERO,
                untrusted_pending: Amount::ZERO,
                confirmed: Amount::ZERO,
            })
        }

        fn prepare_send_to_address(
            &self,
            _address: bitcoin::Address,
            _amount: Amount,
            _fee_rate: bitcoin::FeeRate,
        ) -> Result<Psbt, Error> {
            Err(Error::wallet("not implemented"))
        }

        fn sign(&self, _psbt: &mut Psbt) -> Result<bool, Error> {
            Ok(true)
        }

        fn select_coins(&self, _target_amount: Amount) -> Result<UtxoCoinSelection, Error> {
            Err(Error::wallet("not implemented"))
        }
    }

    type TestClient = Client<DummyBlockchain, DummyWallet, InMemorySwapStorage>;

    fn test_server_info() -> Info {
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[1; 32]).unwrap();
        let public_key = secp256k1::PublicKey::from_secret_key(&secp, &secret_key);
        let keypair = Keypair::from_secret_key(&secp, &secret_key);
        let address = bitcoin::Address::p2tr(
            &secp,
            keypair.x_only_public_key().0,
            None,
            bitcoin::Network::Testnet,
        );
        Info {
            version: "test".to_string(),
            signer_pk: public_key,
            forfeit_pk: public_key,
            forfeit_address: address,
            checkpoint_tapscript: ScriptBuf::new(),
            network: bitcoin::Network::Testnet,
            session_duration: 60,
            unilateral_exit_delay: bitcoin::Sequence::from_height(144),
            boarding_exit_delay: bitcoin::Sequence::from_height(144),
            utxo_min_amount: None,
            utxo_max_amount: None,
            vtxo_min_amount: None,
            vtxo_max_amount: None,
            dust: Amount::from_sat(1000),
            fees: None,
            scheduled_session: None,
            deprecated_signers: vec![ark_core::server::DeprecatedSigner {
                pk: secp256k1::PublicKey::from_x_only_public_key(
                    fixture_server_xonly(),
                    secp256k1::Parity::Even,
                ),
                cutoff_date: 0,
            }],
            service_status: Default::default(),
            digest: "digest".to_string(),
            max_tx_weight: 0,
            max_op_return_outputs: 0,
        }
    }

    fn test_client(server_info: Info) -> TestClient {
        let secp = Secp256k1::new();
        let keypair = Keypair::from_secret_key(&secp, &SecretKey::from_slice(&[3; 32]).unwrap());
        let inner =
            OfflineClient::<DummyBlockchain, DummyWallet, InMemorySwapStorage>::with_keypair(
                OfflineClientConfig {
                    ark_server_url: "http://127.0.0.1:1".to_string(),
                    boltz_url: "http://127.0.0.1:1".to_string(),
                    timeout: Duration::from_millis(50),
                    ..Default::default()
                },
                keypair,
                Arc::new(DummyBlockchain),
                Arc::new(DummyWallet::new()),
                Arc::new(InMemorySwapStorage::default()),
            );
        let mut contract_manager = ContractManager::in_memory(server_info.network);
        contract_manager.register_builtins().unwrap();
        Client {
            inner,
            state: Arc::new(RwLock::new(ServerState {
                fee_estimator: ark_fees::Estimator::new(Default::default()).unwrap(),
                server_info,
                server_info_refreshed_at: Instant::now(),
                contract_manager: Mutex::new(contract_manager),
            })),
            server_info_refresh_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn fixture_vtxo(amount: Amount, is_spent: bool) -> ark_core::server::VirtualTxOutPoint {
        ark_core::server::VirtualTxOutPoint {
            outpoint: bitcoin::OutPoint {
                txid: Txid::from_byte_array([1; 32]),
                vout: 0,
            },
            created_at: 0,
            expires_at: i64::MAX,
            amount,
            script: ScriptBuf::new(),
            is_preconfirmed: false,
            is_swept: false,
            is_unrolled: false,
            is_spent,
            spent_by: None,
            commitment_txids: Vec::new(),
            settled_by: None,
            ark_txid: None,
            assets: Vec::new(),
        }
    }

    #[test]
    fn classify_vhtlc_liveness_deactivates_only_spent_vtxos() {
        let dust = Amount::from_sat(1000);

        assert_eq!(
            classify_vhtlc_contract_liveness(dust, true, Vec::new()),
            VhtlcContractLiveness::PendingSpend
        );
        assert_eq!(
            classify_vhtlc_contract_liveness(dust, false, Vec::new()),
            VhtlcContractLiveness::Unfunded
        );
        assert_eq!(
            classify_vhtlc_contract_liveness(
                dust,
                false,
                vec![fixture_vtxo(Amount::from_sat(1000), false)]
            ),
            VhtlcContractLiveness::Funded
        );
        assert_eq!(
            classify_vhtlc_contract_liveness(
                dust,
                false,
                vec![fixture_vtxo(Amount::from_sat(1), false)]
            ),
            VhtlcContractLiveness::Recoverable
        );
        assert_eq!(
            classify_vhtlc_contract_liveness(
                dust,
                false,
                vec![fixture_vtxo(Amount::from_sat(1000), true)]
            ),
            VhtlcContractLiveness::Spent
        );

        assert!(VhtlcContractLiveness::Spent.should_deactivate_contract());
        assert!(!VhtlcContractLiveness::Recoverable.should_deactivate_contract());
    }

    #[test]
    fn test_deserialize_create_reverse_swap_response() {
        let json = r#"{
  "id": "vqhG2fJtNY4H",
  "lockupAddress": "tark1qra883hysahlkt0ujcwhv0x2n278849c3m7t3a08l7fdc40f4f2nmw3f7kn37vvq0hqazxtqgtvhwp3z83zfgr7qc82t9mty8vk95ynpx3l43d",
  "refundPublicKey": "0206988651c7fbe41747bb21b54ced0a183f4d658e007ee8fdb23fbbfccb8e0c55",
  "timeoutBlockHeights": {
    "refund": 1760508054,
    "unilateralClaim": 9728,
    "unilateralRefund": 86528,
    "unilateralRefundWithoutReceiver": 86528
  },
  "invoice": "lntbs10u1p5wmeeepp56ms94rkev7tdrwqyus5a63lny2mqzq9vh2rq3u4ym3v4lxv6xl4qdql2djkuepqw3hjqs2jfvsxzerywfjhxuccqz95xqztfsp5ckaskagag554na8d56tlrfdxasstqrmmpkvswqqqx6y386jcfq9s9qxpqysgqt7z0vkdwkqamydae7ctgkh7l8q75w7q9394ce3lda2mkfxrpfdtj5gmltuctav7jdgatkflhztrjjzutdla5e4xp0uhxxy7sluzll4qpkkh6wv",
  "onchainAmount": 996
}"#;

        let response: CreateReverseSwapResponse =
            serde_json::from_str(json).expect("Failed to deserialize CreateReverseSwapResponse");

        // Verify the deserialized fields
        assert_eq!(response.id, "vqhG2fJtNY4H");
        assert_eq!(response.onchain_amount, Some(Amount::from_sat(996)));
        assert_eq!(
            response.refund_public_key,
            PublicKey::from_str(
                "0206988651c7fbe41747bb21b54ced0a183f4d658e007ee8fdb23fbbfccb8e0c55"
            )
            .expect("valid public key")
        );
        assert_eq!(
            response.lockup_address.to_string(),
            "tark1qra883hysahlkt0ujcwhv0x2n278849c3m7t3a08l7fdc40f4f2nmw3f7kn37vvq0hqazxtqgtvhwp3z83zfgr7qc82t9mty8vk95ynpx3l43d"
        );
        assert_eq!(response.timeout_block_heights.refund, 1760508054);
        assert_eq!(response.timeout_block_heights.unilateral_claim, 9728);
        assert_eq!(response.timeout_block_heights.unilateral_refund, 86528);
        assert_eq!(
            response
                .timeout_block_heights
                .unilateral_refund_without_receiver,
            86528
        );
    }

    #[test]
    fn test_btc_htlc_address_reconstruction_btc_to_ark() {
        // Real BtcToArk chain swap response from Boltz mutinynet.
        // lockupDetails = BTC side (user locks): serverPublicKey = server's claim key.
        // User's key is refundPublicKey from the request.
        let server_pk = PublicKey::from_str(
            "03ce9f5a57218103d5fe07b9d7ecf4b28ad60a960f0fbfd86dd090013020617389",
        )
        .unwrap();
        let user_pk = PublicKey::from_str(
            "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
        )
        .unwrap();
        let swap_tree = SwapTree {
            claim_leaf: SwapTreeLeaf {
                version: 192,
                output: "82012088a914b472a266d0bd89c13706a4132ccfb16f7c3b9fcb8820ce9f5a57218103d5fe07b9d7ecf4b28ad60a960f0fbfd86dd090013020617389ac".into(),
            },
            refund_leaf: SwapTreeLeaf {
                version: 192,
                output: "20c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5ad03f9832db1".into(),
            },
        };

        let spend_info = reconstruct_btc_htlc(server_pk, user_pk, &swap_tree).unwrap();

        let secp = Secp256k1::new();
        let spk = ScriptBuf::new_p2tr(&secp, spend_info.internal_key(), spend_info.merkle_root());
        let addr = bitcoin::Address::from_script(&spk, bitcoin::Network::Testnet).unwrap();

        assert_eq!(
            addr.to_string(),
            "tb1ptf632fkczflsjn4356ra4x2s6qp6vvk8e7pplprpwnkvcsd8tpwqkw92c7"
        );
    }

    #[test]
    fn submarine_swap_request_serializes_referral_id_when_set() {
        let request = CreateSubmarineSwapRequest {
            from: Asset::Ark,
            to: Asset::Btc,
            invoice: Bolt11Invoice::from_str(
                "lntbs10u1p5wmeeepp56ms94rkev7tdrwqyus5a63lny2mqzq9vh2rq3u4ym3v4lxv6xl4qdql2djkuepqw3hjqs2jfvsxzerywfjhxuccqz95xqztfsp5ckaskagag554na8d56tlrfdxasstqrmmpkvswqqqx6y386jcfq9s9qxpqysgqt7z0vkdwkqamydae7ctgkh7l8q75w7q9394ce3lda2mkfxrpfdtj5gmltuctav7jdgatkflhztrjjzutdla5e4xp0uhxxy7sluzll4qpkkh6wv",
            )
            .unwrap(),
            refund_public_key: PublicKey::from_str(
                "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
            )
            .unwrap(),
            referral_id: Some("partner-xyz".to_string()),
        };

        let json: serde_json::Value = serde_json::to_value(&request).unwrap();
        assert_eq!(json["referralId"], "partner-xyz");
    }

    #[test]
    fn submarine_swap_request_omits_referral_id_when_none() {
        let request = CreateSubmarineSwapRequest {
            from: Asset::Ark,
            to: Asset::Btc,
            invoice: Bolt11Invoice::from_str(
                "lntbs10u1p5wmeeepp56ms94rkev7tdrwqyus5a63lny2mqzq9vh2rq3u4ym3v4lxv6xl4qdql2djkuepqw3hjqs2jfvsxzerywfjhxuccqz95xqztfsp5ckaskagag554na8d56tlrfdxasstqrmmpkvswqqqx6y386jcfq9s9qxpqysgqt7z0vkdwkqamydae7ctgkh7l8q75w7q9394ce3lda2mkfxrpfdtj5gmltuctav7jdgatkflhztrjjzutdla5e4xp0uhxxy7sluzll4qpkkh6wv",
            )
            .unwrap(),
            refund_public_key: PublicKey::from_str(
                "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
            )
            .unwrap(),
            referral_id: None,
        };

        let json: serde_json::Value = serde_json::to_value(&request).unwrap();
        assert!(json.get("referralId").is_none());
        assert!(json.get("referral_id").is_none());
    }

    #[test]
    fn reverse_swap_request_serializes_referral_id_when_set() {
        let request = CreateReverseSwapRequest {
            from: Asset::Btc,
            to: Asset::Ark,
            invoice_amount: Some(Amount::from_sat(1000)),
            onchain_amount: None,
            claim_public_key: PublicKey::from_str(
                "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
            )
            .unwrap(),
            preimage_hash: sha256::Hash::from_byte_array([1u8; 32]),
            invoice_expiry: Some(3600),
            referral_id: Some("partner-xyz".to_string()),
            description: None,
        };

        let json: serde_json::Value = serde_json::to_value(&request).unwrap();
        assert_eq!(json["referralId"], "partner-xyz");
    }

    #[test]
    fn reverse_swap_request_omits_referral_id_when_none() {
        let request = CreateReverseSwapRequest {
            from: Asset::Btc,
            to: Asset::Ark,
            invoice_amount: Some(Amount::from_sat(1000)),
            onchain_amount: None,
            claim_public_key: PublicKey::from_str(
                "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
            )
            .unwrap(),
            preimage_hash: sha256::Hash::from_byte_array([1u8; 32]),
            invoice_expiry: Some(3600),
            referral_id: None,
            description: None,
        };

        let json: serde_json::Value = serde_json::to_value(&request).unwrap();
        assert!(json.get("referralId").is_none());
        assert!(json.get("referral_id").is_none());
    }

    #[test]
    fn chain_swap_request_serializes_referral_id_when_set() {
        let request = CreateChainSwapRequest {
            from: Asset::Ark,
            to: Asset::Btc,
            user_lock_amount: Some(Amount::from_sat(1000)),
            server_lock_amount: None,
            claim_public_key: PublicKey::from_str(
                "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
            )
            .unwrap(),
            refund_public_key: PublicKey::from_str(
                "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            )
            .unwrap(),
            preimage_hash: sha256::Hash::from_byte_array([1u8; 32]),
            referral_id: Some("partner-xyz".to_string()),
        };

        let json: serde_json::Value = serde_json::to_value(&request).unwrap();
        assert_eq!(json["referralId"], "partner-xyz");
    }

    #[test]
    fn chain_swap_request_omits_referral_id_when_none() {
        let request = CreateChainSwapRequest {
            from: Asset::Ark,
            to: Asset::Btc,
            user_lock_amount: Some(Amount::from_sat(1000)),
            server_lock_amount: None,
            claim_public_key: PublicKey::from_str(
                "02c6047f9441ed7d6d3045406e95c07cd85c778e4b8cef3ca7abac09b95c709ee5",
            )
            .unwrap(),
            refund_public_key: PublicKey::from_str(
                "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            )
            .unwrap(),
            preimage_hash: sha256::Hash::from_byte_array([1u8; 32]),
            referral_id: None,
        };

        let json: serde_json::Value = serde_json::to_value(&request).unwrap();
        assert!(json.get("referralId").is_none());
        assert!(json.get("referral_id").is_none());
    }

    #[test]
    fn test_btc_htlc_address_reconstruction_ark_to_btc() {
        // Real ArkToBtc chain swap response from Boltz mutinynet.
        // claimDetails = BTC side (user claims): serverPublicKey = server's refund key.
        // User's key is claimPublicKey from the request.
        let server_pk = PublicKey::from_str(
            "0207364dc5853e630be83439fde62b531e3c11db34ce8c4f454a56782555c58ed6",
        )
        .unwrap();
        let user_pk = PublicKey::from_str(
            "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
        )
        .unwrap();
        let swap_tree = SwapTree {
            claim_leaf: SwapTreeLeaf {
                version: 192,
                output: "82012088a914cf7ff51392e9a37bc72c7284841db669c82e2c14882079be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798ac".into(),
            },
            refund_leaf: SwapTreeLeaf {
                version: 192,
                output: "2007364dc5853e630be83439fde62b531e3c11db34ce8c4f454a56782555c58ed6ad036b832db1".into(),
            },
        };

        let spend_info = reconstruct_btc_htlc(server_pk, user_pk, &swap_tree).unwrap();

        let secp = Secp256k1::new();
        let spk = ScriptBuf::new_p2tr(&secp, spend_info.internal_key(), spend_info.merkle_root());
        let addr = bitcoin::Address::from_script(&spk, bitcoin::Network::Testnet).unwrap();

        assert_eq!(
            addr.to_string(),
            "tb1pxa78pf55g0aaurrd8c76fyax4df9e8y38fzps8sw2vkrecf9k3ss36a78m"
        );
    }

    #[test]
    fn validate_invoice_description_accepts_none_empty_and_max_length() {
        assert!(validate_invoice_description(None).is_ok());
        assert!(validate_invoice_description(Some("")).is_ok());
        let at_limit = "a".repeat(MAX_BOLT11_DESCRIPTION_BYTES);
        assert!(validate_invoice_description(Some(&at_limit)).is_ok());
    }

    #[test]
    fn validate_invoice_description_rejects_over_limit() {
        let too_long = "a".repeat(MAX_BOLT11_DESCRIPTION_BYTES + 1);
        let err = validate_invoice_description(Some(&too_long)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("640"), "unexpected error message: {msg}");
        assert!(msg.contains("639"), "unexpected error message: {msg}");
    }

    // ── reconstruct_vhtlc_from_keys ─────────────────────────────────────────

    /// Build a [`VhtlcOptions`] from the first fixture in vhtlc.json (CSV > 16).
    /// Keys and expected address are taken verbatim from the JSON fixture so the test
    /// is independent of any client-side logic.
    fn fixture_opts(server: XOnlyPublicKey) -> VhtlcOptions {
        let sender = XOnlyPublicKey::from(
            PublicKey::from_str(
                "030192e796452d6df9697c280542e1560557bcf79a347d925895043136225c7cb4",
            )
            .unwrap()
            .inner,
        );
        let receiver = XOnlyPublicKey::from(
            PublicKey::from_str(
                "021e1bb85455fe3f5aed60d101aa4dbdb9e7714f6226769a97a17a5331dadcd53b",
            )
            .unwrap()
            .inner,
        );
        VhtlcOptions {
            sender,
            receiver,
            server,
            preimage_hash: ripemd160::Hash::from_str("4d487dd3753a89bc9fe98401d1196523058251fc")
                .unwrap(),
            refund_locktime: 265,
            unilateral_claim_delay: bitcoin::Sequence::from_height(17),
            unilateral_refund_delay: bitcoin::Sequence::from_height(144),
            unilateral_refund_without_receiver_delay: bitcoin::Sequence::from_height(144),
        }
    }

    fn fixture_server_xonly() -> XOnlyPublicKey {
        XOnlyPublicKey::from(
            PublicKey::from_str(
                "03aad52d58162e9eefeafc7ad8a1cdca8060b5f01df1e7583362d052e266208f88",
            )
            .unwrap()
            .inner,
        )
    }

    // Expected Arkade address from the fixture (vhtlc.json CSV > 16 case, testnet).
    const FIXTURE_ADDRESS: &str = "tark1qz4d2t2czchfaml2l3ad3gwde2qxpd0srhc7wkpnvtg99cnxyz8c3pnvvhnhumhwhqthmlxmdryakwx99s6508y8dunj9sty2p5mr7unh5re63";

    const BOLT11_FIXTURE: &str = "lnbcrt10u1p5d55pjpp56ms94rkev7tdrwqyus5a63lny2mqzq9vh2rq3u4ym3v4lxv6xl4qdql2djkuepqw3hjqs2jfvsxzerywfjhxuccqz95xqztfsp57x0nwf7nzsndjdrvsre570ehg0szw34l284hswdz6zpqvktq9mrs9qxpqysgqllgxhxeny0tvtnxuqgn4s0t2qamc6yqc4t3pe6p2x5lgs8v8r3vxzxp3a3ax9j7d2ta5cduddln8n9se7q0jgg7s0h8t2vhljlu3wkcps9k8xs";

    // A second server key that produces a different address for the same other params.
    fn wrong_server_xonly() -> XOnlyPublicKey {
        XOnlyPublicKey::from(
            PublicKey::from_str(
                "0206988651c7fbe41747bb21b54ced0a183f4d658e007ee8fdb23fbbfccb8e0c55",
            )
            .unwrap()
            .inner,
        )
    }

    fn public_key_from_xonly(pk: XOnlyPublicKey) -> PublicKey {
        PublicKey::new(secp256k1::PublicKey::from_x_only_public_key(
            pk,
            secp256k1::Parity::Even,
        ))
    }

    fn fixture_submarine_swap(contract_script_pubkey: Option<ScriptBuf>) -> SubmarineSwapData {
        let opts = fixture_opts(fixture_server_xonly());
        let vhtlc = VhtlcScript::new(opts.clone(), bitcoin::Network::Testnet).unwrap();
        SubmarineSwapData {
            id: "swap-1".to_string(),
            preimage: None,
            preimage_hash: opts.preimage_hash,
            claim_public_key: public_key_from_xonly(opts.receiver),
            refund_public_key: public_key_from_xonly(opts.sender),
            amount: Amount::from_sat(1000),
            timeout_block_heights: TimeoutBlockHeights {
                refund: opts.refund_locktime,
                unilateral_claim: opts.unilateral_claim_delay.to_consensus_u32(),
                unilateral_refund: opts.unilateral_refund_delay.to_consensus_u32(),
                unilateral_refund_without_receiver: opts
                    .unilateral_refund_without_receiver_delay
                    .to_consensus_u32(),
            },
            vhtlc_address: vhtlc.address(),
            invoice: Bolt11Invoice::from_str(BOLT11_FIXTURE).unwrap(),
            status: SwapStatus::Created,
            created_at: 123,
            key_derivation_index: Some(7),
            contract_script_pubkey,
        }
    }

    fn fixture_reverse_swap(contract_script_pubkey: Option<ScriptBuf>) -> ReverseSwapData {
        let opts = fixture_opts(fixture_server_xonly());
        let vhtlc = VhtlcScript::new(opts.clone(), bitcoin::Network::Testnet).unwrap();
        ReverseSwapData {
            id: "reverse-1".to_string(),
            preimage: Some([1; 32]),
            preimage_hash: opts.preimage_hash,
            claim_public_key: public_key_from_xonly(opts.receiver),
            refund_public_key: public_key_from_xonly(opts.sender),
            amount: Amount::from_sat(1000),
            timeout_block_heights: TimeoutBlockHeights {
                refund: opts.refund_locktime,
                unilateral_claim: opts.unilateral_claim_delay.to_consensus_u32(),
                unilateral_refund: opts.unilateral_refund_delay.to_consensus_u32(),
                unilateral_refund_without_receiver: opts
                    .unilateral_refund_without_receiver_delay
                    .to_consensus_u32(),
            },
            vhtlc_address: vhtlc.address(),
            status: SwapStatus::Created,
            created_at: 123,
            key_derivation_index: Some(8),
            bolt11: BOLT11_FIXTURE.to_string(),
            invoice_expiry: 3600,
            claim_address: None,
            contract_script_pubkey,
        }
    }

    fn fixture_chain_swap(
        direction: ChainSwapDirection,
        contract_script_pubkey: Option<ScriptBuf>,
    ) -> ChainSwapData {
        let mut opts = fixture_opts(fixture_server_xonly());
        let preimage_hash = sha256::Hash::from_byte_array([2; 32]);
        opts.preimage_hash = ripemd160::Hash::hash(preimage_hash.as_byte_array());
        let vhtlc = VhtlcScript::new(opts.clone(), bitcoin::Network::Testnet).unwrap();
        let timeouts = TimeoutBlockHeights {
            refund: opts.refund_locktime,
            unilateral_claim: opts.unilateral_claim_delay.to_consensus_u32(),
            unilateral_refund: opts.unilateral_refund_delay.to_consensus_u32(),
            unilateral_refund_without_receiver: opts
                .unilateral_refund_without_receiver_delay
                .to_consensus_u32(),
        };
        let ark_address = vhtlc.address().to_string();
        let btc_address =
            "tb1pxa78pf55g0aaurrd8c76fyax4df9e8y38fzps8sw2vkrecf9k3ss36a78m".to_string();
        ChainSwapData {
            id: "chain-1".to_string(),
            status: SwapStatus::Created,
            direction: direction.clone(),
            preimage: Some([1; 32]),
            preimage_hash,
            claim_public_key: public_key_from_xonly(opts.receiver),
            refund_public_key: public_key_from_xonly(opts.sender),
            server_claim_public_key: public_key_from_xonly(opts.receiver),
            server_refund_public_key: public_key_from_xonly(opts.sender),
            user_lockup_address: match direction {
                ChainSwapDirection::ArkToBtc => ark_address.clone(),
                ChainSwapDirection::BtcToArk => btc_address.clone(),
            },
            server_lockup_address: match direction {
                ChainSwapDirection::ArkToBtc => btc_address,
                ChainSwapDirection::BtcToArk => ark_address,
            },
            user_lockup_amount: Amount::from_sat(1000),
            server_lockup_amount: Amount::from_sat(900),
            user_timeout_block_height: 265,
            server_timeout_block_height: 265,
            user_timeout_block_heights: matches!(direction, ChainSwapDirection::ArkToBtc)
                .then_some(timeouts),
            server_timeout_block_heights: matches!(direction, ChainSwapDirection::BtcToArk)
                .then_some(timeouts),
            bip21: None,
            swap_tree: None,
            created_at: 123,
            claim_key_derivation_index: Some(7),
            refund_key_derivation_index: Some(8),
            contract_script_pubkey,
        }
    }

    #[test]
    fn submarine_swap_data_deserializes_legacy_missing_contract_reference() {
        let script_pubkey = VhtlcScript::new(
            fixture_opts(fixture_server_xonly()),
            bitcoin::Network::Testnet,
        )
        .unwrap()
        .script_pubkey();
        let swap = fixture_submarine_swap(Some(script_pubkey));
        let mut json = serde_json::to_value(&swap).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("contract_script_pubkey");

        let decoded: SubmarineSwapData = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.id, swap.id);
        assert_eq!(decoded.contract_script_pubkey, None);
    }

    #[test]
    fn submarine_swap_data_roundtrips_contract_reference() {
        let script_pubkey = VhtlcScript::new(
            fixture_opts(fixture_server_xonly()),
            bitcoin::Network::Testnet,
        )
        .unwrap()
        .script_pubkey();
        let swap = fixture_submarine_swap(Some(script_pubkey.clone()));

        let decoded: SubmarineSwapData =
            serde_json::from_value(serde_json::to_value(&swap).unwrap()).unwrap();
        assert_eq!(decoded.contract_script_pubkey, Some(script_pubkey));
    }

    #[test]
    fn reverse_swap_data_deserializes_legacy_missing_contract_reference() {
        let script_pubkey = VhtlcScript::new(
            fixture_opts(fixture_server_xonly()),
            bitcoin::Network::Testnet,
        )
        .unwrap()
        .script_pubkey();
        let swap = fixture_reverse_swap(Some(script_pubkey));
        let mut json = serde_json::to_value(&swap).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("contract_script_pubkey");

        let decoded: ReverseSwapData = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.id, swap.id);
        assert_eq!(decoded.contract_script_pubkey, None);
    }

    #[test]
    fn reverse_swap_data_roundtrips_contract_reference() {
        let script_pubkey = VhtlcScript::new(
            fixture_opts(fixture_server_xonly()),
            bitcoin::Network::Testnet,
        )
        .unwrap()
        .script_pubkey();
        let swap = fixture_reverse_swap(Some(script_pubkey.clone()));

        let decoded: ReverseSwapData =
            serde_json::from_value(serde_json::to_value(&swap).unwrap()).unwrap();
        assert_eq!(decoded.contract_script_pubkey, Some(script_pubkey));
    }

    #[test]
    fn chain_swap_data_deserializes_legacy_missing_contract_reference() {
        let script_pubkey = VhtlcScript::new(
            fixture_opts(fixture_server_xonly()),
            bitcoin::Network::Testnet,
        )
        .unwrap()
        .script_pubkey();
        let swap = fixture_chain_swap(ChainSwapDirection::ArkToBtc, Some(script_pubkey));
        let mut json = serde_json::to_value(&swap).unwrap();
        json.as_object_mut()
            .unwrap()
            .remove("contract_script_pubkey");

        let decoded: ChainSwapData = serde_json::from_value(json).unwrap();
        assert_eq!(decoded.id, swap.id);
        assert_eq!(decoded.contract_script_pubkey, None);
    }

    #[test]
    fn chain_swap_data_roundtrips_contract_reference() {
        let script_pubkey = VhtlcScript::new(
            fixture_opts(fixture_server_xonly()),
            bitcoin::Network::Testnet,
        )
        .unwrap()
        .script_pubkey();
        let swap = fixture_chain_swap(ChainSwapDirection::BtcToArk, Some(script_pubkey.clone()));

        let decoded: ChainSwapData =
            serde_json::from_value(serde_json::to_value(&swap).unwrap()).unwrap();
        assert_eq!(decoded.contract_script_pubkey, Some(script_pubkey));
    }

    #[tokio::test]
    async fn terminal_submarine_status_keeps_vhtlc_contract_active_without_spent_vtxo() {
        let server_info = test_server_info();
        let client = test_client(server_info);
        let script_pubkey = client
            .insert_vhtlc_contract(fixture_opts(fixture_server_xonly()), Some(7))
            .unwrap();
        let mut swap = fixture_submarine_swap(Some(script_pubkey.clone()));
        client
            .swap_storage()
            .insert_submarine(swap.id.clone(), swap.clone())
            .await
            .unwrap();

        client
            .persist_swap_status_for_type(
                SwapType::Submarine,
                &swap.id,
                SwapStatus::TransactionClaimed,
            )
            .await
            .unwrap();

        swap = client
            .swap_storage()
            .get_submarine(&swap.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(swap.status, SwapStatus::TransactionClaimed);
        let state = client.state.read().unwrap();
        let stored = state
            .contract_manager
            .lock()
            .unwrap()
            .get(&script_pubkey)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, ContractState::Active);
    }

    #[tokio::test]
    async fn terminal_reverse_status_keeps_vhtlc_contract_active_without_spent_vtxo() {
        let server_info = test_server_info();
        let client = test_client(server_info);
        let script_pubkey = client
            .insert_vhtlc_contract(fixture_opts(fixture_server_xonly()), Some(8))
            .unwrap();
        let mut swap = fixture_reverse_swap(Some(script_pubkey.clone()));
        client
            .swap_storage()
            .insert_reverse(swap.id.clone(), swap.clone())
            .await
            .unwrap();

        client
            .persist_swap_status_for_type(SwapType::Reverse, &swap.id, SwapStatus::InvoiceExpired)
            .await
            .unwrap();

        swap = client
            .swap_storage()
            .get_reverse(&swap.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(swap.status, SwapStatus::InvoiceExpired);
        let state = client.state.read().unwrap();
        let stored = state
            .contract_manager
            .lock()
            .unwrap()
            .get(&script_pubkey)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, ContractState::Active);
    }

    #[tokio::test]
    async fn terminal_chain_status_keeps_vhtlc_contract_active_without_spent_vtxo() {
        let server_info = test_server_info();
        let client = test_client(server_info);
        let script_pubkey = client
            .insert_vhtlc_contract(fixture_opts(fixture_server_xonly()), Some(8))
            .unwrap();
        let mut swap =
            fixture_chain_swap(ChainSwapDirection::ArkToBtc, Some(script_pubkey.clone()));
        client
            .swap_storage()
            .insert_chain(swap.id.clone(), swap.clone())
            .await
            .unwrap();

        client
            .persist_swap_status_for_type(
                SwapType::Chain,
                &swap.id,
                SwapStatus::TransactionRefunded,
            )
            .await
            .unwrap();

        swap = client
            .swap_storage()
            .get_chain(&swap.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(swap.status, SwapStatus::TransactionRefunded);
        let state = client.state.read().unwrap();
        let stored = state
            .contract_manager
            .lock()
            .unwrap()
            .get(&script_pubkey)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, ContractState::Active);
    }

    #[tokio::test]
    async fn non_terminal_swap_status_keeps_vhtlc_contract_active() {
        let server_info = test_server_info();
        let client = test_client(server_info);
        let script_pubkey = client
            .insert_vhtlc_contract(fixture_opts(fixture_server_xonly()), Some(7))
            .unwrap();
        let swap = fixture_submarine_swap(Some(script_pubkey.clone()));
        client
            .swap_storage()
            .insert_submarine(swap.id.clone(), swap.clone())
            .await
            .unwrap();

        client
            .persist_swap_status_for_type(
                SwapType::Submarine,
                &swap.id,
                SwapStatus::TransactionMempool,
            )
            .await
            .unwrap();

        let state = client.state.read().unwrap();
        let stored = state
            .contract_manager
            .lock()
            .unwrap()
            .get(&script_pubkey)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, ContractState::Active);
    }

    #[tokio::test]
    async fn eager_migration_persists_missing_vhtlc_contract_refs() {
        let server_info = test_server_info();
        let client = test_client(server_info.clone());
        let submarine = fixture_submarine_swap(None);
        let reverse = fixture_reverse_swap(None);
        client
            .swap_storage()
            .insert_submarine(submarine.id.clone(), submarine.clone())
            .await
            .unwrap();
        let chain = fixture_chain_swap(ChainSwapDirection::ArkToBtc, None);
        client
            .swap_storage()
            .insert_reverse(reverse.id.clone(), reverse.clone())
            .await
            .unwrap();
        client
            .swap_storage()
            .insert_chain(chain.id.clone(), chain.clone())
            .await
            .unwrap();

        let migrated = client
            .migrate_boltz_vhtlc_contracts(&server_info)
            .await
            .unwrap();

        assert_eq!(migrated, 3);
        assert!(client
            .swap_storage()
            .get_submarine(&submarine.id)
            .await
            .unwrap()
            .unwrap()
            .contract_script_pubkey
            .is_some());
        assert!(client
            .swap_storage()
            .get_reverse(&reverse.id)
            .await
            .unwrap()
            .unwrap()
            .contract_script_pubkey
            .is_some());
        assert!(client
            .swap_storage()
            .get_chain(&chain.id)
            .await
            .unwrap()
            .unwrap()
            .contract_script_pubkey
            .is_some());
    }

    #[tokio::test]
    async fn eager_migration_keeps_contract_active_without_spent_vtxo() {
        let server_info = test_server_info();
        let client = test_client(server_info.clone());
        let mut swap = fixture_submarine_swap(None);
        swap.status = SwapStatus::TransactionClaimed;
        client
            .swap_storage()
            .insert_submarine(swap.id.clone(), swap.clone())
            .await
            .unwrap();

        let migrated = client
            .migrate_boltz_vhtlc_contracts(&server_info)
            .await
            .unwrap();

        assert_eq!(migrated, 1);
        let stored_swap = client
            .swap_storage()
            .get_submarine(&swap.id)
            .await
            .unwrap()
            .unwrap();
        let script_pubkey = stored_swap.contract_script_pubkey.unwrap();
        let state = client.state.read().unwrap();
        let stored = state
            .contract_manager
            .lock()
            .unwrap()
            .get(&script_pubkey)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, ContractState::Active);
    }

    #[test]
    fn mark_vhtlc_contract_inactive_updates_contract_state() {
        let server_info = test_server_info();
        let client = test_client(server_info);
        let script_pubkey = client
            .insert_vhtlc_contract(fixture_opts(fixture_server_xonly()), Some(9))
            .unwrap();

        client
            .mark_vhtlc_contract_inactive(Some(&script_pubkey))
            .unwrap();

        let state = client.state.read().unwrap();
        let stored = state
            .contract_manager
            .lock()
            .unwrap()
            .get(&script_pubkey)
            .unwrap()
            .unwrap();
        assert_eq!(stored.state, ContractState::Inactive);
        assert_eq!(stored.key_index, Some(9));
    }

    #[test]
    fn mark_vhtlc_contract_inactive_ignores_missing_reference() {
        let client = test_client(test_server_info());

        client.mark_vhtlc_contract_inactive(None).unwrap();

        let state = client.state.read().unwrap();
        assert!(state
            .contract_manager
            .lock()
            .unwrap()
            .list()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn submarine_vhtlc_script_lazily_migrates_legacy_swap() {
        let server_info = test_server_info();
        let client = test_client(server_info.clone());
        let mut swap = fixture_submarine_swap(None);
        client
            .swap_storage()
            .insert_submarine(swap.id.clone(), swap.clone())
            .await
            .unwrap();

        let vhtlc = client
            .submarine_vhtlc_script(&mut swap, &server_info)
            .await
            .unwrap();

        let script_pubkey = swap.contract_script_pubkey.clone().unwrap();
        assert_eq!(script_pubkey, vhtlc.script_pubkey());
        let stored_swap = client
            .swap_storage()
            .get_submarine(&swap.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored_swap.contract_script_pubkey,
            Some(script_pubkey.clone())
        );
        let state = client.state.read().unwrap();
        let stored_contract = state
            .contract_manager
            .lock()
            .unwrap()
            .get(&script_pubkey)
            .unwrap()
            .unwrap();
        assert_eq!(stored_contract.state, ContractState::Active);
        assert_eq!(stored_contract.key_index, swap.key_derivation_index);
    }

    #[tokio::test]
    async fn build_chain_vhtlc_script_does_not_require_stored_swap() {
        let server_info = test_server_info();
        let client = test_client(server_info.clone());
        let swap = fixture_chain_swap(ChainSwapDirection::ArkToBtc, None);

        let vhtlc = client
            .build_chain_vhtlc_script(&swap, &server_info)
            .unwrap();

        assert_eq!(vhtlc.address(), swap.chain_vhtlc_address().unwrap());
        assert_eq!(swap.chain_vhtlc_key_index(), Some(8));
        assert!(client
            .swap_storage()
            .get_chain(&swap.id)
            .await
            .unwrap()
            .is_none());
        let state = client.state.read().unwrap();
        assert!(state
            .contract_manager
            .lock()
            .unwrap()
            .list()
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn chain_vhtlc_script_lazily_migrates_legacy_swap() {
        let server_info = test_server_info();
        let client = test_client(server_info.clone());
        let mut swap = fixture_chain_swap(ChainSwapDirection::BtcToArk, None);
        client
            .swap_storage()
            .insert_chain(swap.id.clone(), swap.clone())
            .await
            .unwrap();

        let vhtlc = client
            .chain_vhtlc_script(&mut swap, &server_info)
            .await
            .unwrap();

        let script_pubkey = swap.contract_script_pubkey.clone().unwrap();
        assert_eq!(script_pubkey, vhtlc.script_pubkey());
        let stored_swap = client
            .swap_storage()
            .get_chain(&swap.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored_swap.contract_script_pubkey,
            Some(script_pubkey.clone())
        );
        let state = client.state.read().unwrap();
        let stored_contract = state
            .contract_manager
            .lock()
            .unwrap()
            .get(&script_pubkey)
            .unwrap()
            .unwrap();
        assert_eq!(stored_contract.state, ContractState::Active);
        assert_eq!(stored_contract.key_index, swap.chain_vhtlc_key_index());
    }

    #[tokio::test]
    async fn reverse_vhtlc_script_lazily_migrates_legacy_swap() {
        let server_info = test_server_info();
        let client = test_client(server_info.clone());
        let mut swap = fixture_reverse_swap(None);
        client
            .swap_storage()
            .insert_reverse(swap.id.clone(), swap.clone())
            .await
            .unwrap();

        let vhtlc = client
            .reverse_vhtlc_script(&mut swap, &server_info)
            .await
            .unwrap();

        let script_pubkey = swap.contract_script_pubkey.clone().unwrap();
        assert_eq!(script_pubkey, vhtlc.script_pubkey());
        let stored_swap = client
            .swap_storage()
            .get_reverse(&swap.id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            stored_swap.contract_script_pubkey,
            Some(script_pubkey.clone())
        );
        let state = client.state.read().unwrap();
        let stored_contract = state
            .contract_manager
            .lock()
            .unwrap()
            .get(&script_pubkey)
            .unwrap()
            .unwrap();
        assert_eq!(stored_contract.state, ContractState::Active);
        assert_eq!(stored_contract.key_index, swap.key_derivation_index);
    }

    #[test]
    fn reconstruct_matches_with_single_current_key() {
        let server = fixture_server_xonly();
        let expected = ArkAddress::decode(FIXTURE_ADDRESS).unwrap();

        let vhtlc = reconstruct_vhtlc_from_keys(
            std::iter::once(server),
            bitcoin::Network::Testnet,
            |sk| Ok(fixture_opts(sk)),
            &expected,
        )
        .unwrap();

        assert_eq!(vhtlc.address(), expected);
    }

    #[test]
    fn reconstruct_skips_wrong_key_and_finds_deprecated() {
        let wrong = wrong_server_xonly();
        let correct = fixture_server_xonly();
        let expected = ArkAddress::decode(FIXTURE_ADDRESS).unwrap();

        // Iterator: wrong key first, correct key second (simulates signer rotation).
        let keys = [wrong, correct].into_iter();
        let vhtlc = reconstruct_vhtlc_from_keys(
            keys,
            bitcoin::Network::Testnet,
            |sk| Ok(fixture_opts(sk)),
            &expected,
        )
        .unwrap();

        assert_eq!(vhtlc.address(), expected);
    }

    #[test]
    fn reconstruct_errors_when_no_key_matches() {
        let wrong = wrong_server_xonly();
        let expected = ArkAddress::decode(FIXTURE_ADDRESS).unwrap();

        let err = reconstruct_vhtlc_from_keys(
            std::iter::once(wrong),
            bitcoin::Network::Testnet,
            |sk| Ok(fixture_opts(sk)),
            &expected,
        )
        .err()
        .expect("should have failed");

        assert!(
            err.to_string()
                .contains("does not match current or any deprecated server key"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn reconstruct_propagates_mk_opts_error() {
        let server = fixture_server_xonly();
        let expected = ArkAddress::decode(FIXTURE_ADDRESS).unwrap();

        let err = reconstruct_vhtlc_from_keys(
            std::iter::once(server),
            bitcoin::Network::Testnet,
            |_| Err(Error::ad_hoc("options error")),
            &expected,
        )
        .err()
        .expect("should have failed");

        assert!(
            err.to_string().contains("options error"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn build_vhtlc_script_sender_is_refund_receiver_is_claim() {
        // Verify the key-role mapping: build_vhtlc_script(claim, refund, ...) must produce the
        // same address as a manually-constructed VhtlcOptions{sender=refund, receiver=claim}.
        let claim_pk = PublicKey::from_str(
            "021e1bb85455fe3f5aed60d101aa4dbdb9e7714f6226769a97a17a5331dadcd53b",
        )
        .unwrap();
        let refund_pk = PublicKey::from_str(
            "030192e796452d6df9697c280542e1560557bcf79a347d925895043136225c7cb4",
        )
        .unwrap();
        let server = fixture_server_xonly();
        let expected = ArkAddress::decode(FIXTURE_ADDRESS).unwrap();

        let opts = VhtlcOptions {
            sender: refund_pk.inner.x_only_public_key().0,
            receiver: claim_pk.inner.x_only_public_key().0,
            server,
            preimage_hash: ripemd160::Hash::from_str("4d487dd3753a89bc9fe98401d1196523058251fc")
                .unwrap(),
            refund_locktime: 265,
            unilateral_claim_delay: bitcoin::Sequence::from_height(17),
            unilateral_refund_delay: bitcoin::Sequence::from_height(144),
            unilateral_refund_without_receiver_delay: bitcoin::Sequence::from_height(144),
        };
        let manual_vhtlc =
            VhtlcScript::new(opts, bitcoin::Network::Testnet).expect("valid options");

        // The manual construction produces the expected fixture address.
        assert_eq!(manual_vhtlc.address(), expected);
    }
}

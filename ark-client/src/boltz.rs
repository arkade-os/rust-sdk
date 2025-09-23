use crate::error::ErrorContext;
use crate::swap_storage::SwapStorage;
use crate::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::OffchainTransactions;
use ark_core::send::VtxoInput;
use ark_core::send::VTXO_CONDITION_KEY;
use ark_core::send::{build_offchain_transactions, VTXO_TREE_EXPIRY_PSBT_KEY};
use ark_core::server::GetVtxosRequest;
use ark_core::vhtlc::VhtlcOptions;
use ark_core::vhtlc::VhtlcScript;
use ark_core::ArkAddress;
use bitcoin::consensus::Encodable;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::io::Write;
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::taproot::LeafVersion;
use bitcoin::Amount;
use bitcoin::PublicKey;
use bitcoin::Sequence;
use bitcoin::Txid;
use bitcoin::VarInt;
use bitcoin::XOnlyPublicKey;
use bitcoin::{psbt, Psbt};
use lightning_invoice::Bolt11Invoice;
use lightning_invoice::ParseOrSemanticError;
use serde::Deserialize;
use serde::Serialize;
use std::str::FromStr;

const BOLTZ_URL: &str = "http://localhost:9001";

pub struct BoltzSwapInvoice {
    pub invoice: Bolt11Invoice,
    pub swap_data: SwapData,
}

#[derive(Clone, Debug)]
pub struct BoltzSubmarineSwapResult {
    pub swap_id: String,
    pub txid: Txid,
}

impl<B, W, S> Client<B, W, S>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
{
    // SUBMARINE SWAP

    /// This is a submarine swap (Ark to Lightning).
    ///
    /// Arguments:
    /// - `invoice`: a Bolt11Invoice to get paid
    ///
    /// Returns:
    ///
    /// - TXID of VHTLC transaction.
    /// - Swap ID.
    pub async fn pay_ln_invoice(&self, invoice: String) -> Result<BoltzSubmarineSwapResult, Error> {
        let refund_public_key = self.inner.kp.public_key();

        let bolt11_invoice = Bolt11Invoice::from_str(invoice.as_str()).unwrap();

        let request = CreateSubmarineSwapRequest {
            from: Asset::Ark,
            to: Asset::Btc,
            invoice,
            refund_public_key: refund_public_key.to_string(),
        };
        let url = format!("{BOLTZ_URL}/v2/swap/submarine");

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
        let preimage_hash = bolt11_invoice.payment_hash();

        let string = response.text().await.expect("to work");
        dbg!(&string);

        let swap_response: CreateSubmarineSwapResponse = serde_json::from_str(&string)
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to deserialize submarine swap response")?;

        self.swap_storage
            .insert(
                swap_response.id.clone(),
                SwapData {
                    id: swap_response.id.clone(),
                    swap_type: SwapType::Submarine,
                    status: SwapStatus::Created,
                    created_at: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs(),
                    metadata: SwapMetadata::Submarine {
                        address: swap_response.address.clone(),
                        preimage_hash: *preimage_hash,
                        // TODO: set this?
                        redeem_script: "".to_string(),
                        accept_zero_conf: swap_response.accept_zero_conf,
                        expected_amount: swap_response.expected_amount,
                        claim_public_key: swap_response.claim_public_key,
                        timeout_block_height: swap_response.timeout_block_heights,
                        blinding_key: None,
                    },
                },
            )
            .await?;

        let address = ArkAddress::decode(swap_response.address.as_str()).unwrap();
        let amount = Amount::from_sat(swap_response.expected_amount);
        let txid = self.send_vtxo(address, amount).await?;

        Ok(BoltzSubmarineSwapResult {
            swap_id: swap_response.id,
            txid,
        })
    }

    /// Waits for the invoice to be paid by Boltz, after our Ark payment has been claimed.
    pub async fn wait_for_invoice_paid(&self, swap_id: &str) -> Result<(), Error> {
        use futures::StreamExt;

        let stream = self.subscribe_to_swap_updates(swap_id.to_string());
        tokio::pin!(stream);

        while let Some(status_result) = stream.next().await {
            match status_result {
                Ok(status) => {
                    tracing::debug!(current = ?status, "Swap status");
                    if status == SwapStatus::InvoicePaid {
                        return Ok(());
                    }
                }
                Err(e) => return Err(e),
            }
        }

        Err(Error::ad_hoc("Status stream ended unexpectedly"))
    }

    /// Attempts to refund a VHTLC
    ///
    /// Arguments
    /// - `swap_id` - the swap to refund
    /// - `collaborative`: whether to refund collaboratively with the swap provider
    pub async fn refund_vhtlc(&self, swap_id: &str, collaborative: bool) -> Result<Txid, Error> {
        let swap_data = self
            .swap_storage
            .get(swap_id)
            .await?
            .ok_or(Error::ad_hoc("Swap not found"))?;

        // TODO: Should probably persist or deterministically derive this.
        let refund_pk = self.inner.kp.public_key();
        // TODO: don't expect here
        let claim_pk = swap_data.metadata.claim_pk().expect("to have claim key");
        let claim_pk = secp256k1::PublicKey::from_str(&claim_pk).expect("to be xonly");

        // TODO: Use a different key!
        let server_pk = self.server_info.pk.x_only_public_key().0;

        let preimage_hash = swap_data.metadata.preimage_hash();

        let timeout_block_heights = swap_data.metadata.timeout_block_heights();

        dbg!("1");
        let vhtlc = VhtlcScript::new(
            VhtlcOptions {
                sender: refund_pk.x_only_public_key().0,
                // FIXME: wrong key, should be claim_pk
                receiver: claim_pk.x_only_public_key().0,
                server: server_pk,
                preimage_hash,
                refund_locktime: timeout_block_heights.refund,
                unilateral_claim_delay: Sequence::from_height(
                    timeout_block_heights.unilateral_claim,
                ),
                unilateral_refund_delay: Sequence::from_height(
                    timeout_block_heights.unilateral_refund,
                ),
                unilateral_refund_without_receiver_delay: Sequence::from_height(
                    timeout_block_heights.unilateral_refund_without_receiver,
                ),
            },
            self.server_info.network,
        )
        .map_err(Error::ad_hoc)?;

        dbg!("2");

        let vhtlc_address = vhtlc.address();
        if vhtlc_address.to_string() != swap_data.metadata.address() {
            return Err(Error::ad_hoc(format!(
                "VHTLC address ({vhtlc_address}) does not match swap address ({})",
                swap_data.metadata.address()
            )));
        }

        dbg!("3");

        let vhtlc_outpoint = {
            let request = GetVtxosRequest::new_for_addresses(&[vhtlc_address]);

            let list = timeout_op(
                self.inner.timeout,
                self.network_client().list_vtxos(request),
            )
            .await
            .context("Failed to fetch VHTLC")??;

            // We expect a single outpoint.
            let all = list.all();
            let vhtlc_outpoint = all.first().ok_or_else(|| {
                Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
            })?;

            vhtlc_outpoint.clone()
        };

        let refund_script = if collaborative {
            vhtlc.refund_script()
        } else {
            // TODO: implement refund after timeout
            unimplemented!();
        };

        let script_ver = (refund_script, LeafVersion::TapScript);
        let spend_info = vhtlc.taproot_info().expect("info");
        let control_block = spend_info
            .control_block(&script_ver)
            .ok_or(Error::ad_hoc("control block not found for refund script"))?;

        let script_pubkey = vhtlc.script_pubkey().expect("script pubkey");

        let vhtlc_input = VtxoInput::new(
            script_ver.0,
            control_block,
            vhtlc.tapscripts(),
            script_pubkey,
            swap_data.metadata.amount(),
            vhtlc_outpoint.outpoint,
        );

        let (claim_address, _) = self.get_offchain_address()?;
        let claim_amount = swap_data.metadata.amount();

        let outputs = vec![(&claim_address, claim_amount)];

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(&outputs, None, &[vhtlc_input.clone()], &self.server_info)?;

        // TODO: this needs to be different if NOT collaborative
        let expiry = swap_data.metadata.timeout_block_heights().refund;

        let sign_fn = |input: &mut psbt::Input,
                       msg: secp256k1::Message|
         -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
            {
                // FIXME: TODO: the below is probably wrong
                let sequence = Sequence::from_height(expiry as u16);

                // Initialized with a 1, because we only have one witness element: the length.
                let mut bytes = vec![];

                // TODO: maybe?
                let length = VarInt::from(4u64);

                length.consensus_encode(&mut bytes).unwrap();

                let sequence = sequence.to_consensus_u32();

                bytes.write_all(&sequence.to_le_bytes()).unwrap();

                input.unknown.insert(
                    psbt::raw::Key {
                        type_value: u8::MAX,
                        key: VTXO_TREE_EXPIRY_PSBT_KEY.to_vec(),
                    },
                    bytes,
                );
            }

            let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, self.kp());
            let pk = self.kp().x_only_public_key().0;

            Ok((sig, pk))
        };

        // TODO: Handle error properly.
        let checkpoint_tx = checkpoint_txs.first().expect("one");

        sign_ark_transaction(
            sign_fn,
            &mut ark_tx,
            &[(checkpoint_tx.1.clone(), checkpoint_tx.2)],
            0,
        )?;

        dbg!("8");

        let signed_ark_tx = if collaborative {
            let url = format!("{BOLTZ_URL}/v2/swap/submarine/{swap_id}/refund/ark");
            let client = reqwest::Client::new();
            let response = client
                .post(&url)
                .json(&RefundSwapRequest {
                    transaction: dbg!(ark_tx.to_string()),
                })
                .send()
                .await
                .map_err(Error::ad_hoc)?;

            let string = response.text().await.map_err(Error::ad_hoc)?;

            let refund_response: RefundSwapResponse =
                serde_json::from_str(&string).map_err(Error::ad_hoc)?;

            Psbt::from_str(refund_response.transaction.as_str())
                .map_err(Error::ad_hoc)
                .context("Could not parse refund transaction to psbt")?
        } else {
            ark_tx
        };
        dbg!("9");

        let ark_txid = signed_ark_tx.unsigned_tx.compute_txid();

        let res = self
            .network_client()
            .submit_offchain_transaction_request(
                signed_ark_tx,
                checkpoint_txs
                    .into_iter()
                    .map(|(psbt, _, _, _)| psbt)
                    .collect(),
            )
            .await
            .unwrap();

        // TODO: Handle error properly.
        let mut checkpoint_psbt = res.signed_checkpoint_txs.first().expect("one").clone();

        sign_checkpoint_transaction(sign_fn, &mut checkpoint_psbt, &vhtlc_input)?;

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, vec![checkpoint_psbt]),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize offchain transaction")?;

        tracing::info!(txid = %ark_txid, "Refunded VHTLC");

        Ok(ark_txid)
    }

    // REVERSE SUBMARINE SWAP

    // This is a reverse submarine swap (Lightning to Ark).
    //
    // For now, generate secret and claim PK internally (could extend to allow caller to pass these
    // in).
    //
    // Returns:
    //
    // - Lightning invoice.
    pub async fn get_ln_invoice(&self, amount: Amount) -> Result<BoltzSwapInvoice, Error> {
        let preimage: [u8; 32] = musig::rand::random();
        let preimage_hash = sha256::Hash::hash(&preimage);

        let claim_public_key = self.inner.kp.public_key();

        let request = CreateReverseSwapRequest {
            from: Asset::Btc,
            to: Asset::Ark,
            invoice_amount: amount.to_sat(),
            claim_public_key: claim_public_key.to_string(),
            preimage_hash: preimage_hash.to_byte_array().to_lower_hex_string(),
        };

        let url = format!("{BOLTZ_URL}/v2/swap/reverse");

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

        let invoice: Bolt11Invoice = response
            .invoice
            .parse()
            .map_err(|e: ParseOrSemanticError| Error::ad_hoc(e.to_string()))
            .context("failed to parse BOLT11 invoice")?;

        // Persist the swap and subscribe to WebSocket updates
        let swap = SwapData {
            id: response.id.clone(),
            swap_type: SwapType::Reverse,
            status: SwapStatus::Created,
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
            metadata: SwapMetadata::Reverse {
                preimage,
                preimage_hash,
                refund_public_key: response.refund_public_key,
                lockup_address: response.lockup_address.clone(),
                timeout_block_heights: response.timeout_block_heights,
                onchain_amount: response.onchain_amount,
                invoice: response.invoice.clone(),
            },
        };

        self.swap_storage
            .insert(response.id.clone(), swap.clone())
            .await
            .context("failed to persist swap data")?;

        Ok(BoltzSwapInvoice {
            invoice,
            swap_data: swap,
        })
    }

    /// Waits for a payment and settles it into our own wallet
    pub async fn wait_for_payment(&self, swap_id: &str) -> Result<(), Error> {
        use futures::StreamExt;

        let swap = self
            .swap_storage
            .get(swap_id)
            .await
            .context("failed to get swap data")?
            .ok_or_else(|| Error::ad_hoc(format!("swap data not found: {swap_id}")))?;

        let stream = self.subscribe_to_swap_updates(swap_id.to_string());
        tokio::pin!(stream);

        while let Some(status_result) = stream.next().await {
            match status_result {
                Ok(status) => {
                    tracing::debug!(current = ?status, "Swap status");
                    if matches!(
                        status,
                        SwapStatus::TransactionMempool | SwapStatus::TransactionConfirmed
                    ) {
                        break;
                    }
                }
                Err(e) => return Err(e),
            }
        }

        tracing::debug!("Ark transaction for swap found");

        let preimage = swap.metadata.preimage().expect("preimage");

        let refund_pk = swap.metadata.refund_pk().expect("refund pk");

        // TODO: Should probably persist or deterministically derive this.
        let claim_pk = self.inner.kp.public_key();

        // TODO: Use a different key!
        let server_pk = self.server_info.pk.x_only_public_key().0;

        let preimage_hash = swap.metadata.preimage_hash();

        let timeout_block_heights = swap.metadata.timeout_block_heights();

        let vhtlc = VhtlcScript::new(
            VhtlcOptions {
                sender: refund_pk.inner.x_only_public_key().0,
                receiver: claim_pk.x_only_public_key().0,
                server: server_pk,
                preimage_hash,
                refund_locktime: timeout_block_heights.refund,
                unilateral_claim_delay: Sequence::from_height(
                    timeout_block_heights.unilateral_claim,
                ),
                unilateral_refund_delay: Sequence::from_height(
                    timeout_block_heights.unilateral_refund,
                ),
                unilateral_refund_without_receiver_delay: Sequence::from_height(
                    timeout_block_heights.unilateral_refund_without_receiver,
                ),
            },
            self.server_info.network,
        )
        .map_err(Error::ad_hoc)?;

        let vhtlc_address = vhtlc.address();
        if vhtlc_address.to_string() != swap.metadata.address() {
            return Err(Error::ad_hoc(format!(
                "VHTLC address ({vhtlc_address}) does not match swap address ({})",
                swap.metadata.address()
            )));
        }

        // TODO: Ideally we can skip this if the vout is always the same (probably 0).
        let vhtlc_outpoint = {
            let request = GetVtxosRequest::new_for_addresses(&[vhtlc_address]);

            let list = timeout_op(
                self.inner.timeout,
                self.network_client().list_vtxos(request),
            )
            .await
            .context("Failed to fetch VHTLC")??;

            // We expect a single outpoint.
            let all = list.all();
            let vhtlc_outpoint = all.first().ok_or_else(|| {
                Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
            })?;

            vhtlc_outpoint.clone()
        };

        let (claim_address, _) = self.get_offchain_address()?;
        let claim_amount = swap.metadata.amount();

        let outputs = vec![(&claim_address, claim_amount)];

        let spend_info = vhtlc.taproot_info().expect("info");
        let script_ver = (vhtlc.claim_script(), LeafVersion::TapScript);
        let control_block = spend_info
            .control_block(&script_ver)
            .ok_or(Error::ad_hoc("control block not found for claim script"))?;

        let script_pubkey = vhtlc.script_pubkey().expect("script pubkey");

        let vhtlc_input = VtxoInput::new(
            script_ver.0,
            control_block,
            vhtlc.tapscripts(),
            script_pubkey,
            claim_amount,
            vhtlc_outpoint.outpoint,
        );

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(&outputs, None, &[vhtlc_input.clone()], &self.server_info)?;

        let sign_fn = |input: &mut psbt::Input,
                       msg: secp256k1::Message|
         -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
            // Add preimage to PSBT input.
            {
                // Initialized with a 1, because we only have one witness element: the preimage.
                let mut bytes = vec![1];

                let length = VarInt::from(preimage.len() as u64);

                length.consensus_encode(&mut bytes).unwrap();

                bytes.write_all(&preimage).unwrap();

                input.unknown.insert(
                    psbt::raw::Key {
                        type_value: u8::MAX,
                        key: VTXO_CONDITION_KEY.to_vec(),
                    },
                    bytes,
                );
            }

            let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, self.kp());
            let pk = self.kp().x_only_public_key().0;

            Ok((sig, pk))
        };

        // TODO: Handle error properly.
        let checkpoint_tx = checkpoint_txs.first().expect("one");

        sign_ark_transaction(
            sign_fn,
            &mut ark_tx,
            &[(checkpoint_tx.1.clone(), checkpoint_tx.2)],
            0,
        )?;

        let ark_txid = ark_tx.unsigned_tx.compute_txid();

        let res = self
            .network_client()
            .submit_offchain_transaction_request(
                ark_tx,
                checkpoint_txs
                    .into_iter()
                    .map(|(psbt, _, _, _)| psbt)
                    .collect(),
            )
            .await
            .unwrap();

        // TODO: Handle error properly.
        let mut checkpoint_psbt = res.signed_checkpoint_txs.first().expect("one").clone();

        sign_checkpoint_transaction(sign_fn, &mut checkpoint_psbt, &vhtlc_input)?;

        timeout_op(
            self.inner.timeout,
            self.network_client()
                .finalize_offchain_transaction(ark_txid, vec![checkpoint_psbt]),
        )
        .await?
        .map_err(Error::ark_server)
        .context("failed to finalize offchain transaction")?;

        tracing::info!(txid = %ark_txid, "Spent VHTLC");

        Ok(())
    }

    pub fn subscribe_to_swap_updates(
        &self,
        swap_id: String,
    ) -> impl futures::Stream<Item = Result<SwapStatus, Error>> + '_ {
        async_stream::stream! {
            let mut last_status: Option<SwapStatus> = None;
            let url = format!("{BOLTZ_URL}/v2/swap/{swap_id}");

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

                                // Only yield if status has changed
                                if last_status.as_ref() != Some(&current_status) {
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
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

/// Persistent swap data
///
/// This structure maintains swap state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapData {
    /// Unique swap identifier
    pub id: String,
    /// Type of swap (submarine or reverse)
    pub swap_type: SwapType,
    /// Current swap status
    pub status: SwapStatus,
    /// Unix timestamp when swap was created
    pub created_at: u64,
    /// Swap-specific metadata
    pub metadata: SwapMetadata,
}

/// Type of Boltz swap
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SwapType {
    /// On-chain to Lightning swap
    Submarine,
    /// Lightning to on-chain swap
    Reverse,
}

/// All possible states of a Boltz swap
///
/// Swaps progress through these states during their lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SwapStatus {
    /// Initial state when swap is created
    #[serde(rename = "swap.created")]
    Created,
    /// Lockup transaction detected in mempool
    #[serde(rename = "transaction.mempool")]
    TransactionMempool,
    /// Lockup transaction confirmed on-chain
    #[serde(rename = "transaction.confirmed")]
    TransactionConfirmed,
    /// Transaction Refunded
    #[serde(rename = "transaction.refunded")]
    TransactionRefunded,
    /// Transaction Failed
    #[serde(rename = "transaction.failed")]
    TransactionFailed,
    /// Transaction Claimed
    #[serde(rename = "transaction.claimed")]
    TransactionClaimed,
    /// Lightning invoice has been set
    #[serde(rename = "invoice.set")]
    InvoiceSet,
    /// Waiting for Lightning invoice payment
    #[serde(rename = "invoice.pending")]
    InvoicePending,
    /// Lightning invoice successfully paid
    #[serde(rename = "invoice.paid")]
    InvoicePaid,
    /// Lightning invoice payment failed
    #[serde(rename = "invoice.failedToPay")]
    InvoiceFailedToPay,
    /// Invoice Expired
    #[serde(rename = "invoice.expired")]
    InvoiceExpired,
    /// Swap expired - can be refunded
    #[serde(rename = "swap.expired")]
    SwapExpired,
    /// Swap failed with error
    #[serde(rename = "error")]
    Error { error: String },
}

/// Swap metadata fields based on swap type
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SwapMetadata {
    /// Metadata for reverse submarine swaps (Lightning to on-chain)
    Reverse {
        /// Preimage for the swap
        preimage: [u8; 32],
        /// Hash of the preimage
        preimage_hash: sha256::Hash,
        /// Public key for refund
        refund_public_key: PublicKey,
        /// Address where funds are locked
        lockup_address: String,
        /// Block height when swap times out
        timeout_block_heights: TimeoutBlockHeights,
        /// Amount to be sent on-chain
        onchain_amount: u64,
        /// Invoice associated with the swap
        invoice: String,
    },
    /// Metadata for submarine swaps (ark to Lightning)
    Submarine {
        /// Address to send funds to
        address: String,
        /// Extracted from the bolt11 invoice
        preimage_hash: sha256::Hash,
        /// Redeem script for the swap
        redeem_script: String,
        /// Whether zero-confirmation transactions are accepted
        accept_zero_conf: bool,
        /// Expected amount to be received
        expected_amount: u64,
        /// Public key for claiming funds
        claim_public_key: String,
        /// Block height when swap times out
        timeout_block_height: TimeoutBlockHeights,
        /// Optional blinding key for confidential transactions
        blinding_key: Option<String>,
    },
}

impl SwapMetadata {
    /// Retrieves the preimage if available
    ///
    /// # Returns
    /// - `Some(String)` containing the preimage for reverse swaps
    /// - `None` for submarine swaps
    pub fn preimage(&self) -> Option<[u8; 32]> {
        match self {
            SwapMetadata::Reverse { preimage, .. } => Some(*preimage),
            SwapMetadata::Submarine { .. } => None,
        }
    }

    pub fn preimage_hash(&self) -> sha256::Hash {
        match self {
            SwapMetadata::Reverse { preimage_hash, .. } => *preimage_hash,
            SwapMetadata::Submarine { preimage_hash, .. } => *preimage_hash,
        }
    }

    pub fn address(&self) -> String {
        match self {
            SwapMetadata::Reverse { lockup_address, .. } => lockup_address.clone(),
            SwapMetadata::Submarine { address, .. } => address.clone(),
        }
    }

    pub fn amount(&self) -> Amount {
        let amount = match self {
            SwapMetadata::Reverse { onchain_amount, .. } => *onchain_amount,
            SwapMetadata::Submarine {
                expected_amount, ..
            } => *expected_amount,
        };
        Amount::from_sat(amount)
    }

    pub fn invoice(&self) -> Option<Bolt11Invoice> {
        match self {
            SwapMetadata::Reverse { invoice, .. } => {
                let invoice = invoice.parse::<Bolt11Invoice>().unwrap();
                Some(invoice)
            }
            SwapMetadata::Submarine { .. } => None,
        }
    }

    pub fn refund_pk(&self) -> Option<PublicKey> {
        match self {
            SwapMetadata::Reverse {
                refund_public_key, ..
            } => Some(*refund_public_key),
            SwapMetadata::Submarine { .. } => None,
        }
    }

    pub fn claim_pk(&self) -> Option<String> {
        match self {
            SwapMetadata::Reverse { .. } => None,
            SwapMetadata::Submarine {
                claim_public_key, ..
            } => Some(claim_public_key.clone()),
        }
    }

    pub fn timeout_block_heights(&self) -> TimeoutBlockHeights {
        match self {
            SwapMetadata::Reverse {
                timeout_block_heights,
                ..
            } => timeout_block_heights.clone(),
            SwapMetadata::Submarine {
                timeout_block_height,
                ..
            } => timeout_block_height.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReverseSwapRequest {
    pub from: Asset,
    pub to: Asset,
    pub invoice_amount: u64,
    pub claim_public_key: String,
    pub preimage_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Asset {
    Btc,
    Ark,
    Lightning,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateReverseSwapResponse {
    pub id: String,
    pub lockup_address: String,
    pub refund_public_key: PublicKey,
    pub timeout_block_heights: TimeoutBlockHeights,
    pub invoice: String,
    pub onchain_amount: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Copy)]
#[serde(rename_all = "camelCase")]
pub struct TimeoutBlockHeights {
    pub refund: u32,
    pub unilateral_claim: u16,
    pub unilateral_refund: u16,
    pub unilateral_refund_without_receiver: u16,
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
#[serde(rename_all = "camelCase")]
pub struct SwapStatusResponse {
    status: SwapStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SwapTransaction {
    id: String,
    hex: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSubmarineSwapRequest {
    pub from: Asset,
    pub to: Asset,
    pub invoice: String,
    #[serde(rename = "refundPublicKey")]
    pub refund_public_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSubmarineSwapResponse {
    pub id: String,
    pub address: String,
    pub accept_zero_conf: bool,
    pub expected_amount: u64,
    pub claim_public_key: String,
    pub timeout_block_heights: TimeoutBlockHeights,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetSwapStatusResponse {
    pub status: SwapStatus,
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
pub struct RefundSwapRequest {
    transaction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefundSwapResponse {
    transaction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

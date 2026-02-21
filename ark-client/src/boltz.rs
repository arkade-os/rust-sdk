use crate::batch::BatchOutputType;
use crate::error::ErrorContext as _;
use crate::swap_storage::SwapStorage;
use crate::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::intent;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::send::OffchainTransactions;
use ark_core::send::VtxoInput;
use ark_core::server::parse_sequence_number;
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
use std::str::FromStr;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

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

impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: crate::KeyProvider,
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
        let refund_public_key = self
            .next_keypair(crate::key_provider::KeypairIndex::New)?
            .public_key();

        let preimage_hash = invoice.payment_hash();
        let preimage_hash = ripemd160::Hash::hash(preimage_hash.as_byte_array());

        let request = CreateSubmarineSwapRequest {
            from: Asset::Ark,
            to: Asset::Btc,
            invoice,
            refund_public_key: refund_public_key.into(),
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
    /// payments with an Ark wallet.
    ///
    /// # Arguments
    ///
    /// - `invoice`: a [`Bolt11Invoice`] to be paid.
    ///
    /// # Returns
    ///
    /// - A [`SubmarineSwapResult`], including an identifier for the swap and the TXID of the Ark
    ///   transaction that funds the VHTLC.
    pub async fn pay_ln_invoice(
        &self,
        invoice: Bolt11Invoice,
    ) -> Result<SubmarineSwapResult, Error> {
        let keypair = self.next_keypair(crate::key_provider::KeypairIndex::New)?;
        let refund_public_key = keypair.public_key();

        let preimage_hash = invoice.payment_hash();
        let preimage_hash = ripemd160::Hash::hash(preimage_hash.as_byte_array());

        let request = CreateSubmarineSwapRequest {
            from: Asset::Ark,
            to: Asset::Btc,
            invoice,
            refund_public_key: refund_public_key.into(),
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
                },
            )
            .await?;

        let vhtlc_address = swap_response.address;
        let amount = swap_response.expected_amount;
        let txid = self.send_vtxo(vhtlc_address, amount).await?;

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

        let stream = self.subscribe_to_swap_updates(swap_id.to_string());
        tokio::pin!(stream);

        while let Some(status_result) = stream.next().await {
            match status_result {
                Ok(status) => {
                    tracing::debug!(swap_id, current = ?status, "Swap status");
                    match status {
                        SwapStatus::InvoicePaid => {
                            let preimage = self
                                .extract_submarine_swap_preimage(swap_id)
                                .await
                                .context(
                                    "invoice paid but failed to extract preimage from claim tx",
                                )?;

                            return Ok(preimage);
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
                        SwapStatus::InvoiceSet
                        | SwapStatus::InvoicePending
                        | SwapStatus::Created
                        | SwapStatus::TransactionMempool
                        | SwapStatus::TransactionConfirmed
                        | SwapStatus::TransactionRefunded
                        | SwapStatus::TransactionFailed
                        | SwapStatus::TransactionClaimed
                        | SwapStatus::InvoiceFailedToPay
                        | SwapStatus::SwapExpired => {}
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
        let swap_data = self
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
        let claim_txs = self
            .network_client()
            .get_virtual_txs(vec![claim_txid.to_string()], None)
            .await
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
        let mut updated = swap_data.clone();
        updated.preimage = Some(preimage);
        self.swap_storage()
            .update_submarine(swap_id, updated)
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
        let swap_data = self
            .swap_storage()
            .get_submarine(swap_id)
            .await?
            .ok_or(Error::ad_hoc("Submarine swap not found"))?;

        let timeout_block_heights = swap_data.timeout_block_heights;

        let vhtlc = VhtlcScript::new(
            VhtlcOptions {
                sender: swap_data.refund_public_key.into(),
                receiver: swap_data.claim_public_key.into(),
                server: self.server_info.signer_pk.into(),
                preimage_hash: swap_data.preimage_hash,
                refund_locktime: timeout_block_heights.refund,
                unilateral_claim_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_claim as i64,
                )
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral claim timeout: {e}")))?,
                unilateral_refund_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_refund as i64,
                )
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral refund timeout: {e}")))?,
                unilateral_refund_without_receiver_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_refund_without_receiver as i64,
                )
                .map_err(|e| {
                    Error::ad_hoc(format!("invalid refund without receiver timeout: {e}"))
                })?,
            },
            self.server_info.network,
        )
        .map_err(Error::ad_hoc)?;

        let vhtlc_address = vhtlc.address();
        if vhtlc_address != swap_data.vhtlc_address {
            return Err(Error::ad_hoc(format!(
                "VHTLC address ({vhtlc_address}) does not match swap address ({})",
                swap_data.vhtlc_address
            )));
        }

        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(self.server_info.dust, virtual_tx_outpoints);

            // We expect a single outpoint.
            let mut unspent = vtxo_list.all_unspent();
            let vhtlc_outpoint = unspent.next().ok_or_else(|| {
                Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
            })?;

            vhtlc_outpoint.clone()
        };

        let (refund_address, _) = self.get_offchain_address()?;
        let refund_amount = swap_data.amount;

        let outputs = vec![(&refund_address, refund_amount)];

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
        );

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &outputs,
            None,
            std::slice::from_ref(&vhtlc_input),
            &self.server_info,
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
        let swap_data = self
            .swap_storage()
            .get_submarine(swap_id)
            .await?
            .ok_or(Error::ad_hoc("Submarine swap not found"))?;

        let timeout_block_heights = swap_data.timeout_block_heights;

        let vhtlc = VhtlcScript::new(
            VhtlcOptions {
                sender: swap_data.refund_public_key.into(),
                receiver: swap_data.claim_public_key.into(),
                server: self.server_info.signer_pk.into(),
                preimage_hash: swap_data.preimage_hash,
                refund_locktime: timeout_block_heights.refund,
                unilateral_claim_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_claim as i64,
                )
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral claim timeout: {e}")))?,
                unilateral_refund_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_refund as i64,
                )
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral refund timeout: {e}")))?,
                unilateral_refund_without_receiver_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_refund_without_receiver as i64,
                )
                .map_err(|e| {
                    Error::ad_hoc(format!("invalid refund without receiver timeout: {e}"))
                })?,
            },
            self.server_info.network,
        )
        .map_err(Error::ad_hoc)?;

        let vhtlc_address = vhtlc.address();
        if vhtlc_address != swap_data.vhtlc_address {
            return Err(Error::ad_hoc(format!(
                "VHTLC address ({vhtlc_address}) does not match swap address ({})",
                swap_data.vhtlc_address
            )));
        }

        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(self.server_info.dust, virtual_tx_outpoints);

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

        let (refund_address, _) = self.get_offchain_address()?;
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
        );

        let commitment_txid = self
            .join_next_batch(
                rng,
                Vec::new(),
                vec![vhtlc_input],
                BatchOutputType::Board {
                    to_address: refund_address,
                    to_amount: refund_amount,
                },
            )
            .await
            .context("failed to join batch")?;

        tracing::info!(txid = %commitment_txid, "Refunded VHTLC via settlement");

        Ok(commitment_txid)
    }

    /// Refund a VHTLC with collaboration from Boltz.
    ///
    /// This path requires Boltz's cooperation to sign the refund transaction. It allows refunding
    /// a submarine swap before the timelock expires. For refunds after timelock expiry without
    /// Boltz cooperation, use [`Client::refund_expired_vhtlc`] instead.
    pub async fn refund_vhtlc(&self, swap_id: &str) -> Result<Txid, Error> {
        let swap_data = self
            .swap_storage()
            .get_submarine(swap_id)
            .await?
            .ok_or(Error::ad_hoc("submarine swap not found"))?;

        let timeout_block_heights = swap_data.timeout_block_heights;

        let vhtlc = VhtlcScript::new(
            VhtlcOptions {
                sender: swap_data.refund_public_key.into(),
                receiver: swap_data.claim_public_key.into(),
                server: self.server_info.signer_pk.into(),
                preimage_hash: swap_data.preimage_hash,
                refund_locktime: timeout_block_heights.refund,
                unilateral_claim_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_claim as i64,
                )
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral claim timeout: {e}")))?,
                unilateral_refund_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_refund as i64,
                )
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral refund timeout: {e}")))?,
                unilateral_refund_without_receiver_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_refund_without_receiver as i64,
                )
                .map_err(|e| {
                    Error::ad_hoc(format!("invalid refund without receiver timeout: {e}"))
                })?,
            },
            self.server_info.network,
        )
        .map_err(Error::ad_hoc)?;

        let vhtlc_address = vhtlc.address();
        if vhtlc_address != swap_data.vhtlc_address {
            return Err(Error::ad_hoc(format!(
                "VHTLC address ({vhtlc_address}) does not match swap address ({})",
                swap_data.vhtlc_address
            )));
        }

        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(self.server_info.dust, virtual_tx_outpoints);

            // We expect a single outpoint.
            let mut unspent = vtxo_list.all_unspent();
            let vhtlc_outpoint = unspent.next().ok_or_else(|| {
                Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
            })?;

            vhtlc_outpoint.clone()
        };

        let (refund_address, _) = self.get_offchain_address()?;
        let refund_amount = swap_data.amount;

        let outputs = vec![(&refund_address, refund_amount)];

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
        );

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &outputs,
            None,
            std::slice::from_ref(&vhtlc_input),
            &self.server_info,
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

        tracing::info!(swap_id, txid = %ark_txid, "Refunded VHTLC via collaborative refund");

        Ok(ark_txid)
    }

    // Reverse submarine swap.

    /// Generate a BOLT11 invoice to perform a reverse submarine swap via Boltz. This allows to
    /// receive Lightning payments into an Ark wallet.
    ///
    /// # Arguments
    ///
    /// - `amount`: the expected [`Amount`] to be received.
    ///
    /// # Returns
    ///
    /// - A `ReverseSwapResult`, including an identifier for the reverse swap and the
    ///   [`Bolt11Invoice`] to be paid.
    pub async fn get_ln_invoice(
        &self,
        amount: SwapAmount,
        expiry_secs: Option<u64>,
    ) -> Result<ReverseSwapResult, Error> {
        let preimage: [u8; 32] = rand::random();
        let preimage_hash_sha256 = sha256::Hash::hash(&preimage);
        let preimage_hash = ripemd160::Hash::hash(preimage_hash_sha256.as_byte_array());

        let claim_public_key = self
            .next_keypair(crate::key_provider::KeypairIndex::New)?
            .public_key();

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

        let swap = ReverseSwapData {
            id: response.id.clone(),
            status: SwapStatus::Created,
            preimage: Some(preimage),
            vhtlc_address: response.lockup_address,
            preimage_hash,
            refund_public_key: response.refund_public_key,
            amount: swap_amount,
            claim_public_key: claim_public_key.into(),
            timeout_block_heights: response.timeout_block_heights,
            created_at: created_at.as_secs(),
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

    /// Generate a BOLT11 invoice using a provided SHA256 preimage hash for a reverse submarine
    /// swap via Boltz. This allows receiving Lightning payments when the preimage is managed
    /// externally.
    ///
    /// # Arguments
    ///
    /// - `amount`: the expected [`Amount`] to be received.
    /// - `preimage_hash_sha256`: the SHA256 hash of the preimage. The preimage itself is not stored
    ///   and must be provided later when claiming via [`Self::claim_vhtlc`].
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
    ) -> Result<ReverseSwapResult, Error> {
        let preimage_hash = ripemd160::Hash::hash(preimage_hash_sha256.as_byte_array());

        let keypair = self.next_keypair(crate::key_provider::KeypairIndex::New)?;
        let claim_public_key = keypair.public_key();

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

        let swap = ReverseSwapData {
            id: response.id.clone(),
            status: SwapStatus::Created,
            preimage: None, // Preimage not known at creation time
            vhtlc_address: response.lockup_address,
            preimage_hash,
            refund_public_key: response.refund_public_key,
            amount: swap_amount,
            claim_public_key: claim_public_key.into(),
            timeout_block_heights: response.timeout_block_heights,
            created_at: created_at.as_secs(),
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

        let stream = self.subscribe_to_swap_updates(swap_id.to_string());
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
                        | SwapStatus::InvoiceSet
                        | SwapStatus::InvoicePending
                        | SwapStatus::InvoicePaid
                        | SwapStatus::InvoiceFailedToPay
                        | SwapStatus::SwapExpired => {}
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
        let swap = self
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

        let timeout_block_heights = swap.timeout_block_heights;

        let vhtlc = VhtlcScript::new(
            VhtlcOptions {
                sender: swap.refund_public_key.into(),
                receiver: swap.claim_public_key.into(),
                server: self.server_info.signer_pk.into(),
                preimage_hash: swap.preimage_hash,
                refund_locktime: timeout_block_heights.refund,
                unilateral_claim_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_claim as i64,
                )
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral claim timeout: {e}")))?,
                unilateral_refund_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_refund as i64,
                )
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral refund timeout: {e}")))?,
                unilateral_refund_without_receiver_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_refund_without_receiver as i64,
                )
                .map_err(|e| {
                    Error::ad_hoc(format!("invalid refund without receiver timeout: {e}"))
                })?,
            },
            self.server_info.network,
        )
        .map_err(Error::ad_hoc)
        .context("failed to build VHTLC script")?;

        let vhtlc_address = vhtlc.address();
        if vhtlc_address != swap.vhtlc_address {
            return Err(Error::ad_hoc(format!(
                "VHTLC address ({vhtlc_address}) does not match swap address ({})",
                swap.vhtlc_address
            )));
        }

        // TODO: Ideally we can skip this if the vout is always the same (probably 0).
        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(self.server_info.dust, virtual_tx_outpoints);

            // We expect a single outpoint.
            let mut unspent = vtxo_list.all_unspent();
            let vhtlc_outpoint = unspent.next().ok_or_else(|| {
                Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
            })?;

            vhtlc_outpoint.clone()
        };

        let (claim_address, _) = self
            .get_offchain_address()
            .context("failed to get offchain address")?;
        let claim_amount = swap.amount;

        let outputs = vec![(&claim_address, claim_amount)];

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
        );

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &outputs,
            None,
            std::slice::from_ref(&vhtlc_input),
            &self.server_info,
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
            .context("failed to sign Ark TX")?;

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
        use futures::StreamExt;

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

        let stream = self.subscribe_to_swap_updates(swap_id.to_string());
        tokio::pin!(stream);

        while let Some(status_result) = stream.next().await {
            match status_result {
                Ok(status) => {
                    tracing::debug!(current = ?status, "Swap status");

                    match status {
                        SwapStatus::TransactionMempool | SwapStatus::TransactionConfirmed => break,
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
                        | SwapStatus::InvoiceSet
                        | SwapStatus::InvoicePending
                        | SwapStatus::InvoicePaid
                        | SwapStatus::InvoiceFailedToPay
                        | SwapStatus::SwapExpired => {}
                    }
                }
                Err(e) => return Err(e),
            }
        }

        tracing::debug!("Ark transaction for swap found");

        let timeout_block_heights = swap.timeout_block_heights;

        let vhtlc = VhtlcScript::new(
            VhtlcOptions {
                sender: swap.refund_public_key.into(),
                receiver: swap.claim_public_key.into(),
                server: self.server_info.signer_pk.into(),
                preimage_hash: swap.preimage_hash,
                refund_locktime: timeout_block_heights.refund,
                unilateral_claim_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_claim as i64,
                )
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral claim timeout: {e}")))?,
                unilateral_refund_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_refund as i64,
                )
                .map_err(|e| Error::ad_hoc(format!("invalid unilateral refund timeout: {e}")))?,
                unilateral_refund_without_receiver_delay: parse_sequence_number(
                    timeout_block_heights.unilateral_refund_without_receiver as i64,
                )
                .map_err(|e| {
                    Error::ad_hoc(format!("invalid refund without receiver timeout: {e}"))
                })?,
            },
            self.server_info.network,
        )
        .map_err(Error::ad_hoc)
        .context("failed to build VHTLC script")?;

        let vhtlc_address = vhtlc.address();
        if vhtlc_address != swap.vhtlc_address {
            return Err(Error::ad_hoc(format!(
                "VHTLC address ({vhtlc_address}) does not match swap address ({})",
                swap.vhtlc_address
            )));
        }

        // TODO: Ideally we can skip this if the vout is always the same (probably 0).
        let vhtlc_outpoint = {
            let virtual_tx_outpoints = self
                .get_virtual_tx_outpoints(std::iter::once(vhtlc_address))
                .await?;

            let vtxo_list = VtxoList::new(self.server_info.dust, virtual_tx_outpoints);

            // We expect a single outpoint.
            let mut unspent = vtxo_list.all_unspent();
            let vhtlc_outpoint = unspent.next().ok_or_else(|| {
                Error::ad_hoc(format!("no outpoint found for address {vhtlc_address}"))
            })?;

            vhtlc_outpoint.clone()
        };

        let (claim_address, _) = self
            .get_offchain_address()
            .context("failed to get offchain address")?;
        let claim_amount = swap.amount;

        let outputs = vec![(&claim_address, claim_amount)];

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
        );

        let OffchainTransactions {
            mut ark_tx,
            checkpoint_txs,
        } = build_offchain_transactions(
            &outputs,
            None,
            std::slice::from_ref(&vhtlc_input),
            &self.server_info,
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
            .context("failed to sign Ark TX")?;

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

        tracing::info!(txid = %ark_txid, "Spent VHTLC");

        Ok(ClaimVhtlcResult {
            swap_id: swap_id.to_string(),
            claim_txid: ark_txid,
            claim_amount,
            preimage,
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

        // Fetch submarine swap fees (ARK -> BTC)
        let submarine_url = format!("{}/v2/swap/submarine", &self.inner.boltz_url);
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

        // Fetch reverse swap fees (BTC -> ARK)
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
    /// Lightning invoice has been set.
    #[serde(rename = "invoice.set")]
    InvoiceSet,
    /// Waiting for Lightning invoice payment.
    #[serde(rename = "invoice.pending")]
    InvoicePending,
    /// Lightning invoice successfully paid.
    #[serde(rename = "invoice.paid")]
    InvoicePaid,
    /// Lightning invoice payment failed.
    #[serde(rename = "invoice.failedToPay")]
    InvoiceFailedToPay,
    /// Invoice expired.
    #[serde(rename = "invoice.expired")]
    InvoiceExpired,
    /// Swap expired - can be refunded.
    #[serde(rename = "swap.expired")]
    SwapExpired,
    /// Swap failed with error.
    #[serde(rename = "error")]
    Error { error: String },
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

/// Fee information for submarine swaps (Ark -> Lightning).
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

/// Fee information for reverse swaps (Lightning -> Ark).
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
    /// Fees for submarine swaps (Ark -> Lightning).
    pub submarine: SubmarineSwapFees,
    /// Fees for reverse swaps (Lightning -> Ark).
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

#[cfg(test)]
mod tests {
    use super::*;

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
}

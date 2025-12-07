use crate::Blockchain;
use crate::Client;
use crate::Error;
use crate::error::ErrorContext as _;
use crate::swap_storage::SwapStorage;
use crate::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use ark_core::ArkAddress;
use ark_core::VTXO_CONDITION_KEY;
use ark_core::send::OffchainTransactions;
use ark_core::send::VtxoInput;
use ark_core::send::build_offchain_transactions;
use ark_core::send::sign_ark_transaction;
use ark_core::send::sign_checkpoint_transaction;
use ark_core::server::GetVtxosRequest;
use ark_core::server::parse_sequence_number;
use ark_core::vhtlc::VhtlcOptions;
use ark_core::vhtlc::VhtlcScript;
use bitcoin::Amount;
use bitcoin::Psbt;
use bitcoin::PublicKey;
use bitcoin::Txid;
use bitcoin::VarInt;
use bitcoin::XOnlyPublicKey;
use bitcoin::absolute::LockTime;
use bitcoin::consensus::Encodable;
use bitcoin::hashes::Hash;
use bitcoin::hashes::ripemd160;
use bitcoin::hashes::sha256;
use bitcoin::io::Write;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::taproot::LeafVersion;
use lightning_invoice::Bolt11Invoice;
use serde::Deserialize;
use serde::Serialize;
use serde_with::DisplayFromStr;
use serde_with::serde_as;
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
    /// Boltz will first need to claim our VHTLC before paying the invoice.
    pub async fn wait_for_invoice_paid(&self, swap_id: &str) -> Result<(), Error> {
        use futures::StreamExt;

        let stream = self.subscribe_to_swap_updates(swap_id.to_string());
        tokio::pin!(stream);

        while let Some(status_result) = stream.next().await {
            match status_result {
                Ok(status) => {
                    tracing::debug!(swap_id, current = ?status, "Swap status");
                    match status {
                        SwapStatus::InvoicePaid => {
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
            Some(LockTime::from_consensus(
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

    /// Refund a VHTLC with collaboration from Boltz.
    // TODO: This path is not supported by Boltz yet.
    pub async fn refund_vhtlc(&self, swap_id: &str) -> Result<Txid, Error> {
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

        let (refund_address, _) = self.get_offchain_address()?;
        let refund_amount = swap_data.amount;

        let outputs = vec![(&refund_address, refund_amount)];

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
            Some(LockTime::from_consensus(
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
                // TODO: Implement this once Boltz supports this path and we can test it.

                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &kp);
                let pk = kp.x_only_public_key().0;

                Ok(vec![(sig, pk)])
            };

        sign_ark_transaction(sign_fn, &mut ark_tx, 0)?;

        let url = format!(
            "{}/v2/swap/submarine/{swap_id}/refund/ark",
            self.inner.boltz_url
        );
        let client = reqwest::Client::new();
        let response = client
            .post(&url)
            .json(&RefundSwapRequest {
                transaction: ark_tx.to_string(),
            })
            .send()
            .await
            .map_err(Error::ad_hoc)?;

        let refund_response: RefundSwapResponse = response.json().await.map_err(Error::ad_hoc)?;
        if let Some(err) = refund_response.error.as_deref() {
            return Err(Error::ad_hoc(format!("Boltz refund request failed: {err}")));
        }

        let signed_ark_tx = Psbt::from_str(refund_response.transaction.as_str())
            .map_err(Error::ad_hoc)
            .context("Could not parse refund transaction to psbt")?;

        let ark_txid = signed_ark_tx.unsigned_tx.compute_txid();

        let res = self
            .network_client()
            .submit_offchain_transaction_request(signed_ark_tx, checkpoint_txs)
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
        let preimage: [u8; 32] = musig::rand::random();
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
            let request = GetVtxosRequest::new_for_addresses(&[vhtlc_address]);

            let list = timeout_op(
                self.inner.timeout,
                self.network_client().list_vtxos(request),
            )
            .await
            .context("failed to fetch VHTLC")?
            .map_err(Error::ark_server)
            .context("failed to fetch VHTLC")?;

            // We expect a single outpoint.
            let all = list.all();
            let vhtlc_outpoint = all.first().ok_or_else(|| {
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
            let request = GetVtxosRequest::new_for_addresses(&[vhtlc_address]);

            let list = timeout_op(
                self.inner.timeout,
                self.network_client().list_vtxos(request),
            )
            .await
            .context("failed to fetch VHTLC")?
            .map_err(Error::ark_server)
            .context("failed to fetch VHTLC")?;

            // We expect a single outpoint.
            let all = list.all();
            let vhtlc_outpoint = all.first().ok_or_else(|| {
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RefundSwapResponse {
    transaction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
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

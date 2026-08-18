use crate::error::ErrorContext as _;
use crate::swap_storage::SwapStorage;
use crate::utils::sleep;
use crate::utils::timeout_op;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use ark_core::batch;
use ark_core::batch::aggregate_nonces;
use ark_core::batch::complete_delegate_forfeit_txs;
use ark_core::batch::create_and_sign_forfeit_txs;
use ark_core::batch::create_asset_preservation_packet;
use ark_core::batch::generate_nonce_tree;
use ark_core::batch::sign_batch_tree_tx;
use ark_core::batch::sign_commitment_psbt;
use ark_core::batch::Delegate;
use ark_core::batch::NonceKps;
use ark_core::contract::SpendPathKind;
use ark_core::intent;
use ark_core::script::extract_checksig_pubkeys;
use ark_core::server;
use ark_core::server::BatchTreeEventType;
use ark_core::server::PartialSigTree;
use ark_core::server::StreamEvent;
use ark_core::ArkAddress;
use ark_core::ArkNote;
use ark_core::ExplorerUtxo;
use ark_core::TxGraph;
use backon::ExponentialBuilder;
use backon::Retryable;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use futures::StreamExt;
use rand::CryptoRng;
use rand::Rng;
use std::collections::HashMap;
use std::collections::HashSet;

impl<B, W, S> Client<B, W, S>
where
    B: Blockchain,
    W: OnchainWallet,
    S: SwapStorage + 'static,
{
    /// Settle _all_ prior VTXOs and boarding outputs into the next batch, generating new confirmed
    /// VTXOs.
    ///
    /// Most callers should prefer [`Self::settle`], which only renews VTXOs that have actually
    /// expired. Settling unexpired VTXOs is rarely necessary.
    pub async fn settle_all<R>(&self, rng: &mut R) -> Result<Option<Txid>, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        self.settle_at(crate::utils::unix_now()?, rng).await
    }

    pub(crate) async fn settle_at<R>(&self, now: i64, rng: &mut R) -> Result<Option<Txid>, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        let server_info = self.server_info().await?;

        // Get off-chain address and send all funds to this address, no change output 🦄
        let (to_address, _) = self.get_offchain_address_with_server_info(&server_info)?;

        let (boarding_inputs, vtxo_inputs, _) = self
            .fetch_commitment_transaction_inputs(&server_info, now)
            .await?;

        tracing::debug!(
            offchain_adress = %to_address.encode(),
            ?boarding_inputs,
            ?vtxo_inputs,
            "Attempting to settle outputs"
        );

        if boarding_inputs.is_empty() && vtxo_inputs.is_empty() {
            tracing::debug!("No inputs to board with");
            return Ok(None);
        }

        let commitment_txid = self
            .join_batches_chunked(rng, &server_info, boarding_inputs, vtxo_inputs, to_address)
            .await?;

        tracing::info!(%commitment_txid, "Settlement success");

        Ok(Some(commitment_txid))
    }

    /// Settle prior VTXOs that have expired or are recoverable, together with all available
    /// boarding outputs, into the next batch, generating new confirmed VTXOs.
    ///
    /// Healthy (unexpired) VTXOs are left untouched. This is the path callers typically want when
    /// periodically renewing their wallet: healthy VTXOs do not need to be touched, and including
    /// them would only inflate batch fees. Boarding outputs are always included because callers
    /// generally want freshly funded coins to enter the Ark.
    ///
    /// NOTE: sub-dust recoverable VTXOs can only be rescued when their combined value exceeds the
    /// server's dust threshold; otherwise the batch protocol rejects the settlement with a
    /// `cannot settle into sub-dust VTXO` error. When the wallet holds isolated sub-dust amounts,
    /// fall back to [`Self::settle_all`], which can roll them in alongside healthy VTXOs that
    /// act as carrier value.
    pub async fn settle<R>(&self, rng: &mut R) -> Result<Option<Txid>, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        let server_info = self.server_info().await?;

        let vtxo_list = self.list_vtxos_with_server_info(&server_info).await?;
        let vtxo_outpoints: Vec<OutPoint> = vtxo_list
            .recoverable()
            .map(|entry| entry.vtxo().outpoint)
            .collect();

        let (boarding_inputs, _, _) = self
            .fetch_commitment_transaction_inputs(&server_info, crate::utils::unix_now()?)
            .await?;
        let boarding_outpoints: Vec<OutPoint> =
            boarding_inputs.iter().map(|i| i.outpoint()).collect();

        if vtxo_outpoints.is_empty() && boarding_outpoints.is_empty() {
            tracing::debug!("No expired/recoverable VTXOs or boarding outputs to settle");
            return Ok(None);
        }

        tracing::debug!(
            num_vtxos = vtxo_outpoints.len(),
            num_boarding = boarding_outpoints.len(),
            "Attempting to settle expired/recoverable VTXOs and boarding outputs"
        );

        self.settle_vtxos_with_server_info(rng, &server_info, &vtxo_outpoints, &boarding_outpoints)
            .await
    }

    /// Settle _all_ prior VTXOs, boarding outputs, and the provided ArkNotes into the next batch.
    ///
    /// ArkNotes are bearer tokens that can be redeemed by revealing their preimage.
    /// This method combines them with regular VTXOs and boarding outputs into a single
    /// settlement transaction.
    pub async fn settle_with_notes<R>(
        &self,
        rng: &mut R,
        notes: Vec<ArkNote>,
    ) -> Result<Option<Txid>, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        let server_info = self.server_info().await?;

        let (to_address, _) = self.get_offchain_address_with_server_info(&server_info)?;

        let (boarding_inputs, vtxo_inputs, mut total_amount) = self
            .fetch_commitment_transaction_inputs(&server_info, crate::utils::unix_now()?)
            .await?;

        // Convert arknotes to intent inputs and add their value to total
        let note_inputs: Vec<intent::Input> = notes
            .iter()
            .map(|note| {
                total_amount += note.value();
                note.to_intent_input()
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Combine VTXO inputs with note inputs
        let all_vtxo_inputs: Vec<intent::Input> =
            vtxo_inputs.into_iter().chain(note_inputs).collect();

        tracing::debug!(
            offchain_address = %to_address.encode(),
            ?boarding_inputs,
            num_vtxo_inputs = all_vtxo_inputs.len(),
            num_notes = notes.len(),
            %total_amount,
            "Attempting to settle outputs with notes"
        );

        if boarding_inputs.is_empty() && all_vtxo_inputs.is_empty() {
            tracing::debug!("No inputs to settle");
            return Ok(None);
        }

        let commitment_txid = self
            .join_batches_chunked(
                rng,
                &server_info,
                boarding_inputs,
                all_vtxo_inputs,
                to_address,
            )
            .await?;

        tracing::info!(%commitment_txid, num_notes = notes.len(), "Settlement with notes success");

        Ok(Some(commitment_txid))
    }

    /// Settle specific VTXOs and boarding outputs by outpoint into the next batch, generating new
    /// confirmed VTXOs.
    ///
    /// Unlike [`Self::settle`], this method allows the caller to specify exactly which VTXOs and
    /// boarding outputs to settle by providing their outpoints.
    pub async fn settle_vtxos<R>(
        &self,
        rng: &mut R,
        vtxo_outpoints: &[OutPoint],
        boarding_outpoints: &[OutPoint],
    ) -> Result<Option<Txid>, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        let server_info = self.server_info().await?;
        self.settle_vtxos_with_server_info(rng, &server_info, vtxo_outpoints, boarding_outpoints)
            .await
    }

    pub(crate) async fn settle_vtxos_with_server_info<R>(
        &self,
        rng: &mut R,
        server_info: &server::Info,
        vtxo_outpoints: &[OutPoint],
        boarding_outpoints: &[OutPoint],
    ) -> Result<Option<Txid>, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        // Get off-chain address and send all funds to this address, no change output.
        let (to_address, _) = self.get_offchain_address_with_server_info(server_info)?;

        let (all_boarding_inputs, all_vtxo_inputs, _) = self
            .fetch_commitment_transaction_inputs(server_info, crate::utils::unix_now()?)
            .await?;

        // Filter boarding inputs to only those specified.
        let boarding_inputs: Vec<_> = all_boarding_inputs
            .into_iter()
            .filter(|input| boarding_outpoints.contains(&input.outpoint()))
            .collect();

        // Filter VTXO inputs to only those specified.
        let vtxo_inputs: Vec<_> = all_vtxo_inputs
            .into_iter()
            .filter(|input| vtxo_outpoints.contains(&input.outpoint()))
            .collect();

        // Recalculate total amount from filtered inputs.
        let total_amount = boarding_inputs
            .iter()
            .map(|i| i.amount())
            .chain(vtxo_inputs.iter().map(|i| i.amount()))
            .fold(Amount::ZERO, |acc, a| acc + a);

        tracing::debug!(
            offchain_address = %to_address.encode(),
            ?boarding_inputs,
            ?vtxo_inputs,
            %total_amount,
            "Attempting to settle specific outputs"
        );

        if boarding_inputs.is_empty() && vtxo_inputs.is_empty() {
            tracing::debug!("No matching inputs to settle");
            return Ok(None);
        }

        let commitment_txid = self
            .join_batches_chunked(rng, server_info, boarding_inputs, vtxo_inputs, to_address)
            .await?;

        tracing::info!(%commitment_txid, "Settlement of specific VTXOs success");

        Ok(Some(commitment_txid))
    }

    /// Settle the given inputs into `to_address`, splitting them across multiple sequential
    /// batches if a single intent proof would exceed the proof weight limit.
    ///
    /// Returns the commitment transaction ID of the last batch joined.
    async fn join_batches_chunked<R>(
        &self,
        rng: &mut R,
        server_info: &server::Info,
        boarding_inputs: Vec<batch::OnChainInput>,
        vtxo_inputs: Vec<intent::Input>,
        to_address: ArkAddress,
    ) -> Result<Txid, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        let chunks = match self.max_intent_proof_weight(server_info) {
            Some(max_weight) => chunk_settlement_inputs(
                boarding_inputs,
                vtxo_inputs,
                &to_address,
                max_weight,
                server_info.dust,
            )?,
            None => {
                let mut chunk = SettlementChunk::new();
                for input in boarding_inputs {
                    chunk.push(SettlementInput::Onchain(input));
                }
                for input in vtxo_inputs {
                    chunk.push(SettlementInput::Vtxo(input));
                }
                vec![chunk]
            }
        };

        let n_chunks = chunks.len();
        if n_chunks > 1 {
            tracing::info!(
                n_chunks,
                "Settlement inputs exceed the intent proof weight limit; \
                 splitting settlement across multiple batches"
            );
        }

        let mut commitment_txid = None;

        for (i, chunk) in chunks.into_iter().enumerate() {
            let join_next_batch = || async {
                self.join_next_batch(
                    &mut rng.clone(),
                    server_info,
                    chunk.onchain_inputs.clone(),
                    chunk.vtxo_inputs.clone(),
                    BatchOutputType::Board {
                        to_address,
                        to_amount: chunk.total_amount,
                    },
                )
                .await
            };

            // Joining a batch can fail depending on the timing, so we try a few times.
            let txid = join_next_batch
                .retry(ExponentialBuilder::default().with_max_times(3))
                .sleep(sleep)
                .when(|err| !err.is_server_info_changed() && !err.is_intent_proof_too_large())
                .notify(|err: &Error, dur: std::time::Duration| {
                    tracing::warn!("Retrying joining next batch after {dur:?}. Error: {err}",);
                })
                .await
                .context("Failed to join batch")?;

            if n_chunks > 1 {
                tracing::info!(
                    commitment_txid = %txid,
                    chunk = i + 1,
                    n_chunks,
                    "Settled batch chunk"
                );
            }

            commitment_txid = Some(txid);
        }

        commitment_txid.ok_or_else(|| Error::ad_hoc("no settlement chunks to join"))
    }

    /// Settle _some_ prior VTXOs and boarding outputs into the next batch, generating UTXOs as
    /// outputs to a new commitment transaction.
    pub async fn collaborative_redeem<R>(
        &self,
        rng: &mut R,
        to_address: Address,
        to_amount: Amount,
    ) -> Result<Txid, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        let server_info = self.server_info().await?;

        let (change_address, _) = self.get_offchain_address_with_server_info(&server_info)?;

        let (boarding_inputs, vtxo_inputs, total_amount) = self
            .fetch_commitment_transaction_inputs(&server_info, crate::utils::unix_now()?)
            .await?;

        let onchain_fee = self.eval_onchain_output_fee(ark_fees::Output {
            amount: to_amount.to_sat(),
            script: to_address.script_pubkey().to_string(),
        })?;

        // Fee comes out of change, not the send amount.
        let change_amount = total_amount
            .checked_sub(to_amount)
            .and_then(|a| a.checked_sub(onchain_fee))
            .ok_or_else(|| {
                Error::coin_select(format!(
                    "insufficient balance: {total_amount} < {to_amount} (send) + {onchain_fee} (fee)"
                ))
            })?;

        tracing::info!(
            %to_address,
            send_amount = %to_amount,
            fee = %onchain_fee,
            change_address = %change_address.encode(),
            %change_amount,
            ?boarding_inputs,
            "Attempting to collaboratively redeem outputs"
        );

        let join_next_batch = || async {
            self.join_next_batch(
                &mut rng.clone(),
                &server_info,
                boarding_inputs.clone(),
                vtxo_inputs.clone(),
                BatchOutputType::OffBoard {
                    to_address: to_address.clone(),
                    to_amount,
                    change_address,
                    change_amount,
                },
            )
            .await
        };

        // Joining a batch can fail depending on the timing, so we try a few times.
        let commitment_txid = join_next_batch
            .retry(ExponentialBuilder::default().with_max_times(3))
            .sleep(sleep)
            .when(|err| !err.is_server_info_changed() && !err.is_intent_proof_too_large())
            .notify(|err: &Error, dur: std::time::Duration| {
                tracing::warn!("Retrying joining next batch after {dur:?}. Error: {err}");
            })
            .await
            .context("Failed to join batch")?;

        tracing::info!(%commitment_txid, "Collaborative redeem success");

        Ok(commitment_txid)
    }

    /// Settle a selection of VTXOs into the next batch, generating UTXOs as
    /// outputs to a new commitment transaction.
    pub async fn collaborative_redeem_vtxo_selection<R>(
        &self,
        rng: &mut R,
        input_vtxos: impl Iterator<Item = OutPoint> + Clone,
        to_address: Address,
        to_amount: Amount,
    ) -> Result<Txid, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        let server_info = self.server_info().await?;

        let (change_address, _) = self.get_offchain_address_with_server_info(&server_info)?;

        let vtxo_inputs = self
            .selected_batch_settleable_vtxo_inputs(&server_info, input_vtxos)
            .await?;

        if vtxo_inputs.is_empty() {
            return Err(Error::ad_hoc("no matching VTXO outpoints found"));
        }

        // Check that total amount is sufficient
        let total_input_amount = vtxo_inputs
            .iter()
            .fold(Amount::ZERO, |acc, vtxo| acc + vtxo.amount());

        let onchain_fee = self.eval_onchain_output_fee(ark_fees::Output {
            amount: to_amount.to_sat(),
            script: to_address.script_pubkey().to_string(),
        })?;

        // Fee comes out of change, not the send amount.
        let change_amount = total_input_amount
            .checked_sub(to_amount)
            .and_then(|a| a.checked_sub(onchain_fee))
            .ok_or_else(|| {
                Error::coin_select(format!(
                    "insufficient VTXO amount: {total_input_amount} < {to_amount} (send) + {onchain_fee} (fee)"
                ))
            })?;

        tracing::info!(
            %to_address,
            send_amount = %to_amount,
            fee = %onchain_fee,
            change_address = %change_address.encode(),
            %change_amount,
            "Attempting to collaboratively redeem outputs"
        );

        let join_next_batch = || async {
            self.join_next_batch(
                &mut rng.clone(),
                &server_info,
                Vec::new(),
                vtxo_inputs.clone(),
                BatchOutputType::OffBoard {
                    to_address: to_address.clone(),
                    to_amount,
                    change_address,
                    change_amount,
                },
            )
            .await
        };

        // Joining a batch can fail depending on the timing, so we try a few times.
        let commitment_txid = join_next_batch
            .retry(ExponentialBuilder::default().with_max_times(3))
            .sleep(sleep)
            .when(|err| !err.is_server_info_changed() && !err.is_intent_proof_too_large())
            .notify(|err: &Error, dur: std::time::Duration| {
                tracing::warn!("Retrying joining next batch after {dur:?}. Error: {err}");
            })
            .await
            .context("Failed to join batch")?;

        tracing::info!(%commitment_txid, "Collaborative redeem success");

        Ok(commitment_txid)
    }

    pub(crate) async fn selected_batch_settleable_vtxo_inputs(
        &self,
        server_info: &server::Info,
        input_vtxos: impl IntoIterator<Item = OutPoint>,
    ) -> Result<Vec<intent::Input>, Error> {
        let requested: HashSet<OutPoint> = input_vtxos.into_iter().collect();

        let vtxo_list = self
            .list_vtxos_with_server_info(server_info)
            .await
            .context("failed to get VTXO list")?;
        let now = crate::utils::unix_now()?;

        let matching_unspent = vtxo_list
            .all_unspent()
            .filter(|entry| requested.contains(&entry.vtxo().outpoint))
            .collect::<Vec<_>>();

        let settleable = vtxo_list
            .batch_settleable_at(server_info, now)
            .filter(|entry| requested.contains(&entry.vtxo().outpoint))
            .collect::<Vec<_>>();
        let settleable_outpoints = settleable
            .iter()
            .map(|entry| entry.vtxo().outpoint)
            .collect::<HashSet<_>>();

        let blocked = matching_unspent
            .iter()
            .filter(|entry| !settleable_outpoints.contains(&entry.vtxo().outpoint))
            .map(|entry| entry.vtxo().outpoint.to_string())
            .collect::<Vec<_>>();
        if !blocked.is_empty() {
            return Err(Error::ad_hoc(format!(
                "selected VTXO outpoints are not batch-settleable because their signer cutoff has passed: {}",
                blocked.join(", ")
            )));
        }

        settleable
            .into_iter()
            .map(|entry| {
                let spend_selection = entry.spend_selection(SpendPathKind::Forfeit)?;

                Ok(intent::Input::new_with_spend_selection(
                    entry.vtxo().outpoint,
                    entry.exit_delay()?,
                    TxOut {
                        value: entry.vtxo().amount,
                        script_pubkey: entry.script_pubkey(),
                    },
                    entry.tapscripts(),
                    spend_selection,
                    false,
                    entry.vtxo().is_swept,
                    entry.vtxo().assets.clone(),
                ))
            })
            .collect::<Result<Vec<_>, Error>>()
    }

    /// Generate a delegate for settling VTXOs on behalf of the owner.
    ///
    /// The owner pre-signs the intent and forfeit transactions, allowing another party to complete
    /// the settlement at a later time using the provided `delegate_cosigner_pk`.
    ///
    /// # Arguments
    ///
    /// * `delegate_cosigner_pk` - The cosigner public key that the delegate will use
    /// * `select_recoverable_vtxos` - Whether to include recoverable VTXOs
    ///
    /// # Returns
    ///
    /// A [`Delegate`] struct containing all the pre-signed data needed for settlement.
    pub async fn generate_delegate(
        &self,
        delegate_cosigner_pk: PublicKey,
    ) -> Result<Delegate, Error> {
        let server_info = self.server_info().await?;

        // Get off-chain address and send all funds to this address.
        let (to_address, _) = self.get_offchain_address_with_server_info(&server_info)?;

        // Simply collect all VTXOs that can be settled.
        let (_, vtxo_inputs, _) = self
            .fetch_commitment_transaction_inputs(&server_info, crate::utils::unix_now()?)
            .await?;

        let total_amount = vtxo_inputs
            .iter()
            .fold(Amount::ZERO, |acc, v| acc + v.amount());

        if vtxo_inputs.is_empty() {
            return Err(Error::ad_hoc("no inputs to settle via delegate"));
        }

        let mut outputs = vec![intent::Output::Offchain(TxOut {
            value: total_amount,
            script_pubkey: to_address.to_p2tr_script_pubkey(),
        })];

        if let Some(packet) = create_asset_preservation_packet(&vtxo_inputs, &outputs)? {
            outputs.push(intent::Output::AssetPacket(packet.to_txout()));
        }

        // A delegate is a single pre-signed intent, so it cannot be chunked at settlement time.
        // Reject an oversized intent now, while the owner can still act on it.
        if let Some(max_weight) = self.max_intent_proof_weight(&server_info) {
            let weight = intent::estimate_proof_weight(&vtxo_inputs, &outputs)?.to_wu();

            if weight > max_weight {
                return Err(Error::intent_proof_too_large(weight, max_weight));
            }
        }

        let delegate = batch::prepare_delegate_psbts(
            vtxo_inputs,
            outputs,
            delegate_cosigner_pk,
            &server_info.forfeit_address,
            server_info.dust,
        )?;

        Ok(delegate)
    }

    /// Sign a set of delegate PSBTs, including the intent PSBT and the forfeit PSBTs.
    pub fn sign_delegate_psbts(
        &self,
        intent_psbt: &mut Psbt,
        forfeit_psbts: &mut [Psbt],
    ) -> Result<(), Error> {
        let sign_fn =
            |input: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                match &input.witness_script {
                    None => Err(ark_core::Error::ad_hoc(
                        "Missing witness script for psbt::Input",
                    )),
                    Some(script) => {
                        let mut res = vec![];
                        let pks = extract_checksig_pubkeys(script);
                        for pk in pks {
                            if let Ok(keypair) = self.keypair_by_pk(&pk) {
                                let sig = Secp256k1::new().sign_schnorr_no_aux_rand(&msg, &keypair);
                                let pk = keypair.x_only_public_key().0;
                                res.push((sig, pk));
                            }
                        }
                        Ok(res)
                    }
                }
            };

        batch::sign_delegate_psbts(sign_fn, intent_psbt, forfeit_psbts)?;

        Ok(())
    }

    /// Settle a delegate by completing the batch protocol using pre-signed data.
    ///
    /// This method allows Bob to settle Alice's VTXOs using the pre-signed intent and forfeit
    /// transactions from the [`Delegate`] struct.
    ///
    /// NOTE: the intent is pre-signed, so its proof weight cannot be checked or chunked here; it
    /// is validated against the server's proof weight limit in [`Self::generate_delegate`].
    ///
    /// # Arguments
    ///
    /// * `rng` - Random number generator for nonce generation
    /// * `delegate` - The delegate struct containing pre-signed data
    /// * `own_cosigner_kp` - Bob's cosigner keypair (must match the delegate_cosigner_pk)
    ///
    /// # Returns
    ///
    /// The commitment transaction ID if successful.
    pub async fn settle_delegate<R>(
        &self,
        rng: &mut R,
        delegate: Delegate,
        own_cosigner_kp: Keypair,
    ) -> Result<Txid, Error>
    where
        R: Rng + CryptoRng,
    {
        // Verify the cosigner key matches
        if own_cosigner_kp.public_key() != delegate.delegate_cosigner_pk {
            return Err(Error::ad_hoc(
                "provided cosigner keypair does not match delegate_cosigner_pk",
            ));
        }

        let server_info = self.server_info().await?;

        // Register the pre-signed intent
        let intent_id = timeout_op(
            self.inner.timeout,
            self.network_client()
                .register_intent(delegate.intent.clone()),
        )
        .await
        .context("failed to register delegated intent")??;

        tracing::debug!(intent_id, "Registered delegated intent");

        let network_client = self.network_client();

        #[derive(Debug, PartialEq, Eq)]
        enum Step {
            Start,
            BatchStarted,
            BatchSigningStarted,
            Finalized,
        }

        impl Step {
            fn next(&self) -> Step {
                match self {
                    Step::Start => Step::BatchStarted,
                    Step::BatchStarted => Step::BatchSigningStarted,
                    Step::BatchSigningStarted => Step::Finalized,
                    Step::Finalized => Step::Finalized,
                }
            }
        }

        let mut step = Step::Start;

        let own_cosigner_kps = [own_cosigner_kp];
        let own_cosigner_pks = own_cosigner_kps
            .iter()
            .map(|k| k.public_key())
            .collect::<Vec<_>>();

        let mut batch_id: Option<String> = None;

        let vtxo_input_outpoints = delegate
            .forfeit_psbts
            .iter()
            .map(|psbt| psbt.unsigned_tx.input[0].previous_output)
            .collect::<Vec<_>>();

        let topics = vtxo_input_outpoints
            .iter()
            .map(ToString::to_string)
            .chain(
                own_cosigner_pks
                    .iter()
                    .map(|pk| pk.serialize().to_lower_hex_string()),
            )
            .collect();

        let mut stream = network_client.get_event_stream(topics).await?;

        let (ark_forfeit_pk, _) = server_info.forfeit_pk.x_only_public_key();

        let mut unsigned_commitment_tx = None;

        let mut vtxo_batch_tree_graph_chunks = Some(Vec::new());
        let mut vtxo_batch_tree_graph: Option<TxGraph> = None;

        let mut connectors_graph_chunks = Some(Vec::new());
        let mut batch_expiry = None;

        let mut agg_nonce_pks = HashMap::new();

        let mut our_nonce_trees: Option<HashMap<Keypair, NonceKps>> = None;

        loop {
            match timeout_op(self.inner.timeout, stream.next())
                .await
                .context("timed out waiting for batch event")?
            {
                Some(Ok(event)) => match event {
                    StreamEvent::BatchStarted(e) => {
                        if step != Step::Start {
                            continue;
                        }

                        let hash = sha256::Hash::hash(intent_id.as_bytes());
                        let hash = hash.as_byte_array().to_vec().to_lower_hex_string();

                        if e.intent_id_hashes.iter().any(|h| h == &hash) {
                            timeout_op(
                                self.inner.timeout,
                                self.network_client()
                                    .confirm_registration(intent_id.clone()),
                            )
                            .await
                            .context("failed to confirm intent registration")??;

                            tracing::info!(batch_id = e.id, intent_id, "Intent ID found for batch");

                            batch_id = Some(e.id);

                            step = Step::BatchStarted;

                            batch_expiry = Some(e.batch_expiry);
                        } else {
                            tracing::debug!(
                                batch_id = e.id,
                                intent_id,
                                "Intent ID not found for batch"
                            );
                        }
                    }
                    StreamEvent::TreeTx(e) => {
                        if step != Step::BatchStarted && step != Step::BatchSigningStarted {
                            continue;
                        }

                        match e.batch_tree_event_type {
                            BatchTreeEventType::Vtxo => {
                                match &mut vtxo_batch_tree_graph_chunks {
                                    Some(vtxo_batch_tree_graph_chunks) => {
                                        tracing::debug!("Got new VTXO batch-tree graph chunk");

                                        vtxo_batch_tree_graph_chunks.push(e.tx_graph_chunk)
                                    }
                                    None => {
                                        return Err(Error::ark_server(
                                            "received unexpected VTXO batch-tree graph chunk",
                                        ));
                                    }
                                };
                            }
                            BatchTreeEventType::Connector => {
                                match connectors_graph_chunks {
                                    Some(ref mut connectors_graph_chunks) => {
                                        tracing::debug!("Got new connectors graph chunk");

                                        connectors_graph_chunks.push(e.tx_graph_chunk)
                                    }
                                    None => {
                                        return Err(Error::ark_server(
                                            "received unexpected connectors graph chunk",
                                        ));
                                    }
                                };
                            }
                        }
                    }
                    StreamEvent::TreeSignature(e) => {
                        if step != Step::BatchSigningStarted {
                            continue;
                        }

                        match e.batch_tree_event_type {
                            BatchTreeEventType::Vtxo => {
                                match vtxo_batch_tree_graph {
                                    Some(ref mut vtxo_batch_tree_graph) => {
                                        vtxo_batch_tree_graph.apply(|graph| {
                                            if graph.root().unsigned_tx.compute_txid() != e.txid {
                                                Ok(true)
                                            } else {
                                                graph.set_signature(e.signature);

                                                Ok(false)
                                            }
                                        })?;
                                    }
                                    None => {
                                        return Err(Error::ark_server(
                                            "received batch-tree signature without transaction graph",
                                        ));
                                    }
                                };
                            }
                            BatchTreeEventType::Connector => {
                                return Err(Error::ark_server(
                                    "received batch-tree signature for connector tree",
                                ));
                            }
                        }
                    }
                    StreamEvent::TreeSigningStarted(e) => {
                        if step != Step::BatchStarted {
                            continue;
                        }

                        let chunks = vtxo_batch_tree_graph_chunks.take().ok_or(Error::ark_server(
                            "received batch-tree signing started event without VTXO batch-tree graph chunks",
                        ))?;
                        vtxo_batch_tree_graph =
                            Some(TxGraph::new(chunks).map_err(Error::from).context(
                                "failed to build VTXO batch-tree graph before generating nonces",
                            )?);

                        tracing::info!(batch_id = e.id, "Batch signing started");

                        for own_cosigner_pk in own_cosigner_pks.iter() {
                            if !&e.cosigners_pubkeys.iter().any(|p| p == own_cosigner_pk) {
                                return Err(Error::ark_server(format!(
                                    "own cosigner PK is not present in cosigner PKs: {own_cosigner_pk}"
                                )));
                            }
                        }

                        let mut our_nonce_tree_map = HashMap::new();
                        for own_cosigner_kp in own_cosigner_kps {
                            let own_cosigner_pk = own_cosigner_kp.public_key();
                            let nonce_tree = generate_nonce_tree(
                                rng,
                                vtxo_batch_tree_graph
                                    .as_ref()
                                    .expect("VTXO batch-tree graph"),
                                own_cosigner_pk,
                                &e.unsigned_commitment_tx,
                            )
                            .map_err(Error::from)
                            .context("failed to generate VTXO nonce tree")?;

                            tracing::info!(
                                cosigner_pk = %own_cosigner_pk,
                                "Submitting nonce tree for cosigner PK"
                            );

                            network_client
                                .submit_tree_nonces(
                                    &e.id,
                                    own_cosigner_pk,
                                    nonce_tree.to_nonce_pks(),
                                )
                                .await
                                .map_err(Error::ark_server)
                                .context("failed to submit VTXO nonce tree")?;

                            our_nonce_tree_map.insert(own_cosigner_kp, nonce_tree);
                        }

                        unsigned_commitment_tx = Some(e.unsigned_commitment_tx);
                        our_nonce_trees = Some(our_nonce_tree_map);

                        step = step.next();
                    }
                    StreamEvent::TreeNonces(e) => {
                        if step != Step::BatchSigningStarted {
                            continue;
                        }

                        let tree_tx_nonce_pks = e.nonces;

                        let cosigner_pk = match tree_tx_nonce_pks.0.iter().find(|(pk, _)| {
                            own_cosigner_pks
                                .iter()
                                .any(|p| &&p.x_only_public_key().0 == pk)
                        }) {
                            Some((pk, _)) => *pk,
                            None => {
                                tracing::debug!(
                                    batch_id = e.id,
                                    txid = %e.txid,
                                    "Received irrelevant TreeNonces event"
                                );

                                continue;
                            }
                        };

                        tracing::debug!(
                            batch_id = e.id,
                            txid = %e.txid,
                            %cosigner_pk,
                            "Received TreeNonces event"
                        );

                        let agg_nonce_pk = aggregate_nonces(tree_tx_nonce_pks);

                        agg_nonce_pks.insert(e.txid, agg_nonce_pk);

                        if vtxo_batch_tree_graph.is_none() {
                            let chunks = vtxo_batch_tree_graph_chunks.take().ok_or(Error::ark_server(
                                "received batch-tree nonces event without VTXO batch-tree graph chunks",
                            ))?;
                            vtxo_batch_tree_graph = Some(
                                TxGraph::new(chunks)
                                    .map_err(Error::from)
                                    .context("failed to build VTXO batch-tree graph before batch-tree signing")?,
                            );
                        }
                        let vtxo_batch_tree_graph_ref =
                            vtxo_batch_tree_graph.as_ref().expect("just populated");

                        if agg_nonce_pks.len() == vtxo_batch_tree_graph_ref.nb_of_nodes() {
                            let cosigner_kp = own_cosigner_kps
                                .iter()
                                .find(|kp| kp.public_key().x_only_public_key().0 == cosigner_pk)
                                .ok_or_else(|| {
                                    Error::ad_hoc("no cosigner keypair to sign for own PK")
                                })?;

                            let our_nonce_trees = our_nonce_trees.as_mut().ok_or(
                                Error::ark_server("missing nonce trees during batch protocol"),
                            )?;

                            let our_nonce_tree =
                                our_nonce_trees
                                    .get_mut(cosigner_kp)
                                    .ok_or(Error::ark_server(
                                        "missing nonce tree during batch protocol",
                                    ))?;

                            let unsigned_commitment_tx = unsigned_commitment_tx
                                .as_ref()
                                .ok_or_else(|| Error::ad_hoc("missing commitment TX"))?;

                            let batch_expiry = batch_expiry
                                .ok_or_else(|| Error::ad_hoc("missing batch expiry"))?;

                            let mut partial_sig_tree = PartialSigTree::default();
                            for (txid, _) in vtxo_batch_tree_graph_ref.as_map() {
                                let agg_nonce_pk = agg_nonce_pks.get(&txid).ok_or_else(|| {
                                    Error::ad_hoc(format!(
                                        "missing aggregated nonce PK for TX {txid}"
                                    ))
                                })?;

                                let sigs = sign_batch_tree_tx(
                                    txid,
                                    batch_expiry,
                                    ark_forfeit_pk,
                                    cosigner_kp,
                                    *agg_nonce_pk,
                                    vtxo_batch_tree_graph_ref,
                                    unsigned_commitment_tx,
                                    our_nonce_tree,
                                )
                                .map_err(Error::from)
                                .context("failed to sign VTXO batch-tree transactions")?;

                                partial_sig_tree.0.extend(sigs.0);
                            }

                            network_client
                                .submit_tree_signatures(
                                    &e.id,
                                    cosigner_kp.public_key(),
                                    partial_sig_tree,
                                )
                                .await
                                .map_err(Error::ark_server)
                                .context("failed to submit VTXO batch-tree signatures")?;
                        }
                    }
                    StreamEvent::TreeNoncesAggregated(e) => {
                        tracing::debug!(batch_id = e.id, "Batch combined nonces generated");
                    }
                    StreamEvent::BatchFinalization(e) => {
                        if step != Step::BatchSigningStarted {
                            continue;
                        }

                        tracing::debug!(
                            commitment_txid = %e.commitment_tx.unsigned_tx.compute_txid(),
                            "Batch finalization started (delegate)"
                        );

                        let chunks = connectors_graph_chunks.take().ok_or(Error::ark_server(
                            "received batch finalization event without connectors",
                        ))?;

                        if chunks.is_empty() {
                            tracing::debug!(batch_id = e.id, "No delegated forfeit transactions");
                        } else {
                            let connectors_graph = TxGraph::new(chunks)
                                .map_err(Error::from)
                                .context(
                                "failed to build connectors graph before completing forfeit TXs",
                            )?;

                            tracing::debug!(
                                batch_id = e.id,
                                "Completing delegated forfeit transactions"
                            );

                            let signed_forfeit_psbts = complete_delegate_forfeit_txs(
                                &delegate.forfeit_psbts,
                                &connectors_graph.leaves(),
                            )?;

                            network_client
                                .submit_signed_forfeit_txs(signed_forfeit_psbts, None)
                                .await?;
                        }

                        step = step.next();
                    }
                    StreamEvent::BatchFinalized(e) => {
                        if step != Step::Finalized {
                            continue;
                        }

                        let commitment_txid = e.commitment_txid;

                        tracing::info!(batch_id = e.id, %commitment_txid, "Delegated batch finalized");

                        return Ok(commitment_txid);
                    }
                    StreamEvent::BatchFailed(ref e) => {
                        if Some(&e.id) == batch_id.as_ref() {
                            return Err(Error::ark_server(format!(
                                "batch failed {}: {}",
                                e.id, e.reason
                            )));
                        }

                        tracing::debug!("Unrelated batch failed: {e:?}");
                    }
                    StreamEvent::Heartbeat => {}
                    StreamEvent::StreamStarted(_) => {}
                },
                Some(Err(e)) => {
                    tracing::error!("Got error from event stream");

                    return Err(Error::ark_server(e));
                }
                None => {
                    return Err(Error::ark_server("dropped batch event stream"));
                }
            }
        }
    }

    /// Get all the [`batch::OnChainInput`]s and [`batch::VtxoInput`]s that can be used to join an
    /// upcoming batch.
    pub(crate) async fn fetch_commitment_transaction_inputs(
        &self,
        server_info: &server::Info,
        now: i64,
    ) -> Result<(Vec<batch::OnChainInput>, Vec<intent::Input>, Amount), Error> {
        let now = u64::try_from(now).map_err(|_| Error::ad_hoc("negative timestamp"))?;

        // Get all known boarding outputs.
        let boarding_outputs = self.boarding_outputs()?;

        let mut boarding_inputs: Vec<batch::OnChainInput> = Vec::new();
        let mut total_amount = Amount::ZERO;

        // To track unique outpoints and prevent duplicates
        let mut seen_outpoints = HashSet::new();

        // Find outpoints for each boarding output.
        for boarding_output in boarding_outputs {
            let outpoints = timeout_op(
                self.inner.timeout,
                self.blockchain().find_outpoints(boarding_output.address()),
            )
            .await
            .context("failed to find outpoints")??;

            for o in outpoints.iter() {
                if let ExplorerUtxo {
                    outpoint,
                    amount,
                    confirmation_blocktime: Some(confirmation_blocktime),
                    confirmations,
                    is_spent: false,
                } = o
                {
                    // Check for duplicate outpoints
                    if seen_outpoints.contains(outpoint) {
                        continue;
                    }

                    // Skip boarding outputs whose server key is past its cooperative-sign
                    // cutoff — the operator won't co-sign the old key's forfeit path.
                    // These must be recovered via unilateral exit (send_on_chain).
                    if server_info
                        .signer_requires_recovery_at(boarding_output.server_pk(), now as i64)
                    {
                        continue;
                    }

                    // Only include confirmed boarding outputs with an _inactive_ exit path.
                    if !boarding_output.can_be_claimed_unilaterally_by_owner(
                        std::time::Duration::from_secs(now),
                        std::time::Duration::from_secs(*confirmation_blocktime),
                        *confirmations,
                    ) {
                        // Mark this outpoint as seen
                        seen_outpoints.insert(*outpoint);

                        let script_pubkey = boarding_output.script_pubkey();
                        let tapscripts = boarding_output.tapscripts();
                        let spend_selection =
                            boarding_output.spend_selection(SpendPathKind::Forfeit)?;

                        boarding_inputs.push(batch::OnChainInput::new_with_spend_selection(
                            boarding_output.exit_delay(),
                            script_pubkey,
                            tapscripts,
                            spend_selection,
                            boarding_output.owner_pk(),
                            *amount,
                            *outpoint,
                        ));
                        total_amount += *amount;
                    }
                }
            }
        }

        let vtxo_list = self.list_vtxos_with_server_info(server_info).await?;
        // Reuse the caller-supplied timestamp (not a fresh wall-clock) so the VTXO cutoff filter
        // below is evaluated against the same instant as the boarding filter above, and so a
        // test-injected `now` deterministically controls both.
        let settleable_vtxos: Vec<_> = vtxo_list
            .batch_settleable_at(server_info, now as i64)
            .collect();

        total_amount += settleable_vtxos
            .iter()
            .fold(Amount::ZERO, |acc, entry| acc + entry.vtxo().amount);

        let vtxo_inputs = settleable_vtxos
            .into_iter()
            .map(|entry| {
                let spend_selection = entry.spend_selection(SpendPathKind::Forfeit)?;

                Ok(intent::Input::new_with_spend_selection(
                    entry.vtxo().outpoint,
                    entry.exit_delay()?,
                    TxOut {
                        value: entry.vtxo().amount,
                        script_pubkey: entry.script_pubkey(),
                    },
                    entry.tapscripts(),
                    spend_selection,
                    false,
                    entry.vtxo().is_swept,
                    entry.vtxo().assets.clone(),
                ))
            })
            .collect::<Result<Vec<_>, Error>>()?;

        Ok((boarding_inputs, vtxo_inputs, total_amount))
    }

    /// Prepare an intent for batch registration or fee estimation.
    ///
    /// This creates a signed intent PSBT along with all the data needed to participate
    /// in the batch protocol.
    pub(crate) fn prepare_intent<R>(
        &self,
        rng: &mut R,
        onchain_inputs: Vec<batch::OnChainInput>,
        vtxo_inputs: Vec<intent::Input>,
        output_type: BatchOutputType,
        intent_kind: PrepareIntentKind,
        dust: Amount,
    ) -> Result<PreparedIntent, Error>
    where
        R: Rng + CryptoRng,
    {
        if onchain_inputs.is_empty() && vtxo_inputs.is_empty() {
            return Err(Error::ad_hoc("cannot prepare intent without inputs"));
        }

        // Generate an (ephemeral) cosigner keypair.
        let cosigner_keypair = Keypair::new(self.secp(), rng);

        let vtxo_input_outpoints = vtxo_inputs.iter().map(|i| i.outpoint()).collect::<Vec<_>>();

        let inputs = combined_intent_inputs(&onchain_inputs, &vtxo_inputs);

        let mut outputs = vec![];

        match output_type {
            BatchOutputType::Board {
                to_address,
                to_amount,
            } => {
                if to_amount < dust {
                    return Err(Error::ad_hoc(format!(
                        "cannot settle into sub-dust VTXO: {to_amount} < {dust}"
                    )));
                }

                outputs.push(intent::Output::Offchain(TxOut {
                    value: to_amount,
                    script_pubkey: to_address.to_p2tr_script_pubkey(),
                }));
            }
            BatchOutputType::OffBoard {
                to_address,
                to_amount,
                change_amount,
                ..
            } if change_amount == Amount::ZERO => {
                outputs.push(intent::Output::Onchain(TxOut {
                    value: to_amount,
                    script_pubkey: to_address.script_pubkey(),
                }));
            }
            BatchOutputType::OffBoard {
                to_address,
                to_amount,
                change_address,
                change_amount,
            } => {
                if change_amount < dust {
                    return Err(Error::ad_hoc(format!(
                        "cannot settle with sub-dust change VTXO: {change_amount} < {dust}"
                    )));
                }

                outputs.push(intent::Output::Onchain(TxOut {
                    value: to_amount,
                    script_pubkey: to_address.script_pubkey(),
                }));

                outputs.push(intent::Output::Offchain(TxOut {
                    value: change_amount,
                    script_pubkey: change_address.to_p2tr_script_pubkey(),
                }));
            }
        }

        let cosigner_pk = cosigner_keypair.public_key();

        let secp = Secp256k1::new();

        let sign_for_vtxo_fn =
            |input: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<Vec<(schnorr::Signature, XOnlyPublicKey)>, ark_core::Error> {
                match &input.witness_script {
                    None => Err(ark_core::Error::ad_hoc(
                        "Missing witness script in psbt::Input when signing intent",
                    )),
                    Some(script) => {
                        let pks = extract_checksig_pubkeys(script);
                        let mut res = vec![];
                        for pk in pks {
                            if let Ok(keypair) = self.keypair_by_pk(&pk) {
                                let sig = secp.sign_schnorr_no_aux_rand(&msg, &keypair);
                                res.push((sig, keypair.public_key().into()))
                            }
                        }
                        Ok(res)
                    }
                }
            };

        let sign_for_onchain_fn =
            |input: &mut psbt::Input,
             msg: secp256k1::Message|
             -> Result<(schnorr::Signature, XOnlyPublicKey), ark_core::Error> {
                let onchain_input = onchain_inputs
                    .iter()
                    .find(|o| {
                        Some(o.script_pubkey().clone())
                            == input.witness_utxo.clone().map(|w| w.script_pubkey)
                    })
                    .ok_or_else(|| {
                        ark_core::Error::ad_hoc(
                            "could not find signing key for onchain input: {input:?}",
                        )
                    })?;

                let owner_pk = onchain_input.owner_pk();
                let sig = self
                    .sign_for_pk(&owner_pk, &msg)
                    .map_err(|e| ark_core::Error::ad_hoc(e.to_string()))?;

                Ok((sig, owner_pk))
            };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Error::ad_hoc(e.to_string()))
            .context("failed to compute now timestamp")?;
        let now = now.as_secs();
        let expire_at = now + (2 * 60);

        if let Some(packet) = create_asset_preservation_packet(&inputs, &outputs)? {
            outputs.push(intent::Output::AssetPacket(packet.to_txout()));
        }

        let mut onchain_output_indexes = Vec::new();
        for (i, output) in outputs.iter().enumerate() {
            if matches!(output, intent::Output::Onchain(_)) {
                onchain_output_indexes.push(i);
            }
        }

        let message = match intent_kind {
            PrepareIntentKind::EstimateFee => intent::IntentMessage::EstimateIntentFee {
                onchain_output_indexes,
                valid_at: now,
                expire_at,
                own_cosigner_pks: vec![cosigner_pk],
            },
            PrepareIntentKind::Register => intent::IntentMessage::Register {
                onchain_output_indexes,
                valid_at: now,
                expire_at,
                own_cosigner_pks: vec![cosigner_pk],
            },
        };

        let intent = intent::make_intent(
            sign_for_vtxo_fn,
            sign_for_onchain_fn,
            inputs,
            outputs.clone(),
            message,
        )?;

        Ok(PreparedIntent {
            intent,
            cosigner_keypair,
            vtxo_input_outpoints,
            outputs,
            onchain_inputs,
            vtxo_inputs,
        })
    }

    /// The effective upper bound for the weight of an intent proof, if any: the server's
    /// advertised `max_tx_weight`, further capped by the configured `max_intent_proof_weight`.
    fn max_intent_proof_weight(&self, server_info: &server::Info) -> Option<u64> {
        let server_max = u64::try_from(server_info.max_tx_weight)
            .ok()
            .filter(|w| *w > 0);

        match (server_max, self.inner.max_intent_proof_weight) {
            (Some(server), Some(config)) => Some(server.min(config)),
            (Some(server), None) => Some(server),
            (None, config) => config,
        }
    }

    pub(crate) async fn join_next_batch<R>(
        &self,
        rng: &mut R,
        server_info: &server::Info,
        onchain_inputs: Vec<batch::OnChainInput>,
        vtxo_inputs: Vec<intent::Input>,
        output_type: BatchOutputType,
    ) -> Result<Txid, Error>
    where
        R: Rng + CryptoRng,
    {
        let prepared = self.prepare_intent(
            rng,
            onchain_inputs,
            vtxo_inputs,
            output_type,
            PrepareIntentKind::Register,
            server_info.dust,
        )?;

        let PreparedIntent {
            intent,
            cosigner_keypair,
            vtxo_input_outpoints,
            outputs,
            onchain_inputs,
            vtxo_inputs,
        } = prepared;

        // Fail fast if the server would reject the intent proof for being too heavy: retrying
        // with the same inputs can never succeed.
        if let Some(max_weight) = self.max_intent_proof_weight(server_info) {
            let inputs = combined_intent_inputs(&onchain_inputs, &vtxo_inputs);
            let weight = intent::estimate_proof_weight(&inputs, &outputs)?.to_wu();

            if weight > max_weight {
                return Err(Error::intent_proof_too_large(weight, max_weight));
            }
        }

        let onchain_input_outpoints = onchain_inputs
            .iter()
            .map(|i| i.outpoint())
            .collect::<Vec<_>>();

        let own_cosigner_kps = [cosigner_keypair];
        let own_cosigner_pks = own_cosigner_kps
            .iter()
            .map(|k| k.public_key())
            .collect::<Vec<_>>();

        let secp = Secp256k1::new();

        let mut step = Step::Start;

        let intent_id = timeout_op(
            self.inner.timeout,
            self.network_client().register_intent(intent),
        )
        .await
        .context("failed to register intent")??;

        tracing::debug!(
            intent_id,
            ?onchain_input_outpoints,
            ?vtxo_input_outpoints,
            ?outputs,
            "Registered intent for batch"
        );

        let network_client = self.network_client();

        let mut batch_id: Option<String> = None;

        let topics = vtxo_input_outpoints
            .iter()
            .map(ToString::to_string)
            .chain(
                own_cosigner_pks
                    .iter()
                    .map(|pk| pk.serialize().to_lower_hex_string()),
            )
            .collect();

        let mut stream = network_client.get_event_stream(topics).await?;

        let (ark_forfeit_pk, _) = server_info.forfeit_pk.x_only_public_key();

        let mut unsigned_commitment_tx = None;

        let mut vtxo_batch_tree_graph_chunks = Some(Vec::new());
        let mut vtxo_batch_tree_graph: Option<TxGraph> = None;

        let mut connectors_graph_chunks = Some(Vec::new());
        let mut batch_expiry = None;

        let mut agg_nonce_pks = HashMap::new();

        let mut our_nonce_trees: Option<HashMap<Keypair, NonceKps>> = None;
        loop {
            match timeout_op(self.inner.timeout, stream.next())
                .await
                .context("timed out waiting for batch event")?
            {
                Some(Ok(event)) => match event {
                    StreamEvent::BatchStarted(e) => {
                        if step != Step::Start {
                            continue;
                        }

                        let hash = sha256::Hash::hash(intent_id.as_bytes());
                        let hash = hash.as_byte_array().to_vec().to_lower_hex_string();

                        if e.intent_id_hashes.iter().any(|h| h == &hash) {
                            timeout_op(
                                self.inner.timeout,
                                self.network_client()
                                    .confirm_registration(intent_id.clone()),
                            )
                            .await
                            .context("failed to confirm intent registration")??;

                            tracing::info!(batch_id = e.id, intent_id, "Intent ID found for batch");

                            batch_id = Some(e.id);

                            // Depending on whether we are generating new VTXOs or not, we continue
                            // with a different step in the state machine.
                            step = match outputs
                                .iter()
                                .any(|o| matches!(o, intent::Output::Offchain(_)))
                            {
                                true => Step::BatchStarted,
                                false => Step::BatchSigningStarted,
                            };

                            batch_expiry = Some(e.batch_expiry);
                        } else {
                            tracing::debug!(
                                batch_id = e.id,
                                intent_id,
                                "Intent ID not found for batch"
                            );
                        }
                    }
                    StreamEvent::TreeTx(e) => {
                        if step != Step::BatchStarted && step != Step::BatchSigningStarted {
                            continue;
                        }

                        match e.batch_tree_event_type {
                            BatchTreeEventType::Vtxo => {
                                match &mut vtxo_batch_tree_graph_chunks {
                                    Some(vtxo_batch_tree_graph_chunks) => {
                                        tracing::debug!("Got new VTXO batch-tree graph chunk");

                                        vtxo_batch_tree_graph_chunks.push(e.tx_graph_chunk)
                                    }
                                    None => {
                                        return Err(Error::ark_server(
                                            "received unexpected VTXO batch-tree graph chunk",
                                        ));
                                    }
                                };
                            }
                            BatchTreeEventType::Connector => {
                                match connectors_graph_chunks {
                                    Some(ref mut connectors_graph_chunks) => {
                                        tracing::debug!("Got new connectors graph chunk");

                                        connectors_graph_chunks.push(e.tx_graph_chunk)
                                    }
                                    None => {
                                        return Err(Error::ark_server(
                                            "received unexpected connectors graph chunk",
                                        ));
                                    }
                                };
                            }
                        }
                    }
                    StreamEvent::TreeSignature(e) => {
                        if step != Step::BatchSigningStarted {
                            continue;
                        }

                        match e.batch_tree_event_type {
                            BatchTreeEventType::Vtxo => {
                                match vtxo_batch_tree_graph {
                                    Some(ref mut vtxo_batch_tree_graph) => {
                                        vtxo_batch_tree_graph.apply(|graph| {
                                            if graph.root().unsigned_tx.compute_txid() != e.txid {
                                                Ok(true)
                                            } else {
                                                graph.set_signature(e.signature);

                                                Ok(false)
                                            }
                                        })?;
                                    }
                                    None => {
                                        return Err(Error::ark_server(
                                            "received batch-tree signature without transaction graph",
                                        ));
                                    }
                                };
                            }
                            BatchTreeEventType::Connector => {
                                return Err(Error::ark_server(
                                    "received batch-tree signature for connector tree",
                                ));
                            }
                        }
                    }
                    StreamEvent::TreeSigningStarted(e) => {
                        if step != Step::BatchStarted {
                            continue;
                        }

                        let chunks = vtxo_batch_tree_graph_chunks.take().ok_or(Error::ark_server(
                            "received batch-tree signing started event without VTXO batch-tree graph chunks",
                        ))?;
                        vtxo_batch_tree_graph =
                            Some(TxGraph::new(chunks).map_err(Error::from).context(
                                "failed to build VTXO batch-tree graph before generating nonces",
                            )?);

                        tracing::info!(batch_id = e.id, "Batch signing started");

                        for own_cosigner_pk in own_cosigner_pks.iter() {
                            if !&e.cosigners_pubkeys.iter().any(|p| p == own_cosigner_pk) {
                                return Err(Error::ark_server(format!(
                                    "own cosigner PK is not present in cosigner PKs: {own_cosigner_pk}"
                                )));
                            }
                        }

                        // We generate and submit a nonce tree for every cosigner key we provide.
                        let mut our_nonce_tree_map = HashMap::new();
                        for own_cosigner_kp in own_cosigner_kps {
                            let own_cosigner_pk = own_cosigner_kp.public_key();
                            let nonce_tree = generate_nonce_tree(
                                rng,
                                vtxo_batch_tree_graph
                                    .as_ref()
                                    .expect("VTXO batch-tree graph"),
                                own_cosigner_pk,
                                &e.unsigned_commitment_tx,
                            )
                            .map_err(Error::from)
                            .context("failed to generate VTXO nonce tree")?;

                            tracing::info!(
                                cosigner_pk = %own_cosigner_pk,
                                "Submitting nonce tree for cosigner PK"
                            );

                            network_client
                                .submit_tree_nonces(
                                    &e.id,
                                    own_cosigner_pk,
                                    nonce_tree.to_nonce_pks(),
                                )
                                .await
                                .map_err(Error::ark_server)
                                .context("failed to submit VTXO nonce tree")?;

                            our_nonce_tree_map.insert(own_cosigner_kp, nonce_tree);
                        }

                        unsigned_commitment_tx = Some(e.unsigned_commitment_tx);
                        our_nonce_trees = Some(our_nonce_tree_map);

                        step = step.next();
                    }
                    StreamEvent::TreeNonces(e) => {
                        if step != Step::BatchSigningStarted {
                            continue;
                        }

                        let tree_tx_nonce_pks = e.nonces;

                        let cosigner_pk = match tree_tx_nonce_pks.0.iter().find(|(pk, _)| {
                            own_cosigner_pks
                                .iter()
                                .any(|p| &&p.x_only_public_key().0 == pk)
                        }) {
                            Some((pk, _)) => *pk,
                            None => {
                                tracing::debug!(
                                    batch_id = e.id,
                                    txid = %e.txid,
                                    "Received irrelevant TreeNonces event"
                                );

                                continue;
                            }
                        };

                        tracing::debug!(
                            batch_id = e.id,
                            txid = %e.txid,
                            %cosigner_pk,
                            "Received TreeNonces event"
                        );

                        let agg_nonce_pk = aggregate_nonces(tree_tx_nonce_pks);

                        agg_nonce_pks.insert(e.txid, agg_nonce_pk);

                        if vtxo_batch_tree_graph.is_none() {
                            let chunks = vtxo_batch_tree_graph_chunks.take().ok_or(Error::ark_server(
                                "received batch-tree nonces event without VTXO batch-tree graph chunks",
                            ))?;
                            vtxo_batch_tree_graph = Some(
                                TxGraph::new(chunks)
                                    .map_err(Error::from)
                                    .context("failed to build VTXO batch-tree graph before batch-tree signing")?,
                            );
                        }
                        let vtxo_batch_tree_graph_ref =
                            vtxo_batch_tree_graph.as_ref().expect("just populated");

                        // Once we collect an aggregated nonce per transaction in our VTXO
                        // batch-tree graph, we can sign and submit our partial signatures.
                        if agg_nonce_pks.len() == vtxo_batch_tree_graph_ref.nb_of_nodes() {
                            let cosigner_kp = own_cosigner_kps
                                .iter()
                                .find(|kp| kp.public_key().x_only_public_key().0 == cosigner_pk)
                                .ok_or_else(|| {
                                    Error::ad_hoc("no cosigner keypair to sign for own PK")
                                })?;

                            let our_nonce_trees = our_nonce_trees.as_mut().ok_or(
                                Error::ark_server("missing nonce trees during batch protocol"),
                            )?;

                            let our_nonce_tree =
                                our_nonce_trees
                                    .get_mut(cosigner_kp)
                                    .ok_or(Error::ark_server(
                                        "missing nonce tree during batch protocol",
                                    ))?;

                            let unsigned_commitment_tx = unsigned_commitment_tx
                                .as_ref()
                                .ok_or_else(|| Error::ad_hoc("missing commitment TX"))?;

                            let batch_expiry = batch_expiry
                                .ok_or_else(|| Error::ad_hoc("missing batch expiry"))?;

                            let mut partial_sig_tree = PartialSigTree::default();
                            for (txid, _) in vtxo_batch_tree_graph_ref.as_map() {
                                let agg_nonce_pk = agg_nonce_pks.get(&txid).ok_or_else(|| {
                                    Error::ad_hoc(format!(
                                        "missing aggregated nonce PK for TX {txid}"
                                    ))
                                })?;

                                let sigs = sign_batch_tree_tx(
                                    txid,
                                    batch_expiry,
                                    ark_forfeit_pk,
                                    cosigner_kp,
                                    *agg_nonce_pk,
                                    vtxo_batch_tree_graph_ref,
                                    unsigned_commitment_tx,
                                    our_nonce_tree,
                                )
                                .map_err(Error::from)
                                .context("failed to sign VTXO batch-tree transactions")?;

                                partial_sig_tree.0.extend(sigs.0);
                            }

                            network_client
                                .submit_tree_signatures(
                                    &e.id,
                                    cosigner_kp.public_key(),
                                    partial_sig_tree,
                                )
                                .await
                                .map_err(Error::ark_server)
                                .context("failed to submit VTXO batch-tree signatures")?;
                        }
                    }
                    StreamEvent::TreeNoncesAggregated(e) => {
                        tracing::debug!(batch_id = e.id, "Batch combined nonces generated");
                    }
                    StreamEvent::BatchFinalization(e) => {
                        if step != Step::BatchSigningStarted {
                            continue;
                        }

                        tracing::debug!(
                            commitment_txid = %e.commitment_tx.unsigned_tx.compute_txid(),
                            "Batch finalization started"
                        );

                        let signed_forfeit_psbts = if !vtxo_inputs.is_empty() {
                            let chunks =
                                connectors_graph_chunks.take().ok_or(Error::ark_server(
                                    "received batch finalization event without connectors",
                                ))?;

                            if chunks.is_empty() {
                                tracing::debug!(batch_id = e.id, "No forfeit transactions");

                                Vec::new()
                            } else {
                                let connectors_graph = TxGraph::new(chunks)
                                    .map_err(Error::from)
                                    .context(
                                    "failed to build connectors graph before signing forfeit TXs",
                                )?;

                                tracing::debug!(batch_id = e.id, "Batch finalization started");

                                create_and_sign_forfeit_txs(
                                    |input: &mut psbt::Input, msg: secp256k1::Message| match &input
                                    .witness_script
                                {
                                    None => Err(ark_core::Error::ad_hoc(
                                        "Missing witness script in psbt::Input when signing forfeit",
                                    )),
                                    Some(script) => {
                                        let pks = extract_checksig_pubkeys(script);
                                        let mut res = vec![];
                                        for pk in pks {
                                            if let Ok(keypair) =
                                            self.keypair_by_pk(&pk) {
                                                let sig =
                                                    secp.sign_schnorr_no_aux_rand(&msg, &keypair);
                                                res.push((sig, keypair.public_key().into()))
                                            }
                                        }
                                        Ok(res)
                                    }
                                    },
                                    vtxo_inputs.as_slice(),
                                    &connectors_graph.leaves(),
                                    &server_info.forfeit_address,
                                    server_info.dust,
                                )
                                .map_err(Error::from)?
                            }
                        } else {
                            Vec::new()
                        };

                        let commitment_psbt = if onchain_inputs.is_empty() {
                            None
                        } else {
                            let mut commitment_psbt = e.commitment_tx;

                            let sign_for_pk_fn = |pk: &XOnlyPublicKey,
                                                  msg: &secp256k1::Message|
                             -> Result<
                                schnorr::Signature,
                                ark_core::Error,
                            > {
                                self.sign_for_pk(pk, msg)
                                    .map_err(|e| ark_core::Error::ad_hoc(e.to_string()))
                            };

                            sign_commitment_psbt(
                                sign_for_pk_fn,
                                &mut commitment_psbt,
                                &onchain_inputs,
                            )
                            .map_err(Error::from)?;

                            Some(commitment_psbt)
                        };

                        if !signed_forfeit_psbts.is_empty() || commitment_psbt.is_some() {
                            network_client
                                .submit_signed_forfeit_txs(signed_forfeit_psbts, commitment_psbt)
                                .await?;
                        }

                        step = step.next();
                    }
                    StreamEvent::BatchFinalized(e) => {
                        if step != Step::Finalized {
                            continue;
                        }

                        let commitment_txid = e.commitment_txid;

                        tracing::info!(batch_id = e.id, %commitment_txid, "Batch finalized");

                        return Ok(commitment_txid);
                    }
                    StreamEvent::BatchFailed(ref e) => {
                        if Some(&e.id) == batch_id.as_ref() {
                            return Err(Error::ark_server(format!(
                                "batch failed {}: {}",
                                e.id, e.reason
                            )));
                        }

                        tracing::debug!("Unrelated batch failed: {e:?}");
                    }
                    StreamEvent::Heartbeat => {}
                    StreamEvent::StreamStarted(_) => {}
                },
                Some(Err(e)) => {
                    tracing::error!("Got error from event stream");

                    return Err(Error::ark_server(e));
                }
                None => {
                    return Err(Error::ark_server("dropped batch event stream"));
                }
            }
        }

        #[derive(Debug, PartialEq, Eq)]
        enum Step {
            Start,
            BatchStarted,
            BatchSigningStarted,
            Finalized,
        }

        impl Step {
            fn next(&self) -> Step {
                match self {
                    Step::Start => Step::BatchStarted,
                    Step::BatchStarted => Step::BatchSigningStarted,
                    Step::BatchSigningStarted => Step::Finalized,
                    Step::Finalized => Step::Finalized, // we can't go further
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PrepareIntentKind {
    Register,
    EstimateFee,
}

#[derive(Debug, Clone)]
pub(crate) enum BatchOutputType {
    Board {
        to_address: ArkAddress,
        to_amount: Amount,
    },
    OffBoard {
        to_address: Address,
        to_amount: Amount,
        change_address: ArkAddress,
        change_amount: Amount,
    },
}

/// Combine boarding outputs and VTXOs into the flat input list used to build an intent proof,
/// mirroring the conversion done when registering the intent.
fn combined_intent_inputs(
    onchain_inputs: &[batch::OnChainInput],
    vtxo_inputs: &[intent::Input],
) -> Vec<intent::Input> {
    onchain_inputs
        .iter()
        .map(|o| {
            intent::Input::new(
                o.outpoint(),
                o.sequence(),
                None,
                TxOut {
                    value: o.amount(),
                    script_pubkey: o.script_pubkey().clone(),
                },
                o.tapscripts().to_vec(),
                o.spend_info().clone(),
                true,
                false,
                Vec::new(),
            )
        })
        .chain(vtxo_inputs.iter().cloned())
        .collect()
}

/// Estimate the weight of the intent proof for settling the given inputs into a single offchain
/// VTXO at `to_address`.
fn estimate_settlement_proof_weight(
    onchain_inputs: &[batch::OnChainInput],
    vtxo_inputs: &[intent::Input],
    to_address: &ArkAddress,
) -> Result<u64, Error> {
    let inputs = combined_intent_inputs(onchain_inputs, vtxo_inputs);

    let total_amount = inputs
        .iter()
        .fold(Amount::ZERO, |acc, input| acc + input.amount());

    let mut outputs = vec![intent::Output::Offchain(TxOut {
        value: total_amount,
        script_pubkey: to_address.to_p2tr_script_pubkey(),
    })];

    if let Some(packet) = create_asset_preservation_packet(&inputs, &outputs)? {
        outputs.push(intent::Output::AssetPacket(packet.to_txout()));
    }

    let weight = intent::estimate_proof_weight(&inputs, &outputs)?;

    Ok(weight.to_wu())
}

/// A subset of settlement inputs whose intent proof fits under the proof weight limit.
#[derive(Debug)]
struct SettlementChunk {
    onchain_inputs: Vec<batch::OnChainInput>,
    vtxo_inputs: Vec<intent::Input>,
    total_amount: Amount,
}

impl SettlementChunk {
    fn new() -> Self {
        Self {
            onchain_inputs: Vec::new(),
            vtxo_inputs: Vec::new(),
            total_amount: Amount::ZERO,
        }
    }

    fn is_empty(&self) -> bool {
        self.onchain_inputs.is_empty() && self.vtxo_inputs.is_empty()
    }

    fn push(&mut self, input: SettlementInput) {
        match input {
            SettlementInput::Onchain(input) => {
                self.total_amount += input.amount();
                self.onchain_inputs.push(input);
            }
            SettlementInput::Vtxo(input) => {
                self.total_amount += input.amount();
                self.vtxo_inputs.push(input);
            }
        }
    }

    fn pop(&mut self, input_was_onchain: bool) -> SettlementInput {
        if input_was_onchain {
            let input = self.onchain_inputs.pop().expect("just pushed");
            self.total_amount -= input.amount();
            SettlementInput::Onchain(input)
        } else {
            let input = self.vtxo_inputs.pop().expect("just pushed");
            self.total_amount -= input.amount();
            SettlementInput::Vtxo(input)
        }
    }

    /// All inputs in this chunk as `(is_onchain, index, amount)`.
    fn input_amounts(&self) -> Vec<(bool, usize, Amount)> {
        self.onchain_inputs
            .iter()
            .enumerate()
            .map(|(i, input)| (true, i, input.amount()))
            .chain(
                self.vtxo_inputs
                    .iter()
                    .enumerate()
                    .map(|(i, input)| (false, i, input.amount())),
            )
            .collect()
    }

    fn remove(&mut self, is_onchain: bool, index: usize) -> SettlementInput {
        if is_onchain {
            let input = self.onchain_inputs.remove(index);
            self.total_amount -= input.amount();
            SettlementInput::Onchain(input)
        } else {
            let input = self.vtxo_inputs.remove(index);
            self.total_amount -= input.amount();
            SettlementInput::Vtxo(input)
        }
    }

    fn clone_input(&self, is_onchain: bool, index: usize) -> SettlementInput {
        if is_onchain {
            SettlementInput::Onchain(self.onchain_inputs[index].clone())
        } else {
            SettlementInput::Vtxo(self.vtxo_inputs[index].clone())
        }
    }

    fn estimate_proof_weight(&self, to_address: &ArkAddress) -> Result<u64, Error> {
        estimate_settlement_proof_weight(&self.onchain_inputs, &self.vtxo_inputs, to_address)
    }
}

enum SettlementInput {
    Onchain(batch::OnChainInput),
    Vtxo(intent::Input),
}

impl SettlementInput {
    fn is_onchain(&self) -> bool {
        matches!(self, SettlementInput::Onchain(_))
    }
}

/// Split settlement inputs into chunks whose intent proofs each fit under `max_weight` weight
/// units, preserving input order (boarding outputs first, then VTXOs).
///
/// Every chunk settles into its own offchain VTXO of the chunk's total amount, so each chunk
/// must also clear the `dust` threshold; chunks that fall short are topped up by moving inputs
/// over from other chunks.
///
/// Returns an error if a single input on its own already exceeds the limit, or if the chunks
/// cannot be balanced to clear the dust threshold. Errors surface before any batch is joined, so
/// a settlement never fails halfway through its chunks.
const SINGLE_INPUT_TOO_LARGE_CONTEXT: &str =
    "a single input's proof alone exceeds the weight limit; it cannot be batch-settled";

fn chunk_settlement_inputs(
    onchain_inputs: Vec<batch::OnChainInput>,
    vtxo_inputs: Vec<intent::Input>,
    to_address: &ArkAddress,
    max_weight: u64,
    dust: Amount,
) -> Result<Vec<SettlementChunk>, Error> {
    let inputs = onchain_inputs
        .into_iter()
        .map(SettlementInput::Onchain)
        .chain(vtxo_inputs.into_iter().map(SettlementInput::Vtxo));

    let mut chunks = Vec::new();
    let mut current = SettlementChunk::new();

    for input in inputs {
        let is_onchain = input.is_onchain();

        current.push(input);

        let weight = current.estimate_proof_weight(to_address)?;

        if weight > max_weight {
            let input = current.pop(is_onchain);

            if current.is_empty() {
                // A single input already busts the limit; splitting cannot help.
                return Err(Error::intent_proof_too_large(weight, max_weight)
                    .context(SINGLE_INPUT_TOO_LARGE_CONTEXT));
            }

            chunks.push(std::mem::replace(&mut current, SettlementChunk::new()));

            current.push(input);

            let weight = current.estimate_proof_weight(to_address)?;
            if weight > max_weight {
                return Err(Error::intent_proof_too_large(weight, max_weight)
                    .context(SINGLE_INPUT_TOO_LARGE_CONTEXT));
            }
        }
    }

    if !current.is_empty() {
        chunks.push(current);
    }

    rebalance_sub_dust_chunks(&mut chunks, to_address, max_weight, dust)?;

    Ok(chunks)
}

/// Top up chunks whose total amount is below the dust threshold by moving inputs over from
/// other chunks, without pushing any chunk's intent proof over `max_weight`.
///
/// Before chunking existed, small recoverable VTXOs were always carried by larger inputs into a
/// single above-dust output; this preserves that behavior across chunk boundaries.
///
/// A lone chunk is left untouched: its sub-dust failure mode predates chunking and is reported
/// by `prepare_intent` ("cannot settle into sub-dust VTXO").
fn rebalance_sub_dust_chunks(
    chunks: &mut Vec<SettlementChunk>,
    to_address: &ArkAddress,
    max_weight: u64,
    dust: Amount,
) -> Result<(), Error> {
    if chunks.len() <= 1 {
        return Ok(());
    }

    for receiver in 0..chunks.len() {
        while !chunks[receiver].is_empty() && chunks[receiver].total_amount < dust {
            if !move_input_into_chunk(chunks, receiver, to_address, max_weight, dust)? {
                let amount = chunks[receiver].total_amount;
                return Err(Error::ad_hoc(format!(
                    "cannot balance settlement chunks: chunk amount {amount} is below the dust \
                     threshold {dust} and no input can be moved into it without exceeding the \
                     proof weight limit {max_weight}"
                )));
            }
        }
    }

    // Donors may have been drained empty; drop them.
    chunks.retain(|chunk| !chunk.is_empty());

    Ok(())
}

/// Move a single input from some other chunk into `chunks[receiver]`, keeping every donor either
/// empty or above the dust threshold and the receiver's proof under `max_weight`.
///
/// Returns `false` if no input can be moved.
fn move_input_into_chunk(
    chunks: &mut [SettlementChunk],
    receiver: usize,
    to_address: &ArkAddress,
    max_weight: u64,
    dust: Amount,
) -> Result<bool, Error> {
    // Prefer donating from the richest chunk, and prefer big inputs, so the receiver clears the
    // dust threshold in as few moves as possible.
    let mut donors: Vec<usize> = (0..chunks.len()).filter(|i| *i != receiver).collect();
    donors.sort_by_key(|i| std::cmp::Reverse(chunks[*i].total_amount));

    for donor in donors {
        let mut candidates = chunks[donor].input_amounts();
        candidates.sort_by_key(|(_, _, amount)| std::cmp::Reverse(*amount));

        for (is_onchain, index, amount) in candidates {
            // The donor must stay settleable: either it keeps an above-dust total or it is
            // drained completely.
            let donor_remaining = chunks[donor].total_amount - amount;
            let donor_drained = donor_remaining == Amount::ZERO;
            if donor_remaining < dust && !donor_drained {
                continue;
            }

            // Simulate the move with a clone first, so a rejected candidate leaves the donor
            // untouched and the remaining candidate indices valid.
            let candidate = chunks[donor].clone_input(is_onchain, index);
            chunks[receiver].push(candidate);
            let over_weight = chunks[receiver].estimate_proof_weight(to_address)? > max_weight;
            chunks[receiver].pop(is_onchain);

            if over_weight {
                continue;
            }

            let input = chunks[donor].remove(is_onchain, index);
            chunks[receiver].push(input);

            return Ok(true);
        }
    }

    Ok(false)
}

/// Prepared intent data ready for batch registration.
pub(crate) struct PreparedIntent {
    /// The signed intent.
    pub intent: intent::Intent,
    /// The ephemeral cosigner keypair.
    pub cosigner_keypair: Keypair,
    /// VTXO input outpoints (used for event stream topics).
    pub vtxo_input_outpoints: Vec<OutPoint>,
    /// Intent outputs (used to determine batch protocol steps).
    pub outputs: Vec<intent::Output>,
    /// The original onchain inputs (needed for commitment signing).
    pub onchain_inputs: Vec<batch::OnChainInput>,
    /// The original VTXO inputs (needed for forfeit signing).
    pub vtxo_inputs: Vec<intent::Input>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use ark_core::script::multisig_script;
    use bitcoin::hashes::Hash;
    use bitcoin::taproot::LeafVersion;
    use bitcoin::taproot::TaprootBuilder;
    use bitcoin::Network;
    use bitcoin::Sequence;

    fn test_address_and_spend_info() -> (
        ArkAddress,
        bitcoin::ScriptBuf,
        bitcoin::taproot::ControlBlock,
    ) {
        let secp = Secp256k1::new();

        let server_pk: PublicKey =
            "0250929b74c1a04954b78b4b6035e97a5e078a5a0f28ec96d547bfee9ace803ac0"
                .parse()
                .unwrap();
        let owner_pk: PublicKey =
            "03dff1d77f2a671c5f36183726db2341be58f8be17d2a3d1d2cd47b7b0f5f2d624"
                .parse()
                .unwrap();

        let server_xonly = server_pk.x_only_public_key().0;
        let owner_xonly = owner_pk.x_only_public_key().0;

        let spend_script = multisig_script(server_xonly, owner_xonly);
        let spend_info = TaprootBuilder::new()
            .add_leaf(0, spend_script.clone())
            .unwrap()
            .finalize(&secp, server_xonly)
            .unwrap();
        let control_block = spend_info
            .control_block(&(spend_script.clone(), LeafVersion::TapScript))
            .unwrap();

        let address = ArkAddress::new(Network::Regtest, server_xonly, spend_info.output_key());

        (address, spend_script, control_block)
    }

    fn vtxo_input(outpoint_tag: u8, amount: Amount) -> intent::Input {
        let (address, spend_script, control_block) = test_address_and_spend_info();

        intent::Input::new(
            OutPoint::new(Txid::from_byte_array([outpoint_tag; 32]), 0),
            Sequence::MAX,
            None,
            TxOut {
                value: amount,
                script_pubkey: address.to_p2tr_script_pubkey(),
            },
            vec![spend_script.clone()],
            (spend_script, control_block),
            false,
            false,
            Vec::new(),
        )
    }

    fn vtxo_inputs(amounts: &[u64]) -> Vec<intent::Input> {
        amounts
            .iter()
            .enumerate()
            .map(|(i, amount)| vtxo_input(i as u8 + 1, Amount::from_sat(*amount)))
            .collect()
    }

    /// The estimated proof weight for settling `n` identical test inputs.
    fn weight_of(n: usize) -> u64 {
        let (address, _, _) = test_address_and_spend_info();
        let inputs = vtxo_inputs(&vec![10_000; n]);
        estimate_settlement_proof_weight(&[], &inputs, &address).unwrap()
    }

    fn chunk_outpoints(chunks: &[SettlementChunk]) -> Vec<OutPoint> {
        chunks
            .iter()
            .flat_map(|chunk| chunk.vtxo_inputs.iter().map(|input| input.outpoint()))
            .collect()
    }

    const DUST: Amount = Amount::from_sat(330);

    #[test]
    fn estimated_proof_weight_grows_per_input() {
        let w1 = weight_of(1);
        let w2 = weight_of(2);
        let w3 = weight_of(3);

        assert!(w1 < w2);
        assert!(w2 < w3);

        // Each identical input adds the same weight: input base (outpoint, script length,
        // sequence) plus its witness (two 64-byte signatures, leaf script, control block).
        assert_eq!(w2 - w1, w3 - w2);
        let per_input = w2 - w1;
        assert!(
            (300..600).contains(&per_input),
            "unexpected per-input weight: {per_input}"
        );
    }

    #[test]
    fn one_chunk_when_everything_fits() {
        let (address, _, _) = test_address_and_spend_info();
        let inputs = vtxo_inputs(&[10_000; 10]);

        let chunks =
            chunk_settlement_inputs(Vec::new(), inputs.clone(), &address, weight_of(10), DUST)
                .unwrap();

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].total_amount, Amount::from_sat(100_000));
        assert_eq!(
            chunk_outpoints(&chunks),
            inputs.iter().map(|i| i.outpoint()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn splits_into_weight_bounded_chunks_preserving_order() {
        let (address, _, _) = test_address_and_spend_info();
        let inputs = vtxo_inputs(&[10_000; 10]);
        let max_weight = weight_of(3);

        let chunks =
            chunk_settlement_inputs(Vec::new(), inputs.clone(), &address, max_weight, DUST)
                .unwrap();

        assert_eq!(chunks.len(), 4); // 3 + 3 + 3 + 1.

        for chunk in chunks.iter() {
            assert!(chunk.estimate_proof_weight(&address).unwrap() <= max_weight);
            assert!(chunk.total_amount >= DUST);
        }

        assert_eq!(
            chunk_outpoints(&chunks),
            inputs.iter().map(|i| i.outpoint()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn errors_when_a_single_input_exceeds_the_limit() {
        let (address, _, _) = test_address_and_spend_info();
        let inputs = vtxo_inputs(&[10_000; 3]);

        let err = chunk_settlement_inputs(Vec::new(), inputs, &address, weight_of(1) - 1, DUST)
            .unwrap_err();

        assert!(err.is_intent_proof_too_large());
        assert!(err.to_string().contains("cannot be batch-settled"));
    }

    #[test]
    fn rebalances_sub_dust_trailing_chunk() {
        let (address, _, _) = test_address_and_spend_info();
        // Six healthy inputs and one sub-dust input which ends up alone in the trailing chunk.
        let inputs = vtxo_inputs(&[10_000, 10_000, 10_000, 10_000, 10_000, 10_000, 100]);
        let max_weight = weight_of(3);

        let chunks =
            chunk_settlement_inputs(Vec::new(), inputs.clone(), &address, max_weight, DUST)
                .unwrap();

        for chunk in chunks.iter() {
            assert!(chunk.estimate_proof_weight(&address).unwrap() <= max_weight);
            assert!(chunk.total_amount >= DUST);
        }

        // No input was lost or duplicated.
        let mut outpoints = chunk_outpoints(&chunks);
        outpoints.sort();
        let mut expected = inputs.iter().map(|i| i.outpoint()).collect::<Vec<_>>();
        expected.sort();
        assert_eq!(outpoints, expected);

        let total = chunks
            .iter()
            .fold(Amount::ZERO, |acc, chunk| acc + chunk.total_amount);
        assert_eq!(total, Amount::from_sat(60_100));
    }

    #[test]
    fn errors_when_sub_dust_chunks_cannot_be_balanced() {
        let (address, _, _) = test_address_and_spend_info();
        // Two sub-dust inputs which cannot share a chunk: the proof weight limit only fits one
        // input per chunk.
        let inputs = vtxo_inputs(&[100, 100]);

        let err =
            chunk_settlement_inputs(Vec::new(), inputs, &address, weight_of(1), DUST).unwrap_err();

        assert!(err.to_string().contains("cannot balance settlement chunks"));
    }
}

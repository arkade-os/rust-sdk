use crate::Blockchain;
use crate::Client;
use crate::Error;
use crate::error::ErrorContext as _;
use crate::swap_storage::SwapStorage;
use crate::utils::sleep;
use crate::utils::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use ark_core::ArkAddress;
use ark_core::ExplorerUtxo;
use ark_core::TxGraph;
use ark_core::batch;
use ark_core::batch::Delegate;
use ark_core::batch::NonceKps;
use ark_core::batch::aggregate_nonces;
use ark_core::batch::complete_delegate_forfeit_txs;
use ark_core::batch::create_and_sign_forfeit_txs;
use ark_core::batch::generate_nonce_tree;
use ark_core::batch::sign_batch_tree_tx;
use ark_core::batch::sign_commitment_psbt;
use ark_core::intent;
use ark_core::script::extract_checksig_pubkeys;
use ark_core::server::BatchTreeEventType;
use ark_core::server::PartialSigTree;
use ark_core::server::StreamEvent;
use backon::ExponentialBuilder;
use backon::Retryable;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::Psbt;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use bitcoin::hashes::Hash;
use bitcoin::hashes::sha256;
use bitcoin::hex::DisplayHex;
use bitcoin::key::Keypair;
use bitcoin::key::Secp256k1;
use bitcoin::psbt;
use bitcoin::secp256k1;
use bitcoin::secp256k1::PublicKey;
use bitcoin::secp256k1::schnorr;
use futures::StreamExt;
use jiff::Timestamp;
use rand::CryptoRng;
use rand::Rng;
use std::collections::HashMap;

impl<B, W, S, K> Client<B, W, S, K>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
    S: SwapStorage + 'static,
    K: crate::KeyProvider,
{
    /// Settle _all_ prior VTXOs and boarding outputs into the next batch, generating new confirmed
    /// VTXOs.
    pub async fn settle<R>(&self, rng: &mut R) -> Result<Option<Txid>, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        // Get off-chain address and send all funds to this address, no change output 🦄
        let (to_address, _) = self.get_offchain_address()?;

        let (boarding_inputs, vtxo_inputs, total_amount) =
            self.fetch_commitment_transaction_inputs().await?;

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

        let join_next_batch = || async {
            self.join_next_batch(
                &mut rng.clone(),
                boarding_inputs.clone(),
                vtxo_inputs.clone(),
                BatchOutputType::Board {
                    to_address,
                    to_amount: total_amount,
                },
            )
            .await
        };

        // Joining a batch can fail depending on the timing, so we try a few times.
        let commitment_txid = join_next_batch
            .retry(ExponentialBuilder::default().with_max_times(0))
            .sleep(sleep)
            // TODO: Use `when` to only retry certain errors.
            .notify(|err: &Error, dur: std::time::Duration| {
                tracing::warn!("Retrying joining next batch after {dur:?}. Error: {err}",);
            })
            .await
            .context("Failed to join batch")?;

        tracing::info!(%commitment_txid, "Settlement success");

        Ok(Some(commitment_txid))
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
        let (change_address, _) = self.get_offchain_address()?;

        let (boarding_inputs, vtxo_inputs, total_amount) =
            self.fetch_commitment_transaction_inputs().await?;

        let onchain_fee = self
            .server_info
            .fees
            .as_ref()
            .map(|f| f.intent_fee.onchain_output)
            .unwrap_or(Amount::ZERO);

        // Deduct fee from the requested amount.
        let net_to_amount = to_amount.checked_sub(onchain_fee).ok_or_else(|| {
            Error::coin_select(
                "cannot deduct fees from offboard amount ({onchain_fee} > {to_amount})",
            )
        })?;

        let change_amount = total_amount.checked_sub(to_amount).ok_or_else(|| {
            Error::coin_select("cannot afford to send {to_amount}, only have {total_amount}")
        })?;

        tracing::info!(
            %to_address,
            gross_amount = %to_amount,
            net_amount = %net_to_amount,
            fee = %onchain_fee,
            change_address = %change_address.encode(),
            %change_amount,
            ?boarding_inputs,
            "Attempting to collaboratively redeem outputs"
        );

        let join_next_batch = || async {
            self.join_next_batch(
                &mut rng.clone(),
                boarding_inputs.clone(),
                vtxo_inputs.clone(),
                BatchOutputType::OffBoard {
                    to_address: to_address.clone(),
                    to_amount: net_to_amount,
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
            // TODO: Use `when` to only retry certain errors.
            .notify(|err: &Error, dur: std::time::Duration| {
                tracing::warn!("Retrying joining next batch after {dur:?}. Error: {err}");
            })
            .await
            .context("Failed to join batch")?;

        tracing::info!(%commitment_txid, "Collaborative redeem success");

        Ok(commitment_txid)
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
        // Get off-chain address and send all funds to this address.
        let (to_address, _) = self.get_offchain_address()?;

        // Simply collect all VTXOs that can be settled.
        let (_, vtxo_inputs, _) = self.fetch_commitment_transaction_inputs().await?;

        let total_amount = vtxo_inputs
            .iter()
            .fold(Amount::ZERO, |acc, v| acc + v.amount());

        if vtxo_inputs.is_empty() {
            return Err(Error::ad_hoc("no inputs to settle via delegate"));
        }

        let server_info = &self.server_info;

        let outputs = vec![intent::Output::Offchain(TxOut {
            value: total_amount,
            script_pubkey: to_address.to_p2tr_script_pubkey(),
        })];

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
        let server_info = &self.server_info;

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

        let mut vtxo_graph_chunks = Some(Vec::new());
        let mut vtxo_graph: Option<TxGraph> = None;

        let mut connectors_graph_chunks = Some(Vec::new());
        let mut batch_expiry = None;

        let mut agg_nonce_pks = HashMap::new();

        let mut our_nonce_trees: Option<HashMap<Keypair, NonceKps>> = None;

        loop {
            match stream.next().await {
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
                                match &mut vtxo_graph_chunks {
                                    Some(vtxo_graph_chunks) => {
                                        tracing::debug!("Got new VTXO graph chunk");

                                        vtxo_graph_chunks.push(e.tx_graph_chunk)
                                    }
                                    None => {
                                        return Err(Error::ark_server(
                                            "received unexpected VTXO graph chunk",
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
                                match vtxo_graph {
                                    Some(ref mut vtxo_graph) => {
                                        vtxo_graph.apply(|graph| {
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
                                            "received batch tree signature without TX graph",
                                        ));
                                    }
                                };
                            }
                            BatchTreeEventType::Connector => {
                                return Err(Error::ark_server(
                                    "received batch tree signature for connectors tree",
                                ));
                            }
                        }
                    }
                    StreamEvent::TreeSigningStarted(e) => {
                        if step != Step::BatchStarted {
                            continue;
                        }

                        let chunks = vtxo_graph_chunks.take().ok_or(Error::ark_server(
                            "received tree signing started event without VTXO graph chunks",
                        ))?;
                        vtxo_graph = Some(
                            TxGraph::new(chunks)
                                .map_err(Error::from)
                                .context("failed to build VTXO graph before generating nonces")?,
                        );

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
                                vtxo_graph.as_ref().expect("VTXO graph"),
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

                        let vtxo_graph = match vtxo_graph {
                            Some(ref vtxo_graph) => vtxo_graph,
                            None => {
                                let chunks = vtxo_graph_chunks.take().ok_or(Error::ark_server(
                                    "received tree nonces event without VTXO graph chunks",
                                ))?;

                                &TxGraph::new(chunks)
                                    .map_err(Error::from)
                                    .context("failed to build VTXO graph before tree signing")?
                            }
                        };

                        if agg_nonce_pks.len() == vtxo_graph.nb_of_nodes() {
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
                            for (txid, _) in vtxo_graph.as_map() {
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
                                    vtxo_graph,
                                    unsigned_commitment_tx,
                                    our_nonce_tree,
                                )
                                .map_err(Error::from)
                                .context("failed to sign VTXO tree")?;

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
                                .context("failed to submit VTXO tree signatures")?;
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
                            let connectors_graph = TxGraph::new(chunks).map_err(Error::from).context(
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
    async fn fetch_commitment_transaction_inputs(
        &self,
    ) -> Result<(Vec<batch::OnChainInput>, Vec<intent::Input>, Amount), Error> {
        // Get all known boarding outputs.
        let boarding_outputs = self.inner.wallet.get_boarding_outputs()?;

        let mut boarding_inputs: Vec<batch::OnChainInput> = Vec::new();
        let mut total_amount = Amount::ZERO;

        // To track unique outpoints and prevent duplicates
        let mut seen_outpoints = std::collections::HashSet::new();

        let now = Timestamp::now();

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
                    is_spent: false,
                } = o
                {
                    // Check for duplicate outpoints
                    if seen_outpoints.contains(outpoint) {
                        continue;
                    }

                    // Only include confirmed boarding outputs with an _inactive_ exit path.
                    if !boarding_output.can_be_claimed_unilaterally_by_owner(
                        now.as_duration().try_into().map_err(Error::ad_hoc)?,
                        std::time::Duration::from_secs(*confirmation_blocktime),
                    ) {
                        // Mark this outpoint as seen
                        seen_outpoints.insert(*outpoint);

                        boarding_inputs.push(batch::OnChainInput::new(
                            boarding_output.clone(),
                            *amount,
                            *outpoint,
                        ));
                        total_amount += *amount;
                    }
                }
            }
        }

        let (vtxo_list, script_pubkey_to_vtxo_map) = self.list_vtxos().await?;

        total_amount += vtxo_list
            .all_unspent()
            .fold(Amount::ZERO, |acc, vtxo| acc + vtxo.amount);

        let vtxo_inputs = vtxo_list
            .all_unspent()
            .map(|virtual_tx_outpoint| {
                let vtxo = script_pubkey_to_vtxo_map
                    .get(&virtual_tx_outpoint.script)
                    .ok_or_else(|| {
                        ark_core::Error::ad_hoc(format!(
                            "missing VTXO for script pubkey: {}",
                            virtual_tx_outpoint.script
                        ))
                    })?;
                let spend_info = vtxo.forfeit_spend_info()?;

                Ok(intent::Input::new(
                    virtual_tx_outpoint.outpoint,
                    vtxo.exit_delay(),
                    TxOut {
                        value: virtual_tx_outpoint.amount,
                        script_pubkey: vtxo.script_pubkey(),
                    },
                    vtxo.tapscripts(),
                    spend_info,
                    false,
                    virtual_tx_outpoint.is_swept,
                ))
            })
            .collect::<Result<Vec<_>, ark_core::Error>>()?;

        Ok((boarding_inputs, vtxo_inputs, total_amount))
    }

    pub(crate) async fn join_next_batch<R>(
        &self,
        rng: &mut R,
        onchain_inputs: Vec<batch::OnChainInput>,
        vtxo_inputs: Vec<intent::Input>,
        output_type: BatchOutputType,
    ) -> Result<Txid, Error>
    where
        R: Rng + CryptoRng,
    {
        if onchain_inputs.is_empty() && vtxo_inputs.is_empty() {
            return Err(Error::ad_hoc("cannot join batch without inputs"));
        }

        let server_info = &self.server_info;

        // Generate an (ephemeral) cosigner keypair.
        let own_cosigner_kp = Keypair::new(self.secp(), rng);

        let onchain_input_outpoints = onchain_inputs
            .iter()
            .map(|i| i.outpoint())
            .collect::<Vec<_>>();
        let vtxo_input_outpoints = vtxo_inputs.iter().map(|i| i.outpoint()).collect::<Vec<_>>();

        let inputs = {
            let boarding_inputs = onchain_inputs.clone().into_iter().map(|o| {
                intent::Input::new(
                    o.outpoint(),
                    o.boarding_output().exit_delay(),
                    TxOut {
                        value: o.amount(),
                        script_pubkey: o.boarding_output().script_pubkey(),
                    },
                    o.boarding_output().tapscripts(),
                    o.boarding_output().forfeit_spend_info(),
                    true,
                    false,
                )
            });

            boarding_inputs
                .chain(vtxo_inputs.clone())
                .collect::<Vec<_>>()
        };

        let dust = self.server_info.dust;

        let mut outputs = vec![];

        match output_type {
            BatchOutputType::Board {
                to_address,
                to_amount,
            } => {
                if to_amount < self.server_info.dust {
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

        let mut step = Step::Start;

        let own_cosigner_kps = [own_cosigner_kp];
        let own_cosigner_pks = own_cosigner_kps
            .iter()
            .map(|k| k.public_key())
            .collect::<Vec<_>>();

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
                        Some(o.boarding_output().script_pubkey())
                            == input.witness_utxo.clone().map(|w| w.script_pubkey)
                    })
                    .ok_or_else(|| {
                        ark_core::Error::ad_hoc(
                            "could not find signing key for onchain input: {input:?}",
                        )
                    })?;

                let owner_pk = onchain_input.boarding_output().owner_pk();
                let sig = self
                    .inner
                    .wallet
                    .sign_for_pk(&owner_pk, &msg)
                    .map_err(|e| ark_core::Error::ad_hoc(e.to_string()))?;

                Ok((sig, owner_pk))
            };

        let intent = intent::make_intent(
            sign_for_vtxo_fn,
            sign_for_onchain_fn,
            inputs,
            outputs.clone(),
            own_cosigner_pks.clone(),
        )?;

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

        let mut vtxo_graph_chunks = Some(Vec::new());
        let mut vtxo_graph: Option<TxGraph> = None;

        let mut connectors_graph_chunks = Some(Vec::new());
        let mut batch_expiry = None;

        let mut agg_nonce_pks = HashMap::new();

        let mut our_nonce_trees: Option<HashMap<Keypair, NonceKps>> = None;
        loop {
            match stream.next().await {
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
                                match &mut vtxo_graph_chunks {
                                    Some(vtxo_graph_chunks) => {
                                        tracing::debug!("Got new VTXO graph chunk");

                                        vtxo_graph_chunks.push(e.tx_graph_chunk)
                                    }
                                    None => {
                                        return Err(Error::ark_server(
                                            "received unexpected VTXO graph chunk",
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
                                match vtxo_graph {
                                    Some(ref mut vtxo_graph) => {
                                        vtxo_graph.apply(|graph| {
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
                                            "received batch tree signature without TX graph",
                                        ));
                                    }
                                };
                            }
                            BatchTreeEventType::Connector => {
                                return Err(Error::ark_server(
                                    "received batch tree signature for connectors tree",
                                ));
                            }
                        }
                    }
                    StreamEvent::TreeSigningStarted(e) => {
                        if step != Step::BatchStarted {
                            continue;
                        }

                        let chunks = vtxo_graph_chunks.take().ok_or(Error::ark_server(
                            "received tree signing started event without VTXO graph chunks",
                        ))?;
                        vtxo_graph = Some(
                            TxGraph::new(chunks)
                                .map_err(Error::from)
                                .context("failed to build VTXO graph before generating nonces")?,
                        );

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
                                vtxo_graph.as_ref().expect("VTXO graph"),
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

                        let vtxo_graph = match vtxo_graph {
                            Some(ref vtxo_graph) => vtxo_graph,
                            None => {
                                let chunks = vtxo_graph_chunks.take().ok_or(Error::ark_server(
                                    "received tree nonces event without VTXO graph chunks",
                                ))?;

                                &TxGraph::new(chunks)
                                    .map_err(Error::from)
                                    .context("failed to build VTXO graph before tree signing")?
                            }
                        };

                        // Once we collect an aggregated nonce per transaction in our VTXO graph, we
                        // can go ahead with signing and submitting.
                        if agg_nonce_pks.len() == vtxo_graph.nb_of_nodes() {
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
                            for (txid, _) in vtxo_graph.as_map() {
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
                                    vtxo_graph,
                                    unsigned_commitment_tx,
                                    our_nonce_tree,
                                )
                                .map_err(Error::from)
                                .context("failed to sign VTXO tree")?;

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
                                .context("failed to submit VTXO tree signatures")?;
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
                                let connectors_graph =
                                TxGraph::new(chunks).map_err(Error::from).context(
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
                                self.inner
                                    .wallet
                                    .sign_for_pk(pk, msg)
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

                        network_client
                            .submit_signed_forfeit_txs(signed_forfeit_psbts, commitment_psbt)
                            .await?;

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

#[derive(Debug)]
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

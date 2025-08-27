use crate::error::ErrorContext;
use crate::utils::sleep;
use crate::utils::timeout_op;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use crate::ExplorerUtxo;
use ark_core::batch;
use ark_core::batch::create_and_sign_forfeit_txs;
use ark_core::batch::generate_nonce_tree;
use ark_core::batch::sign_batch_tree;
use ark_core::batch::sign_commitment_psbt;
use ark_core::batch::NonceKps;
use ark_core::proof_of_funds;
use ark_core::server::BatchTreeEventType;
use ark_core::server::StreamEvent;
use ark_core::ArkAddress;
use ark_core::TxGraph;
use backon::ExponentialBuilder;
use backon::Retryable;
use bitcoin::hashes::sha256;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::key::Keypair;
use bitcoin::secp256k1;
use bitcoin::secp256k1::schnorr;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::TxOut;
use bitcoin::Txid;
use bitcoin::XOnlyPublicKey;
use futures::StreamExt;
use jiff::Timestamp;
use rand::CryptoRng;
use rand::Rng;
use std::collections::HashMap;

impl<B, W> Client<B, W>
where
    B: Blockchain,
    W: BoardingWallet + OnchainWallet,
{
    /// Settle _all_ prior VTXOs and boarding outputs into the next batch, generating new confirmed
    /// VTXOs.
    pub async fn settle<R>(
        &self,
        rng: &mut R,
        select_recoverable_vtxos: bool,
    ) -> Result<Option<Txid>, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        // Get off-chain address and send all funds to this address, no change output 🦄
        let (to_address, _) = self.get_offchain_address()?;

        let (boarding_inputs, vtxo_inputs, total_amount) = self
            .fetch_commitment_transaction_inputs(select_recoverable_vtxos)
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
        select_recoverable_vtxos: bool,
    ) -> Result<Txid, Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        let (change_address, _) = self.get_offchain_address()?;

        let (boarding_inputs, vtxo_inputs, total_amount) = self
            .fetch_commitment_transaction_inputs(select_recoverable_vtxos)
            .await?;

        let change_amount = total_amount.checked_sub(to_amount).ok_or_else(|| {
            Error::coin_select("cannot afford to send {to_amount}, only have {total_amount}")
        })?;

        tracing::info!(
            %to_address,
            %to_amount,
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
            // TODO: Use `when` to only retry certain errors.
            .notify(|err: &Error, dur: std::time::Duration| {
                tracing::warn!("Retrying joining next batch after {dur:?}. Error: {err}");
            })
            .await
            .context("Failed to join batch")?;

        tracing::info!(%commitment_txid, "Collaborative redeem success");

        Ok(commitment_txid)
    }

    /// Get all the [`batch::OnChainInput`]s and [`batch::VtxoInput`]s that can be used to join an
    /// upcoming batch.
    async fn fetch_commitment_transaction_inputs(
        &self,
        select_recoverable_vtxos: bool,
    ) -> Result<(Vec<batch::OnChainInput>, Vec<batch::VtxoInput>, Amount), Error> {
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

        let spendable_vtxos = self.spendable_vtxos(select_recoverable_vtxos).await?;

        for (virtual_tx_outpoints, _) in spendable_vtxos.iter() {
            total_amount += virtual_tx_outpoints
                .iter()
                .fold(Amount::ZERO, |acc, vtxo| acc + vtxo.amount)
        }

        let vtxo_inputs = spendable_vtxos
            .into_iter()
            .flat_map(|(virtual_tx_outpoints, vtxo)| {
                virtual_tx_outpoints
                    .into_iter()
                    .map(|virtual_tx_outpoint| {
                        batch::VtxoInput::new(
                            vtxo.clone(),
                            virtual_tx_outpoint.amount,
                            virtual_tx_outpoint.outpoint,
                            virtual_tx_outpoint.is_recoverable(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        Ok((boarding_inputs, vtxo_inputs, total_amount))
    }

    async fn join_next_batch<R>(
        &self,
        rng: &mut R,
        onchain_inputs: Vec<batch::OnChainInput>,
        vtxo_inputs: Vec<batch::VtxoInput>,
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
                proof_of_funds::Input::new(
                    o.outpoint(),
                    o.boarding_output().exit_delay(),
                    TxOut {
                        value: o.amount(),
                        script_pubkey: o.boarding_output().script_pubkey(),
                    },
                    o.boarding_output().tapscripts(),
                    o.boarding_output().owner_pk(),
                    o.boarding_output().exit_spend_info(),
                    true,
                )
            });

            let vtxo_inputs = vtxo_inputs.clone().into_iter().map(|v| {
                proof_of_funds::Input::new(
                    v.outpoint(),
                    v.vtxo().exit_delay(),
                    TxOut {
                        value: v.amount(),
                        script_pubkey: v.vtxo().script_pubkey(),
                    },
                    v.vtxo().tapscripts(),
                    v.vtxo().owner_pk(),
                    v.vtxo().exit_spend_info(),
                    false,
                )
            });

            boarding_inputs.chain(vtxo_inputs).collect::<Vec<_>>()
        };

        let mut outputs = vec![];

        match output_type {
            BatchOutputType::Board {
                to_address,
                to_amount,
            } => outputs.push(proof_of_funds::Output::Offchain(TxOut {
                value: to_amount,
                script_pubkey: to_address.to_p2tr_script_pubkey(),
            })),
            BatchOutputType::OffBoard {
                to_address,
                to_amount,
                change_address,
                change_amount,
            } => {
                outputs.push(proof_of_funds::Output::Onchain(TxOut {
                    value: to_amount,
                    script_pubkey: to_address.script_pubkey(),
                }));
                if change_amount > self.server_info.dust {
                    outputs.push(proof_of_funds::Output::Offchain(TxOut {
                        value: change_amount,
                        script_pubkey: change_address.to_p2tr_script_pubkey(),
                    }));
                }
            }
        }

        let mut step = Step::Start;

        let own_cosigner_kps = [own_cosigner_kp];
        let own_cosigner_pks = own_cosigner_kps
            .iter()
            .map(|k| k.public_key())
            .collect::<Vec<_>>();

        let sign_for_onchain_pk_fn = |pk: &XOnlyPublicKey,
                                      msg: &secp256k1::Message|
         -> Result<schnorr::Signature, ark_core::Error> {
            self.inner
                .wallet
                .sign_for_pk(pk, msg)
                .map_err(|e| ark_core::Error::ad_hoc(e.to_string()))
        };

        let (bip322_proof, intent_message) = proof_of_funds::make_bip322_signature(
            &[*self.kp()],
            sign_for_onchain_pk_fn,
            inputs,
            outputs.clone(),
            own_cosigner_pks.clone(),
        )?;

        let intent_id = timeout_op(
            self.inner.timeout,
            self.network_client()
                .register_intent(&intent_message, &bip322_proof),
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

        let (ark_server_pk, _) = server_info.pk.x_only_public_key();

        let mut unsigned_commitment_tx = None;

        let mut vtxo_graph_chunks = Some(Vec::new());
        let mut vtxo_graph: Option<TxGraph> = None;

        let mut connectors_graph_chunks = Some(Vec::new());

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
                                .any(|o| matches!(o, proof_of_funds::Output::Offchain(_)))
                            {
                                true => Step::BatchStarted,
                                false => Step::BatchSigningNoncesGenerated,
                            };
                        } else {
                            tracing::debug!(
                                batch_id = e.id,
                                intent_id,
                                "Intent ID not found for batch"
                            );
                        }
                    }
                    StreamEvent::TreeTx(e) => {
                        if step != Step::BatchStarted && step != Step::BatchSigningNoncesGenerated {
                            continue;
                        }

                        match e.batch_tree_event_type {
                            BatchTreeEventType::Vtxo => {
                                match &mut vtxo_graph_chunks {
                                    Some(vtxo_graph_chunks) => {
                                        vtxo_graph_chunks.push(e.tx_graph_chunk)
                                    }
                                    None => {
                                        return Err(Error::ark_server(
                                            "received unexpected VTXO graph chunk",
                                        ))
                                    }
                                };
                            }
                            BatchTreeEventType::Connector => {
                                match connectors_graph_chunks {
                                    Some(ref mut connectors_graph_chunks) => {
                                        connectors_graph_chunks.push(e.tx_graph_chunk)
                                    }
                                    None => {
                                        return Err(Error::ark_server(
                                            "received unexpected connectors graph chunk",
                                        ))
                                    }
                                };
                            }
                        }
                    }
                    StreamEvent::TreeSignature(e) => {
                        if step != Step::BatchSigningNoncesGenerated {
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
                        continue;
                    }
                    StreamEvent::TreeNoncesAggregated(e) => {
                        if step != Step::BatchSigningStarted {
                            continue;
                        }

                        let agg_pub_nonce_tree = e.tree_nonces;

                        tracing::debug!(
                            batch_id = e.id,
                            ?agg_pub_nonce_tree,
                            "Batch combined nonces generated"
                        );

                        let our_nonce_trees = our_nonce_trees.take().ok_or(Error::ark_server(
                            "missing nonce tree during batch protocol",
                        ))?;

                        let vtxo_graph = match vtxo_graph {
                            Some(ref vtxo_graph) => vtxo_graph,
                            None => {
                                let chunks = vtxo_graph_chunks.take().ok_or(Error::ark_server(
                                    "received tree nonces aggregated event without VTXO graph chunks",
                                ))?;

                                &TxGraph::new(chunks)
                                    .map_err(Error::from)
                                    .context("failed to build VTXO graph before tree signing")?
                            }
                        };

                        let unsigned_commitment_tx = unsigned_commitment_tx
                            .as_ref()
                            .ok_or_else(|| Error::ad_hoc("missing commitment TX"))?;

                        for (cosigner_kp, our_nonce_tree) in our_nonce_trees {
                            let partial_sig_tree = sign_batch_tree(
                                server_info.vtxo_tree_expiry,
                                ark_server_pk,
                                &cosigner_kp,
                                vtxo_graph,
                                unsigned_commitment_tx,
                                our_nonce_tree,
                                &agg_pub_nonce_tree,
                            )
                            .map_err(Error::from)
                            .context("failed to sign VTXO tree")?;

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

                        step = step.next();
                    }
                    StreamEvent::BatchFinalization(e) => {
                        if step != Step::BatchSigningNoncesGenerated {
                            continue;
                        }

                        let signed_forfeit_psbts = if !vtxo_inputs.is_empty() {
                            let chunks =
                                connectors_graph_chunks.take().ok_or(Error::ark_server(
                                    "received batch finalization event without connectors",
                                ))?;

                            let connectors_graph =
                                TxGraph::new(chunks).map_err(Error::from).context(
                                    "failed to build connectors graph before signing forfeit TXs",
                                )?;

                            tracing::debug!(batch_id = e.id, "Batch finalization started");

                            create_and_sign_forfeit_txs(
                                vtxo_inputs.as_slice(),
                                &connectors_graph.leaves(),
                                &server_info.forfeit_address,
                                server_info.dust,
                                |msg, _vtxo| {
                                    let sig = self.secp().sign_schnorr_no_aux_rand(msg, self.kp());
                                    let pk = self.kp().x_only_public_key().0;
                                    (sig, pk)
                                },
                            )
                            .map_err(Error::from)?
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
                        if step != Step::BatchFinalization {
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

                        continue;
                    }
                },
                Some(Err(e)) => {
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
            BatchSigningNoncesGenerated,
            BatchFinalization,
            Finalized,
        }

        impl Step {
            fn next(&self) -> Step {
                match self {
                    Step::Start => Step::BatchStarted,
                    Step::BatchStarted => Step::BatchSigningStarted,
                    Step::BatchSigningStarted => Step::BatchSigningNoncesGenerated,
                    Step::BatchSigningNoncesGenerated => Step::BatchFinalization,
                    Step::BatchFinalization => Step::Finalized,
                    Step::Finalized => Step::Finalized, // we can't go further
                }
            }
        }
    }
}

enum BatchOutputType {
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

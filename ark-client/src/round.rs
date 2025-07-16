use crate::error::ErrorContext;
use crate::utils::sleep;
use crate::wallet::BoardingWallet;
use crate::wallet::OnchainWallet;
use crate::Blockchain;
use crate::Client;
use crate::Error;
use crate::ExplorerUtxo;
use ark_core::proof_of_funds;
use ark_core::round;
use ark_core::round::create_and_sign_forfeit_txs;
use ark_core::round::generate_nonce_tree;
use ark_core::round::sign_round_psbt;
use ark_core::round::sign_vtxo_tree;
use ark_core::round::NonceKps;
use ark_core::server::BatchTreeEventType;
use ark_core::server::RoundStreamEvent;
use ark_core::ArkAddress;
use ark_core::ArkNote;
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
    /// Lift all pending VTXOs and boarding outputs into the Ark, converting them into new,
    /// confirmed VTXOs. We do this by "joining the next round".
    pub async fn board<R>(&self, rng: &mut R, select_recoverable_vtxos: bool) -> Result<(), Error>
    where
        R: Rng + CryptoRng + Clone,
    {
        // Get off-chain address and send all funds to this address, no change output 🦄
        let (to_address, _) = self.get_offchain_address()?;

        let (boarding_inputs, vtxo_inputs, total_amount) = self
            .fetch_round_transaction_inputs(select_recoverable_vtxos)
            .await?;

        tracing::debug!(
            offchain_adress = %to_address.encode(),
            ?boarding_inputs,
            ?vtxo_inputs,
            "Attempting to board the ark"
        );

        if boarding_inputs.is_empty() && vtxo_inputs.is_empty() {
            tracing::debug!("No transactions to board");
            return Ok(());
        }

        let join_next_ark_round = || async {
            self.join_next_ark_round(
                &mut rng.clone(),
                boarding_inputs.clone(),
                vtxo_inputs.clone(),
                &[],
                RoundOutputType::Board {
                    to_address,
                    to_amount: total_amount,
                },
            )
            .await
        };

        // Joining a round can fail depending on the timing, so we try a few times.
        let txid = join_next_ark_round
            .retry(ExponentialBuilder::default().with_max_times(0))
            .sleep(sleep)
            // TODO: Use `when` to only retry certain errors.
            .notify(|err: &Error, dur: std::time::Duration| {
                tracing::warn!("Retrying joining next Ark round after {dur:?}. Error: {err}",);
            })
            .await
            .context("Failed to join round")?;

        tracing::info!(%txid, "Boarding success");

        Ok(())
    }

    // In go client: CollaborativeRedeem.
    pub async fn off_board<R>(
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
            .fetch_round_transaction_inputs(select_recoverable_vtxos)
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
            "Attempting to off-board the ark"
        );

        let join_next_ark_round = || async {
            self.join_next_ark_round(
                &mut rng.clone(),
                boarding_inputs.clone(),
                vtxo_inputs.clone(),
                &vec![],
                RoundOutputType::OffBoard {
                    to_address: to_address.clone(),
                    to_amount,
                    change_address,
                    change_amount,
                },
            )
            .await
        };

        // Joining a round can fail depending on the timing, so we try a few times.
        let txid = join_next_ark_round
            .retry(ExponentialBuilder::default().with_max_times(3))
            .sleep(sleep)
            // TODO: Use `when` to only retry certain errors.
            .notify(|err: &Error, dur: std::time::Duration| {
                tracing::warn!("Retrying joining next Ark round after {dur:?}. Error: {err}");
            })
            .await
            .context("Failed to join round")?;

        tracing::info!(%txid, "Off-boarding success");

        Ok(txid)
    }

    /// Get all the [`round::OnChainInput`]s and [`round::VtxoInput`]s that can be used to join an
    /// upcoming round.
    pub async fn fetch_round_transaction_inputs(
        &self,
        select_recoverable_vtxos: bool,
    ) -> Result<(Vec<round::OnChainInput>, Vec<round::VtxoInput>, Amount), Error> {
        // Get all known boarding outputs.
        let boarding_outputs = self.inner.wallet.get_boarding_outputs()?;

        let mut boarding_inputs: Vec<round::OnChainInput> = Vec::new();
        let mut total_amount = Amount::ZERO;

        // To track unique outpoints and prevent duplicates
        let mut seen_outpoints = std::collections::HashSet::new();

        let now = Timestamp::now();

        // Find outpoints for each boarding output.
        for boarding_output in boarding_outputs {
            let outpoints = self
                .blockchain()
                .find_outpoints(boarding_output.address())
                .await?;

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

                        boarding_inputs.push(round::OnChainInput::new(
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

        for (vtxo_outpoints, _) in spendable_vtxos.iter() {
            total_amount += vtxo_outpoints
                .iter()
                .fold(Amount::ZERO, |acc, vtxo| acc + vtxo.amount)
        }

        let vtxo_inputs = spendable_vtxos
            .into_iter()
            .flat_map(|(vtxo_outpoints, vtxo)| {
                vtxo_outpoints
                    .into_iter()
                    .map(|vtxo_outpoint| {
                        round::VtxoInput::new(
                            vtxo.clone(),
                            vtxo_outpoint.amount,
                            vtxo_outpoint.outpoint,
                            vtxo_outpoint.is_recoverable(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();

        Ok((boarding_inputs, vtxo_inputs, total_amount))
    }

    pub async fn join_next_ark_round<R>(
        &self,
        rng: &mut R,
        // Boarding inputs
        onchain_inputs: Vec<round::OnChainInput>,
        // VTXO inputs
        vtxo_inputs: Vec<round::VtxoInput>,
        // Arknote inputs
        notes: &[ArkNote],
        output_type: RoundOutputType,
    ) -> Result<Txid, Error>
    where
        R: Rng + CryptoRng,
    {
        if onchain_inputs.is_empty() && vtxo_inputs.is_empty() && notes.is_empty() {
            return Err(Error::ad_hoc("cannot join round without inputs"));
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

            let notes = notes
                .into_iter()
                .map(|n| n.into())
                .collect::<Vec<proof_of_funds::Input>>();

            boarding_inputs
                .chain(vtxo_inputs)
                .chain(notes)
                .collect::<Vec<_>>()
        };

        let mut outputs = vec![];

        match output_type {
            RoundOutputType::Board {
                to_address,
                to_amount,
            } => outputs.push(proof_of_funds::Output::Offchain(TxOut {
                value: to_amount,
                script_pubkey: to_address.to_p2tr_script_pubkey(),
            })),
            RoundOutputType::OffBoard {
                to_address,
                to_amount,
                change_address,
                change_amount,
            } => {
                outputs.push(proof_of_funds::Output::Onchain(TxOut {
                    value: to_amount,
                    script_pubkey: to_address.script_pubkey(),
                }));
                outputs.push(proof_of_funds::Output::Offchain(TxOut {
                    value: change_amount,
                    script_pubkey: change_address.to_p2tr_script_pubkey(),
                }));
            }
        }

        let mut step = RoundStep::Start;

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
            self.kp(),
            sign_for_onchain_pk_fn,
            inputs,
            outputs.clone(),
            own_cosigner_pks.clone(),
        )?;

        let intent_id = self
            .network_client()
            .register_intent(&intent_message, &bip322_proof)
            .await?;

        tracing::debug!(
            intent_id,
            ?onchain_input_outpoints,
            ?vtxo_input_outpoints,
            ?outputs,
            "Registered intent for round"
        );

        let network_client = self.network_client();

        let mut round_id: Option<String> = None;

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

        let mut unsigned_round_tx = None;

        let mut vtxo_graph_chunks = Some(Vec::new());
        let mut vtxo_graph: Option<TxGraph> = None;

        let mut connectors_graph_chunks = Some(Vec::new());

        let mut our_nonce_trees: Option<HashMap<Keypair, NonceKps>> = None;
        loop {
            match stream.next().await {
                Some(Ok(event)) => match event {
                    RoundStreamEvent::BatchStarted(e) => {
                        if step != RoundStep::Start {
                            continue;
                        }

                        let hash = sha256::Hash::hash(intent_id.as_bytes());
                        let hash = hash.as_byte_array().to_vec().to_lower_hex_string();

                        if e.intent_id_hashes.iter().any(|h| h == &hash) {
                            self.network_client()
                                .confirm_registration(intent_id.clone())
                                .await?;

                            tracing::info!(round_id = e.id, intent_id, "Intent ID found for round");

                            round_id = Some(e.id);

                            // Depending on whether we are generating new VTXOs or not, we continue
                            // with a different step in the state machine.
                            step = match outputs
                                .iter()
                                .any(|o| matches!(o, proof_of_funds::Output::Offchain(_)))
                            {
                                true => RoundStep::BatchStarted,
                                false => RoundStep::RoundSigningNoncesGenerated,
                            };
                        } else {
                            tracing::debug!(
                                round_id = e.id,
                                intent_id,
                                "Intent ID not found for round"
                            );
                        }
                    }
                    RoundStreamEvent::TreeTx(e) => {
                        if step != RoundStep::BatchStarted
                            && step != RoundStep::RoundSigningNoncesGenerated
                        {
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
                    RoundStreamEvent::TreeSignature(e) => {
                        if step != RoundStep::RoundSigningNoncesGenerated {
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
                    RoundStreamEvent::TreeSigningStarted(e) => {
                        if step != RoundStep::BatchStarted {
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

                        tracing::info!(round_id = e.id, "Round signing started");

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
                                &e.unsigned_round_tx,
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

                        unsigned_round_tx = Some(e.unsigned_round_tx);
                        our_nonce_trees = Some(our_nonce_tree_map);

                        step = step.next();
                        continue;
                    }
                    RoundStreamEvent::TreeNoncesAggregated(e) => {
                        if step != RoundStep::RoundSigningStarted {
                            continue;
                        }

                        let agg_pub_nonce_tree = e.tree_nonces;

                        tracing::debug!(
                            round_id = e.id,
                            ?agg_pub_nonce_tree,
                            "Round combined nonces generated"
                        );

                        let our_nonce_trees = our_nonce_trees.take().ok_or(Error::ark_server(
                            "missing nonce tree during round protocol",
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

                        let unsigned_round_tx = unsigned_round_tx
                            .as_ref()
                            .ok_or_else(|| Error::ad_hoc("missing round TX"))?;

                        for (cosigner_kp, our_nonce_tree) in our_nonce_trees {
                            let partial_sig_tree = sign_vtxo_tree(
                                server_info.vtxo_tree_expiry,
                                ark_server_pk,
                                &cosigner_kp,
                                vtxo_graph,
                                unsigned_round_tx,
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
                    RoundStreamEvent::BatchFinalization(e) => {
                        if step != RoundStep::RoundSigningNoncesGenerated {
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

                            tracing::debug!(round_id = e.id, "Round finalization started");

                            create_and_sign_forfeit_txs(
                                self.kp(),
                                vtxo_inputs.as_slice(),
                                &connectors_graph.leaves(),
                                &server_info.forfeit_address,
                                server_info.dust,
                            )
                            .map_err(Error::from)?
                        } else {
                            Vec::new()
                        };

                        let round_psbt = if onchain_inputs.is_empty() {
                            None
                        } else {
                            let mut round_psbt = e.commitment_tx;

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

                            sign_round_psbt(sign_for_pk_fn, &mut round_psbt, &onchain_inputs)
                                .map_err(Error::from)?;

                            Some(round_psbt)
                        };

                        network_client
                            .submit_signed_forfeit_txs(signed_forfeit_psbts, round_psbt)
                            .await?;

                        step = step.next();
                    }
                    RoundStreamEvent::BatchFinalized(e) => {
                        if step != RoundStep::RoundFinalization {
                            continue;
                        }

                        let round_txid = e.commitment_txid;

                        tracing::info!(round_id = e.id, %round_txid, "Round finalized");

                        return Ok(round_txid);
                    }
                    RoundStreamEvent::BatchFailed(ref e) => {
                        if Some(&e.id) == round_id.as_ref() {
                            return Err(Error::ark_server(format!(
                                "batch failed {}: {}",
                                e.id, e.reason
                            )));
                        }

                        tracing::debug!("Unrelated round failed: {e:?}");

                        continue;
                    }
                },
                Some(Err(e)) => {
                    return Err(Error::ark_server(e));
                }
                None => {
                    return Err(Error::ark_server("dropped round event stream"));
                }
            }
        }

        #[derive(Debug, PartialEq, Eq)]
        enum RoundStep {
            Start,
            BatchStarted,
            RoundSigningStarted,
            RoundSigningNoncesGenerated,
            RoundFinalization,
            Finalized,
        }

        impl RoundStep {
            fn next(&self) -> RoundStep {
                match self {
                    RoundStep::Start => RoundStep::BatchStarted,
                    RoundStep::BatchStarted => RoundStep::RoundSigningStarted,
                    RoundStep::RoundSigningStarted => RoundStep::RoundSigningNoncesGenerated,
                    RoundStep::RoundSigningNoncesGenerated => RoundStep::RoundFinalization,
                    RoundStep::RoundFinalization => RoundStep::Finalized,
                    RoundStep::Finalized => RoundStep::Finalized, // we can't go further
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub enum RoundOutputType {
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

use crate::apis;
use crate::apis::ark_service_api::ark_service_confirm_registration;
use crate::apis::ark_service_api::ark_service_delete_intent;
use crate::apis::ark_service_api::ark_service_finalize_tx;
use crate::apis::ark_service_api::ark_service_get_info;
use crate::apis::ark_service_api::ark_service_register_intent;
use crate::apis::ark_service_api::ark_service_submit_signed_forfeit_txs;
use crate::apis::ark_service_api::ark_service_submit_tree_nonces;
use crate::apis::ark_service_api::ark_service_submit_tree_signatures;
use crate::apis::ark_service_api::ark_service_submit_tx;
use crate::apis::indexer_service_api::indexer_service_get_virtual_txs;
use crate::apis::indexer_service_api::indexer_service_get_vtxos;
use crate::apis::indexer_service_api::indexer_service_subscribe_for_scripts;
use crate::apis::indexer_service_api::indexer_service_unsubscribe_for_scripts;
use crate::models;
use crate::models::{
    ConfirmRegistrationRequest, Intent, SubmitSignedForfeitTxsRequest, SubmitTreeNoncesRequest,
    SubmitTreeSignaturesRequest, SubscribeForScriptsRequest, UnsubscribeForScriptsRequest,
};
use crate::Error;
use ark_core::server::FinalizeOffchainTxResponse;
use ark_core::server::GetVtxosRequest;
use ark_core::server::GetVtxosRequestFilter;
use ark_core::server::GetVtxosRequestReference;
use ark_core::server::IndexerPage;
use ark_core::server::ListVtxo;
use ark_core::server::NoncePks;
use ark_core::server::PartialSigTree;
use ark_core::server::StreamEvent;
use ark_core::server::SubmitOffchainTxResponse;
use ark_core::server::SubscriptionResponse;
use ark_core::server::VirtualTxOutPoint;
use ark_core::server::VirtualTxsResponse;
use ark_core::ArkAddress;
use bitcoin::base64;
use bitcoin::base64::Engine;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Psbt;
use bitcoin::Txid;
use futures::stream;
use futures::Stream;
use futures::StreamExt;

pub struct Client {
    configuration: apis::configuration::Configuration,
}

impl Client {
    pub fn new(ark_server_url: String) -> Self {
        let configuration = apis::configuration::Configuration {
            base_path: ark_server_url,
            ..Default::default()
        };

        Self { configuration }
    }

    pub async fn get_info(&self) -> Result<ark_core::server::Info, Error> {
        let info = ark_service_get_info(&self.configuration)
            .await
            .map_err(Error::request)?;

        let info = info.try_into()?;

        Ok(info)
    }

    pub async fn submit_offchain_transaction_request(
        &self,
        ark_tx: Psbt,
        checkpoint_txs: Vec<Psbt>,
    ) -> Result<SubmitOffchainTxResponse, Error> {
        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let ark_tx = base64.encode(ark_tx.serialize());

        let checkpoint_txs = checkpoint_txs
            .into_iter()
            .map(|tx| Some(base64.encode(tx.serialize())))
            .collect();

        let res = ark_service_submit_tx(
            &self.configuration,
            models::SubmitTxRequest {
                signed_ark_tx: Some(ark_tx),
                checkpoint_txs,
            },
        )
        .await
        .map_err(Error::request)?;

        let signed_ark_tx = res.final_ark_tx;
        let signed_ark_tx = signed_ark_tx.ok_or(Error::request("Signed ark tx not received"))?;

        let signed_ark_tx = base64.decode(signed_ark_tx).map_err(Error::conversion)?;
        let signed_ark_tx = Psbt::deserialize(&signed_ark_tx).map_err(Error::conversion)?;

        let signed_checkpoint_txs = res
            .signed_checkpoint_txs
            .ok_or(Error::request("Signed checkpoint tx not received"))?
            .into_iter()
            .map(|tx| {
                let tx = base64.decode(tx).map_err(Error::conversion)?;
                let tx = Psbt::deserialize(&tx).map_err(Error::conversion)?;

                Ok(tx)
            })
            .collect::<Result<Vec<_>, Error>>()?;

        Ok(SubmitOffchainTxResponse {
            signed_ark_tx,
            signed_checkpoint_txs,
        })
    }

    pub async fn finalize_offchain_transaction(
        &self,
        txid: Txid,
        checkpoint_txs: Vec<Psbt>,
    ) -> Result<FinalizeOffchainTxResponse, Error> {
        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let checkpoint_txs = checkpoint_txs
            .into_iter()
            .map(|tx| Some(base64.encode(tx.serialize())))
            .collect();

        ark_service_finalize_tx(
            &self.configuration,
            models::FinalizeTxRequest {
                ark_txid: Some(txid.to_string()),
                final_checkpoint_txs: checkpoint_txs,
            },
        )
        .await
        .map_err(Error::request)?;

        Ok(FinalizeOffchainTxResponse {})
    }

    pub async fn list_vtxos(&self, request: GetVtxosRequest) -> Result<ListVtxo, Error> {
        let reference = request.reference();
        let filter = request.filter();

        let (scripts, outpoints) = match reference {
            GetVtxosRequestReference::Scripts(s) => (
                Some(s.iter().map(|s| s.to_hex_string()).clone().collect()),
                None,
            ),
            GetVtxosRequestReference::OutPoints(o) => {
                (None, Some(o.iter().map(|o| o.to_string()).collect()))
            }
        };
        let (spendable_only, spent_only, recoverable_only) = match filter {
            None => (Some(false), Some(false), Some(false)),
            Some(filter) => match filter {
                GetVtxosRequestFilter::Spendable => (Some(true), Some(false), Some(false)),
                GetVtxosRequestFilter::Spent => (Some(false), Some(true), Some(false)),
                GetVtxosRequestFilter::Recoverable => (Some(false), Some(false), Some(true)),
            },
        };

        let page_period_size: Option<i32> = None;
        let page_period_index: Option<i32> = None;

        let response = indexer_service_get_vtxos(
            &self.configuration,
            scripts,
            outpoints,
            spendable_only,
            spent_only,
            recoverable_only,
            page_period_size,
            page_period_index,
        )
        .await
        .map_err(Error::request)?;

        let vtxos = response.vtxos.ok_or(Error::request("VTXOs not received"))?;

        let mut spent = vtxos
            .iter()
            .filter_map(
                |vtxo| match (vtxo.is_spent, vtxo.is_unrolled, vtxo.is_swept) {
                    (Some(true), _, _) | (_, Some(true), _) | (_, _, Some(true)) => {
                        Some(VirtualTxOutPoint::try_from(vtxo.clone()))
                    }
                    _ => None,
                },
            )
            .collect::<Result<Vec<_>, crate::conversions::ConversionError>>()?;

        let mut spendable = vtxos
            .iter()
            .filter_map(
                |vtxo| match (vtxo.is_unrolled, vtxo.is_spent, vtxo.is_swept) {
                    (Some(false) | None, Some(false) | None, Some(false) | None) => {
                        Some(VirtualTxOutPoint::try_from(vtxo.clone()))
                    }
                    _ => None,
                },
            )
            .collect::<Result<Vec<_>, crate::conversions::ConversionError>>()?;

        let mut spent_by_redeem = Vec::new();
        for spendable_vtxo in spendable.clone() {
            let was_spent_by_redeem = spendable.iter().any(|v| v.is_unrolled);

            if was_spent_by_redeem {
                spent_by_redeem.push(spendable_vtxo);
            }
        }

        // FIXME: Maybe this is no longer necessary (copied from ark-grpc)

        // Remove "spendable" VTXOs that were actually already spent by an Ark transaction from the
        // list of spendable VTXOs.
        spendable.retain(|i| !spent_by_redeem.contains(i));

        // Add them to the list of spent VTXOs.
        spent.append(&mut spent_by_redeem);

        Ok(ListVtxo::new(spent, spendable))
    }

    pub async fn register_intent(
        &self,
        intent_message: &ark_core::intent::IntentMessage,
        proof: &Psbt,
    ) -> Result<String, Error> {
        let message = intent_message.encode().map_err(Error::conversion)?;
        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let bytes = proof.serialize();

        let proof = base64.encode(&bytes);

        let response = ark_service_register_intent(
            &self.configuration,
            models::RegisterIntentRequest {
                intent: Some(Intent {
                    proof: Some(proof),
                    message: Some(message),
                }),
            },
        )
        .await
        .map_err(Error::request)?;
        let intent_id = response
            .intent_id
            .ok_or(Error::request("Could not get intent id"))?;

        Ok(intent_id)
    }

    pub async fn delete_intent(
        &self,
        intent_message: &ark_core::intent::IntentMessage,
        proof: &Psbt,
    ) -> Result<(), Error> {
        let message = intent_message.encode().map_err(Error::conversion)?;
        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let bytes = proof.serialize();

        let proof = base64.encode(&bytes);
        ark_service_delete_intent(
            &self.configuration,
            models::DeleteIntentRequest {
                intent: Some(Intent {
                    proof: Some(proof),
                    message: Some(message),
                }),
            },
        )
        .await
        .map_err(Error::request)?;

        Ok(())
    }

    pub async fn get_event_stream(
        &self,
        topics: Vec<String>,
    ) -> Result<impl Stream<Item = Result<StreamEvent, Error>> + Unpin, Error> {
        // Build the URL with query parameters
        let mut url = format!("{}/v1/batch/events", self.configuration.base_path);
        if !topics.is_empty() {
            let query_params: Vec<String> = topics
                .iter()
                .map(|topic| format!("topics={}", urlencoding::encode(topic)))
                .collect();
            url = format!("{}?{}", url, query_params.join("&"));
        }

        // Create the request for SSE
        let client = &self.configuration.client;
        let request = client
            .get(&url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(Error::request)?;

        // Check if the request was successful
        if !request.status().is_success() {
            return Err(Error::request(format!(
                "Event stream request failed with status: {}",
                request.status()
            )));
        }

        // Convert the response into a byte stream using async chunks
        let byte_stream = request.bytes_stream();

        // Create the SSE event stream
        let stream = stream::unfold(byte_stream, |mut byte_stream| async move {
            match byte_stream.next().await {
                Some(chunk_result) => {
                    let result = match chunk_result {
                        Ok(bytes) => {
                            let event = String::from_utf8(bytes.to_vec());
                            match event {
                                Ok(event) => {
                                    if let Ok(response) =
                                        serde_json::from_str::<models::GetEventStreamResponse>(
                                            &event,
                                        )
                                    {
                                        match StreamEvent::try_from(response) {
                                            Ok(stream_event) => Ok(stream_event),
                                            Err(e) => Err(Error::conversion(e)),
                                        }
                                    } else {
                                        // Handle parse error
                                        Err(Error::conversion("Failed to parse JSON"))
                                    }
                                }
                                Err(error) => Err(Error::conversion(error)),
                            }
                        }
                        Err(e) => Err(Error::request(e)),
                    };
                    Some((result, byte_stream))
                }
                None => None,
            }
        });

        Ok(Box::pin(stream))
    }
    pub async fn confirm_registration(&self, intent_id: String) -> Result<(), Error> {
        ark_service_confirm_registration(
            &self.configuration,
            ConfirmRegistrationRequest {
                intent_id: Some(intent_id),
            },
        )
        .await
        .map_err(Error::request)?;

        Ok(())
    }

    pub async fn submit_tree_nonces(
        &self,
        batch_id: &str,
        cosigner_pubkey: PublicKey,
        pub_nonce_tree: NoncePks,
    ) -> Result<(), Error> {
        let tree_nonces = pub_nonce_tree.encode();

        ark_service_submit_tree_nonces(
            &self.configuration,
            SubmitTreeNoncesRequest {
                batch_id: Some(batch_id.to_string()),
                pubkey: Some(cosigner_pubkey.to_string()),
                tree_nonces: Some(tree_nonces),
            },
        )
        .await
        .map_err(Error::request)?;

        Ok(())
    }

    pub async fn submit_tree_signatures(
        &self,
        batch_id: &str,
        cosigner_pk: PublicKey,
        partial_sig_tree: PartialSigTree,
    ) -> Result<(), Error> {
        let tree_signatures = partial_sig_tree.encode();

        ark_service_submit_tree_signatures(
            &self.configuration,
            SubmitTreeSignaturesRequest {
                batch_id: Some(batch_id.to_string()),
                pubkey: Some(cosigner_pk.to_string()),
                tree_signatures: Some(tree_signatures),
            },
        )
        .await
        .map_err(Error::request)?;

        Ok(())
    }

    pub async fn submit_signed_forfeit_txs(
        &self,
        signed_forfeit_txs: Vec<Psbt>,
        signed_commitment_tx: Option<Psbt>,
    ) -> Result<(), Error> {
        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let signed_commitment_tx = signed_commitment_tx
            .map(|tx| base64.encode(tx.serialize()))
            .unwrap_or_default();

        ark_service_submit_signed_forfeit_txs(
            &self.configuration,
            SubmitSignedForfeitTxsRequest {
                signed_forfeit_txs: signed_forfeit_txs
                    .iter()
                    .map(|psbt| Some(base64.encode(psbt.serialize())))
                    .collect(),
                signed_commitment_tx: Some(signed_commitment_tx),
            },
        )
        .await
        .map_err(Error::request)?;

        Ok(())
    }

    /// Allows to subscribe for tx notifications related to the provided
    /// vtxo scripts.
    ///
    /// It can also be used to update an existing subscriptions by adding
    /// new scripts to it.
    ///
    /// Note: for new subscriptions, don't provide a `subscription_id`
    ///
    /// Returns the subscription id if successful
    pub async fn subscribe_to_scripts(
        &self,
        scripts: Vec<ArkAddress>,
        subscription_id: Option<String>,
    ) -> Result<String, Error> {
        let scripts = scripts
            .iter()
            .map(|address| address.to_p2tr_script_pubkey().to_hex_string())
            .collect::<Vec<_>>();

        // For new subscription we expect empty string ("") here
        let subscription_id = subscription_id.unwrap_or_default();

        let response = indexer_service_subscribe_for_scripts(
            &self.configuration,
            SubscribeForScriptsRequest {
                scripts: Some(scripts),
                subscription_id: Some(subscription_id),
            },
        )
        .await
        .map_err(Error::request)?;

        let subscription_id = response
            .subscription_id
            .ok_or(Error::request("No subscription id"))?;

        Ok(subscription_id)
    }

    /// Allows to remove scripts from an existing subscription.
    pub async fn unsubscribe_from_scripts(
        &self,
        scripts: Vec<ArkAddress>,
        subscription_id: String,
    ) -> Result<(), Error> {
        let scripts = scripts
            .iter()
            .map(|address| address.to_p2tr_script_pubkey().to_hex_string())
            .collect::<Vec<_>>();

        let _ = indexer_service_unsubscribe_for_scripts(
            &self.configuration,
            UnsubscribeForScriptsRequest {
                subscription_id: Some(subscription_id),
                scripts: Some(scripts),
            },
        )
        .await
        .map_err(Error::request)?;

        Ok(())
    }

    pub async fn get_subscription(
        &self,
        subscription_id: String,
    ) -> Result<impl Stream<Item = Result<SubscriptionResponse, Error>> + Unpin, Error> {
        // Build the URL with subscription_id parameter
        let url = format!(
            "{}/v1/script/subscription/{subscription_id}",
            self.configuration.base_path,
        );

        // Create the request for SSE
        let client = &self.configuration.client;
        let request = client
            .get(&url)
            .header("Accept", "text/event-stream")
            .send()
            .await
            .map_err(Error::request)?;

        // Check if the request was successful
        if !request.status().is_success() {
            return Err(Error::request(format!(
                "Subscription stream request failed with status: {}",
                request.status()
            )));
        }

        // Convert the response into a byte stream using async chunks
        let byte_stream = request.bytes_stream();

        // Create the SSE event stream
        let stream = stream::unfold(byte_stream, |mut byte_stream| async move {
            loop {
                match byte_stream.next().await {
                    Some(chunk_result) => {
                        let result = match chunk_result {
                            Ok(bytes) => {
                                let event = String::from_utf8(bytes.to_vec());
                                match event {
                                    Ok(event) if !event.trim().is_empty() => {
                                        if let Ok(response) =
                                            serde_json::from_str::<models::GetSubscriptionResponse>(
                                                &event,
                                            )
                                        {
                                            match SubscriptionResponse::try_from(response) {
                                                Ok(subscription_response) => {
                                                    Ok(subscription_response)
                                                }
                                                Err(e) => Err(Error::conversion(e)),
                                            }
                                        } else {
                                            // Handle parse error
                                            Err(Error::conversion("Failed to parse JSON"))
                                        }
                                    }
                                    Ok(_) => {
                                        // Handle empty string case - skip and continue to next
                                        // iteration
                                        tracing::trace!("Ignoring empty response");
                                        continue;
                                    }
                                    Err(error) => Err(Error::conversion(error)),
                                }
                            }
                            Err(e) => Err(Error::request(e)),
                        };
                        return Some((result, byte_stream));
                    }
                    None => return None,
                }
            }
        });

        Ok(Box::pin(stream))
    }

    pub async fn get_virtual_txs(
        &self,
        txids: Vec<String>,
        size_and_index: Option<(i32, i32)>,
    ) -> Result<VirtualTxsResponse, Error> {
        let (size, index) = size_and_index
            .map(|(sz, indx)| (Some(sz), Some(indx)))
            .unwrap_or_default();
        let response = indexer_service_get_virtual_txs(&self.configuration, txids, size, index)
            .await
            .map_err(Error::request)?;

        let base64 = &base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let txs = response
            .txs
            .unwrap_or_default()
            .into_iter()
            .map(|tx| {
                let bytes = base64.decode(&tx).map_err(Error::conversion)?;
                let psbt = Psbt::deserialize(&bytes).map_err(Error::conversion)?;

                Ok(psbt)
            })
            .collect::<Result<Vec<Psbt>, Error>>()?;

        Ok(VirtualTxsResponse {
            txs,
            page: response.page.map(|a| IndexerPage {
                current: a.current.unwrap_or_default(),
                next: a.next.unwrap_or_default(),
                total: a.total.unwrap_or_default(),
            }),
        })
    }
}

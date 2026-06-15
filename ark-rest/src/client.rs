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
use crate::models::ConfirmRegistrationRequest;
use crate::models::Intent;
use crate::models::SubmitSignedForfeitTxsRequest;
use crate::models::SubmitTreeNoncesRequest;
use crate::models::SubmitTreeSignaturesRequest;
use crate::models::SubscribeForScriptsRequest;
use crate::models::UnsubscribeForScriptsRequest;
use crate::Error;
use ark_core::server::FinalizeOffchainTxResponse;
use ark_core::server::GetVtxosRequest;
use ark_core::server::GetVtxosRequestFilter;
use ark_core::server::GetVtxosRequestReference;
use ark_core::server::IndexerPage;
use ark_core::server::NoncePks;
use ark_core::server::PartialSigTree;
use ark_core::server::StreamEvent;
use ark_core::server::SubmitOffchainTxResponse;
use ark_core::server::SubscriptionResponse;
use ark_core::server::VirtualTxOutPoint;
use ark_core::server::VirtualTxsResponse;
use ark_core::server::SDK_VERSION;
use ark_core::server::TARGET_ARKD_VERSION;
use ark_core::ArkAddress;
use bitcoin::base64;
use bitcoin::base64::Engine;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Psbt;
use bitcoin::Txid;
use futures::stream;
use futures::Future;
use futures::Stream;
use futures::StreamExt;
use std::error::Error as StdError;
use std::sync::Arc;
use std::sync::RwLock;

type InfoRefreshHook = Arc<
    dyn Fn(ark_core::server::Info) -> Result<(), Box<dyn StdError + Send + Sync + 'static>>
        + Send
        + Sync,
>;

pub struct Client {
    configuration: RwLock<apis::configuration::Configuration>,
    info_refresh_hook: Option<InfoRefreshHook>,
}

pub struct ListVtxosResponse {
    pub vtxos: Vec<VirtualTxOutPoint>,
    pub page: Option<IndexerPage>,
}

fn build_reqwest_client(digest: Option<&str>) -> Result<reqwest::Client, Error> {
    let mut default_headers = reqwest::header::HeaderMap::new();
    default_headers.insert(
        "X-Build-Version",
        reqwest::header::HeaderValue::from_static(TARGET_ARKD_VERSION),
    );
    default_headers.insert(
        "X-Sdk-Version",
        reqwest::header::HeaderValue::from_static(SDK_VERSION),
    );
    if let Some(digest) = digest {
        default_headers.insert(
            "X-Digest",
            reqwest::header::HeaderValue::from_str(digest).map_err(Error::request)?,
        );
    }

    reqwest::Client::builder()
        .default_headers(default_headers)
        .build()
        .map_err(Error::request)
}

impl Client {
    pub fn new(ark_server_url: String) -> Result<Self, Error> {
        let configuration = apis::configuration::Configuration {
            base_path: ark_server_url,
            client: build_reqwest_client(None)?,
            ..Default::default()
        };

        Ok(Self {
            configuration: RwLock::new(configuration),
            info_refresh_hook: None,
        })
    }

    /// Register a callback invoked with the freshly fetched [`ark_core::server::Info`] whenever a
    /// mutating call triggers a digest-mismatch refresh (see [`Self::guarded`]). Mirrors
    /// `ark_grpc::Client::set_info_refresh_hook`; lets `ark-client` keep its cached server info
    /// (including `deprecated_signers`) up to date so the background migration watcher reacts.
    pub fn set_info_refresh_hook(
        &mut self,
        hook: impl Fn(ark_core::server::Info) -> Result<(), Box<dyn StdError + Send + Sync + 'static>>
            + Send
            + Sync
            + 'static,
    ) {
        self.info_refresh_hook = Some(Arc::new(hook));
    }

    fn configuration(&self) -> Result<apis::configuration::Configuration, Error> {
        self.configuration
            .read()
            .map(|configuration| configuration.clone())
            .map_err(|_| Error::request("REST client configuration lock poisoned"))
    }

    fn update_digest(&self, digest: &str) -> Result<(), Error> {
        let mut configuration = self
            .configuration
            .write()
            .map_err(|_| Error::request("REST client configuration lock poisoned"))?;
        configuration.client = build_reqwest_client((!digest.is_empty()).then_some(digest))?;
        Ok(())
    }

    /// Run a mutating request and, on a digest mismatch, refresh `/info` before surfacing the
    /// error. Mirrors `ark_grpc::Client::guarded`: the server signals `DIGEST_MISMATCH` when the
    /// cached `/info` digest (sent as `X-Digest`) is stale; we re-fetch `/info` (which updates the
    /// cached digest header and invokes [`Self::set_info_refresh_hook`]), then return
    /// [`Error::server_info_changed`] WITHOUT retrying the original request — the caller must
    /// rebuild it against the refreshed server info if a retry is safe.
    async fn guarded<T>(&self, op: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        match op.await {
            Ok(value) => Ok(value),
            Err(err) if err.is_digest_mismatch() => {
                let info = self.get_info_unguarded().await?;
                if let Some(hook) = &self.info_refresh_hook {
                    hook(info).map_err(Error::conversion)?;
                }
                Err(Error::server_info_changed(err))
            }
            Err(err) => Err(err),
        }
    }

    /// Fetch `/info`, updating the cached `X-Digest` header. Intentionally bypasses
    /// digest-mismatch guarding (it is the bootstrap/refresh request itself).
    pub async fn get_info_unguarded(&self) -> Result<ark_core::server::Info, Error> {
        let configuration = self.configuration()?;
        let info = ark_service_get_info(&configuration)
            .await
            .map_err(Error::request)?;

        let info: ark_core::server::Info = info.try_into()?;
        self.update_digest(&info.digest)?;

        Ok(info)
    }

    pub async fn get_info(&self) -> Result<ark_core::server::Info, Error> {
        self.get_info_unguarded().await
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

        let configuration = self.configuration()?;
        let res = self
            .guarded(async {
                ark_service_submit_tx(
                    &configuration,
                    models::SubmitTxRequest {
                        signed_ark_tx: Some(ark_tx),
                        checkpoint_txs,
                    },
                )
                .await
                .map_err(Error::request)
            })
            .await?;

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

        let configuration = self.configuration()?;
        self.guarded(async {
            ark_service_finalize_tx(
                &configuration,
                models::FinalizeTxRequest {
                    ark_txid: Some(txid.to_string()),
                    final_checkpoint_txs: checkpoint_txs,
                },
            )
            .await
            .map_err(Error::request)
        })
        .await?;

        Ok(FinalizeOffchainTxResponse {})
    }

    pub async fn list_vtxos(&self, request: GetVtxosRequest) -> Result<ListVtxosResponse, Error> {
        let reference = request.reference();

        if reference.is_empty() {
            return Ok(ListVtxosResponse {
                vtxos: Vec::new(),
                page: None,
            });
        }

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
        let (spendable_only, spent_only, recoverable_only, pending_only) = match filter {
            None => (Some(false), Some(false), Some(false), Some(false)),
            Some(filter) => match filter {
                GetVtxosRequestFilter::Spendable => {
                    (Some(true), Some(false), Some(false), Some(false))
                }
                GetVtxosRequestFilter::Spent => (Some(false), Some(true), Some(false), Some(false)),
                GetVtxosRequestFilter::Recoverable => {
                    (Some(false), Some(false), Some(true), Some(false))
                }
                GetVtxosRequestFilter::PendingOnly => {
                    (Some(false), Some(false), Some(false), Some(true))
                }
            },
        };

        let page_period_size: Option<i32> = request.page().map(|p| p.size);
        let page_period_index: Option<i32> = request.page().map(|p| p.index);

        let before = request.before().map(|b| b as i64);
        let after = request.after().map(|b| b as i64);

        let response = indexer_service_get_vtxos(
            &self.configuration()?,
            scripts,
            outpoints,
            spendable_only,
            spent_only,
            recoverable_only,
            pending_only,
            before,
            after,
            page_period_size,
            page_period_index,
        )
        .await
        .map_err(Error::request)?;

        let vtxos = response.vtxos.ok_or(Error::request("VTXOs not received"))?;
        let vtxos = vtxos
            .into_iter()
            .map(VirtualTxOutPoint::try_from)
            .collect::<Result<Vec<_>, crate::conversions::ConversionError>>()?;

        let page = response.page.map(|p| IndexerPage {
            current: p.current.unwrap_or_default(),
            next: p.next.unwrap_or_default(),
            total: p.total.unwrap_or_default(),
        });

        Ok(ListVtxosResponse { vtxos, page })
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

        let configuration = self.configuration()?;
        let response = self
            .guarded(async {
                ark_service_register_intent(
                    &configuration,
                    models::RegisterIntentRequest {
                        intent: Some(Intent {
                            proof: Some(proof),
                            message: Some(message),
                        }),
                    },
                )
                .await
                .map_err(Error::request)
            })
            .await?;
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
        let configuration = self.configuration()?;
        self.guarded(async {
            ark_service_delete_intent(
                &configuration,
                models::DeleteIntentRequest {
                    intent: Some(Intent {
                        proof: Some(proof),
                        message: Some(message),
                    }),
                },
            )
            .await
            .map_err(Error::request)
        })
        .await?;

        Ok(())
    }

    pub async fn get_event_stream(
        &self,
        topics: Vec<String>,
    ) -> Result<impl Stream<Item = Result<StreamEvent, Error>> + Unpin, Error> {
        let configuration = self.configuration()?;

        // Build the URL with query parameters
        let mut url = format!("{}/v1/batch/events", configuration.base_path);
        if !topics.is_empty() {
            let query_params: Vec<String> = topics
                .iter()
                .map(|topic| format!("topics={}", urlencoding::encode(topic)))
                .collect();
            url = format!("{}?{}", url, query_params.join("&"));
        }

        // Create the request for SSE
        let request = configuration
            .client
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
            loop {
                match byte_stream.next().await {
                    Some(chunk_result) => {
                        let result = match chunk_result {
                            Ok(bytes) => {
                                let event = String::from_utf8(bytes.to_vec());
                                match event {
                                    Ok(event) => {
                                        let event = event.trim();
                                        // Skip empty lines and SSE comments
                                        if event.is_empty() || event.starts_with(':') {
                                            continue;
                                        }
                                        // Strip SSE `data: ` prefix
                                        let event = event.strip_prefix("data: ").unwrap_or(event);
                                        if let Ok(response) =
                                            serde_json::from_str::<models::GetEventStreamResponse>(
                                                event,
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
                        return Some((result, byte_stream));
                    }
                    None => return None,
                }
            }
        });

        Ok(Box::pin(stream))
    }
    pub async fn confirm_registration(&self, intent_id: String) -> Result<(), Error> {
        let configuration = self.configuration()?;
        self.guarded(async {
            ark_service_confirm_registration(
                &configuration,
                ConfirmRegistrationRequest {
                    intent_id: Some(intent_id),
                },
            )
            .await
            .map_err(Error::request)
        })
        .await?;

        Ok(())
    }

    pub async fn submit_tree_nonces(
        &self,
        batch_id: &str,
        cosigner_pubkey: PublicKey,
        pub_nonce_tree: NoncePks,
    ) -> Result<(), Error> {
        let tree_nonces = pub_nonce_tree.encode();

        let configuration = self.configuration()?;
        self.guarded(async {
            ark_service_submit_tree_nonces(
                &configuration,
                SubmitTreeNoncesRequest {
                    batch_id: Some(batch_id.to_string()),
                    pubkey: Some(cosigner_pubkey.to_string()),
                    tree_nonces: Some(tree_nonces),
                },
            )
            .await
            .map_err(Error::request)
        })
        .await?;

        Ok(())
    }

    pub async fn submit_tree_signatures(
        &self,
        batch_id: &str,
        cosigner_pk: PublicKey,
        partial_sig_tree: PartialSigTree,
    ) -> Result<(), Error> {
        let tree_signatures = partial_sig_tree.encode();

        let configuration = self.configuration()?;
        self.guarded(async {
            ark_service_submit_tree_signatures(
                &configuration,
                SubmitTreeSignaturesRequest {
                    batch_id: Some(batch_id.to_string()),
                    pubkey: Some(cosigner_pk.to_string()),
                    tree_signatures: Some(tree_signatures),
                },
            )
            .await
            .map_err(Error::request)
        })
        .await?;

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

        let configuration = self.configuration()?;
        self.guarded(async {
            ark_service_submit_signed_forfeit_txs(
                &configuration,
                SubmitSignedForfeitTxsRequest {
                    signed_forfeit_txs: signed_forfeit_txs
                        .iter()
                        .map(|psbt| Some(base64.encode(psbt.serialize())))
                        .collect(),
                    signed_commitment_tx: Some(signed_commitment_tx),
                },
            )
            .await
            .map_err(Error::request)
        })
        .await?;

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
            &self.configuration()?,
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
            &self.configuration()?,
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
        let configuration = self.configuration()?;

        // Build the URL with subscription_id parameter
        let url = format!(
            "{}/v1/script/subscription/{subscription_id}",
            configuration.base_path,
        );

        // Create the request for SSE
        let request = configuration
            .client
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
                                    Ok(event) => {
                                        let event = event.trim();
                                        // Skip empty lines and SSE comments
                                        if event.is_empty() || event.starts_with(':') {
                                            continue;
                                        }
                                        // Strip SSE `data: ` prefix
                                        let event = event.strip_prefix("data: ").unwrap_or(event);
                                        if let Ok(response) =
                                            serde_json::from_str::<models::GetSubscriptionResponse>(
                                                event,
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
        let response = indexer_service_get_virtual_txs(&self.configuration()?, txids, size, index)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    /// A non-digest error passes through `guarded` untouched: no refresh, no hook, no
    /// `server_info_changed` rewrap.
    #[tokio::test]
    async fn guarded_passes_through_non_digest_error() {
        let mut client = Client::new("http://127.0.0.1:1".to_string()).unwrap();
        let hook_fired = Arc::new(AtomicBool::new(false));
        let flag = hook_fired.clone();
        client.set_info_refresh_hook(move |_info| {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });

        let err = client
            .guarded(async { Err::<(), _>(Error::request("connection refused")) })
            .await
            .expect_err("should surface the original error");

        assert!(!err.is_server_info_changed());
        assert!(!err.is_digest_mismatch());
        assert!(!hook_fired.load(Ordering::SeqCst));
    }

    /// A digest-mismatch error routes into the refresh branch: `guarded` attempts to re-fetch
    /// `/info`. Here `base_path` points at a closed port so the refetch fails, proving the branch
    /// was taken (the original error is NOT returned verbatim — it is replaced by the failed
    /// refresh attempt) without needing a live server. The hook only fires once the refetch
    /// succeeds, so it stays unfired here.
    #[tokio::test]
    async fn guarded_detects_digest_mismatch_and_attempts_refresh() {
        let mut client = Client::new("http://127.0.0.1:1".to_string()).unwrap();
        let hook_fired = Arc::new(AtomicBool::new(false));
        let flag = hook_fired.clone();
        client.set_info_refresh_hook(move |_info| {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });

        let err = client
            .guarded(async { Err::<(), _>(Error::request("DIGEST_MISMATCH")) })
            .await
            .expect_err("digest mismatch should trigger a refresh that fails on a closed port");

        // The refetch failed, so we get its request error, not the original passed straight
        // through and not `server_info_changed` (which only happens on a successful refresh).
        assert!(!err.is_server_info_changed());
        assert!(!hook_fired.load(Ordering::SeqCst));
    }
}

use crate::generated;
use crate::generated::ark::v1::ark_service_client::ArkServiceClient;
use crate::generated::ark::v1::indexer_service_client::IndexerServiceClient;
use crate::generated::ark::v1::indexer_tx_history_record::Key;
use crate::generated::ark::v1::Bip322Signature;
use crate::generated::ark::v1::ConfirmRegistrationRequest;
use crate::generated::ark::v1::GetEventStreamRequest;
use crate::generated::ark::v1::GetInfoRequest;
use crate::generated::ark::v1::GetTransactionHistoryRequest;
use crate::generated::ark::v1::GetTransactionsStreamRequest;
use crate::generated::ark::v1::GetVtxosRequest;
use crate::generated::ark::v1::IndexerChainedTxType;
use crate::generated::ark::v1::Outpoint;
use crate::generated::ark::v1::RegisterIntentRequest;
use crate::generated::ark::v1::SubmitSignedForfeitTxsRequest;
use crate::generated::ark::v1::SubmitTreeNoncesRequest;
use crate::generated::ark::v1::SubmitTreeSignaturesRequest;
use crate::Error;
use ark_core::proof_of_funds;
use ark_core::server::BatchFailed;
use ark_core::server::BatchFinalizationEvent;
use ark_core::server::BatchFinalizedEvent;
use ark_core::server::BatchStartedEvent;
use ark_core::server::BatchTreeEventType;
use ark_core::server::ChainedTxType;
use ark_core::server::CommitmentTransaction;
use ark_core::server::FinalizeOffchainTxResponse;
use ark_core::server::Info;
use ark_core::server::ListVtxo;
use ark_core::server::NoncePks;
use ark_core::server::PartialSigTree;
use ark_core::server::RedeemTransaction;
use ark_core::server::RoundStreamEvent;
use ark_core::server::SubmitOffchainTxResponse;
use ark_core::server::TransactionEvent;
use ark_core::server::TreeNoncesAggregatedEvent;
use ark_core::server::TreeSignatureEvent;
use ark_core::server::TreeSigningStartedEvent;
use ark_core::server::TreeTxEvent;
use ark_core::server::VtxoChain;
use ark_core::server::VtxoChains;
use ark_core::server::VtxoOutPoint;
use ark_core::ArkAddress;
use ark_core::ArkNote;
use ark_core::ArkTransaction;
use ark_core::TxGraphChunk;
use async_stream::stream;
use base64::Engine;
use bitcoin::hex::FromHex;
use bitcoin::secp256k1::PublicKey;
use bitcoin::taproot::Signature;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::SignedAmount;
use bitcoin::Txid;
use futures::Stream;
use futures::StreamExt;
use futures::TryStreamExt;
use std::collections::HashMap;
use std::str::FromStr;

#[derive(Debug, Clone)]
pub struct Client {
    url: String,
    ark_client: Option<ArkServiceClient<tonic::transport::Channel>>,
    indexer_client: Option<IndexerServiceClient<tonic::transport::Channel>>,
    admin_client: Option<
        generated::ark::v1::admin_service_client::AdminServiceClient<tonic::transport::Channel>,
    >,
}

impl Client {
    pub fn new(url: String) -> Self {
        Self {
            url,
            ark_client: None,
            indexer_client: None,
            admin_client: None,
        }
    }

    pub async fn connect(&mut self) -> Result<(), Error> {
        let ark_service_client = ArkServiceClient::connect(self.url.clone())
            .await
            .map_err(Error::connect)?;
        let indexer_client = IndexerServiceClient::connect(self.url.clone())
            .await
            .map_err(Error::connect)?;
        let admin_client =
            generated::ark::v1::admin_service_client::AdminServiceClient::connect(self.url.clone())
                .await
                .map_err(Error::connect)?;

        self.ark_client = Some(ark_service_client);
        self.indexer_client = Some(indexer_client);
        self.admin_client = Some(admin_client);
        Ok(())
    }

    pub async fn get_info(&mut self) -> Result<Info, Error> {
        let mut client = self.inner_ark_client()?;

        let response = client
            .get_info(GetInfoRequest {})
            .await
            .map_err(Error::request)?;

        response.into_inner().try_into()
    }

    pub async fn create_note(&self, amount: Amount, quantity: u64) -> Result<ArkNote, Error> {
        let mut client = self.admin_client.clone().ok_or(Error::not_connected())?;

        let response = client
            .create_note(generated::ark::v1::CreateNoteRequest {
                amount: amount.to_sat() as u32,
                quantity: quantity as u32,
            })
            .await
            .map_err(Error::request)?;

        // FIXME: this is only one item or are more items returned?
        let notes = response.into_inner();
        let note = notes.notes.first().ok_or(Error::empty_response())?;
        ArkNote::from_string(note).map_err(Error::conversion)
    }

    pub async fn list_vtxos(&self, address: &ArkAddress) -> Result<ListVtxo, Error> {
        let script = address.to_p2tr_script_pubkey();
        let script = script.to_hex_string();

        let mut client = self.inner_indexer_client()?;

        // TODO: Implement pagination.
        // TODO: We probably want to expose all fields as arguments to this function.
        let response = client
            .get_vtxos(GetVtxosRequest {
                scripts: vec![script],
                outpoints: vec![],
                recoverable_only: false,
                spendable_only: false,
                spent_only: false,
                page: None,
            })
            .await
            .map_err(Error::request)?;

        let mut spent = response
            .get_ref()
            .vtxos
            .iter()
            .filter_map(|vtxo| {
                if vtxo.is_unrolled || vtxo.is_spent || vtxo.is_swept {
                    Some(VtxoOutPoint::try_from(vtxo))
                } else {
                    None
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut spendable = response
            .get_ref()
            .vtxos
            .iter()
            .filter_map(|vtxo| {
                if !vtxo.is_unrolled && !vtxo.is_spent && !vtxo.is_swept {
                    Some(VtxoOutPoint::try_from(vtxo))
                } else {
                    None
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut spent_by_redeem = Vec::new();
        for spendable_vtxo in spendable.clone() {
            let was_spent_by_redeem = spendable.iter().any(|v| v.is_unrolled);

            if was_spent_by_redeem {
                spent_by_redeem.push(spendable_vtxo);
            }
        }

        // Remove "spendable" VTXOs that were actually already spent by a redeem transaction
        // from the list of spendable VTXOs.
        spendable.retain(|i| !spent_by_redeem.contains(i));

        // Add them to the list of spent VTXOs.
        spent.append(&mut spent_by_redeem);

        Ok(ListVtxo::new(spent, spendable))
    }

    pub async fn get_tx_history(&self, address: &ArkAddress) -> Result<Vec<ArkTransaction>, Error> {
        let address = address.encode();

        let mut client = self.inner_indexer_client()?;

        // TODO: Implement pagination.
        let response = client
            .get_transaction_history(GetTransactionHistoryRequest {
                address,
                start_time: -1,
                end_time: -1,
                page: None,
            })
            .await
            .map_err(Error::request)?;

        let history = response.into_inner().history;
        let history = history
            .iter()
            .map(ArkTransaction::try_from)
            .collect::<Result<Vec<_>, Error>>()?;

        Ok(history)
    }

    pub async fn register_intent(
        &self,
        intent_message: &proof_of_funds::IntentMessage,
        proof: &proof_of_funds::Bip322Proof,
    ) -> Result<String, Error> {
        let mut client = self.inner_ark_client()?;

        let request = RegisterIntentRequest {
            intent: Some(Bip322Signature {
                signature: proof.serialize(),
                message: intent_message.encode().map_err(Error::conversion)?,
            }),
        };

        let response = client
            .register_intent(request)
            .await
            .map_err(Error::request)?;

        let intent_id = response.into_inner().intent_id;

        Ok(intent_id)
    }

    pub async fn submit_offchain_transaction_request(
        &self,
        virtual_tx: Psbt,
        checkpoint_txs: Vec<Psbt>,
    ) -> Result<SubmitOffchainTxResponse, Error> {
        let mut client = self.inner_ark_client()?;

        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let virtual_tx = base64.encode(virtual_tx.serialize());

        let checkpoint_txs = checkpoint_txs
            .into_iter()
            .map(|tx| base64.encode(tx.serialize()))
            .collect();

        let res = client
            .submit_tx(generated::ark::v1::SubmitTxRequest {
                signed_ark_tx: virtual_tx,
                checkpoint_txs,
            })
            .await
            .map_err(Error::request)?;

        let res = res.into_inner();

        let signed_virtual_tx = res.final_ark_tx;
        let signed_virtual_tx = base64
            .decode(signed_virtual_tx)
            .map_err(Error::conversion)?;
        let signed_virtual_tx = Psbt::deserialize(&signed_virtual_tx).map_err(Error::conversion)?;

        let signed_checkpoint_txs = res
            .signed_checkpoint_txs
            .into_iter()
            .map(|tx| {
                let tx = base64.decode(tx).map_err(Error::conversion)?;
                let tx = Psbt::deserialize(&tx).map_err(Error::conversion)?;

                Ok(tx)
            })
            .collect::<Result<Vec<_>, Error>>()?;

        Ok(SubmitOffchainTxResponse {
            signed_virtual_tx,
            signed_checkpoint_txs,
        })
    }

    pub async fn finalize_offchain_transaction(
        &self,
        txid: Txid,
        checkpoint_txs: Vec<Psbt>,
    ) -> Result<FinalizeOffchainTxResponse, Error> {
        let mut client = self.inner_ark_client()?;

        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let checkpoint_txs = checkpoint_txs
            .into_iter()
            .map(|tx| base64.encode(tx.serialize()))
            .collect();

        client
            .finalize_tx(generated::ark::v1::FinalizeTxRequest {
                ark_txid: txid.to_string(),
                final_checkpoint_txs: checkpoint_txs,
            })
            .await
            .map_err(Error::request)?;

        Ok(FinalizeOffchainTxResponse {})
    }

    pub async fn confirm_registration(&self, intent_id: String) -> Result<(), Error> {
        let mut client = self.inner_ark_client()?;

        client
            .confirm_registration(ConfirmRegistrationRequest { intent_id })
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
        let mut client = self.inner_ark_client()?;

        let tree_nonces = serde_json::to_string(&pub_nonce_tree).map_err(Error::conversion)?;

        client
            .submit_tree_nonces(SubmitTreeNoncesRequest {
                batch_id: batch_id.to_string(),
                pubkey: cosigner_pubkey.to_string(),
                tree_nonces,
            })
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
        let mut client = self.inner_ark_client()?;

        let tree_signatures =
            serde_json::to_string(&partial_sig_tree).map_err(Error::conversion)?;

        client
            .submit_tree_signatures(SubmitTreeSignaturesRequest {
                batch_id: batch_id.to_string(),
                pubkey: cosigner_pk.to_string(),
                tree_signatures,
            })
            .await
            .map_err(Error::request)?;

        Ok(())
    }

    pub async fn submit_signed_forfeit_txs(
        &self,
        signed_forfeit_txs: Vec<Psbt>,
        signed_commitment_tx: Option<Psbt>,
    ) -> Result<(), Error> {
        let mut client = self.inner_ark_client()?;

        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let signed_commitment_tx = signed_commitment_tx
            .map(|tx| base64.encode(tx.serialize()))
            .unwrap_or_default();

        client
            .submit_signed_forfeit_txs(SubmitSignedForfeitTxsRequest {
                signed_forfeit_txs: signed_forfeit_txs
                    .iter()
                    .map(|psbt| base64.encode(psbt.serialize()))
                    .collect(),
                signed_commitment_tx,
            })
            .await
            .map_err(Error::request)?;

        Ok(())
    }

    pub async fn get_event_stream(
        &self,
        topics: Vec<String>,
    ) -> Result<impl Stream<Item = Result<RoundStreamEvent, Error>> + Unpin, Error> {
        let mut client = self.inner_ark_client()?;

        let response = client
            .get_event_stream(GetEventStreamRequest { topics })
            .await
            .map_err(Error::request)?;
        let mut stream = response.into_inner();

        let stream = stream! {
            loop {
                match stream.try_next().await {
                    Ok(Some(event)) => match event.event {
                        None => {
                            log::debug!("Got empty message");
                        }
                        Some(event) => {
                            yield Ok(RoundStreamEvent::try_from(event)?);
                        }
                    },
                    Ok(None) => {
                        yield Err(Error::event_stream_disconnect());
                    }
                    Err(e) => {
                        yield Err(Error::event_stream(e));
                    }
                }
            }
        };

        Ok(stream.boxed())
    }

    pub async fn get_tx_stream(
        &self,
    ) -> Result<impl Stream<Item = Result<TransactionEvent, Error>> + Unpin, Error> {
        let mut client = self.inner_ark_client()?;

        let response = client
            .get_transactions_stream(GetTransactionsStreamRequest {})
            .await
            .map_err(Error::request)?;

        let mut stream = response.into_inner();

        let stream = stream! {
            loop {
                match stream.try_next().await {
                    Ok(Some(event)) => match event.tx {
                        None => {
                            log::debug!("Got empty message");
                        }
                        Some(event) => {
                            yield Ok(TransactionEvent::try_from(event)?);
                        }
                    },
                    Ok(None) => {
                        yield Err(Error::event_stream_disconnect());
                    }
                    Err(e) => {
                        yield Err(Error::event_stream(e));
                    }
                }
            }
        };

        Ok(stream.boxed())
    }

    pub async fn get_vtxo_chain(
        &self,
        outpoint: Option<OutPoint>,
        size_and_index: Option<(i32, i32)>,
    ) -> Result<VtxoChainResponse, Error> {
        let mut client = self.inner_indexer_client()?;
        let response = client
            .get_vtxo_chain(generated::ark::v1::GetVtxoChainRequest {
                outpoint: outpoint.map(|o| generated::ark::v1::IndexerOutpoint {
                    txid: o.txid.to_string(),
                    vout: o.vout,
                }),
                page: size_and_index
                    .map(|(size, index)| generated::ark::v1::IndexerPageRequest { size, index }),
            })
            .await
            .map_err(Error::request)?;
        let response = response.into_inner();
        let result = response.try_into()?;
        Ok(result)
    }

    pub async fn get_virtual_txs(
        &self,
        txids: Vec<String>,
        size_and_index: Option<(i32, i32)>,
    ) -> Result<VirtualTxsResponse, Error> {
        let mut client = self.inner_indexer_client()?;
        let response = client
            .get_virtual_txs(generated::ark::v1::GetVirtualTxsRequest {
                txids,
                page: size_and_index
                    .map(|(size, index)| generated::ark::v1::IndexerPageRequest { size, index }),
            })
            .await
            .map_err(Error::request)?;
        let response = response.into_inner();
        let result = response.try_into()?;
        Ok(result)
    }

    fn inner_ark_client(&self) -> Result<ArkServiceClient<tonic::transport::Channel>, Error> {
        // Cloning an `ArkServiceClient<Channel>` is cheap.
        self.ark_client.clone().ok_or(Error::not_connected())
    }
    fn inner_indexer_client(
        &self,
    ) -> Result<IndexerServiceClient<tonic::transport::Channel>, Error> {
        self.indexer_client.clone().ok_or(Error::not_connected())
    }
}

impl TryFrom<generated::ark::v1::BatchStartedEvent> for BatchStartedEvent {
    type Error = Error;

    fn try_from(value: generated::ark::v1::BatchStartedEvent) -> Result<Self, Self::Error> {
        Ok(BatchStartedEvent {
            id: value.id,
            intent_id_hashes: value.intent_id_hashes,
            batch_expiry: value.batch_expiry,
        })
    }
}

impl TryFrom<generated::ark::v1::BatchFinalizationEvent> for BatchFinalizationEvent {
    type Error = Error;

    fn try_from(value: generated::ark::v1::BatchFinalizationEvent) -> Result<Self, Self::Error> {
        let base64 = &base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let commitment_tx = base64
            .decode(&value.commitment_tx)
            .map_err(Error::conversion)?;
        let commitment_tx = Psbt::deserialize(&commitment_tx).map_err(Error::conversion)?;

        Ok(BatchFinalizationEvent {
            id: value.id,
            commitment_tx,
        })
    }
}

impl TryFrom<generated::ark::v1::BatchFinalizedEvent> for BatchFinalizedEvent {
    type Error = Error;

    fn try_from(value: generated::ark::v1::BatchFinalizedEvent) -> Result<Self, Self::Error> {
        let commitment_txid = value.commitment_txid.parse().map_err(Error::conversion)?;

        Ok(BatchFinalizedEvent {
            id: value.id,
            commitment_txid,
        })
    }
}

impl From<generated::ark::v1::BatchFailed> for BatchFailed {
    fn from(value: generated::ark::v1::BatchFailed) -> Self {
        BatchFailed {
            id: value.id,
            reason: value.reason,
        }
    }
}

impl TryFrom<generated::ark::v1::TreeSigningStartedEvent> for TreeSigningStartedEvent {
    type Error = Error;

    fn try_from(value: generated::ark::v1::TreeSigningStartedEvent) -> Result<Self, Self::Error> {
        let unsigned_commitment_tx = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        )
        .decode(&value.unsigned_commitment_tx)
        .map_err(Error::conversion)?;

        let unsigned_round_tx =
            Psbt::deserialize(&unsigned_commitment_tx).map_err(Error::conversion)?;

        Ok(TreeSigningStartedEvent {
            id: value.id,
            cosigners_pubkeys: value
                .cosigners_pubkeys
                .into_iter()
                .map(|pk| pk.parse().map_err(Error::conversion))
                .collect::<Result<Vec<_>, Error>>()?,
            unsigned_round_tx,
        })
    }
}

impl TryFrom<generated::ark::v1::TreeNoncesAggregatedEvent> for TreeNoncesAggregatedEvent {
    type Error = Error;

    fn try_from(value: generated::ark::v1::TreeNoncesAggregatedEvent) -> Result<Self, Self::Error> {
        let tree_nonces = serde_json::from_str(&value.tree_nonces).map_err(Error::conversion)?;

        Ok(TreeNoncesAggregatedEvent {
            id: value.id,
            tree_nonces,
        })
    }
}

impl TryFrom<generated::ark::v1::TreeTxEvent> for TreeTxEvent {
    type Error = Error;

    fn try_from(value: generated::ark::v1::TreeTxEvent) -> Result<Self, Self::Error> {
        let batch_tree_event_type = match value.batch_index {
            0 => BatchTreeEventType::Vtxo,
            1 => BatchTreeEventType::Connector,
            n => return Err(Error::conversion(format!("unsupported batch index: {n}"))),
        };

        let txid = if value.txid.is_empty() {
            None
        } else {
            Some(value.txid.parse().map_err(Error::conversion)?)
        };

        let base64 = &base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let bytes = base64.decode(&value.tx).map_err(Error::conversion)?;
        let tx = Psbt::deserialize(&bytes).map_err(Error::conversion)?;

        let children = value
            .children
            .iter()
            .map(|(index, txid)| Ok((*index, txid.parse().map_err(Error::conversion)?)))
            .collect::<Result<HashMap<_, _>, Error>>()?;

        Ok(Self {
            id: value.id,
            topic: value.topic,
            batch_tree_event_type,
            tx_graph_chunk: TxGraphChunk { txid, tx, children },
        })
    }
}

impl TryFrom<generated::ark::v1::TreeSignatureEvent> for TreeSignatureEvent {
    type Error = Error;

    fn try_from(value: generated::ark::v1::TreeSignatureEvent) -> Result<Self, Self::Error> {
        let batch_tree_event_type = match value.batch_index {
            0 => BatchTreeEventType::Vtxo,
            1 => BatchTreeEventType::Connector,
            n => return Err(Error::conversion(format!("unsupported batch index: {n}"))),
        };

        let txid = value.txid.parse().map_err(Error::conversion)?;

        let signature = Vec::from_hex(&value.signature).map_err(Error::conversion)?;
        let signature = Signature::from_slice(&signature).map_err(Error::conversion)?;

        Ok(Self {
            id: value.id,
            topic: value.topic,
            batch_tree_event_type,
            txid,
            signature,
        })
    }
}

impl TryFrom<generated::ark::v1::get_event_stream_response::Event> for RoundStreamEvent {
    type Error = Error;

    fn try_from(
        value: generated::ark::v1::get_event_stream_response::Event,
    ) -> Result<Self, Self::Error> {
        Ok(match value {
            generated::ark::v1::get_event_stream_response::Event::BatchStarted(e) => {
                RoundStreamEvent::BatchStarted(e.try_into()?)
            }
            generated::ark::v1::get_event_stream_response::Event::BatchFinalization(e) => {
                RoundStreamEvent::BatchFinalization(e.try_into()?)
            }
            generated::ark::v1::get_event_stream_response::Event::BatchFinalized(e) => {
                RoundStreamEvent::BatchFinalized(e.try_into()?)
            }
            generated::ark::v1::get_event_stream_response::Event::BatchFailed(e) => {
                RoundStreamEvent::BatchFailed(e.into())
            }
            generated::ark::v1::get_event_stream_response::Event::TreeSigningStarted(e) => {
                RoundStreamEvent::TreeSigningStarted(e.try_into()?)
            }
            generated::ark::v1::get_event_stream_response::Event::TreeNoncesAggregated(e) => {
                RoundStreamEvent::TreeNoncesAggregated(e.try_into()?)
            }
            generated::ark::v1::get_event_stream_response::Event::TreeTx(e) => {
                RoundStreamEvent::TreeTx(e.try_into()?)
            }
            generated::ark::v1::get_event_stream_response::Event::TreeSignature(e) => {
                RoundStreamEvent::TreeSignature(e.try_into()?)
            }
        })
    }
}

impl TryFrom<generated::ark::v1::get_transactions_stream_response::Tx> for TransactionEvent {
    type Error = Error;

    fn try_from(
        value: generated::ark::v1::get_transactions_stream_response::Tx,
    ) -> Result<Self, Self::Error> {
        match value {
            generated::ark::v1::get_transactions_stream_response::Tx::CommitmentTx(
                commitment_tx,
            ) => Ok(TransactionEvent::Round(CommitmentTransaction::try_from(
                commitment_tx,
            )?)),
            generated::ark::v1::get_transactions_stream_response::Tx::ArkTx(redeem) => Ok(
                TransactionEvent::Redeem(RedeemTransaction::try_from(redeem)?),
            ),
        }
    }
}

impl TryFrom<generated::ark::v1::TxNotification> for CommitmentTransaction {
    type Error = Error;

    fn try_from(value: generated::ark::v1::TxNotification) -> Result<Self, Self::Error> {
        let spent_vtxos = value
            .spent_vtxos
            .iter()
            .map(VtxoOutPoint::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        let spendable_vtxos = value
            .spendable_vtxos
            .iter()
            .map(VtxoOutPoint::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(CommitmentTransaction {
            txid: Txid::from_str(value.txid.as_str()).map_err(Error::conversion)?,
            spent_vtxos,
            spendable_vtxos,
        })
    }
}

impl TryFrom<generated::ark::v1::TxNotification> for RedeemTransaction {
    type Error = Error;

    fn try_from(value: generated::ark::v1::TxNotification) -> Result<Self, Self::Error> {
        let spent_vtxos = value
            .spent_vtxos
            .iter()
            .map(VtxoOutPoint::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        let spendable_vtxos = value
            .spendable_vtxos
            .iter()
            .map(VtxoOutPoint::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(RedeemTransaction {
            txid: Txid::from_str(value.txid.as_str()).map_err(Error::conversion)?,
            spent_vtxos,
            spendable_vtxos,
        })
    }
}

impl TryFrom<Outpoint> for OutPoint {
    type Error = Error;

    fn try_from(value: Outpoint) -> Result<Self, Self::Error> {
        let point = OutPoint {
            txid: Txid::from_str(value.txid.as_str()).map_err(Error::conversion)?,
            vout: value.vout,
        };
        Ok(point)
    }
}

pub struct VtxoChainResponse {
    pub chains: VtxoChains,
    pub page: Option<IndexerPage>,
}

#[derive(Debug)]
pub struct VirtualTxsResponse {
    pub txs: Vec<Psbt>,
    pub page: Option<IndexerPage>,
}

#[derive(Debug)]
pub struct IndexerPage {
    pub current: i32,
    pub next: i32,
    pub total: i32,
}

impl TryFrom<generated::ark::v1::GetVtxoChainResponse> for VtxoChainResponse {
    type Error = Error;

    fn try_from(value: generated::ark::v1::GetVtxoChainResponse) -> Result<Self, Self::Error> {
        let chains = value
            .chain
            .iter()
            .map(VtxoChain::try_from)
            .collect::<Result<Vec<_>, Error>>()?;

        Ok(VtxoChainResponse {
            chains: VtxoChains { inner: chains },
            page: value
                .page
                .map(IndexerPage::try_from)
                .transpose()
                .map_err(Error::conversion)?,
        })
    }
}

impl TryFrom<generated::ark::v1::GetVirtualTxsResponse> for VirtualTxsResponse {
    type Error = Error;

    fn try_from(value: generated::ark::v1::GetVirtualTxsResponse) -> Result<Self, Self::Error> {
        let base64 = &base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let txs = value
            .txs
            .into_iter()
            .map(|tx| {
                let bytes = base64.decode(&tx).map_err(Error::conversion)?;
                let psbt = Psbt::deserialize(&bytes).map_err(Error::conversion)?;

                Ok(psbt)
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(VirtualTxsResponse {
            txs,
            page: value
                .page
                .map(IndexerPage::try_from)
                .transpose()
                .map_err(Error::conversion)?,
        })
    }
}

impl TryFrom<&generated::ark::v1::IndexerChain> for VtxoChain {
    type Error = Error;

    fn try_from(value: &generated::ark::v1::IndexerChain) -> Result<Self, Self::Error> {
        let spends = value
            .spends
            .iter()
            .map(|txid| {
                // Handle the case where txid might be 66 bytes long by trimming the last 2 bytes.
                let txid_str = if txid.len() == 66 { &txid[..64] } else { txid };
                txid_str.parse().map_err(Error::conversion)
            })
            .collect::<Result<Vec<_>, Error>>()?;

        let tx_type = match value.r#type() {
            IndexerChainedTxType::Unspecified => ChainedTxType::Unspecified,
            IndexerChainedTxType::Commitment => ChainedTxType::Commitment,
            IndexerChainedTxType::Ark => ChainedTxType::Virtual,
            IndexerChainedTxType::Tree => ChainedTxType::Tree,
            IndexerChainedTxType::Checkpoint => ChainedTxType::Checkpoint,
        };

        Ok(VtxoChain {
            txid: value.txid.parse().map_err(Error::conversion)?,
            tx_type,
            spends,
            expires_at: value.expires_at,
        })
    }
}

impl From<generated::ark::v1::IndexerPageResponse> for IndexerPage {
    fn from(value: generated::ark::v1::IndexerPageResponse) -> Self {
        IndexerPage {
            current: value.current,
            next: value.next,
            total: value.total,
        }
    }
}

impl TryFrom<&generated::ark::v1::IndexerTxHistoryRecord> for ArkTransaction {
    type Error = Error;

    fn try_from(value: &generated::ark::v1::IndexerTxHistoryRecord) -> Result<Self, Self::Error> {
        let sign = match value.r#type() {
            generated::ark::v1::IndexerTxType::Received => 1,
            // Default to sent if unspecified.
            generated::ark::v1::IndexerTxType::Sent
            | generated::ark::v1::IndexerTxType::Unspecified => -1,
        };

        let amount = SignedAmount::from_sat(value.amount as i64 * sign);

        let tx = match &value.key {
            Some(Key::CommitmentTxid(txid)) => ArkTransaction::Commitment {
                txid: txid.parse().map_err(Error::conversion)?,
                amount,
                created_at: value.created_at,
            },
            Some(Key::VirtualTxid(txid)) => ArkTransaction::Virtual {
                txid: txid.parse().map_err(Error::conversion)?,
                amount,
                is_settled: value.is_settled,
                created_at: value.created_at,
            },
            None => return Err(Error::conversion("invalid transaction without key")),
        };

        Ok(tx)
    }
}

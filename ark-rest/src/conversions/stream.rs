//! Stream event type conversions

use crate::conversions::ConversionError;
use crate::models;
use ark_core::server::BatchFailed;
use ark_core::server::BatchFinalizationEvent;
use ark_core::server::BatchFinalizedEvent;
use ark_core::server::BatchStartedEvent;
use ark_core::server::StreamEvent;
use ark_core::server::TreeNoncesAggregatedEvent;
use ark_core::server::TreeSignatureEvent;
use ark_core::server::TreeSigningStartedEvent;
use ark_core::server::TreeTxEvent;
use bitcoin::base64;
use bitcoin::base64::Engine;
use bitcoin::hex::FromHex;
use bitcoin::secp256k1::PublicKey;
use bitcoin::taproot::Signature;
use bitcoin::Psbt;
use bitcoin::Txid;
use std::str::FromStr;

impl TryFrom<models::V1GetEventStreamResponse> for StreamEvent {
    type Error = ConversionError;

    fn try_from(response: models::V1GetEventStreamResponse) -> Result<Self, Self::Error> {
        if let Some(batch_started) = response.batch_started {
            return Ok(StreamEvent::BatchStarted(batch_started.try_into()?));
        } else if let Some(batch_finalization) = response.batch_finalization {
            return Ok(StreamEvent::BatchFinalization(
                batch_finalization.try_into()?,
            ));
        } else if let Some(batch_finalized) = response.batch_finalized {
            return Ok(StreamEvent::BatchFinalized(batch_finalized.try_into()?));
        } else if let Some(batch_failed) = response.batch_failed {
            return Ok(StreamEvent::BatchFailed(batch_failed.try_into()?));
        } else if let Some(tree_signing_started) = response.tree_signing_started {
            return Ok(StreamEvent::TreeSigningStarted(
                tree_signing_started.try_into()?,
            ));
        } else if let Some(tree_nonces_aggregated) = response.tree_nonces_aggregated {
            return Ok(StreamEvent::TreeNoncesAggregated(
                tree_nonces_aggregated.try_into()?,
            ));
        } else if let Some(tree_tx) = response.tree_tx {
            return Ok(StreamEvent::TreeTx(tree_tx.try_into()?));
        } else if let Some(tree_signature) = response.tree_signature {
            return Ok(StreamEvent::TreeSignature(tree_signature.try_into()?));
        }

        Err(ConversionError("No event found in response".to_string()))
    }
}

impl TryFrom<models::V1BatchStartedEvent> for BatchStartedEvent {
    type Error = ConversionError;

    fn try_from(event: models::V1BatchStartedEvent) -> Result<Self, Self::Error> {
        Ok(BatchStartedEvent {
            id: event
                .id
                .ok_or_else(|| ConversionError("Missing batch id".to_string()))?,
            intent_id_hashes: event.intent_id_hashes.unwrap_or_default(),
            batch_expiry: event
                .batch_expiry
                .ok_or_else(|| ConversionError("Missing batch_expiry".to_string()))?
                .parse::<i64>()
                .map_err(|e| ConversionError(format!("Invalid batch_expiry: {}", e)))?,
        })
    }
}

impl TryFrom<models::V1BatchFinalizationEvent> for BatchFinalizationEvent {
    type Error = ConversionError;

    fn try_from(event: models::V1BatchFinalizationEvent) -> Result<Self, Self::Error> {
        let id = event
            .id
            .ok_or_else(|| ConversionError("Missing batch id".to_string()))?;
        let commitment_tx_hex = event
            .commitment_tx
            .ok_or_else(|| ConversionError("Missing commitment_tx".to_string()))?;

        // Parse the hex string to PSBT
        let base64 = &base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let bytes = base64
            .decode(&commitment_tx_hex)
            .map_err(|e| ConversionError(format!("Invalid base64 tx: {e}")))?;
        let commitment_tx =
            Psbt::deserialize(&bytes).map_err(|e| ConversionError(format!("Invalid PSBT: {e}")))?;

        Ok(BatchFinalizationEvent { id, commitment_tx })
    }
}

impl TryFrom<models::V1BatchFinalizedEvent> for BatchFinalizedEvent {
    type Error = ConversionError;

    fn try_from(event: models::V1BatchFinalizedEvent) -> Result<Self, Self::Error> {
        let id = event
            .id
            .ok_or_else(|| ConversionError("Missing batch id".to_string()))?;
        let commitment_txid_str = event
            .commitment_txid
            .ok_or_else(|| ConversionError("Missing commitment_txid".to_string()))?;
        let commitment_txid = Txid::from_str(&commitment_txid_str)
            .map_err(|e| ConversionError(format!("Invalid commitment_txid: {}", e)))?;

        Ok(BatchFinalizedEvent {
            id,
            commitment_txid,
        })
    }
}

impl TryFrom<models::V1BatchFailedEvent> for BatchFailed {
    type Error = ConversionError;

    fn try_from(event: models::V1BatchFailedEvent) -> Result<Self, Self::Error> {
        Ok(BatchFailed {
            id: event
                .id
                .ok_or_else(|| ConversionError("Missing batch id".to_string()))?,
            reason: event
                .reason
                .ok_or_else(|| ConversionError("Missing reason".to_string()))?,
        })
    }
}

impl TryFrom<models::V1TreeSigningStartedEvent> for TreeSigningStartedEvent {
    type Error = ConversionError;

    fn try_from(event: models::V1TreeSigningStartedEvent) -> Result<Self, Self::Error> {
        let id = event
            .id
            .ok_or_else(|| ConversionError("Missing batch id".to_string()))?;

        let cosigners_pubkeys_str = event
            .cosigners_pubkeys
            .ok_or_else(|| ConversionError("Missing cosigners_pubkeys".to_string()))?;
        let cosigners_pubkeys = cosigners_pubkeys_str
            .into_iter()
            .map(|pk_str| pk_str.parse::<PublicKey>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ConversionError(format!("Invalid cosigner pubkey: {}", e)))?;

        let unsigned_commitment_tx_hex = event
            .unsigned_commitment_tx
            .ok_or_else(|| ConversionError("Missing unsigned_commitment_tx".to_string()))?;

        // Parse the hex string to PSBT
        let base64 = &base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let bytes = base64
            .decode(&unsigned_commitment_tx_hex)
            .map_err(|e| ConversionError(format!("Invalid base64 tx: {e}")))?;
        let unsigned_commitment_tx =
            Psbt::deserialize(&bytes).map_err(|e| ConversionError(format!("Invalid PSBT: {e}")))?;

        Ok(TreeSigningStartedEvent {
            id,
            cosigners_pubkeys,
            unsigned_commitment_tx,
        })
    }
}

impl TryFrom<models::V1TreeNoncesAggregatedEvent> for TreeNoncesAggregatedEvent {
    type Error = ConversionError;

    fn try_from(event: models::V1TreeNoncesAggregatedEvent) -> Result<Self, Self::Error> {
        let id = event
            .id
            .ok_or_else(|| ConversionError("Missing batch id".to_string()))?;

        let tree_nonces_str = event
            .tree_nonces
            .ok_or_else(|| ConversionError("Missing tree_nonces".to_string()))?;

        // Parse the tree_nonces JSON string into NoncePks
        let tree_nonces = serde_json::from_str::<ark_core::server::NoncePks>(&tree_nonces_str)
            .map_err(|e| ConversionError(format!("Invalid tree_nonces JSON: {}", e)))?;

        Ok(TreeNoncesAggregatedEvent { id, tree_nonces })
    }
}

impl TryFrom<models::V1TreeTxEvent> for TreeTxEvent {
    type Error = ConversionError;

    fn try_from(event: models::V1TreeTxEvent) -> Result<Self, Self::Error> {
        let id = event
            .id
            .ok_or_else(|| ConversionError("Missing batch id".to_string()))?;
        let topic = event.topic.unwrap_or_default();

        // Determine BatchTreeEventType from batch_index (simplified mapping)
        let batch_tree_event_type = match event.batch_index {
            Some(0) => ark_core::server::BatchTreeEventType::Vtxo,
            Some(1) => ark_core::server::BatchTreeEventType::Connector,
            _ => ark_core::server::BatchTreeEventType::Vtxo, // Default to Vtxo
        };

        // Parse txid
        let txid_str = event
            .txid
            .ok_or_else(|| ConversionError("Missing txid".to_string()))?;

        let txid = if txid_str.is_empty() {
            None
        } else {
            let txid = Txid::from_str(&txid_str).map_err(|e| {
                ConversionError(format!("Invalid txid: {} but was {}", e, txid_str))
            })?;
            Some(txid)
        };

        // Parse tx (PSBT)
        let tx_hex = event
            .tx
            .ok_or_else(|| ConversionError("Missing tx".to_string()))?;
        let base64 = &base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let bytes = base64
            .decode(&tx_hex)
            .map_err(|e| ConversionError(format!("Invalid base64 tx: {e}")))?;
        let tx =
            Psbt::deserialize(&bytes).map_err(|e| ConversionError(format!("Invalid PSBT: {e}")))?;

        // Parse children map
        let children_str = event.children.unwrap_or_default();
        let mut children = std::collections::HashMap::new();
        for (output_idx_str, child_txid_str) in children_str {
            let output_idx = output_idx_str.parse::<u32>().map_err(|e| {
                ConversionError(format!("Invalid output index '{}': {}", output_idx_str, e))
            })?;
            let child_txid = Txid::from_str(&child_txid_str).map_err(|e| {
                ConversionError(format!("Invalid child txid '{}': {}", child_txid_str, e))
            })?;
            children.insert(output_idx, child_txid);
        }

        let tx_graph_chunk = ark_core::TxGraphChunk { txid, tx, children };

        Ok(TreeTxEvent {
            id,
            topic,
            batch_tree_event_type,
            tx_graph_chunk,
        })
    }
}

impl TryFrom<models::V1TreeSignatureEvent> for TreeSignatureEvent {
    type Error = ConversionError;

    fn try_from(event: models::V1TreeSignatureEvent) -> Result<Self, Self::Error> {
        let id = event
            .id
            .ok_or_else(|| ConversionError("Missing batch id".to_string()))?;
        let topic = event.topic.unwrap_or_default();

        // Determine BatchTreeEventType from batch_index (simplified mapping)
        let batch_tree_event_type = match event.batch_index {
            Some(0) => ark_core::server::BatchTreeEventType::Vtxo,
            Some(1) => ark_core::server::BatchTreeEventType::Connector,
            _ => ark_core::server::BatchTreeEventType::Vtxo, // Default to Vtxo
        };

        // Parse txid
        let txid_str = event
            .txid
            .ok_or_else(|| ConversionError("Missing txid".to_string()))?;
        let txid = Txid::from_str(&txid_str)
            .map_err(|e| ConversionError(format!("Invalid txid: {}", e)))?;

        // Parse signature
        let signature_hex = event
            .signature
            .ok_or_else(|| ConversionError("Missing signature".to_string()))?;
        let signature_bytes = Vec::from_hex(&signature_hex)
            .map_err(|e| ConversionError(format!("Invalid signature hex: {}", e)))?;
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|e| ConversionError(format!("Invalid signature: {}", e)))?;

        Ok(TreeSignatureEvent {
            id,
            topic,
            batch_tree_event_type,
            txid,
            signature,
        })
    }
}

//! Type conversions between generated API types and ark-core types

use crate::models::V1GetInfoResponse;
use crate::models::V1GetSubscriptionResponse;
use crate::models::V1IndexerVtxo;
use bitcoin::base64;
use bitcoin::base64::Engine;
use bitcoin::secp256k1::PublicKey;
use bitcoin::Amount;
use bitcoin::Network;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::ScriptBuf;
use bitcoin::Txid;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::str::FromStr;

pub mod stream;

#[derive(Debug)]
pub struct ConversionError(pub String);

impl std::fmt::Display for ConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Conversion error: {}", self.0)
    }
}

impl StdError for ConversionError {}

impl TryFrom<V1GetInfoResponse> for ark_core::server::Info {
    type Error = ConversionError;

    fn try_from(response: V1GetInfoResponse) -> Result<Self, Self::Error> {
        let signer_pubkey_str = response
            .signer_pubkey
            .ok_or_else(|| ConversionError("Missing signer_pubkey".to_string()))?;
        let pk = signer_pubkey_str.parse::<PublicKey>().map_err(|e| {
            ConversionError(format!("Invalid signer_pubkey '{signer_pubkey_str}': {e}",))
        })?;

        let vtxo_tree_expiry_str = response
            .vtxo_tree_expiry
            .ok_or_else(|| ConversionError("Missing vtxo_tree_expiry".to_string()))?;
        let vtxo_tree_expiry_val = vtxo_tree_expiry_str.parse::<i64>().map_err(|e| {
            ConversionError(format!(
                "Invalid vtxo_tree_expiry '{vtxo_tree_expiry_str}': {e}",
            ))
        })?;
        let vtxo_tree_expiry = parse_sequence_number(vtxo_tree_expiry_val)?;

        let unilateral_exit_delay_str = response
            .unilateral_exit_delay
            .ok_or_else(|| ConversionError("Missing unilateral_exit_delay".to_string()))?;
        let unilateral_exit_delay_val = unilateral_exit_delay_str.parse::<i64>().map_err(|e| {
            ConversionError(format!(
                "Invalid unilateral_exit_delay '{unilateral_exit_delay_str}': {e}",
            ))
        })?;
        let unilateral_exit_delay = parse_sequence_number(unilateral_exit_delay_val)?;

        let boarding_exit_delay_str = response
            .boarding_exit_delay
            .ok_or_else(|| ConversionError("Missing boarding_exit_delay".to_string()))?;
        let boarding_exit_delay_val = boarding_exit_delay_str.parse::<i64>().map_err(|e| {
            ConversionError(format!(
                "Invalid boarding_exit_delay '{boarding_exit_delay_str}': {e}",
            ))
        })?;
        let boarding_exit_delay = parse_sequence_number(boarding_exit_delay_val)?;

        let round_interval_str = response
            .round_interval
            .ok_or_else(|| ConversionError("Missing round_interval".to_string()))?;
        let round_interval = round_interval_str.parse::<i64>().map_err(|e| {
            ConversionError(format!(
                "Invalid round_interval '{round_interval_str}': {e}",
            ))
        })?;

        let network_str = response
            .network
            .ok_or_else(|| ConversionError("Missing network".to_string()))?;
        let network = network_str
            .parse::<Network>()
            .map_err(|e| ConversionError(format!("Invalid network '{network_str}': {e}")))?;

        let dust_str = response
            .dust
            .ok_or_else(|| ConversionError("Missing dust".to_string()))?;
        let dust = dust_str
            .parse::<u64>()
            .map_err(|e| ConversionError(format!("Invalid dust '{dust_str}': {e}")))
            .map(Amount::from_sat)?;

        let forfeit_address_str = response
            .forfeit_address
            .ok_or_else(|| ConversionError("Missing forfeit_address".to_string()))?;
        let forfeit_address = forfeit_address_str
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
            .map_err(|e| {
                ConversionError(format!(
                    "Invalid forfeit_address '{forfeit_address_str}': {e}",
                ))
            })?
            .require_network(network)
            .map_err(|e| {
                ConversionError(format!(
                    "Address network mismatch for '{forfeit_address_str}': {e}",
                ))
            })?;

        let version = response
            .version
            .ok_or_else(|| ConversionError("Missing version".to_string()))?;

        let utxo_min_amount = match response.utxo_min_amount {
            Some(s) => {
                let val = s
                    .parse::<i64>()
                    .map_err(|e| ConversionError(format!("Invalid utxo_min_amount '{s}': {e}")))?;
                if val < 0 {
                    None
                } else {
                    Some(Amount::from_sat(val as u64))
                }
            }
            None => None,
        };

        let utxo_max_amount = match response.utxo_max_amount {
            Some(s) => {
                let val = s
                    .parse::<i64>()
                    .map_err(|e| ConversionError(format!("Invalid utxo_max_amount '{s}': {e}")))?;
                if val < 0 {
                    None
                } else {
                    Some(Amount::from_sat(val as u64))
                }
            }
            None => None,
        };

        let vtxo_min_amount = match response.vtxo_min_amount {
            Some(s) => {
                let val = s
                    .parse::<i64>()
                    .map_err(|e| ConversionError(format!("Invalid vtxo_min_amount '{s}': {e}")))?;
                if val < 0 {
                    None
                } else {
                    Some(Amount::from_sat(val as u64))
                }
            }
            None => None,
        };

        let vtxo_max_amount = match response.vtxo_max_amount {
            Some(s) => {
                let val = s
                    .parse::<i64>()
                    .map_err(|e| ConversionError(format!("Invalid vtxo_max_amount '{s}': {e}")))?;
                if val < 0 {
                    None
                } else {
                    Some(Amount::from_sat(val as u64))
                }
            }
            None => None,
        };

        Ok(ark_core::server::Info {
            pk,
            vtxo_tree_expiry,
            unilateral_exit_delay,
            boarding_exit_delay,
            round_interval,
            network,
            dust,
            forfeit_address,
            version,
            utxo_min_amount,
            utxo_max_amount,
            vtxo_min_amount,
            vtxo_max_amount,
        })
    }
}

impl TryFrom<V1IndexerVtxo> for ark_core::server::VirtualTxOutPoint {
    type Error = ConversionError;

    fn try_from(value: V1IndexerVtxo) -> Result<Self, Self::Error> {
        // Parse outpoint
        let outpoint_data = value
            .outpoint
            .ok_or_else(|| ConversionError("Missing outpoint".to_string()))?;

        let txid_str = outpoint_data
            .txid
            .ok_or_else(|| ConversionError("Missing outpoint txid".to_string()))?;
        let txid = txid_str
            .parse::<Txid>()
            .map_err(|e| ConversionError(format!("Invalid outpoint txid '{txid_str}': {e}")))?;

        let vout = outpoint_data
            .vout
            .ok_or_else(|| ConversionError("Missing outpoint vout".to_string()))?;
        let vout = vout as u32; // Convert i64 to u32

        let outpoint = OutPoint { txid, vout };

        // Parse timestamps
        let created_at_str = value
            .created_at
            .ok_or_else(|| ConversionError("Missing created_at".to_string()))?;
        let created_at = created_at_str
            .parse::<i64>()
            .map_err(|e| ConversionError(format!("Invalid created_at '{created_at_str}': {e}")))?;

        let expires_at_str = value
            .expires_at
            .ok_or_else(|| ConversionError("Missing expires_at".to_string()))?;
        let expires_at = expires_at_str
            .parse::<i64>()
            .map_err(|e| ConversionError(format!("Invalid expires_at '{expires_at_str}': {e}")))?;

        // Parse amount
        let amount_str = value
            .amount
            .ok_or_else(|| ConversionError("Missing amount".to_string()))?;
        let amount_val = amount_str
            .parse::<u64>()
            .map_err(|e| ConversionError(format!("Invalid amount '{amount_str}': {e}")))?;
        let amount = Amount::from_sat(amount_val);

        // Parse script
        let script_str = value
            .script
            .ok_or_else(|| ConversionError("Missing script".to_string()))?;
        let script = ScriptBuf::from_hex(&script_str)
            .map_err(|e| ConversionError(format!("Invalid script hex '{script_str}': {e}")))?;

        // Parse optional spent_by
        let spent_by = value
            .spent_by
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<Txid>())
            .transpose()
            .map_err(|e| ConversionError(format!("Invalid spent_by txid: {e}")))?;

        // Parse commitment_txids
        let commitment_txids = value
            .commitment_txids
            .unwrap_or_default()
            .into_iter()
            .map(|s| s.parse::<Txid>())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ConversionError(format!("Invalid commitment_txid: {e}")))?;

        // Parse optional settled_by
        let settled_by = value
            .settled_by
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<Txid>())
            .transpose()
            .map_err(|e| ConversionError(format!("Invalid settled_by txid: {e}")))?;

        // Parse optional ark_txid
        let ark_txid = value
            .ark_txid
            .filter(|s| !s.is_empty())
            .map(|s| s.parse::<Txid>())
            .transpose()
            .map_err(|e| ConversionError(format!("Invalid ark_txid: {e}")))?;

        Ok(ark_core::server::VirtualTxOutPoint {
            outpoint,
            created_at,
            expires_at,
            amount,
            script,
            is_preconfirmed: value.is_preconfirmed.unwrap_or(false),
            is_swept: value.is_swept.unwrap_or(false),
            is_unrolled: value.is_unrolled.unwrap_or(false),
            is_spent: value.is_spent.unwrap_or(false),
            spent_by,
            commitment_txids,
            settled_by,
            ark_txid,
        })
    }
}

fn parse_sequence_number(value: i64) -> Result<bitcoin::Sequence, ConversionError> {
    /// The threshold that determines whether an expiry or exit delay should be parsed as a
    /// number of blocks or a number of seconds.
    ///
    /// - A value below 512 is considered a number of blocks.
    /// - A value over 512 is considered a number of seconds.
    const ARBITRARY_SEQUENCE_THRESHOLD: i64 = 512;

    let sequence = if value.is_negative() {
        return Err(ConversionError(format!("invalid sequence number: {value}")));
    } else if value < ARBITRARY_SEQUENCE_THRESHOLD {
        bitcoin::Sequence::from_height(value as u16)
    } else {
        bitcoin::Sequence::from_seconds_ceil(value as u32)
            .map_err(|e| ConversionError(format!("Failed parsing sequence number: {e}")))?
    };

    Ok(sequence)
}

impl TryFrom<V1GetSubscriptionResponse> for ark_core::server::SubscriptionResponse {
    type Error = ConversionError;

    fn try_from(value: V1GetSubscriptionResponse) -> Result<Self, Self::Error> {
        let txid = value
            .txid
            .ok_or_else(|| ConversionError("Missing txid".to_string()))?
            .parse()
            .map_err(|e| ConversionError(format!("Invalid txid: {e}")))?;

        let new_vtxos = value
            .new_vtxos
            .unwrap_or_default()
            .into_iter()
            .map(ark_core::server::VirtualTxOutPoint::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ConversionError(format!("Invalid new_vtxos: {e}")))?;

        let spent_vtxos = value
            .spent_vtxos
            .unwrap_or_default()
            .into_iter()
            .map(ark_core::server::VirtualTxOutPoint::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ConversionError(format!("Invalid spent_vtxos: {e}")))?;

        let tx = if let Some(tx_str) = value.tx.filter(|s| !s.is_empty()) {
            let base64 = base64::engine::GeneralPurpose::new(
                &base64::alphabet::STANDARD,
                base64::engine::GeneralPurposeConfig::new(),
            );
            let bytes = base64
                .decode(&tx_str)
                .map_err(|e| ConversionError(format!("Invalid tx base64: {e}")))?;
            Some(
                Psbt::deserialize(&bytes)
                    .map_err(|e| ConversionError(format!("Invalid tx psbt: {e}")))?,
            )
        } else {
            None
        };

        let checkpoint_txs = value
            .checkpoint_txs
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                let out_point = OutPoint::from_str(&k)
                    .map_err(|e| ConversionError(format!("Invalid checkpoint outpoint: {e}")))?;
                let txid = v
                    .txid
                    .ok_or_else(|| ConversionError("Missing checkpoint txid".to_string()))?
                    .parse()
                    .map_err(|e| ConversionError(format!("Invalid checkpoint txid: {e}")))?;
                Ok((out_point, txid))
            })
            .collect::<Result<HashMap<_, _>, ConversionError>>()?;

        let scripts = value
            .scripts
            .unwrap_or_default()
            .iter()
            .map(|h| {
                ScriptBuf::from_hex(h)
                    .map_err(|e| ConversionError(format!("Invalid script hex: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;

        Ok(ark_core::server::SubscriptionResponse {
            txid,
            scripts,
            new_vtxos,
            spent_vtxos,
            tx,
            checkpoint_txs,
        })
    }
}

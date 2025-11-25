//! Type conversions between generated API types and ark-core types

use crate::models::GetInfoResponse;
use crate::models::GetSubscriptionResponse;
use crate::models::IndexerVtxo;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;
use bitcoin::ScriptBuf;
use bitcoin::Txid;
use bitcoin::base64;
use bitcoin::base64::Engine;
use bitcoin::secp256k1::PublicKey;
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

impl TryFrom<crate::models::IntentFeeInfo> for ark_core::server::IntentFeeInfo {
    type Error = ConversionError;

    fn try_from(value: crate::models::IntentFeeInfo) -> Result<Self, Self::Error> {
        Ok(ark_core::server::IntentFeeInfo {
            offchain_input: ark_core::server::parse_fee_amount(value.offchain_input),
            offchain_output: ark_core::server::parse_fee_amount(value.offchain_output),
            onchain_input: ark_core::server::parse_fee_amount(value.onchain_input),
            onchain_output: ark_core::server::parse_fee_amount(value.onchain_output),
        })
    }
}

impl TryFrom<crate::models::FeeInfo> for ark_core::server::FeeInfo {
    type Error = ConversionError;

    fn try_from(value: crate::models::FeeInfo) -> Result<Self, Self::Error> {
        let intent_fee = value
            .intent_fee
            .map(ark_core::server::IntentFeeInfo::try_from)
            .transpose()?
            .unwrap_or_default();

        let tx_fee_rate = value.tx_fee_rate.unwrap_or_default();

        Ok(ark_core::server::FeeInfo {
            intent_fee,
            tx_fee_rate,
        })
    }
}

impl TryFrom<crate::models::ScheduledSession> for ark_core::server::ScheduledSession {
    type Error = ConversionError;

    fn try_from(value: crate::models::ScheduledSession) -> Result<Self, Self::Error> {
        let next_start_time_str = value
            .next_start_time
            .ok_or_else(|| ConversionError("Missing next_start_time".to_string()))?;
        let next_start_time = i64::from_str(&next_start_time_str)
            .map_err(|e| ConversionError(format!("Could not parse next_start_time: {e:#}")))?;

        let next_end_time_str = value
            .next_end_time
            .ok_or_else(|| ConversionError("Missing next_end_time".to_string()))?;
        let next_end_time = i64::from_str(&next_end_time_str)
            .map_err(|e| ConversionError(format!("Could not parse next_end_time: {e:#}")))?;

        let period_str = value
            .period
            .ok_or_else(|| ConversionError("Missing period".to_string()))?;
        let period = i64::from_str(&period_str)
            .map_err(|e| ConversionError(format!("Could not parse period: {e:#}")))?;

        let duration_str = value
            .duration
            .ok_or_else(|| ConversionError("Missing duration".to_string()))?;
        let duration = i64::from_str(&duration_str)
            .map_err(|e| ConversionError(format!("Could not parse duration: {e:#}")))?;

        let fees = value
            .fees
            .map(ark_core::server::FeeInfo::try_from)
            .transpose()?;

        Ok(ark_core::server::ScheduledSession {
            next_start_time,
            next_end_time,
            period,
            duration,
            fees,
        })
    }
}

impl TryFrom<crate::models::DeprecatedSigner> for ark_core::server::DeprecatedSigner {
    type Error = ConversionError;

    fn try_from(value: crate::models::DeprecatedSigner) -> Result<Self, Self::Error> {
        let pubkey_str = value
            .pubkey
            .ok_or_else(|| ConversionError("Missing pubkey in deprecated signer".to_string()))?;
        let pk = pubkey_str
            .parse::<PublicKey>()
            .map_err(|e| ConversionError(format!("Invalid pubkey '{pubkey_str}': {e}")))?;

        let cutoff_date_str = value.cutoff_date.ok_or_else(|| {
            ConversionError("Missing cutoff_date in deprecated signer".to_string())
        })?;
        let cutoff_date = i64::from_str(&cutoff_date_str)
            .map_err(|e| ConversionError(format!("Could not parse cutoff_date: {e:#}")))?;

        Ok(ark_core::server::DeprecatedSigner { pk, cutoff_date })
    }
}

impl TryFrom<GetInfoResponse> for ark_core::server::Info {
    type Error = ConversionError;

    fn try_from(response: GetInfoResponse) -> Result<Self, Self::Error> {
        // Parse signer_pk
        let signer_pubkey_str = response
            .signer_pubkey
            .ok_or_else(|| ConversionError("Missing signer_pubkey".to_string()))?;
        let signer_pk = signer_pubkey_str.parse::<PublicKey>().map_err(|e| {
            ConversionError(format!("Invalid signer_pubkey '{signer_pubkey_str}': {e}"))
        })?;

        // Parse forfeit_pk
        let forfeit_pubkey_str = response
            .forfeit_pubkey
            .ok_or_else(|| ConversionError("Missing forfeit_pubkey".to_string()))?;
        let forfeit_pk = forfeit_pubkey_str.parse::<PublicKey>().map_err(|e| {
            ConversionError(format!(
                "Invalid forfeit_pubkey '{forfeit_pubkey_str}': {e}"
            ))
        })?;

        // Parse checkpoint_tapscript
        let checkpoint_tapscript_str = response
            .checkpoint_tapscript
            .ok_or_else(|| ConversionError("Missing checkpoint_tapscript".to_string()))?;
        let checkpoint_tapscript = ScriptBuf::from_hex(&checkpoint_tapscript_str).map_err(|e| {
            ConversionError(format!(
                "Invalid checkpoint_tapscript hex '{checkpoint_tapscript_str}': {e}"
            ))
        })?;

        // Parse unilateral_exit_delay
        let unilateral_exit_delay_str = response
            .unilateral_exit_delay
            .ok_or_else(|| ConversionError("Missing unilateral_exit_delay".to_string()))?;
        let unilateral_exit_delay_val = i64::from_str(&unilateral_exit_delay_str).map_err(|e| {
            ConversionError(format!("Could not parse unilateral_exit_delay: {e:#}"))
        })?;
        let unilateral_exit_delay = parse_sequence_number(unilateral_exit_delay_val)?;

        // Parse boarding_exit_delay
        let boarding_exit_delay_str = response
            .boarding_exit_delay
            .ok_or_else(|| ConversionError("Missing boarding_exit_delay".to_string()))?;
        let boarding_exit_delay_val = i64::from_str(&boarding_exit_delay_str)
            .map_err(|e| ConversionError(format!("Could not parse boarding_exit_delay: {e:#}")))?;
        let boarding_exit_delay = parse_sequence_number(boarding_exit_delay_val)?;

        // Parse network
        let network_str = response
            .network
            .ok_or_else(|| ConversionError("Missing network".to_string()))?;
        let network = ark_core::server::Network::from_str(&network_str)
            .map_err(|e| ConversionError(format!("Invalid network '{network_str}': {e}")))?;
        let network = bitcoin::Network::from(network);

        // Parse session_duration
        let session_duration_str = response
            .session_duration
            .ok_or_else(|| ConversionError("Missing session_duration".to_string()))?;
        let session_duration = i64::from_str(&session_duration_str)
            .map_err(|e| ConversionError(format!("Could not parse session_duration: {e:#}")))?
            as u64;

        // Parse dust
        let dust_str = response
            .dust
            .ok_or_else(|| ConversionError("Missing dust".to_string()))?;
        let dust_val = i64::from_str(&dust_str)
            .map_err(|e| ConversionError(format!("Could not parse dust: {e:#}")))?;
        let dust = Amount::from_sat(dust_val as u64);

        // Parse forfeit_address
        let forfeit_address_str = response
            .forfeit_address
            .ok_or_else(|| ConversionError("Missing forfeit_address".to_string()))?;
        let forfeit_address = forfeit_address_str
            .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
            .map_err(|e| {
                ConversionError(format!(
                    "Invalid forfeit_address '{forfeit_address_str}': {e}"
                ))
            })?
            .require_network(network)
            .map_err(|e| {
                ConversionError(format!(
                    "Address network mismatch for '{forfeit_address_str}': {e}"
                ))
            })?;

        // Parse version
        let version = response
            .version
            .ok_or_else(|| ConversionError("Missing version".to_string()))?;

        // Parse digest
        let digest = response.digest.unwrap_or_default();

        // Parse utxo amount limits
        let utxo_min_amount = response
            .utxo_min_amount
            .and_then(|s| i64::from_str(&s).ok())
            .and_then(|val| {
                if val >= 0 {
                    Some(Amount::from_sat(val as u64))
                } else {
                    None
                }
            });

        let utxo_max_amount = response
            .utxo_max_amount
            .and_then(|s| i64::from_str(&s).ok())
            .and_then(|val| {
                if val >= 0 {
                    Some(Amount::from_sat(val as u64))
                } else {
                    None
                }
            });

        let vtxo_min_amount = response
            .vtxo_min_amount
            .and_then(|s| i64::from_str(&s).ok())
            .and_then(|val| {
                if val >= 0 {
                    Some(Amount::from_sat(val as u64))
                } else {
                    None
                }
            });

        let vtxo_max_amount = response
            .vtxo_max_amount
            .and_then(|s| i64::from_str(&s).ok())
            .and_then(|val| {
                if val >= 0 {
                    Some(Amount::from_sat(val as u64))
                } else {
                    None
                }
            });

        // Parse fees
        let fees = response
            .fees
            .map(ark_core::server::FeeInfo::try_from)
            .transpose()?;

        // Parse scheduled_session
        let scheduled_session = response
            .scheduled_session
            .map(ark_core::server::ScheduledSession::try_from)
            .transpose()?;

        // Parse deprecated_signers
        let deprecated_signers = response
            .deprecated_signers
            .unwrap_or_default()
            .into_iter()
            .map(ark_core::server::DeprecatedSigner::try_from)
            .collect::<Result<Vec<_>, _>>()?;

        // Parse service_status
        let service_status = response.service_status.unwrap_or_default();

        Ok(ark_core::server::Info {
            version,
            signer_pk,
            forfeit_pk,
            forfeit_address,
            checkpoint_tapscript,
            network,
            session_duration,
            unilateral_exit_delay,
            boarding_exit_delay,
            utxo_min_amount,
            utxo_max_amount,
            vtxo_min_amount,
            vtxo_max_amount,
            dust,
            fees,
            scheduled_session,
            deprecated_signers,
            service_status,
            digest,
        })
    }
}

impl TryFrom<IndexerVtxo> for ark_core::server::VirtualTxOutPoint {
    type Error = ConversionError;

    fn try_from(value: IndexerVtxo) -> Result<Self, Self::Error> {
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
        let created_at = i64::from_str(&created_at_str)
            .map_err(|e| ConversionError(format!("Could not parse created_at: {e:#}")))?;

        let expires_at_str = value
            .expires_at
            .ok_or_else(|| ConversionError("Missing expires_at".to_string()))?;
        let expires_at = i64::from_str(&expires_at_str)
            .map_err(|e| ConversionError(format!("Could not parse expires_at: {e:#}")))?;

        // Parse amount
        let amount_str = value
            .amount
            .ok_or_else(|| ConversionError("Missing amount".to_string()))?;
        let amount_val = u64::from_str(&amount_str)
            .map_err(|e| ConversionError(format!("Could not parse amount: {e:#}")))?;
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

impl TryFrom<crate::models::IndexerSubscriptionEvent> for ark_core::server::SubscriptionEvent {
    type Error = ConversionError;

    fn try_from(event: crate::models::IndexerSubscriptionEvent) -> Result<Self, Self::Error> {
        // Parse txid
        let txid_str = event
            .txid
            .ok_or_else(|| ConversionError("Missing txid in subscription event".to_string()))?;
        let txid = txid_str
            .parse::<Txid>()
            .map_err(|e| ConversionError(format!("Invalid txid '{txid_str}': {e}")))?;

        // Parse scripts
        let scripts = event
            .scripts
            .unwrap_or_default()
            .iter()
            .map(|h| {
                ScriptBuf::from_hex(h)
                    .map_err(|e| ConversionError(format!("Invalid script hex: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;

        // Parse new_vtxos
        let new_vtxos = event
            .new_vtxos
            .unwrap_or_default()
            .into_iter()
            .map(ark_core::server::VirtualTxOutPoint::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ConversionError(format!("Invalid new_vtxos: {e}")))?;

        // Parse spent_vtxos
        let spent_vtxos = event
            .spent_vtxos
            .unwrap_or_default()
            .into_iter()
            .map(ark_core::server::VirtualTxOutPoint::try_from)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| ConversionError(format!("Invalid spent_vtxos: {e}")))?;

        // Parse tx (PSBT)
        let tx = if let Some(tx_str) = event.tx.filter(|s| !s.is_empty()) {
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

        // Parse checkpoint_txs
        let checkpoint_txs = event
            .checkpoint_txs
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| {
                let out_point = OutPoint::from_str(&k)
                    .map_err(|e| ConversionError(format!("Invalid checkpoint outpoint: {e}")))?;
                let txid_str = v
                    .txid
                    .ok_or_else(|| ConversionError("Missing checkpoint txid".to_string()))?;
                let txid = txid_str
                    .parse::<Txid>()
                    .map_err(|e| ConversionError(format!("Invalid checkpoint txid: {e}")))?;
                Ok((out_point, txid))
            })
            .collect::<Result<HashMap<_, _>, ConversionError>>()?;

        Ok(ark_core::server::SubscriptionEvent {
            txid,
            scripts,
            new_vtxos,
            spent_vtxos,
            tx,
            checkpoint_txs,
        })
    }
}

impl TryFrom<GetSubscriptionResponse> for ark_core::server::SubscriptionResponse {
    type Error = ConversionError;

    fn try_from(value: GetSubscriptionResponse) -> Result<Self, Self::Error> {
        // Check if it's a heartbeat or an event
        if value.heartbeat.is_some() {
            Ok(ark_core::server::SubscriptionResponse::Heartbeat)
        } else if let Some(event) = value.event {
            let subscription_event = ark_core::server::SubscriptionEvent::try_from(event)?;
            Ok(ark_core::server::SubscriptionResponse::Event(Box::new(
                subscription_event,
            )))
        } else {
            Err(ConversionError(
                "GetSubscriptionResponse must have either event or heartbeat".to_string(),
            ))
        }
    }
}

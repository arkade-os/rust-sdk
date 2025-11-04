use crate::generated;
use crate::Error;
use ark_core::server;
use ark_core::server::parse_sequence_number;
use ark_core::server::DeprecatedSigner;
use ark_core::server::FeeInfo;
use ark_core::server::IntentFeeInfo;
use ark_core::server::ScheduledSession;
use bitcoin::address::NetworkUnchecked;
use bitcoin::hex::FromHex;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use std::str::FromStr;

impl TryFrom<generated::ark::v1::GetInfoResponse> for server::Info {
    type Error = Error;

    fn try_from(value: generated::ark::v1::GetInfoResponse) -> Result<Self, Self::Error> {
        let signer_pk = value.signer_pubkey.parse().map_err(Error::conversion)?;
        let forfeit_pk = value.forfeit_pubkey.parse().map_err(Error::conversion)?;

        let network =
            server::Network::from_str(value.network.as_str()).map_err(Error::conversion)?;
        let network = bitcoin::Network::from(network);

        let forfeit_address: Address<NetworkUnchecked> =
            value.forfeit_address.parse().map_err(Error::conversion)?;

        let forfeit_address = forfeit_address
            .require_network(network)
            .map_err(Error::conversion)?;

        let checkpoint_tapscript = ScriptBuf::from_bytes(
            Vec::from_hex(&value.checkpoint_tapscript).map_err(Error::conversion)?,
        );

        let session_duration = value.session_duration as u64;

        let unilateral_exit_delay =
            parse_sequence_number(value.unilateral_exit_delay).map_err(Error::conversion)?;
        let boarding_exit_delay =
            parse_sequence_number(value.boarding_exit_delay).map_err(Error::conversion)?;

        let utxo_min_amount = match value.utxo_min_amount.is_positive() {
            true => Some(Amount::from_sat(value.utxo_min_amount as u64)),
            false => None,
        };

        let utxo_max_amount = match value.utxo_max_amount.is_positive() {
            true => Some(Amount::from_sat(value.utxo_max_amount as u64)),
            false => None,
        };

        let vtxo_min_amount = match value.vtxo_min_amount.is_positive() {
            true => Some(Amount::from_sat(value.vtxo_min_amount as u64)),
            false => None,
        };

        let vtxo_max_amount = match value.vtxo_max_amount.is_positive() {
            true => Some(Amount::from_sat(value.vtxo_max_amount as u64)),
            false => None,
        };

        let fees = value.fees.map(FeeInfo::from);
        let scheduled_session = value.scheduled_session.map(ScheduledSession::from);

        let deprecated_signers = value
            .deprecated_signers
            .into_iter()
            .map(DeprecatedSigner::try_from)
            .collect::<Result<Vec<_>, Error>>()?;

        Ok(Self {
            version: value.version,
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
            dust: Amount::from_sat(value.dust as u64),
            fees,
            scheduled_session,
            deprecated_signers,
            service_status: value.service_status,
            digest: value.digest,
        })
    }
}

impl From<generated::ark::v1::FeeInfo> for FeeInfo {
    fn from(value: generated::ark::v1::FeeInfo) -> Self {
        FeeInfo {
            intent_fee: value.intent_fee.map(|i| IntentFeeInfo {
                offchain_input: i.offchain_input,
                offchain_output: i.offchain_output,
                onchain_input: i.onchain_input,
                onchain_output: i.onchain_output,
            }),
            tx_fee_rate: value.tx_fee_rate,
        }
    }
}

impl From<generated::ark::v1::ScheduledSession> for ScheduledSession {
    fn from(value: generated::ark::v1::ScheduledSession) -> Self {
        Self {
            next_start_time: value.next_start_time,
            next_end_time: value.next_end_time,
            period: value.period,
            duration: value.duration,
            fees: value.fees.map(FeeInfo::from),
        }
    }
}

impl TryFrom<generated::ark::v1::DeprecatedSigner> for DeprecatedSigner {
    type Error = Error;

    fn try_from(value: generated::ark::v1::DeprecatedSigner) -> Result<Self, Self::Error> {
        let pk = value.pubkey.parse().map_err(Error::conversion)?;

        Ok(Self {
            pk,
            cutoff_date: value.cutoff_date,
        })
    }
}

impl TryFrom<&generated::ark::v1::IndexerVtxo> for server::VirtualTxOutPoint {
    type Error = Error;

    fn try_from(value: &generated::ark::v1::IndexerVtxo) -> Result<Self, Self::Error> {
        let outpoint = value.outpoint.as_ref().expect("outpoint");
        let outpoint = OutPoint {
            txid: outpoint.txid.parse().map_err(Error::conversion)?,
            vout: outpoint.vout,
        };

        let script = ScriptBuf::from_hex(&value.script).map_err(Error::conversion)?;

        let spent_by = match value.spent_by.is_empty() {
            true => None,
            false => Some(value.spent_by.parse().map_err(Error::conversion)?),
        };

        let commitment_txids = value
            .commitment_txids
            .iter()
            .map(|c| c.parse().map_err(Error::conversion))
            .collect::<Result<Vec<_>, Error>>()?;

        let settled_by = match value.settled_by.is_empty() {
            true => None,
            false => Some(value.settled_by.parse().map_err(Error::conversion)?),
        };

        let ark_txid = match value.ark_txid.is_empty() {
            true => None,
            false => Some(value.ark_txid.parse().map_err(Error::conversion)?),
        };

        Ok(Self {
            outpoint,
            created_at: value.created_at,
            expires_at: value.expires_at,
            amount: Amount::from_sat(value.amount),
            script,
            is_preconfirmed: value.is_preconfirmed,
            is_swept: value.is_swept,
            is_unrolled: value.is_unrolled,
            is_spent: value.is_spent,
            spent_by,
            commitment_txids,
            settled_by,
            ark_txid,
        })
    }
}

impl TryFrom<&generated::ark::v1::Vtxo> for server::VirtualTxOutPoint {
    type Error = Error;

    fn try_from(value: &generated::ark::v1::Vtxo) -> Result<Self, Self::Error> {
        let outpoint = value.outpoint.as_ref().expect("outpoint");
        let outpoint = OutPoint {
            txid: outpoint.txid.parse().map_err(Error::conversion)?,
            vout: outpoint.vout,
        };

        let script = ScriptBuf::from_hex(&value.script).map_err(Error::conversion)?;

        let spent_by = match value.spent_by.is_empty() {
            true => None,
            false => Some(value.spent_by.parse().map_err(Error::conversion)?),
        };

        let commitment_txids = value
            .commitment_txids
            .iter()
            .map(|c| c.parse().map_err(Error::conversion))
            .collect::<Result<Vec<_>, Error>>()?;

        let settled_by = match value.settled_by.is_empty() {
            true => None,
            false => Some(value.settled_by.parse().map_err(Error::conversion)?),
        };

        let ark_txid = match value.ark_txid.is_empty() {
            true => None,
            false => Some(value.ark_txid.parse().map_err(Error::conversion)?),
        };

        Ok(Self {
            outpoint,
            created_at: value.created_at,
            expires_at: value.expires_at,
            amount: Amount::from_sat(value.amount),
            script,
            is_preconfirmed: value.is_preconfirmed,
            is_swept: value.is_swept,
            is_unrolled: value.is_unrolled,
            is_spent: value.is_spent,
            spent_by,
            commitment_txids,
            settled_by,
            ark_txid,
        })
    }
}

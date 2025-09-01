use crate::generated;
use crate::Error;
use ark_core::server;
use ark_core::utils::parse_sequence_number;
use bitcoin::address::NetworkUnchecked;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::ScriptBuf;
use std::str::FromStr;

#[derive(Clone, Debug)]
pub enum Network {
    Bitcoin,
    Testnet,
    Testnet4,
    Signet,
    Regtest,
    Mutinynet,
}

impl From<Network> for bitcoin::Network {
    fn from(value: Network) -> Self {
        match value {
            Network::Bitcoin => bitcoin::Network::Bitcoin,
            Network::Testnet => bitcoin::Network::Testnet,
            Network::Testnet4 => bitcoin::Network::Testnet4,
            Network::Signet => bitcoin::Network::Signet,
            Network::Regtest => bitcoin::Network::Regtest,
            Network::Mutinynet => bitcoin::Network::Signet,
        }
    }
}

impl FromStr for Network {
    type Err = String;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "bitcoin" => Ok(Network::Bitcoin),
            "testnet" => Ok(Network::Testnet),
            "testnet4" => Ok(Network::Testnet4),
            "signet" => Ok(Network::Signet),
            "regtest" => Ok(Network::Regtest),
            "mutinynet" => Ok(Network::Mutinynet),
            _ => Err(format!("Unsupported network {}", s.to_owned())),
        }
    }
}

impl TryFrom<generated::ark::v1::GetInfoResponse> for server::Info {
    type Error = Error;

    fn try_from(value: generated::ark::v1::GetInfoResponse) -> Result<Self, Self::Error> {
        let pk = value.signer_pubkey.parse().map_err(Error::conversion)?;

        let vtxo_tree_expiry =
            parse_sequence_number(value.vtxo_tree_expiry).map_err(Error::conversion)?;
        let unilateral_exit_delay =
            parse_sequence_number(value.unilateral_exit_delay).map_err(Error::conversion)?;
        let boarding_exit_delay =
            parse_sequence_number(value.boarding_exit_delay).map_err(Error::conversion)?;

        let network = Network::from_str(value.network.as_str()).map_err(Error::conversion)?;
        let network = bitcoin::Network::from(network);

        let forfeit_address: Address<NetworkUnchecked> =
            value.forfeit_address.parse().map_err(Error::conversion)?;
        let forfeit_address = forfeit_address
            .require_network(network)
            .map_err(Error::conversion)?;

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

        Ok(Self {
            pk,
            vtxo_tree_expiry,
            unilateral_exit_delay,
            boarding_exit_delay,
            round_interval: value.round_interval,
            network,
            dust: Amount::from_sat(value.dust as u64),
            forfeit_address,
            version: value.version,
            utxo_min_amount,
            utxo_max_amount,
            vtxo_min_amount,
            vtxo_max_amount,
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

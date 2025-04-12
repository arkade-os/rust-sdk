use crate::generated;
use crate::Error;
use ark_core::server;
use base64::Engine;
use bitcoin::address::NetworkUnchecked;
use bitcoin::Address;
use bitcoin::Amount;
use bitcoin::OutPoint;
use bitcoin::Psbt;

impl TryFrom<generated::ark::v1::GetInfoResponse> for server::Info {
    type Error = Error;

    fn try_from(value: generated::ark::v1::GetInfoResponse) -> Result<Self, Self::Error> {
        let pk = value.pubkey.parse().map_err(Error::conversion)?;

        let vtxo_tree_expiry = bitcoin::Sequence::from_seconds_ceil(value.vtxo_tree_expiry as u32)
            .map_err(Error::conversion)?;

        let unilateral_exit_delay =
            bitcoin::Sequence::from_seconds_ceil(value.unilateral_exit_delay as u32)
                .map_err(Error::conversion)?;

        let network = value.network.parse().map_err(Error::conversion)?;

        let forfeit_address: Address<NetworkUnchecked> =
            value.forfeit_address.parse().map_err(Error::conversion)?;
        let forfeit_address = forfeit_address
            .require_network(network)
            .map_err(Error::conversion)?;

        Ok(Self {
            pk,
            vtxo_tree_expiry,
            unilateral_exit_delay,
            round_interval: value.round_interval,
            network,
            dust: Amount::from_sat(value.dust as u64),
            boarding_descriptor_template: value.boarding_descriptor_template,
            vtxo_descriptor_templates: value.vtxo_descriptor_templates,
            forfeit_address,
        })
    }
}

impl TryFrom<&generated::ark::v1::Vtxo> for server::VtxoOutPoint {
    type Error = Error;

    fn try_from(value: &generated::ark::v1::Vtxo) -> Result<Self, Self::Error> {
        let outpoint = value.outpoint.as_ref().expect("outpoint");
        let outpoint = OutPoint {
            txid: outpoint.txid.parse().map_err(Error::conversion)?,
            vout: outpoint.vout,
        };

        let spent_by = match value.spent_by.is_empty() {
            true => None,
            false => Some(value.spent_by.parse().map_err(Error::conversion)?),
        };

        let redeem_txs = if value.redeem_tx.is_empty() {
            Vec::new()
        } else {
            let base64 = base64::engine::GeneralPurpose::new(
                &base64::alphabet::STANDARD,
                base64::engine::GeneralPurposeConfig::new(),
            );

            let mut redeem_txs = Vec::new();
            let mut psbt_data = base64.decode(value.redeem_tx.clone()).map_err(Error::conversion)?;
            
            while !psbt_data.is_empty() {
                let psbt = Psbt::deserialize(&psbt_data).map_err(Error::conversion)?;
                redeem_txs.push(psbt.clone());
                
                let consumed = psbt.serialize().len();
                psbt_data = psbt_data.split_off(consumed);
            }
            
            redeem_txs
        };

        Ok(Self {
            outpoint,
            spent: value.spent,
            round_txid: value.round_txid.parse().map_err(Error::conversion)?,
            spent_by,
            expire_at: value.expire_at,
            swept: value.swept,
            is_pending: value.is_pending,
            redeem_txs,
            amount: Amount::from_sat(value.amount),
            pubkey: value.pubkey.clone(),
            created_at: value.created_at,
        })
    }
}

impl From<server::VtxoOutPoint> for generated::ark::v1::Vtxo {
    fn from(value: server::VtxoOutPoint) -> Self {
        let base64 = base64::engine::GeneralPurpose::new(
            &base64::alphabet::STANDARD,
            base64::engine::GeneralPurposeConfig::new(),
        );

        let redeem_tx = if value.redeem_txs.is_empty() {
            String::new()
        } else {
            // Concatenate all redeem transactions into a single base64 string
            let mut combined_psbt = Vec::new();
            for psbt in value.redeem_txs {
                combined_psbt.extend_from_slice(&psbt.serialize());
            }
            base64.encode(combined_psbt)
        };

        Self {
            outpoint: Some(value.outpoint.into()),
            spent: value.spent,
            round_txid: value.round_txid.to_string(),
            spent_by: value.spent_by.map_or_else(String::new, |txid| txid.to_string()),
            expire_at: value.expire_at,
            swept: value.swept,
            is_pending: value.is_pending,
            redeem_tx,
            amount: value.amount.to_sat(),
            pubkey: value.pubkey,
            created_at: value.created_at,
        }
    }
}

impl From<OutPoint> for generated::ark::v1::Outpoint {
    fn from(value: OutPoint) -> Self {
        Self {
            txid: value.txid.to_string(),
            vout: value.vout,
        }
    }
}

use crate::Error;
use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex;
use bitcoin::hex::FromHex;
use bitcoin::Txid;
use serde::Serialize;
use serde::Serializer;
use std::num::NonZeroU64;

pub mod packet;

/// An asset identifier: (genesis_txid, group_index).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AssetId {
    pub txid: Txid,
    pub group_index: u16,
}

impl AssetId {
    fn encode(&self, buf: &mut Vec<u8>) {
        // txid in its canonical display byte order, followed by group_index as LE bytes.
        let mut txid_bytes = self.txid.to_byte_array();
        txid_bytes.reverse();
        buf.extend_from_slice(&txid_bytes);
        buf.extend_from_slice(&self.group_index.to_le_bytes());
    }
}

impl std::fmt::Display for AssetId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}{}",
            self.txid,
            self.group_index.to_le_bytes().to_lower_hex_string()
        )
    }
}

impl Serialize for AssetId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.collect_str(self)
    }
}

impl std::str::FromStr for AssetId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 68 {
            return Err(Error::ad_hoc(format!(
                "invalid asset ID format '{}', expected 68 hex chars (txid + 2-byte LE group index)",
                s
            )));
        }

        let txid: Txid = s[..64]
            .parse()
            .map_err(|e| Error::ad_hoc(format!("invalid txid in asset ID: {}", e)))?;

        let group_index_bytes = <[u8; 2]>::from_hex(&s[64..])
            .map_err(|e| Error::ad_hoc(format!("invalid group index in asset ID: {}", e)))?;
        let group_index = u16::from_le_bytes(group_index_bytes);

        Ok(Self { txid, group_index })
    }
}

/// Control asset configuration to issue new assets.
#[derive(Clone, Debug)]
pub enum ControlAssetConfig {
    /// Issue an asset with a new control asset.
    New {
        /// Number of control asset units to create.
        amount: NonZeroU64,
    },
    /// Issue an asset with an existing control asset.
    Existing {
        /// Control asset ID.
        id: AssetId,
    },
}

impl ControlAssetConfig {
    /// Instantiate control asset config to issue assets with a _new_ control asset.
    ///
    /// # Arguments
    ///
    /// * `amount` - The number of _control_ asset units to issue.
    pub fn new(amount: u64) -> Result<Self, Error> {
        let amount =
            NonZeroU64::new(amount).ok_or(Error::ad_hoc("control asset amount cannot be zero"))?;

        Ok(Self::New { amount })
    }

    /// Instantiate control asset config to issue assets with an _existing_ control asset.
    ///
    /// # Arguments
    ///
    /// * `id` - The existing control asset ID.
    pub fn existing(id: AssetId) -> Self {
        Self::Existing { id }
    }
}

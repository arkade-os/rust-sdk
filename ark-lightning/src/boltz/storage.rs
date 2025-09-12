//! Boltz Storage module Module
//!
//! This module allow to make persistance of the Boltz swap data
//! and allow async execution of the swap
//!
//! Author: Vincenzo Palazzo <vincenzopalazzodev@gmail.com>

use super::boltz_ws::PersistedSwap;
use nosql_db::NoSQL;
use std::future::Future;
use std::sync::Arc;

pub struct SwapStorageOptions {
    pub path: String,
}

pub struct SwapStorageData {
    pub key: String,
    pub value: PersistedSwap,
}

/// SwapStorage trait that define the API
/// to store and swap data.
pub trait SwapStorage {
    /// Save the swap data
    fn save_swap(
        &self,
        swap: PersistedSwap,
    ) -> impl Future<Output = anyhow::Result<SwapStorageData>> + Send;

    /// Delete the swap data
    fn delete_swap(
        &self,
        swap_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<PersistedSwap>>> + Send;

    /// Pending Swaps
    ///
    /// Fixme: this should implement the paginator!
    fn pending_swaps(&self) -> impl Future<Output = anyhow::Result<Vec<SwapStorageData>>> + Send;

    fn get_swap(
        &self,
        swap_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<PersistedSwap>>> + Send;
}

const SWAP_PREFIX: &str = "swap";
/// NoSql storage implementation
pub struct NoSqlStorage {
    inner: Arc<nosql_sled::SledDB>,
}

impl NoSqlStorage {
    /// Create a new instance of the NoSqlStorage
    pub fn new(opts: SwapStorageOptions) -> Result<Self, nosql_sled::Error> {
        let db = nosql_sled::SledDB::new(&opts.path)?;
        Ok(Self {
            inner: Arc::new(db),
        })
    }

    fn create_internal_key(swap: PersistedSwap) -> String {
        format!("{SWAP_PREFIX}/{}", swap.id)
    }
}

impl SwapStorage for NoSqlStorage {
    fn save_swap(
        &self,
        swap: PersistedSwap,
    ) -> impl Future<Output = anyhow::Result<SwapStorageData>> + Send {
        let db = self.inner.clone();
        let key = Self::create_internal_key(swap.clone());
        async move {
            let data = serde_json::to_string(&swap)
                .map_err(|e| anyhow::anyhow!("Failed to serialize swap: {}", e))?;
            db.put(&key, &data)?;
            Ok(SwapStorageData { key, value: swap })
        }
    }

    fn delete_swap(
        &self,
        swap_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<PersistedSwap>>> + Send {
        let db = self.inner.clone();
        let swap_id = swap_id.to_string();
        async move {
            let data = nosql_sled::SledDB::drop(&db, &swap_id)?;
            let data = if let Some(data) = data {
                serde_json::from_str::<PersistedSwap>(&data).ok()
            } else {
                None
            };
            Ok(data)
        }
    }

    fn pending_swaps(&self) -> impl Future<Output = anyhow::Result<Vec<SwapStorageData>>> + Send {
        let db = self.inner.clone();
        async move {
            let keys = db.keys();
            let swaps = keys
                .into_iter()
                .filter_map(|k| {
                    let data = db.get(&k).ok()?;
                    let swap = serde_json::from_str::<PersistedSwap>(&data).ok()?;
                    Some(SwapStorageData {
                        key: k,
                        value: swap,
                    })
                })
                .collect::<Vec<_>>();
            Ok(swaps)
        }
    }

    fn get_swap(
        &self,
        swap_id: &str,
    ) -> impl Future<Output = anyhow::Result<Option<PersistedSwap>>> + Send {
        let db = self.inner.clone();
        let swap_id = swap_id.to_string();
        async move {
            let data = db.get(&swap_id).ok();
            let data = if let Some(data) = data {
                serde_json::from_str::<PersistedSwap>(&data).ok()
            } else {
                None
            };
            Ok(data)
        }
    }
}

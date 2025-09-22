use crate::boltz::{SwapData, SwapStatus};
use crate::Error;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::SwapStorage;

/// In-memory implementation of [`SwapStorage`].
///
/// This implementation stores swap data in memory using a [`HashMap`] protected by a [`Mutex`].
/// Data is lost when the application restarts, making this suitable for development, testing,
/// and scenarios where persistence is not required.
pub struct InMemorySwapStorage {
    swaps: Arc<Mutex<HashMap<String, SwapData>>>,
}

impl InMemorySwapStorage {
    /// Create a new in-memory swap storage.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use ark_client::InMemorySwapStorage;
    ///
    /// let storage = InMemorySwapStorage::new();
    /// ```
    pub fn new() -> Self {
        Self {
            swaps: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl Default for InMemorySwapStorage {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SwapStorage for InMemorySwapStorage {
    async fn insert(&self, id: String, data: SwapData) -> Result<(), Error> {
        let mut swaps = self
            .swaps
            .lock()
            .map_err(|e| Error::ad_hoc(format!("failed to acquire lock: {}", e)))?;
        swaps.insert(id, data);
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<SwapData>, Error> {
        let swaps = self
            .swaps
            .lock()
            .map_err(|e| Error::ad_hoc(format!("failed to acquire lock: {}", e)))?;
        Ok(swaps.get(id).cloned())
    }

    async fn update_status(&self, id: &str, status: SwapStatus) -> Result<(), Error> {
        let mut swaps = self
            .swaps
            .lock()
            .map_err(|e| Error::ad_hoc(format!("failed to acquire lock: {}", e)))?;
        if let Some(swap) = swaps.get_mut(id) {
            swap.status = status;
            Ok(())
        } else {
            Err(Error::ad_hoc(format!("swap not found: {}", id)))
        }
    }

    async fn list_all(&self) -> Result<Vec<SwapData>, Error> {
        let swaps = self
            .swaps
            .lock()
            .map_err(|e| Error::ad_hoc(format!("failed to acquire lock: {}", e)))?;
        Ok(swaps.values().cloned().collect())
    }

    async fn remove(&self, id: &str) -> Result<Option<SwapData>, Error> {
        let mut swaps = self
            .swaps
            .lock()
            .map_err(|e| Error::ad_hoc(format!("failed to acquire lock: {}", e)))?;
        Ok(swaps.remove(id))
    }
}

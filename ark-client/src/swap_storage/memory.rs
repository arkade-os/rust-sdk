use super::SwapStorage;
use crate::Error;
use crate::boltz::ReverseSwapData;
use crate::boltz::SubmarineSwapData;
use crate::boltz::SwapStatus;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

/// In-memory implementation of [`SwapStorage`].
///
/// This implementation stores swap data in memory using a [`HashMap`] protected by a [`Mutex`].
/// Data is lost when the application restarts, making this suitable for development, testing,
/// and scenarios where persistence is not required.
pub struct InMemorySwapStorage {
    submarine_swaps: Arc<Mutex<HashMap<String, SubmarineSwapData>>>,
    reverse_swaps: Arc<Mutex<HashMap<String, ReverseSwapData>>>,
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
            submarine_swaps: Arc::new(Mutex::new(HashMap::new())),
            reverse_swaps: Arc::new(Mutex::new(HashMap::new())),
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
    async fn insert_submarine(&self, id: String, data: SubmarineSwapData) -> Result<(), Error> {
        let mut swaps = self.submarine_swaps.lock().expect("lock");
        swaps.insert(id, data);
        Ok(())
    }

    async fn insert_reverse(&self, id: String, data: ReverseSwapData) -> Result<(), Error> {
        let mut swaps = self.reverse_swaps.lock().expect("lock");
        swaps.insert(id, data);
        Ok(())
    }

    async fn get_submarine(&self, id: &str) -> Result<Option<SubmarineSwapData>, Error> {
        let swaps = self.submarine_swaps.lock().expect("lock");
        Ok(swaps.get(id).cloned())
    }

    async fn get_reverse(&self, id: &str) -> Result<Option<ReverseSwapData>, Error> {
        let swaps = self.reverse_swaps.lock().expect("lock");
        Ok(swaps.get(id).cloned())
    }

    async fn update_status_submarine(&self, id: &str, status: SwapStatus) -> Result<(), Error> {
        let mut swaps = self.submarine_swaps.lock().expect("lock");
        if let Some(swap) = swaps.get_mut(id) {
            swap.status = status;
            Ok(())
        } else {
            Err(Error::consumer(format!("swap not found: {id}")))
        }
    }

    async fn update_status_reverse(&self, id: &str, status: SwapStatus) -> Result<(), Error> {
        let mut swaps = self.reverse_swaps.lock().expect("lock");
        if let Some(swap) = swaps.get_mut(id) {
            swap.status = status;
            Ok(())
        } else {
            Err(Error::consumer(format!("swap not found: {id}")))
        }
    }

    async fn update_reverse(&self, id: &str, data: ReverseSwapData) -> Result<(), Error> {
        let mut swaps = self.reverse_swaps.lock().expect("lock");
        swaps.insert(id.to_string(), data);
        Ok(())
    }

    async fn list_all_submarine(&self) -> Result<Vec<SubmarineSwapData>, Error> {
        let swaps = self.submarine_swaps.lock().expect("lock");
        Ok(swaps.values().cloned().collect())
    }

    async fn list_all_reverse(&self) -> Result<Vec<ReverseSwapData>, Error> {
        let swaps = self.reverse_swaps.lock().expect("lock");

        Ok(swaps.values().cloned().collect())
    }

    async fn remove_submarine(&self, id: &str) -> Result<Option<SubmarineSwapData>, Error> {
        let mut swaps = self.submarine_swaps.lock().expect("lock");
        Ok(swaps.remove(id))
    }

    async fn remove_reverse(&self, id: &str) -> Result<Option<ReverseSwapData>, Error> {
        let mut swaps = self.reverse_swaps.lock().expect("lock");
        Ok(swaps.remove(id))
    }
}

//! # Swap Storage
//!
//! The Ark client supports pluggable swap storage implementations, allowing you to persist
//! swap data using your preferred storage backend while providing an in-memory fallback by default.
//!
//! ## Available Implementations
//!
//! - [`InMemorySwapStorage`] - Default in-memory implementation for development and testing
//! - [`SqliteSwapStorage`] - SQLite-based persistent implementation for production use
use crate::boltz::ReverseSwapData;
use crate::boltz::SubmarineSwapData;
use crate::boltz::SwapStatus;
use crate::Error;
use async_trait::async_trait;

mod memory;
mod sqlite;

pub use memory::InMemorySwapStorage;
pub use sqlite::SqliteSwapStorage;

/// Trait for storing and retrieving swap data.
///
/// This trait provides a pluggable interface for swap persistence, allowing different
/// storage backends to be used with the Ark client.
#[async_trait]
pub trait SwapStorage: Send + Sync {
    /// Store submarine swap data.
    ///
    /// # Arguments
    /// * `id` - Unique identifier for the swap
    /// * `data` - The swap data to store
    ///
    /// # Errors
    /// Returns an error if the swap cannot be stored (e.g., database error, duplicate ID).
    async fn insert_submarine(&self, id: String, data: SubmarineSwapData) -> Result<(), Error>;

    /// Store reverse submarine swap data.
    ///
    /// # Arguments
    /// * `id` - Unique identifier for the swap
    /// * `data` - The swap data to store
    ///
    /// # Errors
    /// Returns an error if the swap cannot be stored (e.g., database error, duplicate ID).
    async fn insert_reverse(&self, id: String, data: ReverseSwapData) -> Result<(), Error>;

    /// Retrieve submarine swap data by ID.
    ///
    /// # Arguments
    /// * `id` - The unique identifier of the swap to retrieve
    ///
    /// # Returns
    /// * `Ok(Some(data))` if the swap exists
    /// * `Ok(None)` if the swap does not exist
    /// * `Err(error)` if there was an error accessing storage
    async fn get_submarine(&self, id: &str) -> Result<Option<SubmarineSwapData>, Error>;

    /// Retrieve reverse submarine swap data by ID.
    ///
    /// # Arguments
    /// * `id` - The unique identifier of the swap to retrieve
    ///
    /// # Returns
    /// * `Ok(Some(data))` if the swap exists
    /// * `Ok(None)` if the swap does not exist
    /// * `Err(error)` if there was an error accessing storage
    async fn get_reverse(&self, id: &str) -> Result<Option<ReverseSwapData>, Error>;

    /// Update the status of an existing submarine swap.
    ///
    /// This is a convenience method that retrieves the swap, updates its status,
    /// and saves it back to storage.
    ///
    /// # Arguments
    /// * `id` - The unique identifier of the swap to update
    /// * `status` - The new status to set
    ///
    /// # Errors
    /// Returns an error if the swap doesn't exist or cannot be updated.
    async fn update_status_submarine(&self, id: &str, status: SwapStatus) -> Result<(), Error>;

    /// Update the status of an existing reverse submarine swap.
    ///
    /// This is a convenience method that retrieves the swap, updates its status,
    /// and saves it back to storage.
    ///
    /// # Arguments
    /// * `id` - The unique identifier of the swap to update
    /// * `status` - The new status to set
    ///
    /// # Errors
    /// Returns an error if the swap doesn't exist or cannot be updated.
    async fn update_status_reverse(&self, id: &str, status: SwapStatus) -> Result<(), Error>;

    /// Update an existing reverse submarine swap.
    ///
    /// # Arguments
    /// * `id` - The unique identifier of the swap to update
    /// * `data` - The new swap data to set
    ///
    /// # Errors
    /// Returns an error if the swap doesn't exist or cannot be updated.
    async fn update_reverse(&self, id: &str, data: ReverseSwapData) -> Result<(), Error>;

    /// List all stored submarine swaps.
    ///
    /// # Returns
    /// A vector containing all swap data. The order may vary by implementation.
    ///
    /// # Errors
    /// Returns an error if the swaps cannot be retrieved from storage.
    async fn list_all_submarine(&self) -> Result<Vec<SubmarineSwapData>, Error>;

    /// List all stored reverse submarine swaps.
    ///
    /// # Returns
    /// A vector containing all swap data. The order may vary by implementation.
    ///
    /// # Errors
    /// Returns an error if the swaps cannot be retrieved from storage.
    async fn list_all_reverse(&self) -> Result<Vec<ReverseSwapData>, Error>;

    /// Remove submarine swap data by ID.
    ///
    /// # Arguments
    /// * `id` - The unique identifier of the swap to remove
    ///
    /// # Returns
    /// * `Ok(Some(data))` if the swap existed and was removed
    /// * `Ok(None)` if the swap did not exist
    /// * `Err(error)` if there was an error accessing storage
    async fn remove_submarine(&self, id: &str) -> Result<Option<SubmarineSwapData>, Error>;

    /// Remove reverse submarine swap data by ID.
    ///
    /// # Arguments
    /// * `id` - The unique identifier of the swap to remove
    ///
    /// # Returns
    /// * `Ok(Some(data))` if the swap existed and was removed
    /// * `Ok(None)` if the swap did not exist
    /// * `Err(error)` if there was an error accessing storage
    async fn remove_reverse(&self, id: &str) -> Result<Option<ReverseSwapData>, Error>;
}

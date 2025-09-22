use crate::boltz::{SwapData, SwapStatus};
use crate::Error;
use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Pool, Row, Sqlite};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::SwapStorage;

// TODO: move this into its own crate?
/// SQLite-based persistent implementation of [`SwapStorage`].
pub struct SqliteSwapStorage {
    pool: Pool<Sqlite>,
}

impl SqliteSwapStorage {
    /// Create a new SQLite swap storage with the specified database file path.
    ///
    /// The database file and parent directories will be created if they don't exist.
    /// Database migrations will be automatically applied.
    ///
    /// # Arguments
    /// * `db_path` - Path to the SQLite database file
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The database file cannot be created or opened
    /// - Database migrations fail
    /// - Parent directory cannot be created
    pub async fn new<P: AsRef<Path>>(db_path: P) -> Result<Self, Error> {
        let db_path = db_path.as_ref();

        // Create parent directory if it doesn't exist
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::ad_hoc(format!("Failed to create database directory: {}", e))
            })?;
        }

        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|e| Error::ad_hoc(format!("Failed to connect to database: {}", e)))?;

        // Run migrations
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| Error::ad_hoc(format!("Failed to run migrations: {}", e)))?;

        Ok(Self { pool })
    }

    /// Create a new SQLite swap storage with the default database path.
    ///
    /// The default path is `./swaps.db` in the current working directory.
    /// This is convenient for development and single-instance deployments.
    ///
    /// # Errors
    ///
    /// Returns an error if the database cannot be created or opened in the current directory.
    pub async fn new_default() -> Result<Self, Error> {
        let db_path = Path::new("swaps.db");
        Self::new(db_path).await
    }

    fn current_timestamp() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }
}

#[async_trait]
impl SwapStorage for SqliteSwapStorage {
    async fn insert(&self, id: String, data: SwapData) -> Result<(), Error> {
        let data_json = serde_json::to_string(&data)
            .map_err(|e| Error::ad_hoc(format!("Failed to serialize swap data: {}", e)))?;

        let now = Self::current_timestamp();

        sqlx::query("INSERT INTO swaps (id, data, created_at, updated_at) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(&data_json)
            .bind(now)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::ad_hoc(format!("Failed to insert swap: {}", e)))?;

        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Option<SwapData>, Error> {
        let row: Option<SqliteRow> = sqlx::query("SELECT data FROM swaps WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| Error::ad_hoc(format!("Failed to query swap: {}", e)))?;

        match row {
            Some(row) => {
                let data: String = row.get("data");
                let swap_data: SwapData = serde_json::from_str(&data).map_err(|e| {
                    Error::ad_hoc(format!("Failed to deserialize swap data: {}", e))
                })?;
                Ok(Some(swap_data))
            }
            None => Ok(None),
        }
    }

    async fn update_status(&self, id: &str, status: SwapStatus) -> Result<(), Error> {
        // First, get the existing swap
        let mut swap_data = self
            .get(id)
            .await?
            .ok_or_else(|| Error::ad_hoc(format!("Swap not found: {}", id)))?;

        // Update the status
        swap_data.status = status;

        // Serialize and save back
        let data_json = serde_json::to_string(&swap_data)
            .map_err(|e| Error::ad_hoc(format!("Failed to serialize swap data: {}", e)))?;

        let now = Self::current_timestamp();

        let result = sqlx::query("UPDATE swaps SET data = ?, updated_at = ? WHERE id = ?")
            .bind(&data_json)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::ad_hoc(format!("Failed to update swap: {}", e)))?;

        if result.rows_affected() == 0 {
            return Err(Error::ad_hoc(format!("Swap not found: {}", id)));
        }

        Ok(())
    }

    async fn list_all(&self) -> Result<Vec<SwapData>, Error> {
        let rows: Vec<SqliteRow> = sqlx::query("SELECT data FROM swaps ORDER BY created_at ASC")
            .fetch_all(&self.pool)
            .await
            .map_err(|e| Error::ad_hoc(format!("Failed to list swaps: {}", e)))?;

        let mut swaps = Vec::new();
        for row in rows {
            let data: String = row.get("data");
            let swap_data: SwapData = serde_json::from_str(&data)
                .map_err(|e| Error::ad_hoc(format!("Failed to deserialize swap data: {}", e)))?;
            swaps.push(swap_data);
        }

        Ok(swaps)
    }

    async fn remove(&self, id: &str) -> Result<Option<SwapData>, Error> {
        // First get the swap data to return it
        let swap_data = self.get(id).await?;

        if swap_data.is_some() {
            let result = sqlx::query("DELETE FROM swaps WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(|e| Error::ad_hoc(format!("Failed to delete swap: {}", e)))?;

            if result.rows_affected() == 0 {
                return Ok(None);
            }
        }

        Ok(swap_data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boltz::{SwapMetadata, SwapStatus, SwapType};
    use bitcoin::hashes::{sha256, Hash};
    use bitcoin::PublicKey;
    use std::str::FromStr;
    use tempfile::TempDir;

    fn create_test_swap_data(id: &str) -> SwapData {
        SwapData {
            id: id.to_string(),
            swap_type: SwapType::Reverse,
            status: SwapStatus::Created,
            created_at: 1234567890,
            metadata: SwapMetadata::Reverse {
                preimage: [1u8; 32],
                preimage_hash: sha256::Hash::from_slice(&[2u8; 32]).unwrap(),
                refund_public_key: PublicKey::from_str(
                    "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
                )
                .unwrap(),
                lockup_address: "bc1qtest".to_string(),
                timeout_block_heights: crate::boltz::TimeoutBlockHeights {
                    refund: 144,
                    unilateral_claim: 24,
                    unilateral_refund: 144,
                    unilateral_refund_without_receiver: 288,
                },
                onchain_amount: 100000,
                invoice: "lnbc1test".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn test_sqlite_storage_basic_operations() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let storage = SqliteSwapStorage::new(&db_path).await.unwrap();

        // Test insert and get
        let swap1 = create_test_swap_data("swap1");
        storage
            .insert("swap1".to_string(), swap1.clone())
            .await
            .unwrap();

        let retrieved = storage.get("swap1").await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, swap1.id);
        assert_eq!(retrieved.status, swap1.status);

        // Test get non-existent
        let non_existent = storage.get("nonexistent").await.unwrap();
        assert!(non_existent.is_none());

        // Test list_all
        let swap2 = create_test_swap_data("swap2");
        storage
            .insert("swap2".to_string(), swap2.clone())
            .await
            .unwrap();

        let all_swaps = storage.list_all().await.unwrap();
        assert_eq!(all_swaps.len(), 2);

        // Test update_status
        storage
            .update_status("swap1", SwapStatus::InvoicePaid)
            .await
            .unwrap();
        let updated = storage.get("swap1").await.unwrap().unwrap();
        assert_eq!(updated.status, SwapStatus::InvoicePaid);

        // Test remove
        let removed = storage.remove("swap1").await.unwrap();
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().id, "swap1");

        let after_remove = storage.get("swap1").await.unwrap();
        assert!(after_remove.is_none());

        let remaining = storage.list_all().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "swap2");
    }
}

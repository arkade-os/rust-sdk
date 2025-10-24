use super::SwapStorage;
use crate::boltz::ReverseSwapData;
use crate::boltz::SubmarineSwapData;
use crate::boltz::SwapStatus;
use crate::Error;
use async_trait::async_trait;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::sqlite::SqliteRow;
use sqlx::Pool;
use sqlx::Row;
use sqlx::Sqlite;
use std::path::Path;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

// TODO: move this into its own crate?
/// SQLite-based persistent implementation of [`SwapStorage`].
#[derive(Clone)]
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
                Error::consumer(format!("Failed to create database directory: {e}"))
            })?;
        }

        let options = SqliteConnectOptions::new()
            .filename(db_path)
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|e| Error::consumer(format!("Failed to connect to database: {e}")))?;

        // Run migrations
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| Error::consumer(format!("Failed to run migrations: {e}")))?;

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
            .expect("valid duration")
            .as_secs() as i64
    }
}

#[async_trait]
impl SwapStorage for SqliteSwapStorage {
    async fn insert_submarine(&self, id: String, data: SubmarineSwapData) -> Result<(), Error> {
        let data_json = serde_json::to_string(&data).map_err(|e| {
            Error::consumer(format!("Failed to serialize submarine swap data: {e}"))
        })?;

        let now = Self::current_timestamp();

        sqlx::query(
            "INSERT INTO submarine_swaps (id, data, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&data_json)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::consumer(format!("Failed to insert submarine swap: {e}")))?;

        Ok(())
    }

    async fn insert_reverse(&self, id: String, data: ReverseSwapData) -> Result<(), Error> {
        let data_json = serde_json::to_string(&data)
            .map_err(|e| Error::consumer(format!("Failed to serialize reverse swap data: {e}")))?;

        let now = Self::current_timestamp();

        sqlx::query(
            "INSERT INTO reverse_swaps (id, data, created_at, updated_at) VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&data_json)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| Error::consumer(format!("Failed to insert reverse swap: {e}")))?;

        Ok(())
    }

    async fn get_submarine(&self, id: &str) -> Result<Option<SubmarineSwapData>, Error> {
        let row: Option<SqliteRow> = sqlx::query("SELECT data FROM submarine_swaps WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| Error::consumer(format!("Failed to query submarine swap: {e}")))?;

        match row {
            Some(row) => {
                let data: String = row.get("data");
                let swap_data: SubmarineSwapData = serde_json::from_str(&data).map_err(|e| {
                    Error::consumer(format!("Failed to deserialize submarine swap data: {e}"))
                })?;
                Ok(Some(swap_data))
            }
            None => Ok(None),
        }
    }

    async fn get_reverse(&self, id: &str) -> Result<Option<ReverseSwapData>, Error> {
        let row: Option<SqliteRow> = sqlx::query("SELECT data FROM reverse_swaps WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| Error::consumer(format!("Failed to query reverse swap: {e}")))?;

        match row {
            Some(row) => {
                let data: String = row.get("data");
                let swap_data: ReverseSwapData = serde_json::from_str(&data).map_err(|e| {
                    Error::consumer(format!("Failed to deserialize reverse swap data: {e}"))
                })?;
                Ok(Some(swap_data))
            }
            None => Ok(None),
        }
    }

    async fn update_status_submarine(&self, id: &str, status: SwapStatus) -> Result<(), Error> {
        // First, get the existing swap
        let mut swap_data = self
            .get_submarine(id)
            .await?
            .ok_or_else(|| Error::consumer(format!("Submarine swap not found: {id}")))?;

        // Update the status
        swap_data.status = status;

        // Serialize and save back
        let data_json = serde_json::to_string(&swap_data).map_err(|e| {
            Error::consumer(format!("Failed to serialize submarine swap data: {e}"))
        })?;

        let now = Self::current_timestamp();

        let result =
            sqlx::query("UPDATE submarine_swaps SET data = ?, updated_at = ? WHERE id = ?")
                .bind(&data_json)
                .bind(now)
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(|e| Error::consumer(format!("Failed to update submarine swap: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(Error::consumer(format!("Submarine swap not found: {id}")));
        }

        Ok(())
    }

    async fn update_status_reverse(&self, id: &str, status: SwapStatus) -> Result<(), Error> {
        // First, get the existing swap
        let mut swap_data = self
            .get_reverse(id)
            .await?
            .ok_or_else(|| Error::consumer(format!("Reverse swap not found: {id}")))?;

        // Update the status
        swap_data.status = status;

        // Serialize and save back
        let data_json = serde_json::to_string(&swap_data)
            .map_err(|e| Error::consumer(format!("Failed to serialize reverse swap data: {e}")))?;

        let now = Self::current_timestamp();

        let result = sqlx::query("UPDATE reverse_swaps SET data = ?, updated_at = ? WHERE id = ?")
            .bind(&data_json)
            .bind(now)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| Error::consumer(format!("Failed to update reverse swap: {e}")))?;

        if result.rows_affected() == 0 {
            return Err(Error::consumer(format!("Reverse swap not found: {id}")));
        }

        Ok(())
    }

    async fn list_all_submarine(&self) -> Result<Vec<SubmarineSwapData>, Error> {
        let rows: Vec<SqliteRow> =
            sqlx::query("SELECT data FROM submarine_swaps ORDER BY created_at ASC")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| Error::consumer(format!("Failed to list submarine swaps: {e}")))?;

        let mut swaps = Vec::new();
        for row in rows {
            let data: String = row.get("data");
            let swap_data: SubmarineSwapData = serde_json::from_str(&data).map_err(|e| {
                Error::consumer(format!("Failed to deserialize submarine swap data: {e}"))
            })?;
            swaps.push(swap_data);
        }

        Ok(swaps)
    }

    async fn list_all_reverse(&self) -> Result<Vec<ReverseSwapData>, Error> {
        let rows: Vec<SqliteRow> =
            sqlx::query("SELECT data FROM reverse_swaps ORDER BY created_at ASC")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| Error::consumer(format!("Failed to list reverse swaps: {e}")))?;

        let mut swaps = Vec::new();
        for row in rows {
            let data: String = row.get("data");
            let swap_data: ReverseSwapData = serde_json::from_str(&data).map_err(|e| {
                Error::consumer(format!("Failed to deserialize reverse swap data: {e}"))
            })?;
            swaps.push(swap_data);
        }

        Ok(swaps)
    }

    async fn remove_submarine(&self, id: &str) -> Result<Option<SubmarineSwapData>, Error> {
        // First get the swap data to return it
        let swap_data = self.get_submarine(id).await?;

        if swap_data.is_some() {
            let result = sqlx::query("DELETE FROM submarine_swaps WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(|e| Error::consumer(format!("Failed to delete submarine swap: {e}")))?;

            if result.rows_affected() == 0 {
                return Ok(None);
            }
        }

        Ok(swap_data)
    }

    async fn remove_reverse(&self, id: &str) -> Result<Option<ReverseSwapData>, Error> {
        // First get the swap data to return it
        let swap_data = self.get_reverse(id).await?;

        if swap_data.is_some() {
            let result = sqlx::query("DELETE FROM reverse_swaps WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(|e| Error::consumer(format!("Failed to delete reverse swap: {e}")))?;

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
    use ark_core::ArkAddress;
    use bitcoin::hashes::ripemd160;
    use bitcoin::hashes::Hash;
    use bitcoin::Amount;
    use bitcoin::PublicKey;
    use lightning_invoice::Bolt11Invoice;
    use std::str::FromStr;
    use tempfile::TempDir;

    fn create_test_submarine_swap_data(id: &str) -> SubmarineSwapData {
        SubmarineSwapData {
            id: id.to_string(),
            status: SwapStatus::Created,
            preimage_hash: ripemd160::Hash::from_slice(&[2u8; 20]).unwrap(),
            refund_public_key: PublicKey::from_str(
                "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            )
            .unwrap(),
            claim_public_key: PublicKey::from_str(
                "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            )
            .unwrap(),
            vhtlc_address: ArkAddress::decode("tark1qqellv77udfmr20tun8dvju5vgudpf9vxe8jwhthrkn26fz96pawqfdy8nk05rsmrf8h94j26905e7n6sng8y059z8ykn2j5xcuw4xt846qj6x").unwrap(),
            timeout_block_heights: crate::boltz::TimeoutBlockHeights {
                refund: 144,
                unilateral_claim: 24,
                unilateral_refund: 144,
                unilateral_refund_without_receiver: 288,
            },
            amount: Amount::from_sat(100_000),
            invoice: Bolt11Invoice::from_str("lnbcrt10u1p5d55pjpp56ms94rkev7tdrwqyus5a63lny2mqzq9vh2rq3u4ym3v4lxv6xl4qdql2djkuepqw3hjqs2jfvsxzerywfjhxuccqz95xqztfsp57x0nwf7nzsndjdrvsre570ehg0szw34l284hswdz6zpqvktq9mrs9qxpqysgqllgxhxeny0tvtnxuqgn4s0t2qamc6yqc4t3pe6p2x5lgs8v8r3vxzxp3a3ax9j7d2ta5cduddln8n9se7q0jgg7s0h8t2vhljlu3wkcps9k8xs").unwrap(),
            created_at: 1234567890,
        }
    }

    fn create_test_reverse_swap_data(id: &str) -> ReverseSwapData {
        ReverseSwapData {
            id: id.to_string(),
            status: SwapStatus::Created,
            preimage: Some([1u8; 32]),
            vhtlc_address: ArkAddress::decode("tark1qqellv77udfmr20tun8dvju5vgudpf9vxe8jwhthrkn26fz96pawqfdy8nk05rsmrf8h94j26905e7n6sng8y059z8ykn2j5xcuw4xt846qj6x").unwrap(),
            preimage_hash: ripemd160::Hash::from_slice(&[2u8; 20]).unwrap(),
            refund_public_key: PublicKey::from_str(
                "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            )
            .unwrap(),
            amount: Amount::from_sat(100_000),
            claim_public_key: PublicKey::from_str(
                "0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798",
            )
            .unwrap(),
            timeout_block_heights: crate::boltz::TimeoutBlockHeights {
                refund: 144,
                unilateral_claim: 24,
                unilateral_refund: 144,
                unilateral_refund_without_receiver: 288,
            },
            created_at: 1234567890,
        }
    }

    #[tokio::test]
    async fn test_sqlite_storage_submarine_operations() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let storage = SqliteSwapStorage::new(&db_path).await.unwrap();

        // Test insert and get
        let swap1 = create_test_submarine_swap_data("swap1");
        storage
            .insert_submarine("swap1".to_string(), swap1.clone())
            .await
            .unwrap();

        let retrieved = storage.get_submarine("swap1").await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, swap1.id);
        assert_eq!(retrieved.status, swap1.status);

        // Test get non-existent
        let non_existent = storage.get_submarine("nonexistent").await.unwrap();
        assert!(non_existent.is_none());

        // Test list_all
        let swap2 = create_test_submarine_swap_data("swap2");
        storage
            .insert_submarine("swap2".to_string(), swap2.clone())
            .await
            .unwrap();

        let all_swaps = storage.list_all_submarine().await.unwrap();
        assert_eq!(all_swaps.len(), 2);

        // Test update_status
        storage
            .update_status_submarine("swap1", SwapStatus::InvoicePaid)
            .await
            .unwrap();
        let updated = storage.get_submarine("swap1").await.unwrap().unwrap();
        assert_eq!(updated.status, SwapStatus::InvoicePaid);

        // Test remove
        let removed = storage.remove_submarine("swap1").await.unwrap();
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().id, "swap1");

        let after_remove = storage.get_submarine("swap1").await.unwrap();
        assert!(after_remove.is_none());

        let remaining = storage.list_all_submarine().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "swap2");
    }

    #[tokio::test]
    async fn test_sqlite_storage_reverse_operations() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("test.db");

        let storage = SqliteSwapStorage::new(&db_path).await.unwrap();

        // Test insert and get
        let swap1 = create_test_reverse_swap_data("swap1");
        storage
            .insert_reverse("swap1".to_string(), swap1.clone())
            .await
            .unwrap();

        let retrieved = storage.get_reverse("swap1").await.unwrap();
        assert!(retrieved.is_some());
        let retrieved = retrieved.unwrap();
        assert_eq!(retrieved.id, swap1.id);
        assert_eq!(retrieved.status, swap1.status);

        // Test get non-existent
        let non_existent = storage.get_reverse("nonexistent").await.unwrap();
        assert!(non_existent.is_none());

        // Test list_all
        let swap2 = create_test_reverse_swap_data("swap2");
        storage
            .insert_reverse("swap2".to_string(), swap2.clone())
            .await
            .unwrap();

        let all_swaps = storage.list_all_reverse().await.unwrap();
        assert_eq!(all_swaps.len(), 2);

        // Test update_status
        storage
            .update_status_reverse("swap1", SwapStatus::InvoicePaid)
            .await
            .unwrap();
        let updated = storage.get_reverse("swap1").await.unwrap().unwrap();
        assert_eq!(updated.status, SwapStatus::InvoicePaid);

        // Test remove
        let removed = storage.remove_reverse("swap1").await.unwrap();
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().id, "swap1");

        let after_remove = storage.get_reverse("swap1").await.unwrap();
        assert!(after_remove.is_none());

        let remaining = storage.list_all_reverse().await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, "swap2");
    }
}

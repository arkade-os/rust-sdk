//! Test utilities for ark-grpc.
//!
//! This module is only available when the `test-utils` feature is enabled.

// Re-export admin service types for direct gRPC access
pub use crate::generated::ark::v1::admin_service_client::AdminServiceClient;
pub use crate::generated::ark::v1::CreateNoteRequest;
pub use crate::generated::ark::v1::CreateNoteResponse;
use ark_core::ArkNote;

/// Default admin service URL (arkd runs admin on port 7071).
pub const DEFAULT_ADMIN_URL: &str = "http://localhost:7071";

/// Create ArkNotes via the admin gRPC API.
///
/// Uses the default admin URL (`http://localhost:7071`).
///
/// # Arguments
/// * `amount_sats` - Amount of each note in satoshis
/// * `quantity` - Number of notes to create
///
/// # Example
/// ```ignore
/// use ark_grpc::test_utils::create_notes;
///
/// let notes = create_notes(100_000, 2).await.unwrap();
/// assert_eq!(notes.len(), 2);
/// assert_eq!(notes[0].value().to_sat(), 100_000);
/// ```
pub async fn create_notes(
    amount_sats: u32,
    quantity: u32,
) -> Result<Vec<ArkNote>, Box<dyn std::error::Error + Send + Sync>> {
    create_notes_with_url(DEFAULT_ADMIN_URL, amount_sats, quantity).await
}

/// Create ArkNotes via the admin gRPC API with a custom URL.
///
/// # Arguments
/// * `admin_url` - The admin service URL (e.g., "http://localhost:7071")
/// * `amount_sats` - Amount of each note in satoshis
/// * `quantity` - Number of notes to create
pub async fn create_notes_with_url(
    admin_url: &str,
    amount_sats: u32,
    quantity: u32,
) -> Result<Vec<ArkNote>, Box<dyn std::error::Error + Send + Sync>> {
    let mut client = AdminServiceClient::connect(admin_url.to_string()).await?;

    let response = client
        .create_note(CreateNoteRequest {
            amount: amount_sats,
            quantity,
        })
        .await?;

    let notes = response
        .into_inner()
        .notes
        .into_iter()
        .map(|note_str| ArkNote::from_string(&note_str))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(notes)
}

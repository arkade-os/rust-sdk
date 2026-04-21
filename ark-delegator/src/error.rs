/// Errors that can occur when communicating with a delegator service.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("delegator returned error (status {status}): {body}")]
    Server { status: u16, body: String },

    #[error("failed to encode intent: {0}")]
    Intent(#[from] ark_core::Error),
}

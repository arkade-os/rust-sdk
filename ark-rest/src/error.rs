use crate::conversions::ConversionError;
use std::error::Error as StdError;
use std::fmt;

type Source = Box<dyn StdError + Send + Sync + 'static>;

pub struct Error {
    inner: ErrorImpl,
}

struct ErrorImpl {
    kind: Kind,
    source: Option<Source>,
}

#[derive(Debug)]
enum Kind {
    Request,
    Conversion,
    ServerInfoChanged,
}

impl Error {
    fn new(kind: Kind) -> Self {
        Self {
            inner: ErrorImpl { kind, source: None },
        }
    }

    pub(crate) fn with(mut self, source: impl Into<Source>) -> Self {
        self.inner.source = Some(source.into());
        self
    }

    pub(crate) fn request(source: impl Into<Source>) -> Self {
        Error::new(Kind::Request).with(source)
    }

    pub(crate) fn conversion(source: impl Into<Source>) -> Self {
        Error::new(Kind::Conversion).with(source)
    }

    pub(crate) fn server_info_changed(source: impl Into<Source>) -> Self {
        Error::new(Kind::ServerInfoChanged).with(source)
    }

    /// Returns `true` if the failed operation triggered a digest-mismatch refresh of the
    /// cached `/info`. The original request was not retried.
    pub fn is_server_info_changed(&self) -> bool {
        matches!(self.inner.kind, Kind::ServerInfoChanged)
    }

    /// Returns `true` if the server rejected the request because the SDK
    /// version is too old.
    pub fn is_version_mismatch(&self) -> bool {
        self.source_contains_any(&["BUILD_VERSION_TOO_OLD"])
    }

    /// Returns `true` if the server rejected the request because the cached
    /// `/info` digest is stale.
    pub(crate) fn is_digest_mismatch(&self) -> bool {
        matches!(self.inner.kind, Kind::Request)
            && self.source_contains_any(&["DIGEST_MISMATCH", "invalid digest header"])
    }

    fn source_contains_any(&self, markers: &[&str]) -> bool {
        if let Some(source) = &self.inner.source {
            let display = source.to_string();
            let debug = format!("{source:?}");
            return markers
                .iter()
                .any(|marker| display.contains(marker) || debug.contains(marker));
        }
        false
    }

    fn description(&self) -> &str {
        match &self.inner.kind {
            Kind::Request => "request failed",
            Kind::Conversion => "failed to convert between types",
            Kind::ServerInfoChanged => "Ark server info changed while processing the request. Server info was refreshed, but the failed operation was not retried. Rebuild the request and retry if safe",
        }
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_tuple("ark_grpc::Error");

        f.field(&self.inner.kind);

        if let Some(source) = &self.inner.source {
            f.field(source);
        }

        f.finish()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.description())
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner
            .source
            .as_ref()
            .map(|source| &**source as &(dyn StdError + 'static))
    }
}

impl From<ConversionError> for Error {
    fn from(value: ConversionError) -> Self {
        Error::conversion(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_version_mismatch_true_when_source_contains_marker() {
        let err = Error::request("BUILD_VERSION_TOO_OLD: upgrade your client");
        assert!(err.is_version_mismatch());
    }

    #[test]
    fn is_version_mismatch_false_for_other_errors() {
        let err = Error::request("connection refused");
        assert!(!err.is_version_mismatch());
    }

    #[test]
    fn is_version_mismatch_false_when_no_source() {
        let err = Error::new(Kind::Request);
        assert!(!err.is_version_mismatch());
    }

    #[test]
    fn is_digest_mismatch_true_when_source_contains_marker() {
        let err = Error::request("DIGEST_MISMATCH: invalid digest header");
        assert!(err.is_digest_mismatch());
    }

    #[test]
    fn is_digest_mismatch_true_when_generated_error_content_contains_marker() {
        let source = crate::apis::Error::<()>::ResponseError(crate::apis::ResponseContent {
            status: reqwest::StatusCode::PRECONDITION_FAILED,
            content: "DIGEST_MISMATCH: invalid digest header".to_string(),
            entity: None,
        });
        let err = Error::request(source);
        assert!(err.is_digest_mismatch());
    }

    #[test]
    fn is_digest_mismatch_false_for_other_errors() {
        let err = Error::request("connection refused");
        assert!(!err.is_digest_mismatch());
    }

    #[test]
    fn is_server_info_changed_true_for_server_info_changed_kind() {
        let err = Error::server_info_changed("DIGEST_MISMATCH");
        assert!(err.is_server_info_changed());
    }

    #[test]
    fn server_info_changed_is_not_classified_as_digest_mismatch() {
        let err = Error::server_info_changed(Error::request("DIGEST_MISMATCH"));
        assert!(err.is_server_info_changed());
        assert!(!err.is_digest_mismatch());
    }

    #[test]
    fn is_server_info_changed_false_for_other_errors() {
        let err = Error::request("DIGEST_MISMATCH");
        assert!(!err.is_server_info_changed());
    }
}

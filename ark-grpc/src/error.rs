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
    Connect,
    NotConnected,
    Request,
    Conversion,
    EventStreamDisconnect,
    EventStream,
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

    pub(crate) fn connect(source: impl Into<Source>) -> Self {
        Error::new(Kind::Connect).with(source)
    }

    pub(crate) fn not_connected() -> Self {
        Error::new(Kind::NotConnected)
    }

    pub(crate) fn request(source: impl Into<Source>) -> Self {
        Error::new(Kind::Request).with(source)
    }

    pub(crate) fn conversion(source: impl Into<Source>) -> Self {
        Error::new(Kind::Conversion).with(source)
    }

    pub(crate) fn event_stream_disconnect() -> Self {
        Error::new(Kind::EventStreamDisconnect)
    }

    pub(crate) fn event_stream(source: impl Into<Source>) -> Self {
        Error::new(Kind::EventStream).with(source)
    }

    /// Returns `true` if the server rejected the request because the SDK
    /// version is too old.
    pub fn is_version_mismatch(&self) -> bool {
        if let Some(source) = &self.inner.source {
            if let Some(status) = source.downcast_ref::<tonic::Status>() {
                return status.code() == tonic::Code::FailedPrecondition
                    && status.message().contains("BUILD_VERSION_TOO_OLD");
            }
        }
        false
    }

    fn description(&self) -> &str {
        match &self.inner.kind {
            Kind::Connect => "failed to connect to Ark server",
            Kind::NotConnected => "no connection to Ark server",
            Kind::Request => "request failed",
            Kind::Conversion => "failed to convert between types",
            Kind::EventStreamDisconnect => "got disconnected from event stream",
            Kind::EventStream => "error via event stream",
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
        f.write_str(self.description())?;
        if let Some(source) = self.source() {
            f.write_str(&source.to_string())?;
        }

        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_version_mismatch_true_for_matching_status() {
        let status = tonic::Status::failed_precondition("BUILD_VERSION_TOO_OLD");
        let err = Error::request(status);
        assert!(err.is_version_mismatch());
    }

    #[test]
    fn is_version_mismatch_false_for_other_failed_precondition() {
        let status = tonic::Status::failed_precondition("something else");
        let err = Error::request(status);
        assert!(!err.is_version_mismatch());
    }

    #[test]
    fn is_version_mismatch_false_for_other_code() {
        let status = tonic::Status::internal("BUILD_VERSION_TOO_OLD");
        let err = Error::request(status);
        assert!(!err.is_version_mismatch());
    }

    #[test]
    fn is_version_mismatch_false_for_non_tonic_error() {
        let err = Error::request("some string error");
        assert!(!err.is_version_mismatch());
    }

    #[test]
    fn is_version_mismatch_false_when_no_source() {
        let err = Error::not_connected();
        assert!(!err.is_version_mismatch());
    }
}

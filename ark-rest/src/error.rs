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

    /// Returns `true` if the server rejected the request because the SDK
    /// version is too old.
    pub fn is_version_mismatch(&self) -> bool {
        if let Some(source) = &self.inner.source {
            return source.to_string().contains("BUILD_VERSION_TOO_OLD");
        }
        false
    }

    fn description(&self) -> &str {
        match &self.inner.kind {
            Kind::Request => "request failed",
            Kind::Conversion => "failed to convert between types",
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
}

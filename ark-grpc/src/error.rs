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

    pub(crate) fn empty_response() -> Self {
        Error::new(Kind::Conversion).with("empty response from server")
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

use std::error::Error as StdError;
use std::fmt;

type Source = Box<dyn StdError + Send + Sync + 'static>;

#[derive(Debug)]
pub struct Error {
    inner: Box<ErrorImpl>,
}

#[derive(Debug)]
struct ErrorImpl {
    kind: Kind,
    cause: Option<Error>,
}

#[derive(Debug)]
enum Kind {
    /// Ad-hoc error,
    AdHoc(AdHocError),
    /// An error related to interactions with the Ark server.
    ArkServer(ArkServerError),
    /// An error from [`ark_core`].
    Core(CoreError),
    /// An error related to coin selection of VTXOs and boarding outputs.
    CoinSelect(CoinSelectError),
    /// An error related to actions within the wallet.
    Wallet(WalletError),
    /// An error thrown by a user of this library
    Consumer(ConsumerError),
}

#[derive(Debug)]
struct AdHocError {
    source: Source,
}

#[derive(Debug)]
struct ArkServerError {
    source: Source,
}

#[derive(Debug)]
struct CoreError {
    source: ark_core::Error,
}

#[derive(Debug)]
struct CoinSelectError {
    source: Source,
}

#[derive(Debug)]
struct WalletError {
    source: Source,
}

#[derive(Debug)]
struct ConsumerError {
    source: Source,
}

impl Error {
    fn new(kind: Kind) -> Self {
        Self {
            inner: Box::new(ErrorImpl { kind, cause: None }),
        }
    }

    pub(crate) fn ad_hoc(source: impl Into<Source>) -> Self {
        Error::new(Kind::AdHoc(AdHocError {
            source: source.into(),
        }))
    }

    pub(crate) fn ark_server(source: impl Into<Source>) -> Self {
        Error::new(Kind::ArkServer(ArkServerError {
            source: source.into(),
        }))
    }

    pub(crate) fn coin_select(source: impl Into<Source>) -> Self {
        Error::new(Kind::CoinSelect(CoinSelectError {
            source: source.into(),
        }))
    }

    pub fn wallet(source: impl Into<Source>) -> Self {
        Error::new(Kind::Wallet(WalletError {
            source: source.into(),
        }))
    }

    pub fn consumer(source: impl Into<Source>) -> Self {
        Error::new(Kind::Consumer(ConsumerError {
            source: source.into(),
        }))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut err = self;
        loop {
            write!(f, "{}", err.inner.kind)?;
            err = match err.inner.cause.as_ref() {
                None => break,
                Some(err) => err,
            };
            write!(f, ": ")?;
        }
        Ok(())
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Kind::AdHoc(ref err) => err.fmt(f),
            Kind::ArkServer(ref err) => err.fmt(f),
            Kind::Core(ref err) => err.fmt(f),
            Kind::CoinSelect(ref err) => err.fmt(f),
            Kind::Wallet(ref err) => err.fmt(f),
            Kind::Consumer(ref err) => err.fmt(f),
        }
    }
}

impl fmt::Display for AdHocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl fmt::Display for ArkServerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl fmt::Display for CoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl fmt::Display for CoinSelectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl fmt::Display for WalletError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl fmt::Display for ConsumerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl From<ark_core::Error> for Error {
    fn from(value: ark_core::Error) -> Self {
        Self::new(Kind::Core(CoreError { source: value }))
    }
}

pub trait IntoError {
    fn into_error(self) -> Error;
}

impl IntoError for Error {
    fn into_error(self) -> Error {
        self
    }
}

impl IntoError for &'static str {
    fn into_error(self) -> Error {
        Error::ad_hoc(self)
    }
}

impl IntoError for String {
    fn into_error(self) -> Error {
        Error::ad_hoc(self)
    }
}

/// A trait for contextualizing error values.
///
/// This makes it easy to contextualize either `Error` or `Result<T, Error>`.
/// Specifically, in the latter case, it absolves one of the need to call
/// `map_err` everywhere one wants to add context to an error.
///
/// This trick was borrowed from `jiff`, which borrowed it from `anyhow`.
pub trait ErrorContext {
    /// Contextualize the given consequent error with this (`self`) error as
    /// the cause.
    ///
    /// This is equivalent to saying that "consequent is caused by self."
    ///
    /// Note that if an `Error` is given for `kind`, then this panics if it has
    /// a cause. (Because the cause would otherwise be dropped. An error causal
    /// chain is just a linked list, not a tree.)
    fn context(self, consequent: impl IntoError) -> Self;

    /// Like `context`, but hides error construction within a closure.
    ///
    /// This is useful if the creation of the consequent error is not otherwise
    /// guarded and when error construction is potentially "costly" (i.e., it
    /// allocates). The closure avoids paying the cost of contextual error
    /// creation in the happy path.
    ///
    /// Usually this only makes sense to use on a `Result<T, Error>`, otherwise
    /// the closure is just executed immediately anyway.
    fn with_context<E: IntoError>(self, consequent: impl FnOnce() -> E) -> Self;
}

impl ErrorContext for Error {
    fn context(self, consequent: impl IntoError) -> Error {
        let mut err = consequent.into_error();
        assert!(
            err.inner.cause.is_none(),
            "cause of consequence must be `None`"
        );

        err.inner.cause = Some(self);
        err
    }

    fn with_context<E: IntoError>(self, consequent: impl FnOnce() -> E) -> Error {
        let mut err = consequent().into_error();
        assert!(
            err.inner.cause.is_none(),
            "cause of consequence must be `None`"
        );

        err.inner.cause = Some(self);
        err
    }
}

impl<T> ErrorContext for Result<T, Error> {
    fn context(self, consequent: impl IntoError) -> Result<T, Error> {
        self.map_err(|err| err.context(consequent))
    }

    fn with_context<E: IntoError>(self, consequent: impl FnOnce() -> E) -> Result<T, Error> {
        self.map_err(|err| err.with_context(consequent))
    }
}

impl From<ark_grpc::Error> for Error {
    fn from(value: ark_grpc::Error) -> Self {
        Self::ark_server(value)
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        self.inner.cause.as_ref().map(|e| e as &(dyn StdError + 'static))
    }
}

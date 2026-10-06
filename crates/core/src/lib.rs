pub mod runtime;
pub mod text;
pub mod utils;
pub mod web;

/// The `PluginRegistrar` is defined by the application and passed to `plugin_entry`. It's used
/// for a plugin module to register itself with the application.
pub trait PluginRegistrar {
    fn register_plugin(&mut self, plugin: Box<dyn Plugin>);
}

/// `Plugin` is implemented by a plugin library for one or more types. As you need additional
/// callbacks, they can be defined here. These are first class Rust trait objects, so you have the
/// full flexibility of that system. The main thing you'll lose access to is generics, but that's
/// expected with a plugin system
pub trait Plugin {
    /// This is a callback routine implemented by the plugin.
    fn callback1(&self);
    /// Callbacks can take arguments and return values
    fn callback2(&self, i: i32) -> i32;
}

/// The crate's error type.
///
/// Display text is lowercase. [`Self::Other`] keeps an [`anyhow::Error`] so `.context()` can
/// attach what the caller was doing without dropping the original variant: [`Self::is_cancelled`]
/// and [`Self::is_out_of_range`] still match through that wrapper.
#[derive(thiserror::Error, Debug)]
#[non_exhaustive]
pub enum N2linkError {
    #[error("permission denied")]
    PermissionDenied,

    #[error("invalid flows.json: {0}")]
    BadFlowsJson(String),

    #[error("unsupported flows.json format: {0}")]
    UnsupportedFlowsJsonFormat(String),

    #[error("not supported: {0}")]
    NotSupported(String),

    #[error("invalid argument: {0}")]
    BadArgument(&'static str),

    #[error("task cancelled")]
    TaskCancelled,

    #[error("{0}")]
    InvalidOperation(String),

    #[error("out of range")]
    OutOfRange,

    #[error("invalid configuration")]
    Configuration,

    #[error("timed out")]
    Timeout,

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// An error from outside this enum. Its `Display` is the anyhow chain, context included.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T, E = N2linkError> = std::result::Result<T, E>;

/// Attach context to a failure and return [`N2linkError`].
///
/// Same shape as `anyhow::Context`, but the result is this crate's error. A typed
/// [`N2linkError`] stays recoverable through [`N2linkError::is_cancelled`] and
/// [`N2linkError::is_out_of_range`].
pub trait ErrorContext<T> {
    fn context<C>(self, context: C) -> Result<T>
    where
        C: std::fmt::Display + Send + Sync + 'static;

    fn with_context<C, F>(self, f: F) -> Result<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
        F: FnOnce() -> C;
}

impl<T, E> ErrorContext<T> for std::result::Result<T, E>
where
    E: Into<anyhow::Error>,
{
    fn context<C>(self, context: C) -> Result<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
    {
        self.map_err(|err| {
            let err: anyhow::Error = err.into();
            N2linkError::Other(err.context(context))
        })
    }

    fn with_context<C, F>(self, f: F) -> Result<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        self.map_err(|err| {
            let err: anyhow::Error = err.into();
            N2linkError::Other(err.context(f()))
        })
    }
}

impl<T> ErrorContext<T> for Option<T> {
    fn context<C>(self, context: C) -> Result<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
    {
        self.with_context(|| context)
    }

    fn with_context<C, F>(self, f: F) -> Result<T>
    where
        C: std::fmt::Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        self.ok_or_else(|| N2linkError::Other(anyhow::anyhow!("{}", f())))
    }
}

impl N2linkError {
    pub fn invalid_operation(msg: &str) -> Self {
        N2linkError::InvalidOperation(msg.to_owned())
    }

    /// `TaskCancelled`, including when `.context()` wrapped it in [`Self::Other`].
    pub fn is_cancelled(&self) -> bool {
        match self {
            N2linkError::TaskCancelled => true,
            N2linkError::Other(err) => err.downcast_ref::<N2linkError>().is_some_and(Self::is_cancelled),
            _ => false,
        }
    }

    /// `OutOfRange`, including when `.context()` wrapped it in [`Self::Other`].
    pub fn is_out_of_range(&self) -> bool {
        match self {
            N2linkError::OutOfRange => true,
            N2linkError::Other(err) => err.downcast_ref::<N2linkError>().is_some_and(Self::is_out_of_range),
            _ => false,
        }
    }
}

macro_rules! from_other {
    ($($ty:ty),+ $(,)?) => {$(
        impl From<$ty> for N2linkError {
            fn from(err: $ty) -> Self {
                N2linkError::Other(anyhow::Error::from(err))
            }
        }
    )+};
}

// `?` on these keeps working now that `Result` defaults to `N2linkError` rather than
// `anyhow::Error`. `SendError` is converted by hand because the value it holds does not have to
// be `Send`, and `anyhow` only accepts `'static + Send + Sync` errors.
from_other!(
    serde_json::Error,
    config::ConfigError,
    tokio::sync::TryLockError,
    tokio::time::error::Elapsed,
    regex::Error,
    std::str::ParseBoolError,
    std::num::ParseFloatError,
    std::num::ParseIntError,
    tokio_cron_scheduler::JobSchedulerError
);

#[cfg(feature = "js")]
from_other!(rquickjs::Error);

#[cfg(feature = "nodes_storage_watch")]
from_other!(notify::Error);

#[cfg(feature = "jsonata")]
from_other!(jsonata_core::evaluator::EvaluatorError);

impl<T> From<tokio::sync::mpsc::error::SendError<T>> for N2linkError {
    fn from(err: tokio::sync::mpsc::error::SendError<T>) -> Self {
        N2linkError::Other(anyhow::Error::msg(err.to_string()))
    }
}

impl<T> From<tokio::sync::broadcast::error::SendError<T>> for N2linkError {
    fn from(err: tokio::sync::broadcast::error::SendError<T>) -> Self {
        N2linkError::Other(anyhow::Error::msg(err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::{ErrorContext, N2linkError};

    #[test]
    fn error_display_is_lowercase_and_keeps_context() {
        assert_eq!(N2linkError::PermissionDenied.to_string(), "permission denied");
        assert_eq!(N2linkError::TaskCancelled.to_string(), "task cancelled");
        assert_eq!(N2linkError::OutOfRange.to_string(), "out of range");
        let io = N2linkError::from(std::io::Error::new(std::io::ErrorKind::NotFound, "missing"));
        assert_eq!(io.to_string(), "io error: missing");

        let wrapped = Err::<(), _>(N2linkError::OutOfRange).context("reading key").unwrap_err();
        assert!(wrapped.is_out_of_range(), "{wrapped}");
        assert!(wrapped.to_string().contains("reading key"), "{wrapped}");
        assert!(N2linkError::TaskCancelled.is_cancelled());
        assert!(!N2linkError::OutOfRange.is_cancelled());
    }

    #[ctor::ctor]
    fn initialize_test_logger() {
        let stderr = log4rs::append::console::ConsoleAppender::builder()
            .target(log4rs::append::console::Target::Stdout)
            .encoder(Box::new(log4rs::encode::pattern::PatternEncoder::new("[{h({l})}]\t{m}{n}")))
            .build();

        let config = log4rs::Config::builder()
            .appender(log4rs::config::Appender::builder().build("stderr", Box::new(stderr)))
            .build(log4rs::config::Root::builder().appender("stderr").build(log::LevelFilter::Warn))
            .unwrap();

        let _ = log4rs::init_config(config).unwrap();
    }
}

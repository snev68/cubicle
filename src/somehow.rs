//! This is a thin wrapper around [`anyhow`] to use some of its functionality.
//!
//! The key difference with `anyhow` is that `somehow` does not allow implicit
//! conversions from most error types. The reason is that many error types,
//! such as [`std::io::Error`], don't provide enough context to be directyl
//! useful and end-user error messages.
//!
//! The key thing that `somehow` continues to leverage from `anyhow` is its
//! backtraces. As of Aug 2022, [`std::error::Error::backtrace`] is not yet
//! stabilized.

use std::fmt::{self, Debug, Display};

/// The normal return type for functions that may fail with `somehow`.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// An Error type that tracks backtraces and can be created from some other
/// error types (but not all).
///
/// Instances of this type should provide enough context at a low level. For
/// example, "file not found" would be bad, but "file not found: /dev/null"
/// would be OK. Whether this error provides sufficient context at a higher
/// level of the program is not modeled in the types.
pub struct Error(anyhow::Error);

/// See [`anyhow::Error`].
impl Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

/// See [`anyhow::Error`].
impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.0, f)
    }
}

impl From<anyhow::Error> for Error {
    fn from(error: anyhow::Error) -> Self {
        Self(error)
    }
}

#[allow(unused)]
/// Allows implicit converstions from this error type to `somehow::Error`.
macro_rules! allowed_from {
    ($error:ty) => {
        impl From<$error> for Error {
            fn from(error: $error) -> Self {
                Self(anyhow::Error::from(error))
            }
        }
    };
}

#[allow(unused)]
/// Allows implicit converstions from this error type to `somehow::Error`,
/// but annotates them with `TODO_CONTEXT`.
macro_rules! deprecated_from {
    ($error:ty) => {
        impl From<$error> for Error {
            fn from(error: $error) -> Self {
                Self(anyhow::Error::from(error).context($crate::somehow::TODO_CONTEXT))
            }
        }
    };
}

/// Creates a [`somehow::Error`](Error) from a string with format args or
/// another error of any type.
///
/// Like [`anyhow::anyhow!`] but returns a `somehow::Error`.
#[macro_export]
macro_rules! somehow {
    ($msg:literal $(,)?) => { $crate::somehow::Error::from(anyhow::anyhow!($msg)) };
    ($err:expr $(,)?) => { $crate::somehow::Error::from(anyhow::anyhow!($err)) };
    ($fmt:expr, $($arg:tt)*) => { $crate::somehow::Error::from(anyhow::anyhow!($fmt, $($arg)*)) };
}

#[doc(inline)]
pub use somehow;

/// Used to attach explanatory information to any type of error.
///
/// This is implemented for [`std::result::Result`] types with a wide range of
/// errors.
///
/// This is similar to [`anyhow::Context`] but also includes
/// [`Self::todo_context`] and [`Self::enough_context`].
pub trait Context<T> {
    /// Prepends a static string to explain the underlying error.
    fn context<C>(self, context: C) -> Result<T, Error>
    where
        C: Display + Send + Sync + 'static;

    /// Prepends a dynamic string to explain the underlying error.
    fn with_context<C, F>(self, f: F) -> Result<T, Error>
    where
        C: Display + Send + Sync + 'static,
        F: FnOnce() -> C;

    /// Adds a context that admits the underlying error lacks context.
    ///
    /// This is a temporary aid to help in transitioning to better error
    /// messages.
    fn todo_context(self) -> Result<T, Error>;

    /// Marks the error as having sufficient context.
    ///
    /// This is normally used when the static error type typically lacks
    /// context, but it's sufficient here. This can happen when this error
    /// message is unusually good or when the calling code is known to give it
    /// enough context.
    fn enough_context(self) -> Result<T, Error>;
}

static TODO_CONTEXT: &str = "\
The cause of this error lacks context. You can set RUST_BACKTRACE=1 for more
info. A pull request or a GitHub issue with this output and the steps to
reproduce it would be welcome.";

impl<T> Context<T> for Result<T, Error> {
    fn context<C>(self, context: C) -> Result<T, Error>
    where
        C: fmt::Display + Send + Sync + 'static,
    {
        self.map_err(|err| Error(err.0.context(context)))
    }

    fn with_context<C, F>(self, context: F) -> Result<T, Error>
    where
        C: fmt::Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        self.map_err(|err| Error(err.0.context(context())))
    }

    fn todo_context(self) -> Result<T, Error> {
        self.context(TODO_CONTEXT)
    }

    fn enough_context(self) -> Result<T, Error> {
        self
    }
}

impl<T, E> Context<T> for Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn context<C>(self, context: C) -> Result<T, Error>
    where
        C: fmt::Display + Send + Sync + 'static,
    {
        anyhow::Context::context(self, context).map_err(Error)
    }

    fn with_context<C, F>(self, context: F) -> Result<T, Error>
    where
        C: fmt::Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        anyhow::Context::with_context(self, context).map_err(Error)
    }

    fn todo_context(self) -> Result<T, Error> {
        self.context(TODO_CONTEXT)
    }

    fn enough_context(self) -> Result<T, Error> {
        self.map_err(|e| Error(anyhow::Error::from(e)))
    }
}

// clap wants this
impl From<Error> for Box<dyn std::error::Error + Send + Sync + 'static> {
    fn from(error: Error) -> Self {
        Box::<dyn std::error::Error + Send + Sync + 'static>::from(error.0)
    }
}

#[cfg(test)]
mod tests {
    use super::{Context, Error, Result};
    use insta::assert_snapshot;

    #[derive(Debug)]
    struct MyError;
    impl std::fmt::Display for MyError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("MyError")
        }
    }
    impl std::error::Error for MyError {}
    allowed_from!(MyError);

    #[test]
    fn allowed_from() {
        let make_err = || -> Result<()> {
            #[allow(clippy::try_err)]
            Err(MyError)?
        };
        let err = make_err().unwrap_err();
        let debug = format!("{:?}", err);
        assert_snapshot!(debug, @"MyError");
    }

    #[test]
    fn deprecated_from() {
        deprecated_from!(std::io::Error);
        let make_err = || -> Result<f64> {
            #[allow(clippy::try_err)]
            Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?
        };
        let err = make_err().unwrap_err();
        let debug = format!("{:?}", err);
        assert_snapshot!(debug, @r###"
        The cause of this error lacks context. You can set RUST_BACKTRACE=1 for more
        info. A pull request or a GitHub issue with this output and the steps to
        reproduce it would be welcome.

        Caused by:
            unexpected end of file
        "###);
    }

    #[test]
    fn todo_context() {
        let err: std::io::Result<f64> =
            Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        let err: Error = err.todo_context().unwrap_err();
        assert_snapshot!(format!("{err:?}"), @r###"
        The cause of this error lacks context. You can set RUST_BACKTRACE=1 for more
        info. A pull request or a GitHub issue with this output and the steps to
        reproduce it would be welcome.

        Caused by:
            unexpected end of file
        "###);
    }
}

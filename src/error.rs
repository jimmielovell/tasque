use std::fmt;

/// A handler or [`Store`](crate::Store) error. `?` converts any `std::error::Error`,
/// `&str` or `String` into it.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Why [`Tasque::queue`](crate::Tasque::queue) or [`Builder::run`](crate::Builder::run) failed.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No handler takes this type.
    Unregistered(&'static str),
    /// A handler can hand off to a type no handler takes.
    MissingHandler {
        handler: &'static str,
        next: &'static str,
    },
    /// The job couldn't be serialized.
    Encode(BoxError),
    /// The store failed.
    Store(BoxError),
    /// The `Tasque` is shutting down.
    Stopped,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unregistered(ty) => write!(f, "no handler takes {ty}"),
            Error::MissingHandler { handler, next } => {
                write!(
                    f,
                    "handler {handler:?} can return Step::next({next}), but no handler takes {next}"
                )
            }
            Error::Encode(err) => write!(f, "failed to serialize job: {err}"),
            Error::Store(err) => write!(f, "store failed: {err}"),
            Error::Stopped => write!(f, "tasque is shutting down"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Encode(err) | Error::Store(err) => Some(err.as_ref()),
            _ => None,
        }
    }
}

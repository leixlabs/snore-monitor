//! Crate-wide error type.
//!
//! Every fallible operation in the service funnels into this type so that
//! `main` has one place to decide what is fatal and what is merely logged.
//! Errors that reach the audio or HTTP path are converted to a short string and
//! recorded in [`crate::metrics::Metrics::last_error`]; they never carry audio
//! samples or other private content (TECH_SPEC §12).

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Config(#[from] crate::config::ConfigLoadError),
    #[error("i/o error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("audio capture error: {0}")]
    Audio(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("http server error: {0}")]
    Http(String),
    #[error("{0}")]
    Internal(String),
}

impl Error {
    /// Wraps an [`std::io::Error`] with the path it happened on.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(source: std::io::Error) -> Self {
        Error::Io {
            path: PathBuf::new(),
            source,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

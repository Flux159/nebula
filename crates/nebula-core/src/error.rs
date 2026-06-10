use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid VM spec: {0}")]
    InvalidSpec(String),

    #[error("file not found: {0}")]
    FileNotFound(PathBuf),

    #[error("backend `{backend}` failed: {message}")]
    Backend {
        backend: &'static str,
        message: String,
    },

    #[error("backend `{0}` is not available on this host: {1}")]
    BackendUnavailable(&'static str, String),

    #[error("VM did not reach state {expected:?} within {timeout_secs}s (state: {actual:?})")]
    Timeout {
        expected: crate::backend::VmState,
        actual: crate::backend::VmState,
        timeout_secs: u64,
    },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl Error {
    pub fn backend(backend: &'static str, message: impl Into<String>) -> Self {
        Error::Backend {
            backend,
            message: message.into(),
        }
    }
}

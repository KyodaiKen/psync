//! Central error type for psync.
//!
//! Every fallible operation in psync returns [`Result`] so that failures can
//! be reported with context, counted in the final statistics, and mapped to
//! rsync-compatible exit codes.

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, AppError>;

/// Errors that can occur anywhere in psync.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// An I/O operation failed, annotated with what it was about.
    #[error("I/O error {what}: {source}")]
    Io {
        /// Human readable description of the operation, e.g. `opening '/a/b'`.
        what: String,
        /// The underlying OS error.
        #[source]
        source: std::io::Error,
    },
    /// The configuration file could not be loaded or is invalid.
    #[error("configuration error: {0}")]
    Config(String),
    /// Command line usage is invalid.
    #[error("usage error: {0}")]
    Usage(String),
    /// An option was given that psync (a local-only tool) cannot honour.
    #[error("unsupported option: {0} (psync is local-only, remote transfers are not supported)")]
    Unsupported(String),
    /// A feature is not implemented yet.
    #[error("not implemented: {0}")]
    NotImplemented(String),
    /// Drive mounting failed.
    #[error("mount error: {0}")]
    Mount(String),
    /// Source and destination checksums do not match.
    #[error("checksum mismatch: {0}")]
    Checksum(String),
    /// The user cancelled the run (Ctrl-C / SIGTERM).
    #[error("operation cancelled by user")]
    Cancelled,
    /// Any other fatal, non-recoverable problem.
    #[error("{0}")]
    Other(String),
}

impl AppError {
    /// Map an [`std::io::Error`] into a contextualized variant.
    pub fn io(what: impl Into<String>, source: std::io::Error) -> Self {
        AppError::Io {
            what: what.into(),
            source,
        }
    }
}

/// True if this error represents a per-file copy failure (exit code 23
/// territory) rather than a fatal/usage problem.
pub fn is_transfer_error(e: &AppError) -> bool {
    matches!(
        e,
        AppError::Io { .. } | AppError::Checksum(_) | AppError::Other(_)
    )
}

/// Format an error chain for the final report.
pub fn format_chain(e: &AppError) -> String {
    let mut s = format!("{e}");
    let mut src = std::error::Error::source(e);
    while let Some(inner) = src {
        let text = format!("{inner}");
        // Don't duplicate sources already rendered by the variant's own
        // Display (e.g. `Io` shows `{source}` inline).
        if !s.contains(&text) {
            s.push_str(&format!(" -> {text}"));
        }
        src = inner.source();
    }
    s
}


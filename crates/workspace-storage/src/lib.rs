//! File persistence: project TOML files, runtime crash-recovery snapshot,
//! atomic writes, schema versioning and migrations, export/import.
//!
//! Layout:
//! - `~/.config/omarchy-workspaces/{config.toml, rules.toml}`
//! - `~/.local/state/omarchy-workspaces/{projects/<slug>.toml, runtime.json, logs/}`

#![warn(missing_docs)]

pub mod atomic;
pub mod migrations;
pub mod paths;
pub mod projects;
pub mod runtime;

pub use atomic::atomic_write;

/// Errors from reading or writing persistent state.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// Filesystem failure.
    #[error("storage io error: {0}")]
    Io(#[from] std::io::Error),
    /// TOML serialization failure.
    #[error("cannot serialize: {0}")]
    TomlSer(#[from] toml::ser::Error),
    /// JSON (de)serialization failure.
    #[error("cannot serialize: {0}")]
    Json(#[from] serde_json::Error),
    /// A file's content is not what we expect.
    #[error("invalid file {path}: {message}")]
    Invalid {
        /// The offending file.
        path: std::path::PathBuf,
        /// What is wrong.
        message: String,
    },
    /// A file was written by a newer build; refuse rather than destroy.
    #[error(
        "{path} is schema v{found} but this build supports up to v{supported}; upgrade omarchy-workspaces or restore the file"
    )]
    VersionTooNew {
        /// The offending file.
        path: std::path::PathBuf,
        /// Version found in the file.
        found: u32,
        /// Newest supported version.
        supported: u32,
    },
}

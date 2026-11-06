//! Bundles: reading them safely, checking their Compose file, and writing the Compose file a node runs.
//!
//! A bundle is a directory, or a ZIP of one, with `compose.yaml` at its root.
//!
//! - [`zip`] extracts a ZIP and [`dir`] copies or packs a directory, both under [`Limits`] and refusing anything
//!   but plain files and directories.
//! - [`compose`] checks the Compose file against the subset kiwi-nest accepts, and [`manifest`] its
//!   `x-kiwi-nest` block.
//! - [`generate`] writes the Compose file that a node actually runs.
//! - [`overrides`] changes `x-kiwi-nest` as the CLI deploys a bundle (`deploy --domain`, for previews).
//!
//! Nothing here touches anything but the bundle itself, so plain unit tests cover it.

pub mod compose;
pub mod dir;
mod files;
pub mod generate;
pub mod manifest;
pub mod overrides;
pub mod zip;

use std::io::Read;
use std::path::Path;

pub use compose::{CheckOptions, CheckedBundle, Problem, Problems, check};
pub use files::{Entry, FileIndex};

/// The file at a bundle's root that describes the app.
pub const COMPOSE_FILE: &str = "compose.yaml";

/// The largest `compose.yaml` accepted.
pub const MAX_COMPOSE_SIZE: u64 = 1024 * 1024;

/// Limits on what a bundle may contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    /// The largest ZIP accepted, in bytes.
    pub max_archive_size: u64,
    /// The most entries (files and directories).
    pub max_files: usize,
    /// The largest single file, in bytes.
    pub max_file_size: u64,
    /// The largest total of all files, in bytes.
    pub max_expanded_size: u64,
    /// The longest path, in bytes.
    pub max_path_len: usize,
    /// The deepest nesting of directories.
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_archive_size: 256 * 1024 * 1024,
            max_files: 20_000,
            max_file_size: 256 * 1024 * 1024,
            max_expanded_size: 1024 * 1024 * 1024,
            max_path_len: 1024,
            max_depth: 32,
        }
    }
}

impl Limits {
    /// Limits for `volume load`, whose contents are data rather than an app: larger and with more files.
    pub fn for_volumes() -> Limits {
        Limits {
            max_archive_size: 4 * 1024 * 1024 * 1024,
            max_files: 1_000_000,
            max_file_size: 16 * 1024 * 1024 * 1024,
            max_expanded_size: 64 * 1024 * 1024 * 1024,
            max_path_len: 4096,
            max_depth: 64,
        }
    }
}

/// Why a bundle couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("{0}: {1}")]
    Io(String, #[source] std::io::Error),
    #[error("not a valid ZIP: {0}")]
    Zip(String),
    #[error("too large: {0}")]
    TooLarge(String),
    #[error("`{path}` {reason}")]
    UnsafePath { path: String, reason: &'static str },
    #[error("`{0}` is a symlink; bundles can only hold plain files and directories")]
    Symlink(String),
    #[error("`{0}` is a special file; bundles can only hold plain files and directories")]
    SpecialFile(String),
    #[error("`{0}` appears more than once")]
    Duplicate(String),
    #[error("the ZIP lists the same name more than once")]
    DuplicateInZip,
    #[error("`{0}` is both a file and a directory")]
    Conflict(String),
    #[error("`{0}` is encrypted")]
    Encrypted(String),
    #[error("the bundle has no `compose.yaml` file at its root")]
    MissingCompose,
    #[error("`compose.yaml` isn't valid UTF-8")]
    ComposeNotUtf8,
    /// `x-kiwi-nest` couldn't be changed as asked (see [`overrides`]).
    #[error("{0}")]
    Overrides(String),
}

impl BundleError {
    pub(crate) fn io(context: impl std::fmt::Display, e: std::io::Error) -> Self {
        BundleError::Io(context.to_string(), e)
    }
}

/// Reads the bundle's `compose.yaml`, from a directory that [`zip::extract`] or [`dir::copy`] made.
pub fn read_compose(dir: &Path, index: &FileIndex) -> Result<String, BundleError> {
    match index.get(COMPOSE_FILE) {
        Some(Entry::File { size, .. }) if *size > MAX_COMPOSE_SIZE => Err(BundleError::TooLarge(format!(
            "`compose.yaml` is {size} bytes; the limit is {MAX_COMPOSE_SIZE}"
        ))),
        Some(Entry::File { .. }) => {
            let path = dir.join(COMPOSE_FILE);
            let mut bytes = Vec::new();
            files::open_regular(&path)?
                .take(MAX_COMPOSE_SIZE + 1)
                .read_to_end(&mut bytes)
                .map_err(|e| BundleError::io(path.display(), e))?;
            String::from_utf8(bytes).map_err(|_| BundleError::ComposeNotUtf8)
        }
        _ => Err(BundleError::MissingCompose),
    }
}

/// Reads and checks a bundle that [`zip::extract`] or [`dir::copy`] made.
pub fn load(dir: &Path, index: &FileIndex, options: &CheckOptions) -> Result<CheckedBundle, Problems> {
    let yaml = read_compose(dir, index).map_err(|e| Problems(vec![Problem::new("", e.to_string())]))?;
    check(&yaml, index, options)
}

#[cfg(test)]
mod tests;

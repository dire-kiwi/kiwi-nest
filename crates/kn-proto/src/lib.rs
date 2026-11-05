//! Types shared by the kiwi-nest CLI, server and nodes.
//!
//! - [`api`]: requests and responses of the HTTP APIs for the CLI.
//! - [`agent`]: the link between nodes and the server: polls, reports, desired state and commands.
//! - [`state`]: what a node has applied, as it reports it.
//!
//! The API is versioned by its path prefix ([`API_PREFIX`]), so a newer side can keep serving an older one.

pub mod agent;
pub mod api;
pub mod state;

/// The path prefix of every API route in this version.
pub const API_PREFIX: &str = "/v1";

/// The version of this build, from the workspace's `Cargo.toml`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The git commit this build was made from, or `unknown`.
pub const GIT_COMMIT: &str = env!("KN_GIT_COMMIT");

/// `VERSION (commit COMMIT)`, as printed by `kiwi-nest version`.
pub const LONG_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (commit ", env!("KN_GIT_COMMIT"), ")");

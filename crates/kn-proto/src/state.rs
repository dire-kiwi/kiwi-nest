//! What a node has applied for each app, and how apps mount shared volumes.

use serde::{Deserialize, Serialize};

/// The state a node records for an app.
///
/// This is kiwi-nest's view (what it last did); the containers' own state is reported separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppStatus {
    /// A deploy is in progress.
    Deploying,
    /// The last deploy succeeded and the app was started.
    Running,
    /// The last deploy failed; see the app's error.
    Failed,
}

impl std::fmt::Display for AppStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AppStatus::Deploying => "deploying",
            AppStatus::Running => "running",
            AppStatus::Failed => "failed",
        })
    }
}

/// How an app mounts a shared volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedMode {
    /// Its own writable snapshot, made on its first deploy and kept across redeploys.
    Snapshot,
    /// Read-only.
    Ro,
    /// Read-write: every app mounting it sees the writes.
    Rw,
}

impl SharedMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SharedMode::Snapshot => "snapshot",
            SharedMode::Ro => "ro",
            SharedMode::Rw => "rw",
        }
    }
}

impl std::fmt::Display for SharedMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

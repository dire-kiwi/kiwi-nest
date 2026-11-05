//! What a node has applied for each app.

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

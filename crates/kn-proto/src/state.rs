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
    /// The app was deployed or started, and is running.
    Running,
    /// The app was stopped for being idle, or its last start failed (see its error). A request starts it.
    Idle,
    /// The app was stopped on purpose: suspended, or found partly running when the node agent started. Requests
    /// don't start it; `resume` does.
    Suspended,
    /// The last deploy failed; see the app's error. Requests don't start it; a deploy or `resume` does.
    Failed,
}

impl std::fmt::Display for AppStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AppStatus::Deploying => "deploying",
            AppStatus::Running => "running",
            AppStatus::Idle => "idle",
            AppStatus::Suspended => "suspended",
            AppStatus::Failed => "failed",
        })
    }
}

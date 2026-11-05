//! Requests and responses of the node's standalone API.
//!
//! Routes, all under [`crate::API_PREFIX`]:
//!
//! | Route | Body | Response |
//! | --- | --- | --- |
//! | `GET /version` | | [`VersionInfo`] |
//! | `GET /apps` | | `Vec<`[`AppSummary`]`>` |
//! | `PUT /apps/{name}?` [`DeployQuery`] | a ZIP (`application/zip`), or [`DeployDirectory`] as JSON | [`AppInfo`] |
//! | `GET /apps/{name}` | | [`AppInfo`] |
//! | `GET /apps/{name}/logs?` [`LogsQuery`] | | plain text, streamed when following |
//! | `DELETE /apps/{name}?` [`DeleteQuery`] | | `204 No Content` |
//! | `GET /volumes` | | `Vec<`[`VolumeInfo`]`>` |
//! | `PUT /volumes/{name}` | | [`VolumeInfo`] (`201 Created`) |
//! | `GET /volumes/{name}` | | [`VolumeInfo`] |
//! | `PUT /volumes/{name}/data` | a ZIP, or [`DeployDirectory`] as JSON | [`VolumeInfo`] |
//! | `DELETE /volumes/{name}` | | `204 No Content` |
//! | `DELETE /volumes/{name}/snapshots/{app}` | | `204 No Content` |
//!
//! Errors are an [`ErrorBody`] with a 4xx or 5xx status.

use serde::{Deserialize, Serialize};

use crate::state::{AppStatus, SharedMode};

/// `GET /version`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionInfo {
    pub version: String,
    pub commit: String,
}

/// The body of a deploy that asks the node to read a bundle directory on its own machine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeployDirectory {
    /// An absolute path on the node.
    pub directory: String,
}

/// The query of `PUT /apps/{name}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeployQuery {
    /// Refuse if the app already exists, rather than redeploying it.
    #[serde(default)]
    pub create_only: bool,
    /// On the app's first deploy, start each named volume it shares with this app as a snapshot of that app's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

/// The query of `DELETE /apps/{name}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeleteQuery {
    /// Keep the app's subvolumes, so a later deploy of the same name gets them back.
    #[serde(default)]
    pub keep_data: bool,
}

/// The query of `GET /apps/{name}/logs`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogsQuery {
    /// Lines to show from the end of each container's log; all of them if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<u32>,
    /// Keep streaming new lines.
    #[serde(default)]
    pub follow: bool,
    /// Prefix each line with its timestamp.
    #[serde(default)]
    pub timestamps: bool,
    /// Only this service's containers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// One app in `GET /apps`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSummary {
    pub name: String,
    pub status: AppStatus,
    /// Increases by one on every deploy.
    pub generation: u64,
    pub domain: String,
    /// The loopback port the routed service is published on.
    pub port: u16,
    /// What Docker reports for the app's containers, such as `running(2)`, or `absent`.
    pub containers: String,
    /// When the app was last deployed, as RFC 3339.
    pub updated_at: String,
}

/// `GET /apps/{name}`, and the result of a deploy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppInfo {
    pub name: String,
    pub status: AppStatus,
    pub generation: u64,
    /// Why the last deploy failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub domain: String,
    /// The service that gets HTTP traffic, and its port inside the container.
    pub service: String,
    pub container_port: u16,
    /// The loopback port that `service` is published on.
    pub port: u16,
    /// Seconds without requests before the app is stopped; 0 means always on.
    pub idle_timeout_secs: u64,
    /// The Compose project the app runs as.
    pub project: String,
    /// The bundle's digest, `sha256:…`.
    pub bundle_digest: String,
    /// The app's named volumes.
    pub volumes: Vec<String>,
    /// The shared volumes it mounts.
    #[serde(default)]
    pub shared: Vec<SharedMountInfo>,
    /// The app whose volumes this one's started as snapshots of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// What the deploy did with the app's volumes, such as which ones it snapshotted; only in a deploy's response.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub created_at: String,
    pub updated_at: String,
    /// The app's containers as Docker reports them now.
    pub containers: Vec<ContainerInfo>,
}

/// One container of an app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub service: String,
    pub name: String,
    /// `running`, `exited`, `paused`, …
    pub state: String,
    /// `healthy`, `unhealthy`, `starting`, or empty without a healthcheck.
    #[serde(default)]
    pub health: String,
    /// Docker's description, such as `Up 2 minutes (healthy)`.
    #[serde(default)]
    pub status: String,
}

/// A shared volume mounted by an app.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SharedMountInfo {
    pub name: String,
    /// Where it's mounted in the containers.
    pub target: String,
    pub mode: SharedMode,
}

/// A shared volume, in `GET /volumes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VolumeInfo {
    pub name: String,
    pub created_at: String,
    /// When `volume load` last filled it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded_at: Option<String>,
    /// The apps that mount it, and how.
    pub mounts: Vec<VolumeMount>,
    /// The apps with a snapshot of it (deployed or kept).
    pub snapshots: Vec<String>,
    /// What's wrong with it on disk, if anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// An app mounting a shared volume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeMount {
    pub app: String,
    pub mode: SharedMode,
}

/// The body of every error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
    /// Each problem found in a refused bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

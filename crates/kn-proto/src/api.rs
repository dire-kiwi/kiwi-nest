//! Requests and responses of the HTTP APIs for the CLI: the standalone node's, and the server's.
//!
//! Both have these routes, under [`crate::API_PREFIX`]. The server's differ where noted below the table.
//!
//! | Route | Body | Response |
//! | --- | --- | --- |
//! | `GET /version` | | [`VersionInfo`] |
//! | `GET /apps` | | `Vec<`[`AppSummary`]`>` |
//! | `PUT /apps/{name}?` [`DeployQuery`] | a ZIP (`application/zip`), or [`DeployDirectory`] as JSON | [`AppInfo`] |
//! | `GET /apps/{name}` | | [`AppInfo`] |
//! | `GET /apps/{name}/logs?` [`LogsQuery`] | | plain text, streamed when following |
//! | `POST /apps/{name}/suspend` | | [`AppInfo`]; `409` while the app has open requests |
//! | `POST /apps/{name}/resume` | | [`AppInfo`], once the app is ready |
//! | `DELETE /apps/{name}?` [`DeleteQuery`] | | `204 No Content` |
//! | `GET /volumes` | | `Vec<`[`VolumeInfo`]`>` |
//! | `PUT /volumes/{name}` | | [`VolumeInfo`] (`201 Created`) |
//! | `GET /volumes/{name}` | | [`VolumeInfo`] |
//! | `PUT /volumes/{name}/data` | a ZIP, or [`DeployDirectory`] as JSON | [`VolumeInfo`] |
//! | `DELETE /volumes/{name}` | | `204 No Content` |
//! | `DELETE /volumes/{name}/snapshots/{app}` | | `204 No Content` |
//!
//! Errors are an [`ErrorBody`] with a 4xx or 5xx status.
//!
//! **The server** needs `Authorization: Bearer TOKEN` on every route but `/version`. Its changes (deploy, delete,
//! `volume create|load|delete`) answer `202 Accepted` with an [`Accepted`] once the change is saved, and the CLI then
//! waits for the node with `GET /apps/{name}/changes/{generation}` (or `/volumes/…`), which answers a
//! [`ChangeStatus`]. It only takes ZIPs, never a [`DeployDirectory`]. It also has:
//!
//! | Route | Body | Response |
//! | --- | --- | --- |
//! | `GET /whoami` | | [`WhoAmI`] |
//! | `GET /apps/{name}/changes/{generation}?` [`WaitQuery`] | | [`ChangeStatus`] |
//! | `GET /volumes/{name}/changes/{generation}?` [`WaitQuery`] | | [`ChangeStatus`] |
//! | `GET /nodes` | | `Vec<`[`NodeInfo`]`>` |
//! | `GET /tokens` | | `Vec<`[`TokenInfo`]`>` |
//! | `POST /tokens` | [`CreateToken`] | [`CreatedToken`] |
//! | `DELETE /tokens/{id}` | | `204 No Content` |

use serde::{Deserialize, Serialize};

use crate::agent::NodeFacts;
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
    /// Server only: the node for a new app. By default the server picks one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Server only: save the deploy even if its node is offline, to be applied when it's back.
    #[serde(default)]
    pub queue: bool,
}

/// The query of `DELETE /apps/{name}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeleteQuery {
    /// Keep the app's subvolumes, so a later deploy of the same name gets them back.
    #[serde(default)]
    pub keep_data: bool,
    /// Server only: save the delete even if the app's node is offline.
    #[serde(default)]
    pub queue: bool,
}

/// The query of the server's `PUT /volumes/{name}`, `PUT /volumes/{name}/data` and `DELETE /volumes/{name}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VolumeQuery {
    /// The node for a new volume. By default the server picks one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Save the change even if the volume's node is offline.
    #[serde(default)]
    pub queue: bool,
}

/// The server's answer to a change: it's saved, as `generation` of the app or volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Accepted {
    pub name: String,
    pub node: String,
    pub generation: u64,
    /// Whether the node is online; if it isn't, the change waits for it (`--queue`).
    pub node_online: bool,
}

/// The query of `GET …/changes/{generation}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WaitQuery {
    /// Wait up to this many seconds for the change to be done; 0 answers at once.
    #[serde(default)]
    pub wait: u64,
}

/// Whether a node has applied a change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChangeStatus {
    pub name: String,
    pub generation: u64,
    pub node: String,
    pub node_online: bool,
    /// The node has finished the change, or it was superseded before the node got to it.
    pub done: bool,
    /// It was done and it worked.
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// A later generation that replaced this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superseded_by: Option<u64>,
}

/// The query of `GET /apps/{name}/logs`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Server only: the node it's on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Server only: a generation saved on the server that the node hasn't applied yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<u64>,
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
    /// Requests to the app through the gateway whose responses haven't finished, WebSockets included.
    #[serde(default)]
    pub open_requests: u64,
    /// Server only: the node it's on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Server only: a generation saved on the server that the node hasn't applied yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<u64>,
    /// Server only: the node is offline, so this is what it last reported, without containers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
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
    /// Server only: the node it's on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// Server only: a generation saved on the server that the node hasn't applied yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<u64>,
}

/// An app mounting a shared volume.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeMount {
    pub app: String,
    pub mode: SharedMode,
}

/// A node, in the server's `GET /nodes`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub id: u64,
    pub name: String,
    pub online: bool,
    pub joined_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
    /// What it last reported about itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<NodeFacts>,
    /// The apps placed on it.
    pub apps: u64,
    /// The memory its apps may use: each service's `mem_limit`, or 256 MiB without one.
    pub app_memory_bytes: u64,
}

/// The kinds of token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenKind {
    /// Everything.
    Admin,
    /// Apps whose names match the token's patterns.
    Deploy,
    /// Swapped once by a node for a node token.
    Join,
    /// A node's own.
    Node,
}

impl TokenKind {
    pub fn as_str(self) -> &'static str {
        match self {
            TokenKind::Admin => "admin",
            TokenKind::Deploy => "deploy",
            TokenKind::Join => "join",
            TokenKind::Node => "node",
        }
    }
}

impl std::fmt::Display for TokenKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `POST /tokens`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateToken {
    /// `admin`, `deploy` or `join`.
    pub kind: TokenKind,
    /// A description, such as what uses it.
    #[serde(default)]
    pub name: String,
    /// For a deploy token: the app names it may deploy, delete and manage, where `*` matches anything, such as
    /// `blog-pr*`.
    #[serde(default)]
    pub patterns: Vec<String>,
    /// For a deploy token: the apps it may name as `--from` besides its own, such as `blog`.
    #[serde(default)]
    pub from: Vec<String>,
}

/// A token just made: the only time it's shown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreatedToken {
    pub id: u64,
    pub kind: TokenKind,
    pub token: String,
}

/// A token, in `GET /tokens`, without the token itself (the server only keeps its hash).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenInfo {
    pub id: u64,
    pub kind: TokenKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from: Vec<String>,
    /// A node token's node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub created_at: String,
    /// When a join token was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_at: Option<String>,
}

/// `GET /whoami`: the caller's token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WhoAmI {
    pub kind: TokenKind,
    pub name: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub from: Vec<String>,
}

/// The body of every error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
    /// Each problem found in a refused bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

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
//! Both also have the backup routes in [`crate::backup`]; a standalone node needs `--backup-targets` for them.
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
//! | `GET /nests` | | `Vec<`[`NestInfo`]`>` |
//! | `GET /nodes` | | `Vec<`[`NodeInfo`]`>` |
//! | `POST /nodes/{name}/labels` | [`LabelRequest`] | [`NodeInfo`] |
//! | `POST /nodes/{name}/cordon`, `/uncordon` | | [`NodeInfo`] |
//! | `POST /nodes/{name}/forget?` [`ForgetQuery`] | | [`Forgot`] |
//! | `GET /tokens` | | `Vec<`[`TokenInfo`]`>` |
//! | `POST /tokens` | [`CreateToken`] | [`CreatedToken`] |
//! | `DELETE /tokens/{id}` | | `204 No Content` |
//! | `POST /nodes/{name}/prune?` [`PruneQuery`] | | [`PruneReport`] |
//! | `GET /registry` | | [`RegistryInfo`] |
//! | `POST /registry/gc?` [`GcQuery`] | | [`GcReport`] (admin) |
//! | `GET /registry/auth` | | `200`, `401` or `403`: the ingress asks it about every request to the registry |
//! | `GET /upgrade` | | [`UpgradeStatus`] (admin): the server's release rolling out to the nodes |
//! | `POST /upgrade` | | [`UpgradeStatus`] (admin): try the nodes again, after a failure halted the rollout |
//!
//! The server refuses a CLI older than itself with `426 Upgrade Required`: the CLI sends its version in its
//! `User-Agent` (see [`crate::version`]).

use std::collections::BTreeMap;

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
    /// On the app's first deploy, restore its volumes from this backup (a key, as `backup list` shows it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<String>,
    /// The backup target `restore` is in; by default, the first target that has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
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
    /// The node for a new volume's new nest. By default the server picks one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    /// The nest of a new volume; by default, a nest named after the volume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nest: Option<String>,
    /// Save the change even if the volume's node is offline.
    #[serde(default)]
    pub queue: bool,
    /// For a new volume: restore its data from this backup (a key, as `backup list` shows it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<String>,
    /// The backup target `restore` is in; by default, the first target that has it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

/// The server's answer to a change: it's saved, as `generation` of the app or volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Accepted {
    pub name: String,
    pub node: String,
    pub generation: u64,
    /// Whether the node is online; if it isn't, the change waits for it (`--queue`).
    pub node_online: bool,
    /// The app's or volume's nest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nest: Option<String>,
    /// Saved anyway, but worth knowing, such as that the nest's node is out of memory headroom.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
    /// Server only: its nest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nest: Option<String>,
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
    /// The backup its volumes were restored from, as `TARGET:KEY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<String>,
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
    /// Server only: its nest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nest: Option<String>,
    /// Server only: how it came to be in its nest, and why the nest's node was chosen, such as `through --from
    /// blog; nest blog went to node a when blog was deployed: the most memory headroom …`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<String>,
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
    /// Server only: its nest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nest: Option<String>,
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
    /// Cordoned: it gets no new nests (the nests on it still take new members).
    #[serde(default)]
    pub cordoned: bool,
    /// Its labels for placement: those it reports (`--label`), and those set with `node label`, which win.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// The nests placed on it.
    #[serde(default)]
    pub nests: Vec<String>,
    /// The apps placed on it.
    pub apps: u64,
    /// The memory its apps may use: each service's `mem_limit`, or 256 MiB without one.
    pub app_memory_bytes: u64,
    /// Its memory headroom, as placement counts it: its memory, less the reserve, the `mem_limit`s of its
    /// always-on apps, and a share of those of its apps that idle out. Negative when it's overcommitted.
    #[serde(default)]
    pub headroom_bytes: i64,
}

/// A nest, in `GET /nests`: apps and shared volumes that live together on one node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NestInfo {
    pub name: String,
    pub node: String,
    pub node_online: bool,
    pub apps: Vec<String>,
    pub volumes: Vec<String>,
    /// What its apps may use: each service's `mem_limit`, or 256 MiB without one.
    pub memory_bytes: u64,
    /// Why its node was chosen when it was placed.
    pub placement: String,
    pub created_at: String,
}

/// `POST /nodes/{name}/labels`: labels to set and to remove (only those set with `node label`; a node's own
/// `--label`s stay).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LabelRequest {
    #[serde(default)]
    pub set: BTreeMap<String, String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// The query of `POST /nodes/{name}/forget`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ForgetQuery {
    /// Forget the node even though it's online.
    #[serde(default)]
    pub force: bool,
    /// Only say what forgetting it would release, and whether it would be refused.
    #[serde(default)]
    pub dry_run: bool,
}

/// What `node forget` released (or, as a dry run, would release): the names and domains of these apps, and these
/// shared volumes and nests, all free to be used again elsewhere.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Forgot {
    pub node: String,
    pub online: bool,
    pub apps: Vec<String>,
    pub volumes: Vec<String>,
    pub nests: Vec<String>,
    #[serde(default)]
    pub dry_run: bool,
}

/// `GET /registry`: the cluster's image registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryInfo {
    /// The host that image references name, such as `registry.example.com` (with a port if the ingress isn't on
    /// the default one).
    pub host: String,
}

/// The query of `POST /registry/gc`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcQuery {
    /// Only say what would be deleted.
    #[serde(default)]
    pub dry_run: bool,
    /// The grace period in seconds, instead of the server's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_secs: Option<u64>,
}

/// One tag in the registry, and what garbage collection did with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GcTag {
    pub repository: String,
    pub tag: String,
    pub digest: String,
    /// Why it's kept, such as `used by app blog on node a`; empty for one deleted.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
}

/// What `POST /registry/gc` did (or, as a dry run, would do).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcReport {
    pub deleted: Vec<GcTag>,
    pub kept: Vec<GcTag>,
    /// Whether the registry's own garbage collection ran, which frees the deleted images' layers.
    pub collected: bool,
    /// Why it didn't, if it was needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub postponed: Option<String>,
    #[serde(default)]
    pub dry_run: bool,
}

/// The query of `POST /nodes/{name}/prune`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PruneQuery {
    /// The grace period in seconds, instead of the node's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace_secs: Option<u64>,
}

/// What pruning a node's images did: those it removed, and those of its apps' past images it kept, with why.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PruneReport {
    pub removed: Vec<String>,
    pub kept: Vec<KeptImage>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptImage {
    pub image: String,
    pub why: String,
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

/// `GET /upgrade`: how the server's release is rolling out to the nodes, one at a time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpgradeStatus {
    /// The server's version, which nodes are upgraded to.
    pub version: String,
    /// The architectures the server has its release for, and so can upgrade nodes of.
    pub arches: Vec<String>,
    /// A node's upgrade failed, so no other is started until `POST /upgrade` (or a server restart).
    pub halted: bool,
    pub nodes: Vec<NodeUpgrade>,
}

/// One node, in [`UpgradeStatus`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeUpgrade {
    pub node: String,
    /// The version it last reported.
    pub version: String,
    pub online: bool,
    pub state: UpgradeState,
    /// Why it can't be upgraded, or how its upgrade failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpgradeState {
    /// It runs the server's version, or a newer one.
    Current,
    /// It's older, and waits its turn (or for coming online).
    Pending,
    /// It's being upgraded: downloading, or restarting into the new release.
    Upgrading,
    /// Its upgrade failed (see `detail`), which halts the rollout.
    Failed,
    /// It can't be upgraded by the server (see `detail`): upgrade it by hand.
    Manual,
}

impl UpgradeState {
    pub fn as_str(self) -> &'static str {
        match self {
            UpgradeState::Current => "current",
            UpgradeState::Pending => "pending",
            UpgradeState::Upgrading => "upgrading",
            UpgradeState::Failed => "failed",
            UpgradeState::Manual => "manual",
        }
    }
}

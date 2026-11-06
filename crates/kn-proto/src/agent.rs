//! The agent link: how nodes talk to the server.
//!
//! Nodes connect out. Each long-polls the server with a [`Report`] of what it is and what it has applied, and gets
//! back what changed in its desired state since the last poll, and any [`Command`]s for it. Routes, all under
//! [`crate::API_PREFIX`], with the node's token as `Authorization: Bearer …` (except `join`, which carries a join
//! token in its body):
//!
//! | Route | Body | Response |
//! | --- | --- | --- |
//! | `POST /agent/join` | [`JoinRequest`] | [`JoinResponse`] |
//! | `GET /agent/ingress-token` | | [`IngressToken`], for a node that joined before ingress tokens |
//! | `POST /agent/registry` | | [`RegistryCredential`]: a new credential for the registry, replacing the last |
//! | `POST /agent/poll` | [`PollRequest`] | [`PollResponse`], once something changed or the wait is over |
//! | `POST /agent/report` | [`Report`] | `204 No Content`, at once |
//! | `GET /agent/bundles/{digest}` | | the bundle's ZIP |
//! | `GET /agent/backup-targets` | | `Vec<`[`crate::backup::TargetConfig`]`>`: the server's targets, secrets included |
//! | `GET /agent/releases/{version}/{arch}` | | the server's release of kiwi-nest for an architecture, for [`Action::Upgrade`] |
//! | `POST /agent/commands/{id}/result` | [`CommandResult`] | `204 No Content` |
//! | `POST /agent/commands/{id}/output` | the command's output, streamed | `204 No Content` |
//!
//! **Desired state.** For each node the server keeps the apps and shared volumes it should have. Every change to
//! one bumps the node's sequence number, and the entry records the sequence number it changed at ([`AppChange::seq`])
//! and the app's or volume's generation. A poll asks for what changed after `since`, the sequence number of the last
//! poll it got; a node that has just started asks from 0, and gets its whole desired state. Entries are states, not
//! steps: an app deployed twice while its node was offline arrives once, at its latest generation.
//!
//! **Applying.** The node applies one change at a time per app or volume, and records each one it finishes (done or
//! failed) with its generation ([`Applied`]), so a change it gets again, after a restart or a lost response, never
//! runs twice. A change cut off by a restart wasn't finished, so it runs again. Once one finishes, the node sends a
//! report straight away on `/agent/report`, beside its open poll, so the server hears without waiting for the poll.
//!
//! **Commands** are one-off actions (logs, suspend, …). Each is handed out in at most one poll response, and never
//! again. The node runs it in a task of its own, so polling carries on, lists it in
//! [`Report::running_commands`] until its result has been sent, and sends the result back on the same link. A
//! command the server handed out that the next report doesn't list (the response was lost, or the node restarted)
//! has failed.
//!
//! The protocol is versioned ([`PROTOCOL`]), so a server can keep serving nodes one version behind it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::api::{ErrorBody, LogsQuery, Space, VolumeInfo};
use crate::backup::{Kind, Restore};
use crate::state::AppStatus;

/// The version of the agent link that this build speaks. Version 2 added backups: [`Action::Backup`], restores in
/// [`AppWant::Deploy`] and [`VolumeWant::Present`], `GET /agent/backup-targets`, and [`NodeFacts::protocol`]. Version 3
/// added the image registry: [`JoinResponse::registry`], `POST /agent/registry`, [`AppReport::starting`] and
/// [`Action::PruneImages`]. Version 4 added upgrades: [`Action::Upgrade`], `GET /agent/releases/…` and
/// [`NodeFacts::self_upgrade`].
pub const PROTOCOL: u32 = 4;

/// The first version with backups: a server sends a node older than this no backup command and no restore (an
/// older node would ignore the restore, and deploy the app empty).
pub const BACKUPS_PROTOCOL: u32 = 2;

/// The first version with the image registry: a server sends a node older than this no [`Action::PruneImages`]
/// (it couldn't read it). Such a node has no registry credential, and doesn't say when an app is starting.
pub const REGISTRY_PROTOCOL: u32 = 3;

/// The first version with upgrades: a server offers its release to a node older than this never (it couldn't read
/// [`Action::Upgrade`]); such a node is upgraded by hand, with `kiwi-nest upgrade` on it.
pub const UPGRADE_PROTOCOL: u32 = 4;

/// The oldest version a server built from this code still serves.
pub const OLDEST_PROTOCOL: u32 = 1;

/// The longest a poll is held open, in seconds; the server may answer sooner.
pub const MAX_POLL_SECS: u64 = 60;

/// `POST /agent/join`: a node swaps a one-time join token for a token of its own.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinRequest {
    pub token: String,
    /// The node's name, unique in the cluster.
    pub name: String,
    pub facts: NodeFacts,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinResponse {
    pub node_id: u64,
    pub name: String,
    /// The node's own token, which the server stores only as a hash.
    pub token: String,
    /// The token the ingress adds to every request it sends to the node's gateway ([`INGRESS_HEADER`]).
    #[serde(default)]
    pub ingress_token: String,
    /// The node's read-only credential for the cluster's registry, if it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<RegistryLogin>,
}

/// A login for the cluster's image registry, which the node writes into its Docker configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryLogin {
    /// The host that image references name, such as `registry.example.com`.
    pub host: String,
    pub username: String,
    /// A `knr_…` credential, which may only pull.
    pub password: String,
}

/// `POST /agent/registry`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RegistryCredential {
    /// `None` if the cluster has no registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub registry: Option<RegistryLogin>,
}

/// `GET /agent/ingress-token`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngressToken {
    pub ingress_token: String,
}

/// The header that carries a node's ingress token, from the ingress to the node's gateway, which checks it and
/// removes it.
pub const INGRESS_HEADER: &str = "kiwi-nest-ingress";

/// What a node is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeFacts {
    /// `amd64`, `arm64`, or the machine's own name for another architecture.
    pub arch: String,
    pub memory_bytes: u64,
    /// Memory available now (`MemAvailable`).
    pub memory_available_bytes: u64,
    /// The filesystem of the node's volumes (btrfs).
    pub volumes: DiskSpace,
    /// The filesystem of the node's state directory.
    pub state: DiskSpace,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    pub version: String,
    pub commit: String,
    /// The host directories that bundles may mount on this node.
    #[serde(default)]
    pub allowed_host_paths: Vec<String>,
    /// The Compose project prefix of its apps.
    #[serde(default)]
    pub project_prefix: String,
    /// The address its gateway listens on, on loopback.
    #[serde(default)]
    pub gateway: String,
    /// The address its gateway listens on for the ingress (on the node's private network), which needs the node's
    /// ingress token; empty if it has none, so the ingress can't reach its apps.
    #[serde(default)]
    pub ingress: String,
    /// The version of the agent link it speaks ([`PROTOCOL`]); 0 from a node of version 1, which didn't say.
    #[serde(default)]
    pub protocol: u32,
    /// It runs from an installed release (`kiwi-nest node join`), so it can take [`Action::Upgrade`]: download
    /// a release, swap it in and restart.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub self_upgrade: bool,
}

/// A filesystem's size and free space.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskSpace {
    pub total_bytes: u64,
    /// Free space as `statvfs` reports it (available to unprivileged users).
    pub free_bytes: u64,
    /// On btrfs, the space not yet allocated to any chunk, summed over the filesystem's devices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unallocated_bytes: Option<u64>,
}

/// `POST /agent/poll`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PollRequest {
    /// The version of the link the node speaks ([`PROTOCOL`]).
    pub protocol: u32,
    /// The sequence number of the last poll response the node got; 0 for everything.
    pub since: u64,
    /// The longest the node wants to wait for a change, in seconds; the server may wait less.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_secs: Option<u64>,
    pub report: Report,
}

/// What a node tells the server on every poll.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub facts: NodeFacts,
    /// The apps the node has, as it last recorded them.
    #[serde(default)]
    pub apps: Vec<AppReport>,
    /// The shared volumes it has.
    #[serde(default)]
    pub volumes: Vec<VolumeInfo>,
    /// The changes it has finished applying.
    #[serde(default)]
    pub applied: Applied,
    /// Commands it has been given and not yet sent the result of.
    #[serde(default)]
    pub running_commands: Vec<u64>,
    /// Increases with every report the node makes, so the server never replaces a report with an older one that
    /// arrived late.
    #[serde(default)]
    pub serial: u64,
    /// What its volumes take, once it has measured them (each app's volumes are in [`AppReport::space`], each shared
    /// volume's in its [`VolumeInfo`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

/// What a node's subvolumes take on disk, measured now and then (`--usage-interval`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Unix seconds.
    pub measured_at: u64,
    /// The sets of subvolumes that can share data (an app, the apps made `--from` it, the shared volumes they mount
    /// and the snapshots of those), each with the space it takes counting shared data once. Each set is within one
    /// nest.
    #[serde(default)]
    pub groups: Vec<UsageGroup>,
    /// The read-only snapshot sets kept as the parents of incremental backups.
    #[serde(default)]
    pub backups: Space,
}

/// A set of subvolumes that can share data, in [`Usage`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageGroup {
    #[serde(default)]
    pub apps: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    /// The space its subvolumes take, counting data they share with one another once.
    pub bytes: u64,
}

/// One app on a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppReport {
    pub name: String,
    pub status: AppStatus,
    /// The generation in place (after a redeploy that failed early, the previous one).
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub domain: String,
    /// The loopback port its routed service is published on.
    pub port: u16,
    pub bundle_digest: String,
    /// What Docker reports for its containers, such as `running(2)`; `unknown` if Docker couldn't be asked.
    pub containers: String,
    /// It's being started, or deployed, or looked at after the node agent started, so it may be pulling images.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub starting: bool,
    /// When a request through the gateway last began or ended (Unix seconds), if one has since the node started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_request_at: Option<u64>,
    /// Requests through the gateway whose responses haven't finished.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub open_requests: u64,
    /// The space each of its named volumes takes, as last measured.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub space: BTreeMap<String, Space>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// The changes a node has finished applying, by app and by shared volume.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Applied {
    #[serde(default)]
    pub apps: BTreeMap<String, Outcome>,
    #[serde(default)]
    pub volumes: BTreeMap<String, Outcome>,
}

/// How a change ended on the node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub generation: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Each problem found in a refused bundle.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    /// What the change did, such as which volumes it snapshotted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The change was a delete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub deleted: bool,
    /// For a shared volume, the digest of the contents it was last loaded with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_digest: Option<String>,
    /// Unix seconds.
    pub finished_at: u64,
}

/// What the server answers a poll with.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PollResponse {
    /// The node's sequence number now: the `since` of the next poll.
    pub seq: u64,
    /// The apps whose desired state changed after `since`, in the order they changed.
    #[serde(default)]
    pub apps: Vec<AppChange>,
    /// The shared volumes whose desired state changed after `since`, in the order they changed.
    #[serde(default)]
    pub volumes: Vec<VolumeChange>,
    #[serde(default)]
    pub commands: Vec<Command>,
}

/// What the server wants for one app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppChange {
    pub name: String,
    pub generation: u64,
    /// The node sequence number this changed at.
    pub seq: u64,
    pub want: AppWant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppWant {
    /// Deploy this bundle, downloaded by digest.
    Deploy {
        bundle_digest: String,
        /// On the app's first deploy, snapshot the volumes it shares with this app (on the same node).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<String>,
        /// The shared volumes it mounts, which must be in place first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        shared: Vec<String>,
        /// On the app's first deploy, restore its volumes from this backup.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        restore: Option<Restore>,
    },
    /// Delete the app.
    Delete {
        #[serde(default)]
        keep_data: bool,
    },
}

/// What the server wants for one shared volume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeChange {
    pub name: String,
    pub generation: u64,
    pub seq: u64,
    pub want: VolumeWant,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum VolumeWant {
    /// The volume should exist, loaded with these contents (downloaded by digest) if given.
    Present {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data_digest: Option<String>,
        /// When it's created: its data, restored from this backup.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        restore: Option<Restore>,
    },
    Delete,
}

/// A one-off action for a node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Command {
    pub id: u64,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Answers with an [`crate::api::AppInfo`].
    Inspect { app: String },
    /// Answers by streaming the logs to `/agent/commands/{id}/output`, or with an error result.
    Logs { app: String, query: LogsQuery },
    /// Answers with an [`crate::api::AppInfo`].
    Suspend { app: String },
    /// Answers with an [`crate::api::AppInfo`], once the app is ready.
    Resume { app: String },
    /// Answers with a [`VolumeInfo`].
    VolumeInfo { name: String },
    /// Answers with `null`.
    DeleteSnapshot { name: String, app: String },
    /// Removes the images of the node's past apps that nothing uses, and that weren't released within the grace
    /// period (the node's own, unless given). Answers with a [`crate::api::PruneReport`].
    PruneImages {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        grace_secs: Option<u64>,
    },
    /// Upgrades the node to the server's release: it downloads it (`GET /agent/releases/{version}/{arch}`), checks
    /// its size and SHA-256 and that it runs, installs it beside the releases it has, and answers with an
    /// [`UpgradeAnswer`]; then it waits for the changes it's applying, swaps the release in and restarts into it.
    /// Its apps keep running meanwhile.
    Upgrade { version: String, sha256: String, size: u64 },
    /// Backs up an app or a shared volume to the targets named (every target, if none is); answers with a
    /// [`crate::backup::BackupResult`].
    Backup {
        what: Kind,
        name: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        targets: Vec<String>,
        #[serde(default)]
        full: bool,
    },
}

impl Action {
    /// The app or volume it acts on.
    pub fn target(&self) -> &str {
        match self {
            Action::Inspect { app } | Action::Logs { app, .. } | Action::Suspend { app } | Action::Resume { app } => {
                app
            }
            Action::VolumeInfo { name } | Action::DeleteSnapshot { name, .. } | Action::Backup { name, .. } => name,
            Action::PruneImages { .. } | Action::Upgrade { .. } => "",
        }
    }
}

/// What a node answers [`Action::Upgrade`] with, before it restarts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeAnswer {
    /// The version it's restarting into.
    pub version: String,
    /// Whether it's restarting (it isn't if it already runs that version).
    pub restarting: bool,
}

/// `POST /agent/commands/{id}/result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommandResult {
    Ok {
        #[serde(default)]
        value: serde_json::Value,
    },
    Err {
        /// The HTTP status the error maps to.
        status: u16,
        error: ErrorBody,
    },
}

/// The error a server answers a forgotten node with, with `410 Gone`, in a [`ForgottenBody`]; see the design's
/// "Failure and recovery". Nothing else means the node was forgotten: not a network error, not a 401.
pub const FORGOTTEN: &str = "node forgotten";

/// What a server answers a forgotten node's requests with (`410 Gone`). The node only takes it as being forgotten
/// if `error` is [`FORGOTTEN`] and `node_id` is its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForgottenBody {
    pub error: String,
    pub node_id: u64,
    pub name: String,
    /// Unix seconds.
    pub forgotten_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_round_trip() {
        let c = AppChange {
            name: "blog".into(),
            generation: 3,
            seq: 9,
            want: AppWant::Deploy {
                bundle_digest: "sha256:ab".into(),
                from: None,
                shared: vec![],
                restore: None,
            },
        };
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["want"]["kind"], "deploy");
        assert_eq!(serde_json::from_value::<AppChange>(json).unwrap(), c);
        let a = Action::Logs {
            app: "blog".into(),
            query: LogsQuery::default(),
        };
        let json = serde_json::to_string(&a).unwrap();
        assert_eq!(serde_json::from_str::<Action>(&json).unwrap(), a);
        assert_eq!(a.target(), "blog");
    }

    #[test]
    fn older_reports_still_parse() {
        // A report without the optional parts, as an older or smaller node might send.
        let r: Report = serde_json::from_str(
            r#"{"facts":{"arch":"amd64","memory_bytes":1,"memory_available_bytes":1,
                "volumes":{"total_bytes":1,"free_bytes":1},"state":{"total_bytes":1,"free_bytes":1},
                "version":"0.1.0","commit":"x"}}"#,
        )
        .unwrap();
        assert!(r.apps.is_empty() && r.applied.apps.is_empty());
    }
}

//! Upgrades: the server offers its own release to the nodes older than itself, one node at a time.
//!
//! The server has its release for each architecture it can upgrade ([`Offer`]): the files `kiwi-nest upgrade`
//! fetched into its release's `dist/`, checked against their `SHA256SUMS` when it starts, and its own binary for its
//! own architecture. A node gets its architecture's from `GET /agent/releases/{version}/{arch}`.
//!
//! The rollout: whenever no node is being upgraded, the first node (by name) that's online and older than the
//! server, speaks a link with upgrades ([`UPGRADE_PROTOCOL`]) and runs from an installed release gets
//! [`Action::Upgrade`]. It downloads the release, checks it, answers, and restarts into it; its apps keep running.
//! Once it polls again reporting the server's version, the next node's turn comes. A node that refuses, or doesn't
//! come back with the new version within [`crate::ServerConfig::upgrade_restart_timeout`], fails, and that halts the
//! rollout: one bad release stops at one node. `POST /upgrade` (`kiwi-nest upgrade nodes`), or a server restart,
//! tries again.
//!
//! The server keeps serving nodes one link version behind it ([`kn_proto::agent::OLDEST_PROTOCOL`]), so a cluster
//! mid-rollout works as usual.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use kn_proto::agent::{Action, UPGRADE_PROTOCOL, UpgradeAnswer};
use kn_proto::api::{NodeUpgrade, UpgradeState, UpgradeStatus};
use kn_proto::version::Version;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::Server;
use crate::store::nodes::{self, Node};

/// A release the server can hand nodes of one architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// `amd64` or `arm64`, as nodes report it.
    pub arch: String,
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

/// The rollout's state, only in memory: a restart starts afresh.
pub struct Upgrades {
    offers: BTreeMap<String, Offer>,
    state: Mutex<State>,
    wake: Notify,
}

#[derive(Default)]
struct State {
    /// Nodes being upgraded, or whose upgrade failed (and why), by ID.
    tracks: BTreeMap<u64, Track>,
}

#[derive(Debug, Clone)]
enum Track {
    Upgrading,
    Failed(String),
}

impl Upgrades {
    pub fn new(offers: Vec<Offer>) -> Upgrades {
        Upgrades {
            // The first for each architecture.
            offers: offers.into_iter().rev().map(|o| (o.arch.clone(), o)).collect(),
            state: Mutex::new(State::default()),
            wake: Notify::new(),
        }
    }

    /// The release for an architecture, if the server has one.
    pub fn offer(&self, arch: &str) -> Option<&Offer> {
        self.offers.get(arch)
    }

    fn halted(&self) -> bool {
        self.state
            .lock()
            .expect("upgrades")
            .tracks
            .values()
            .any(|t| matches!(t, Track::Failed(_)))
    }

    fn track(&self, id: u64) -> Option<Track> {
        self.state.lock().expect("upgrades").tracks.get(&id).cloned()
    }

    fn set(&self, id: u64, t: Option<Track>) {
        let mut s = self.state.lock().expect("upgrades");
        match t {
            Some(t) => s.tracks.insert(id, t),
            None => s.tracks.remove(&id),
        };
    }

    /// Forgets failures, so the rollout carries on (`POST /upgrade`).
    pub fn retry(&self) {
        self.state
            .lock()
            .expect("upgrades")
            .tracks
            .retain(|_, t| !matches!(t, Track::Failed(_)));
        self.wake.notify_one();
    }

    /// Where a node stands.
    fn classify(&self, n: &Node, online: bool) -> (UpgradeState, Option<String>) {
        let me = Version::current();
        let Some(f) = &n.facts else {
            return (UpgradeState::Pending, Some("it hasn't reported yet".into()));
        };
        let Some(v) = Version::parse(&f.version) else {
            return (
                UpgradeState::Manual,
                Some(format!("it reports version {:?}, which isn't a release's", f.version)),
            );
        };
        if v >= me {
            return (UpgradeState::Current, None);
        }
        match self.track(n.id) {
            Some(Track::Upgrading) => return (UpgradeState::Upgrading, None),
            Some(Track::Failed(e)) => return (UpgradeState::Failed, Some(e)),
            None => {}
        }
        if f.protocol < UPGRADE_PROTOCOL {
            return (
                UpgradeState::Manual,
                Some(format!(
                    "it speaks version {} of the agent link, from before upgrades: run `kiwi-nest upgrade` on it",
                    f.protocol
                )),
            );
        }
        if !f.self_upgrade {
            return (
                UpgradeState::Manual,
                Some(
                    "it doesn't run from an installed release (`kiwi-nest node join` installs one), so it can't \
                     upgrade itself"
                        .into(),
                ),
            );
        }
        if self.offer(&f.arch).is_none() {
            return (
                UpgradeState::Manual,
                Some(format!(
                    "the server has no {} release: run `kiwi-nest upgrade` on the server's host, which fetches every \
                     architecture's",
                    f.arch
                )),
            );
        }
        if !online {
            return (
                UpgradeState::Pending,
                Some("offline: it's upgraded once it's back".into()),
            );
        }
        (UpgradeState::Pending, None)
    }

    /// Where the rollout stands, node by node.
    pub async fn status(&self, s: &Server) -> crate::error::Result<UpgradeStatus> {
        let list = s.store.call(|c| nodes::list(c)).await?;
        let online = s.online();
        Ok(UpgradeStatus {
            version: kn_proto::VERSION.into(),
            arches: self.offers.keys().cloned().collect(),
            halted: self.halted(),
            nodes: list
                .iter()
                .map(|n| {
                    let on = online(n.id);
                    let (state, detail) = self.classify(n, on);
                    NodeUpgrade {
                        node: n.name.clone(),
                        version: n.facts.as_ref().map(|f| f.version.clone()).unwrap_or_default(),
                        online: on,
                        state,
                        detail,
                    }
                })
                .collect(),
        })
    }

    /// The next node to upgrade, unless one is being upgraded or the rollout is halted.
    async fn next(&self, s: &Server) -> crate::error::Result<Option<Node>> {
        if self.offers.is_empty() || self.halted() {
            return Ok(None);
        }
        let list = s.store.call(|c| nodes::list(c)).await?;
        let online = s.online();
        if list
            .iter()
            .any(|n| matches!(self.classify(n, online(n.id)).0, UpgradeState::Upgrading))
        {
            return Ok(None);
        }
        Ok(list.into_iter().find(|n| {
            let on = online(n.id);
            on && self.classify(n, on) == (UpgradeState::Pending, None)
        }))
    }
}

/// Runs the rollout until `stop`: looks for a node to upgrade every few seconds, and at once when woken.
pub async fn run(server: Server, stop: CancellationToken) {
    let u = server.upgrades.clone();
    if u.offers.is_empty() {
        return;
    }
    tracing::info!(
        version = kn_proto::VERSION,
        arches = ?u.offers.keys().collect::<Vec<_>>(),
        "offering this release to older nodes"
    );
    loop {
        match u.next(&server).await {
            Ok(Some(n)) => {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = upgrade(&server, &n) => {}
                }
                continue;
            }
            Ok(None) => {}
            Err(e) => tracing::warn!(error = %e, "couldn't look for nodes to upgrade"),
        }
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = u.wake.notified() => {}
            _ = tokio::time::sleep(server.cfg.upgrade_check_interval) => {}
        }
    }
}

/// Upgrades one node: the command, then waiting for it to come back running the server's version.
async fn upgrade(s: &Server, n: &Node) {
    let u = &s.upgrades;
    let arch = n.facts.as_ref().map(|f| f.arch.clone()).unwrap_or_default();
    let Some(offer) = u.offer(&arch).cloned() else { return };
    let from = n.facts.as_ref().map(|f| f.version.clone()).unwrap_or_default();
    tracing::info!(node = n.name, from, to = kn_proto::VERSION, "upgrading a node");
    u.set(n.id, Some(Track::Upgrading));
    let fail = |e: String| {
        tracing::warn!(
            node = n.name,
            error = e,
            "a node's upgrade failed; the rollout stops here"
        );
        u.set(n.id, Some(Track::Failed(e)));
    };
    let action = Action::Upgrade {
        version: kn_proto::VERSION.into(),
        sha256: offer.sha256.clone(),
        size: offer.size,
    };
    let answer = s
        .command_with(n.id, &n.name, action, "upgrade", s.cfg.upgrade_timeout)
        .await
        .and_then(crate::api::result::<UpgradeAnswer>);
    match answer {
        Ok(a) if a.version == kn_proto::VERSION => {}
        Ok(a) => return fail(format!("it said it would run {}, not {}", a.version, kn_proto::VERSION)),
        Err(e) => return fail(e.to_string()),
    }
    // It restarts into the new release; wait for it to poll again, saying so.
    let deadline = tokio::time::Instant::now() + s.cfg.upgrade_restart_timeout;
    let me = Version::current();
    loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let id = n.id;
        match s.store.call(move |c| nodes::get(c, id)).await {
            Ok(node) if node.forgotten_at.is_some() => {
                u.set(n.id, None);
                return;
            }
            Ok(node) => {
                let v = node.facts.as_ref().and_then(|f| Version::parse(&f.version));
                if s.hub.is_online(id) && v.is_some_and(|v| v >= me) {
                    tracing::info!(node = n.name, version = kn_proto::VERSION, "a node was upgraded");
                    u.set(n.id, None);
                    return;
                }
            }
            Err(e) => tracing::debug!(error = %e, "reading a node being upgraded"),
        }
        if tokio::time::Instant::now() >= deadline {
            return fail(format!(
                "it didn't come back running {} within {}s of saying it would restart",
                kn_proto::VERSION,
                s.cfg.upgrade_restart_timeout.as_secs()
            ));
        }
    }
}

//! The node agent: the node's side of the link to the server (see [`kn_proto::agent`]).
//!
//! - **Joining.** The first start swaps a join token for the node's own token and its ingress token (which the
//!   gateway expects from the ingress), kept in `STATE/agent.json` (mode 0600) with the server's URL, so later starts
//!   need neither. A node that joined before the server gave out ingress tokens asks for its own.
//! - **Polling.** One loop long-polls the server with a report of the node (its facts, its apps and volumes, the
//!   changes it has finished, the commands it's running), and hands what comes back to the applier and to command
//!   tasks. It never waits for either, so it never stalls. When a change finishes, a reporter sends a report at
//!   once on a route of its own, beside the open poll (never cancelling it, so a command on its way can't be
//!   lost), so it reaches the server (and the CLI waiting on it) without waiting for the poll to time out. Reports
//!   carry a serial, so one that arrives late never replaces a newer one.
//! - **Applying.** Each app and each shared volume has a worker that applies its changes one at a time, always the
//!   latest one it has been given. A deploy downloads its bundle by digest, checks the digest, and deploys it with
//!   the server's generation. Every finished change, done or failed, is recorded in `agent.json` with its
//!   generation before it's reported, so a change received again (after a restart, or a lost response) is never
//!   run twice; one cut off by a restart wasn't recorded, so it runs again when the node comes back. A change whose
//!   bundle can't be downloaded yet is retried. A deploy `--from` another app, or mounting a shared volume, waits
//!   for that app's or volume's earlier changes first; deletes and volume changes wait for every earlier change.
//! - **Commands** each run in a task of their own, and their results (or, for logs, their output, streamed) are
//!   sent back on the link.
//! - **Forgotten.** When the server answers that it forgot the node (`node forget`, and only that: see
//!   [`LinkError::Forgotten`]), the agent stops applying anything, takes every app down, sets the node's data aside
//!   and stops; see [`forgotten`]. It refuses to start again until it's given a fresh join token.

pub mod forgotten;
mod link;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use futures_util::StreamExt;
use kn_proto::agent::{
    Action, AppChange, AppWant, Applied, Command, CommandResult, DiskSpace, JoinRequest, NodeFacts, Outcome,
    PollRequest, PollResponse, Report, VolumeChange, VolumeWant,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, Notify, watch};
use tokio_util::sync::CancellationToken;

pub use forgotten::Forgotten;
pub use link::{Link, LinkError, install_tls};

use crate::NodeConfig;
use crate::api::ApiError;
use crate::apps::{Apps, AppsError, DeployOptions, LoadSource, Logs, write_atomic};
use crate::runtime::Runtime;

/// Version 1: the server, the node's ID, name and token, and what it has applied. Version 2 added `forgotten`.
const FILE_VERSION: u32 = 2;

/// How the agent is set up.
#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// The server's URL.
    pub server: String,
    /// The node's name, used when it joins.
    pub name: String,
    /// The one-time token to join with, the first time.
    pub join_token: Option<String>,
    pub labels: BTreeMap<String, String>,
    /// The address the node's gateway listens on, on loopback, as reported to the server.
    pub gateway: String,
    /// The address its gateway listens on for the ingress, as reported to the server; empty if none.
    pub ingress: String,
    /// Extra root certificates (PEM) to trust for the server, such as the ingress's internal CA.
    pub ca_certs: Option<Vec<u8>>,
    /// The longest poll to ask for.
    pub poll_wait: Duration,
    /// The longest wait between attempts when the server can't be reached.
    pub max_backoff: Duration,
}

impl AgentConfig {
    pub fn new(server: impl Into<String>, name: impl Into<String>) -> Self {
        AgentConfig {
            server: server.into(),
            name: name.into(),
            join_token: None,
            labels: BTreeMap::new(),
            gateway: String::new(),
            ingress: String::new(),
            ca_certs: None,
            poll_wait: Duration::from_secs(25),
            max_backoff: Duration::from_secs(5),
        }
    }
}

/// `STATE/agent.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentFile {
    version: u32,
    server: String,
    node_id: u64,
    name: String,
    /// Empty once the node is forgotten.
    token: String,
    /// The token the ingress adds to its requests to the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ingress_token: Option<String>,
    #[serde(default)]
    applied: Applied,
    /// Set once the server said it forgot this node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    forgotten: Option<Forgotten>,
}

/// Reads `STATE/agent.json`, if it's there.
fn read_file(path: &Path) -> anyhow::Result<Option<AgentFile>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let f: AgentFile = serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display()))?;
            if f.version > FILE_VERSION {
                anyhow::bail!("{} was written by a newer kiwi-nest", path.display());
            }
            Ok(Some(f))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("reading {}: {e}", path.display())),
    }
}

/// Run before the node opens its apps. If the server forgot this node: finishes taking its apps down and setting its
/// data aside if that was cut off, so an app that should be down is never adopted (and started) again; then refuses,
/// unless the node is given a fresh join token (`joining`) to start again as a new node.
pub async fn prepare(cfg: &NodeConfig, runtime: &dyn Runtime, joining: bool) -> anyhow::Result<()> {
    let path = cfg.state_dir.join("agent.json");
    let Some(mut f) = read_file(&path)? else { return Ok(()) };
    let Some(mut rec) = f.forgotten.clone() else {
        return Ok(());
    };
    if !rec.done {
        tracing::warn!("the server forgot this node; finishing taking its apps down and setting its data aside");
        rec = forgotten::set_aside(cfg, runtime, rec).await?;
        f.forgotten = Some(rec.clone());
        let bytes = serde_json::to_vec_pretty(&f)?;
        tokio::task::spawn_blocking(move || write_atomic(&path, &bytes)).await??;
    }
    if !joining {
        anyhow::bail!("{}", rec.describe());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Key {
    App(String),
    Volume(String),
}

/// The latest change for one app or volume.
#[derive(Debug, Clone)]
enum Want {
    App(AppChange),
    Volume(VolumeChange),
}

impl Want {
    fn generation(&self) -> u64 {
        match self {
            Want::App(c) => c.generation,
            Want::Volume(c) => c.generation,
        }
    }

    fn seq(&self) -> u64 {
        match self {
            Want::App(c) => c.seq,
            Want::Volume(c) => c.seq,
        }
    }

    /// What must be done first: `None` for every earlier change.
    fn deps(&self) -> Option<Vec<Key>> {
        match self {
            Want::App(AppChange {
                want: AppWant::Deploy { from, shared, .. },
                ..
            }) => Some(
                from.iter()
                    .map(|f| Key::App(f.clone()))
                    .chain(shared.iter().map(|s| Key::Volume(s.clone())))
                    .collect(),
            ),
            _ => None,
        }
    }
}

/// What applying a change came to.
enum Step {
    /// It's finished, done or failed: record it.
    Done(Outcome),
    /// It couldn't be applied yet (the server couldn't be reached, say): try again later.
    Retry(String),
}

/// The node agent.
#[derive(Clone)]
pub struct Agent {
    inner: Arc<Inner>,
}

struct Inner {
    apps: Apps,
    cfg: AgentConfig,
    link: Link,
    path: PathBuf,
    node_id: u64,
    name: String,
    file: Mutex<AgentFile>,
    /// Held while `agent.json` is written, so writes land in order.
    persist: AsyncMutex<()>,
    /// Notified when there's something new to report.
    report_now: Notify,
    running: Mutex<BTreeSet<u64>>,
    work: Mutex<HashMap<Key, watch::Sender<Option<Want>>>>,
    /// Bumped whenever a change finishes or a new one is stored, for workers waiting on others.
    progress: watch::Sender<u64>,
    /// The serial of the next report: milliseconds since the epoch when the agent started, so it keeps growing
    /// across restarts too.
    serial: std::sync::atomic::AtomicU64,
    /// Set once the server says it forgot this node: nothing more is applied or run.
    forgotten: std::sync::atomic::AtomicBool,
    /// Held (shared) while a change is applied or a command that changes something runs, and exclusively once the
    /// node is forgotten, to wait for those in flight.
    applying: tokio::sync::RwLock<()>,
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn failed(generation: u64, e: AppsError) -> Outcome {
    let (_, body) = ApiError::from(e).into_parts();
    Outcome {
        generation,
        ok: false,
        error: Some(body.error),
        problems: body.problems,
        notes: Vec::new(),
        deleted: false,
        data_digest: None,
        finished_at: now(),
    }
}

fn done(generation: u64) -> Outcome {
    Outcome {
        generation,
        ok: true,
        error: None,
        problems: Vec::new(),
        notes: Vec::new(),
        deleted: false,
        data_digest: None,
        finished_at: now(),
    }
}

impl Agent {
    /// Starts the agent's state: reads `agent.json`, or joins the server with the join token and writes it.
    pub async fn start(apps: Apps, cfg: AgentConfig) -> anyhow::Result<Agent> {
        let path = apps.config().state_dir.join("agent.json");
        let file = match read_file(&path)? {
            Some(f) if f.forgotten.is_none() => {
                if f.server.trim_end_matches('/') != cfg.server.trim_end_matches('/') {
                    anyhow::bail!(
                        "this node joined the server at {}, not {}; its state is in {}",
                        f.server,
                        cfg.server,
                        path.display()
                    );
                }
                f
            }
            existing => {
                if let Some(rec) = existing.and_then(|f| f.forgotten) {
                    if !rec.done {
                        anyhow::bail!(
                            "the server forgot this node, and its apps aren't all down yet; start it again to finish"
                        );
                    }
                    if cfg.join_token.is_none() {
                        anyhow::bail!("{}", rec.describe());
                    }
                    tracing::info!(
                        old = rec.name,
                        "this node was forgotten; joining again as a new node with the join token"
                    );
                }
                let token = cfg
                    .join_token
                    .clone()
                    .context("this node hasn't joined a server yet: pass --join-token")?;
                let facts = facts(&apps, &cfg).await;
                let joined = Link::join(
                    &cfg.server,
                    cfg.ca_certs.as_deref(),
                    &JoinRequest {
                        token,
                        name: cfg.name.clone(),
                        facts,
                    },
                )
                .await
                .with_context(|| format!("joining {}", cfg.server))?;
                let f = AgentFile {
                    version: FILE_VERSION,
                    server: cfg.server.clone(),
                    node_id: joined.node_id,
                    name: joined.name,
                    token: joined.token,
                    ingress_token: Some(joined.ingress_token).filter(|t| !t.is_empty()),
                    applied: Applied::default(),
                    forgotten: None,
                };
                let bytes = serde_json::to_vec_pretty(&f)?;
                let p = path.clone();
                tokio::task::spawn_blocking(move || write_atomic(&p, &bytes)).await??;
                tracing::info!(server = cfg.server, node = f.name, "joined the server");
                f
            }
        };
        let link = Link::new(&cfg.server, &file.token, cfg.ca_certs.as_deref())?;
        let mut file = file;
        if file.ingress_token.is_none() && !cfg.ingress.is_empty() {
            let t = link
                .ingress_token()
                .await
                .with_context(|| format!("getting this node's ingress token from {}", cfg.server))?;
            if t.is_empty() {
                anyhow::bail!("the server gave this node an empty ingress token");
            }
            file.ingress_token = Some(t);
            let bytes = serde_json::to_vec_pretty(&file)?;
            let p = path.clone();
            tokio::task::spawn_blocking(move || write_atomic(&p, &bytes)).await??;
        }
        apps.set_targets(Arc::new(ServerTargets { link: link.clone() }));
        Ok(Agent {
            inner: Arc::new(Inner {
                apps,
                link,
                path,
                node_id: file.node_id,
                name: file.name.clone(),
                cfg,
                file: Mutex::new(file),
                persist: AsyncMutex::new(()),
                report_now: Notify::new(),
                running: Mutex::new(BTreeSet::new()),
                work: Mutex::new(HashMap::new()),
                progress: watch::Sender::new(0),
                serial: std::sync::atomic::AtomicU64::new(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis() as u64),
                ),
                forgotten: std::sync::atomic::AtomicBool::new(false),
                applying: tokio::sync::RwLock::new(()),
            }),
        })
    }

    /// The node's name in the cluster.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn node_id(&self) -> u64 {
        self.inner.node_id
    }

    /// The token the gateway expects from the ingress, if the server has given one.
    pub fn ingress_token(&self) -> Option<String> {
        self.inner.file.lock().expect("agent file").ingress_token.clone()
    }

    /// Polls the server, and reports finished changes, until `stop` is cancelled. If the server says it forgot this
    /// node, takes every app down, sets the node's data aside, and fails with why.
    pub async fn run(&self, stop: CancellationToken) -> anyhow::Result<()> {
        let mine = stop.child_token();
        let (forgotten, ()) = tokio::join!(
            async {
                let f = self.inner.clone().poll_loop(mine.clone()).await;
                mine.cancel();
                f
            },
            self.inner.clone().report_loop(mine.clone())
        );
        match forgotten {
            Some(body) => Err(self.inner.forget(body).await),
            None => Ok(()),
        }
    }
}

impl Inner {
    fn applied(&self) -> Applied {
        self.file.lock().expect("agent file").applied.clone()
    }

    fn outcome(&self, key: &Key) -> Option<Outcome> {
        let f = self.file.lock().expect("agent file");
        match key {
            Key::App(n) => f.applied.apps.get(n).cloned(),
            Key::Volume(n) => f.applied.volumes.get(n).cloned(),
        }
    }

    fn applied_generation(&self, key: &Key) -> u64 {
        self.outcome(key).map_or(0, |o| o.generation)
    }

    /// Records a finished change in `agent.json`, then has it reported.
    async fn record(&self, key: &Key, outcome: Outcome) {
        let _guard = self.persist.lock().await;
        let bytes = {
            let mut f = self.file.lock().expect("agent file");
            match key {
                Key::App(n) => f.applied.apps.insert(n.clone(), outcome),
                Key::Volume(n) => f.applied.volumes.insert(n.clone(), outcome),
            };
            serde_json::to_vec_pretty(&*f).expect("the agent file serializes")
        };
        self.write(bytes).await;
        self.progress.send_modify(|n| *n += 1);
        self.report_now.notify_one();
    }

    async fn write(&self, bytes: Vec<u8>) {
        let p = self.path.clone();
        match tokio::task::spawn_blocking(move || write_atomic(&p, &bytes)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "couldn't write agent.json"),
            Err(e) => tracing::error!(error = %e, "couldn't write agent.json"),
        }
    }

    /// Forgets deletes that the server has acknowledged (they were in a report it answered).
    async fn prune(&self, reported: &Applied) {
        let _guard = self.persist.lock().await;
        let bytes = {
            let mut f = self.file.lock().expect("agent file");
            let before = (f.applied.apps.len(), f.applied.volumes.len());
            f.applied
                .apps
                .retain(|k, o| !(o.deleted && o.ok && reported.apps.get(k) == Some(o)));
            f.applied
                .volumes
                .retain(|k, o| !(o.deleted && o.ok && reported.volumes.get(k) == Some(o)));
            if before == (f.applied.apps.len(), f.applied.volumes.len()) {
                return;
            }
            serde_json::to_vec_pretty(&*f).expect("the agent file serializes")
        };
        self.write(bytes).await;
    }

    async fn report(&self) -> Report {
        Report {
            facts: facts(&self.apps, &self.cfg).await,
            apps: self.apps.reports().await,
            volumes: self.apps.volume_list().await.unwrap_or_else(|e| {
                tracing::warn!(error = %e, "couldn't list the shared volumes");
                Vec::new()
            }),
            applied: self.applied(),
            running_commands: self.running.lock().expect("running").iter().copied().collect(),
            serial: self.serial.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// Sends a report whenever a change finishes. If it can't, the next poll carries the news anyway.
    async fn report_loop(self: Arc<Self>, stop: CancellationToken) {
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = self.report_now.notified() => {}
            }
            let report = self.report().await;
            let applied = report.applied.clone();
            match self.link.report(&report).await {
                Ok(()) => self.prune(&applied).await,
                Err(e) => tracing::debug!(error = %e, "sending a report failed; the next poll carries it"),
            }
        }
    }

    /// Returns when `stop` is cancelled, or with what the server said if it forgot this node.
    async fn poll_loop(self: Arc<Self>, stop: CancellationToken) -> Option<kn_proto::agent::ForgottenBody> {
        let mut since = 0;
        let mut backoff = Duration::from_millis(250);
        let mut failing = false;
        loop {
            let report = self.report().await;
            let applied = report.applied.clone();
            let req = PollRequest {
                protocol: kn_proto::agent::PROTOCOL,
                since,
                wait_secs: Some(self.cfg.poll_wait.as_secs().max(1)),
                report,
            };
            let result = tokio::select! {
                _ = stop.cancelled() => return None,
                r = self.link.poll(&req, self.cfg.poll_wait) => r,
            };
            match result {
                Ok(resp) => {
                    if failing {
                        tracing::info!(server = self.cfg.server, "reached the server again");
                        failing = false;
                    }
                    backoff = Duration::from_millis(250);
                    since = resp.seq;
                    self.prune(&applied).await;
                    self.dispatch(resp);
                }
                Err(LinkError::Forgotten(body)) if body.node_id == self.node_id => return Some(body),
                Err(e) => {
                    if !failing {
                        tracing::warn!(server = self.cfg.server, error = %e, "polling the server failed; retrying");
                        failing = true;
                    } else {
                        tracing::debug!(error = %e, "polling the server failed");
                    }
                    tokio::select! {
                        _ = stop.cancelled() => return None,
                        _ = tokio::time::sleep(backoff) => {}
                    }
                    backoff = (backoff * 2).min(self.cfg.max_backoff);
                }
            }
        }
    }

    /// The server forgot this node: stop applying, record it, take every app down and set the data aside. Returns
    /// why the agent stops (or what went wrong, in which case the next start finishes the job).
    async fn forget(&self, body: kn_proto::agent::ForgottenBody) -> anyhow::Error {
        tracing::error!(
            node = self.name,
            "the server says it forgot this node: taking its apps down and setting its data aside"
        );
        self.forgotten.store(true, std::sync::atomic::Ordering::SeqCst);
        // Whatever is being applied finishes first; nothing starts after this.
        let _all = self.applying.write().await;
        let cfg = self.apps.config().clone();
        let rec = Forgotten::new(
            &cfg,
            &self.cfg.server,
            self.node_id,
            &self.name,
            body.forgotten_at,
            now(),
        );
        if let Err(e) = self.mark_forgotten(rec.clone()).await {
            return e.context("recording that the server forgot this node");
        }
        self.apps.close().await;
        match forgotten::set_aside(&cfg, self.apps.runtime(), rec).await {
            Ok(rec) => match self.mark_forgotten(rec.clone()).await {
                Ok(()) => anyhow::anyhow!("{}", rec.describe()),
                Err(e) => e.context("recording that the node's data is set aside"),
            },
            Err(e) => e,
        }
    }

    /// Records in `agent.json` that the node was forgotten, and drops its token.
    async fn mark_forgotten(&self, rec: Forgotten) -> anyhow::Result<()> {
        let _guard = self.persist.lock().await;
        let bytes = {
            let mut f = self.file.lock().expect("agent file");
            f.forgotten = Some(rec);
            f.token.clear();
            f.version = FILE_VERSION;
            serde_json::to_vec_pretty(&*f)?
        };
        let p = self.path.clone();
        tokio::task::spawn_blocking(move || write_atomic(&p, &bytes)).await??;
        Ok(())
    }

    fn dispatch(self: &Arc<Self>, resp: PollResponse) {
        for c in resp.apps {
            self.want(Key::App(c.name.clone()), Want::App(c));
        }
        for c in resp.volumes {
            self.want(Key::Volume(c.name.clone()), Want::Volume(c));
        }
        for c in resp.commands {
            // Listed as running before the next poll is sent, so the server knows it arrived.
            self.running.lock().expect("running").insert(c.id);
            tokio::spawn(self.clone().command(c));
        }
    }

    /// Hands a change to its worker, starting one if needed. An older change than the worker has is ignored.
    ///
    /// Workers blocked on others' earlier changes are woken, since what they wait on depends on every key's pending
    /// sequence number: a change replaced by a later one (with a higher sequence number) may no longer be earlier
    /// than theirs, and if nobody looked again, two workers could each wait on the other's old change for ever.
    fn want(self: &Arc<Self>, key: Key, want: Want) {
        let mut work = self.work.lock().expect("work");
        let stored = match work.get(&key) {
            Some(tx) => tx.send_if_modified(|w| match w {
                Some(old) if old.generation() >= want.generation() => false,
                _ => {
                    *w = Some(want);
                    true
                }
            }),
            None => {
                let (tx, rx) = watch::channel(Some(want));
                work.insert(key.clone(), tx);
                tokio::spawn(self.clone().worker(key, rx));
                true
            }
        };
        drop(work);
        if stored {
            self.progress.send_modify(|n| *n += 1);
        }
    }

    /// The sequence number of a change waiting to be applied for `key`, if any.
    fn pending(&self, key: &Key) -> Option<u64> {
        let w = self.work.lock().expect("work").get(key)?.borrow().clone()?;
        (w.generation() > self.applied_generation(key)).then(|| w.seq())
    }

    /// Whether a change at `seq` must wait: for `deps`' earlier changes, or for every earlier change.
    fn blocked(&self, me: &Key, deps: Option<&[Key]>, seq: u64) -> bool {
        let keys: Vec<Key> = match deps {
            Some(d) => d.to_vec(),
            None => self.work.lock().expect("work").keys().cloned().collect(),
        };
        keys.iter()
            .filter(|k| *k != me)
            .any(|k| self.pending(k).is_some_and(|s| s < seq))
    }

    async fn worker(self: Arc<Self>, key: Key, mut rx: watch::Receiver<Option<Want>>) {
        let mut progress = self.progress.subscribe();
        let mut backoff = Duration::from_millis(500);
        'next: loop {
            // The latest change, once there's one newer than what's applied.
            let want = loop {
                let w = rx.borrow_and_update().clone();
                match w {
                    Some(w) if w.generation() > self.applied_generation(&key) => break w,
                    _ => {
                        if self.retire(&key) || rx.changed().await.is_err() {
                            return;
                        }
                    }
                }
            };
            while self.blocked(&key, want.deps().as_deref(), want.seq()) {
                tokio::select! {
                    _ = progress.changed() => {}
                    _ = rx.changed() => continue 'next,
                }
            }
            let applying = self.applying.read().await;
            if self.forgotten.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let step = self.apply(&want).await;
            drop(applying);
            match step {
                Step::Done(outcome) => {
                    backoff = Duration::from_millis(500);
                    if outcome.ok {
                        tracing::info!(?key, generation = outcome.generation, "applied");
                    } else {
                        tracing::warn!(?key, generation = outcome.generation, error = ?outcome.error, "failed");
                    }
                    self.record(&key, outcome).await;
                }
                Step::Retry(why) => {
                    tracing::warn!(
                        ?key,
                        generation = want.generation(),
                        why,
                        "can't apply this yet; retrying"
                    );
                    tokio::select! {
                        _ = tokio::time::sleep(backoff) => {}
                        _ = rx.changed() => {}
                    }
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                }
            }
        }
    }

    /// Stops the worker of a deleted app or volume, unless a new change came in meanwhile.
    fn retire(&self, key: &Key) -> bool {
        let deleted = self.outcome(key).is_some_and(|o| o.deleted && o.ok);
        if !deleted {
            return false;
        }
        let mut work = self.work.lock().expect("work");
        let newer = work
            .get(key)
            .and_then(|tx| tx.borrow().as_ref().map(|w| w.generation()))
            .is_some_and(|g| g > self.applied_generation(key));
        if newer {
            return false;
        }
        work.remove(key);
        true
    }

    async fn apply(&self, want: &Want) -> Step {
        match want {
            Want::App(c) => match &c.want {
                AppWant::Deploy {
                    bundle_digest,
                    from,
                    restore,
                    ..
                } => self.deploy(c, bundle_digest, from.clone(), restore.clone()).await,
                AppWant::Delete { keep_data } => match self.apps.delete(&c.name, *keep_data).await {
                    Ok(()) | Err(AppsError::NotFound(_)) => Step::Done(Outcome {
                        deleted: true,
                        ..done(c.generation)
                    }),
                    Err(e) => Step::Done(failed(c.generation, e)),
                },
            },
            Want::Volume(c) => self.volume(c).await,
        }
    }

    /// Downloads a bundle into the staging directory.
    async fn download(&self, digest: &str, limit: u64) -> Result<tempfile::NamedTempFile, Step> {
        let upload = self.apps.upload_file().map_err(|e| Step::Retry(e.to_string()))?;
        match self.link.bundle(digest, upload.path(), limit).await {
            Ok(()) => Ok(upload),
            // Replaced by a newer change, whose poll is on its way.
            Err(e) if e.is_not_found() => Err(Step::Retry(format!("the server no longer has it: {e}"))),
            Err(e) => Err(Step::Retry(e.to_string())),
        }
    }

    async fn deploy(
        &self,
        c: &AppChange,
        digest: &str,
        from: Option<String>,
        restore: Option<kn_proto::backup::Restore>,
    ) -> Step {
        let limit = self.apps.config().limits.max_archive_size;
        let upload = match self.download(digest, limit).await {
            Ok(u) => u,
            Err(step) => return step,
        };
        let staged = self.apps.stage_zip(upload.path().to_path_buf()).await;
        drop(upload);
        let staged = match staged {
            Ok(s) => s,
            Err(e) => return Step::Done(failed(c.generation, e)),
        };
        if staged.digest() != digest {
            return Step::Done(Outcome {
                error: Some(format!(
                    "the bundle downloaded has digest {}, not {digest}",
                    staged.digest()
                )),
                ok: false,
                ..done(c.generation)
            });
        }
        let opts = DeployOptions {
            create_only: false,
            from,
            restore,
            generation: Some(c.generation),
        };
        tracing::info!(app = c.name, generation = c.generation, "deploying for the server");
        match self.apps.deploy(&c.name, staged, opts).await {
            Ok(info) => Step::Done(Outcome {
                notes: info.notes,
                ..done(c.generation)
            }),
            Err(e) => Step::Done(failed(c.generation, e)),
        }
    }

    async fn volume(&self, c: &VolumeChange) -> Step {
        let key = Key::Volume(c.name.clone());
        match &c.want {
            VolumeWant::Delete => match self.apps.volume_delete(&c.name).await {
                Ok(()) | Err(AppsError::NotFoundOther(_)) => Step::Done(Outcome {
                    deleted: true,
                    ..done(c.generation)
                }),
                Err(e) => Step::Done(failed(c.generation, e)),
            },
            VolumeWant::Present { data_digest, restore } => {
                let loaded = self.outcome(&key).and_then(|o| o.data_digest);
                let mut notes = Vec::new();
                match self.apps.volume_info(&c.name).await {
                    Ok(_) => {}
                    Err(AppsError::NotFoundOther(_)) => {
                        if let Err(e) = self.apps.volume_create_from(&c.name, restore.as_ref()).await {
                            return Step::Done(failed(c.generation, e));
                        }
                        if let Some(r) = restore {
                            notes.push(format!("restored from `{}` (target `{}`)", r.key, r.target));
                        }
                    }
                    Err(e) => return Step::Retry(e.to_string()),
                }
                let Some(digest) = data_digest.as_ref().filter(|d| Some(*d) != loaded.as_ref()) else {
                    return Step::Done(Outcome {
                        data_digest: loaded,
                        notes,
                        ..done(c.generation)
                    });
                };
                let limit = self.apps.config().load_limits.max_archive_size;
                let upload = match self.download(digest, limit).await {
                    Ok(u) => u,
                    Err(step) => return step,
                };
                let r = self
                    .apps
                    .volume_load(
                        &c.name,
                        LoadSource::Digest {
                            zip: upload.path().to_path_buf(),
                            digest: digest.clone(),
                        },
                    )
                    .await;
                drop(upload);
                match r {
                    Ok(_) => Step::Done(Outcome {
                        data_digest: Some(digest.clone()),
                        ..done(c.generation)
                    }),
                    Err(e) => Step::Done(Outcome {
                        data_digest: loaded,
                        ..failed(c.generation, e)
                    }),
                }
            }
        }
    }

    async fn command(self: Arc<Self>, c: Command) {
        tracing::debug!(id = c.id, action = ?c.action, "running a command");
        fn json<T: Serialize>(r: Result<T, AppsError>) -> CommandResult {
            match r {
                Ok(v) => CommandResult::Ok {
                    value: serde_json::to_value(v).unwrap_or_default(),
                },
                Err(e) => {
                    let (status, error) = ApiError::from(e).into_parts();
                    CommandResult::Err {
                        status: status.as_u16(),
                        error,
                    }
                }
            }
        }
        // Those that change something wait for a forgotten node to finish forgetting, and then don't run. A backup
        // only reads its snapshots, so it doesn't hold forgetting up (an upload can take hours), but it doesn't
        // start once the node is forgotten either.
        let changes = matches!(
            c.action,
            Action::Suspend { .. } | Action::Resume { .. } | Action::DeleteSnapshot { .. }
        );
        if matches!(c.action, Action::Backup { .. }) && self.forgotten.load(std::sync::atomic::Ordering::SeqCst) {
            self.running.lock().expect("running").remove(&c.id);
            return;
        }
        let _applying = if changes {
            Some(self.applying.read().await)
        } else {
            None
        };
        if changes && self.forgotten.load(std::sync::atomic::Ordering::SeqCst) {
            self.running.lock().expect("running").remove(&c.id);
            return;
        }
        let result = match c.action {
            Action::Inspect { app } => Some(json(self.apps.inspect(&app).await)),
            Action::Suspend { app } => Some(json(self.apps.suspend(&app).await)),
            Action::Resume { app } => Some(json(self.apps.resume(&app).await)),
            Action::VolumeInfo { name } => Some(json(self.apps.volume_info(&name).await)),
            Action::DeleteSnapshot { name, app } => Some(json(self.apps.volume_delete_snapshot(&name, &app).await)),
            Action::Backup {
                what,
                name,
                targets,
                full,
            } => Some(json(self.apps.backup(what, &name, &targets, full).await)),
            Action::Logs { app, query } => match self.apps.logs(&app, query).await {
                Ok(logs) => {
                    if let Err(e) = self.stream_logs(c.id, logs).await {
                        tracing::warn!(id = c.id, error = %e, "couldn't send the logs");
                    }
                    None
                }
                Err(e) => Some(json::<()>(Err(e))),
            },
        };
        if let Some(result) = result {
            for attempt in 0..5 {
                match self.link.result(c.id, &result).await {
                    Ok(()) => break,
                    Err(e) if attempt < 4 => {
                        tracing::debug!(id = c.id, error = %e, "sending a command's result failed; retrying");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                    Err(e) => tracing::warn!(id = c.id, error = %e, "couldn't send a command's result"),
                }
            }
        }
        self.running.lock().expect("running").remove(&c.id);
    }

    /// Streams logs to the server until they end or nobody is reading them; a following `docker compose logs` is
    /// then stopped.
    async fn stream_logs(&self, id: u64, logs: Logs) -> Result<(), LinkError> {
        match logs {
            Logs::Complete(bytes) => self.link.output(id, reqwest::Body::from(bytes)).await,
            Logs::Follow(mut child) => {
                let Some(stdout) = child.stdout.take() else {
                    return Err(LinkError::Other("no logs output".into()));
                };
                let done = CancellationToken::new();
                let stream = tokio_util::io::ReaderStream::new(stdout).take_until(done.clone().cancelled_owned());
                let r = self.link.output(id, reqwest::Body::wrap_stream(stream)).await;
                done.cancel();
                let _ = child.start_kill();
                let _ = child.wait().await;
                r
            }
        }
    }
}

/// The server's backup targets, fetched for each backup and restore (so a change to them on the server needs no
/// node restart).
struct ServerTargets {
    link: Link,
}

impl crate::apps::TargetSource for ServerTargets {
    fn targets(&self) -> futures_util::future::BoxFuture<'_, anyhow::Result<Arc<Vec<kn_backup::Target>>>> {
        Box::pin(async move {
            let cfgs = self
                .link
                .backup_targets()
                .await
                .map_err(|e| anyhow::anyhow!("getting the backup targets from the server: {e}"))?;
            if cfgs.is_empty() {
                anyhow::bail!("the server has no backup targets (`kiwi-nest server --backup-targets`)");
            }
            Ok(Arc::new(kn_backup::target::open_all(cfgs)?))
        })
    }
}

/// What the node is, now.
async fn facts(apps: &Apps, cfg: &AgentConfig) -> NodeFacts {
    let node = apps.config().clone();
    let (labels, gateway, ingress) = (cfg.labels.clone(), cfg.gateway.clone(), cfg.ingress.clone());
    tokio::task::spawn_blocking(move || {
        let (memory_bytes, memory_available_bytes) = meminfo().unwrap_or_default();
        let space = |p: &Path, btrfs: bool| {
            let (total_bytes, free_bytes) = kn_btrfs::disk_space(p).unwrap_or_default();
            DiskSpace {
                total_bytes,
                free_bytes,
                unallocated_bytes: btrfs.then(|| kn_btrfs::unallocated(p).ok()).flatten(),
            }
        };
        NodeFacts {
            arch: kn_bundle::manifest::Arch::current()
                .map_or(std::env::consts::ARCH, |a| a.as_str())
                .to_string(),
            memory_bytes,
            memory_available_bytes,
            volumes: space(&node.volumes_dir, true),
            state: space(&node.state_dir, false),
            labels,
            version: kn_proto::VERSION.into(),
            commit: kn_proto::GIT_COMMIT.into(),
            allowed_host_paths: node
                .allowed_host_paths
                .iter()
                .map(|p| p.display().to_string())
                .collect(),
            project_prefix: node.project_prefix.clone(),
            gateway,
            protocol: kn_proto::agent::PROTOCOL,
            ingress,
        }
    })
    .await
    .unwrap_or_default()
}

/// `MemTotal` and `MemAvailable`, in bytes.
fn meminfo() -> Option<(u64, u64)> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let field = |name: &str| -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(name))?;
        let kb: u64 = line[name.len()..].trim().trim_end_matches("kB").trim().parse().ok()?;
        Some(kb * 1024)
    };
    Some((field("MemTotal:")?, field("MemAvailable:")?))
}

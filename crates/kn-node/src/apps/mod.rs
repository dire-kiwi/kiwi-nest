//! Each app's lifecycle on the node, and the node's shared volumes.
//!
//! Every app has one task, and every operation on the app (deploy, inspect, logs, delete, snapshotting it for
//! another app's `--from`, and the gateway's starts and idle stops) goes through it, so an app's operations never
//! overlap. Different apps proceed in parallel. Shared volumes ([`shared`]) each have a lock: deploys that mount one hold it shared, and `volume`
//! commands that change one hold it exclusively.
//!
//! What the node has applied is kept in `STATE/state.json`, written atomically. On disk:
//!
//! ```text
//! STATE/state.json                 the apps (generations, ports, state, subvolume IDs) and shared volumes
//! STATE/staging/                   uploads and bundles being checked
//! STATE/apps/APP/bundle-GEN/       the extracted bundle of APP's generation GEN
//! STATE/apps/APP/compose-GEN.yaml  the Compose file generated from it, outside the bundle
//! VOLUMES/…                        subvolumes; see [`crate::volumes`]
//! ```
//!
//! Every subvolume's ID is recorded once it's in place, and checked before every start, so a volume swapped for
//! something else (a symlink, a plain directory, another subvolume) is refused rather than mounted. A subvolume
//! that's in its place but not recorded (kept with `--keep-data`, or made just before a crash) is adopted by the next
//! deploy.
//!
//! Each app also has a [`Gate`], which the gateway reads to route a request and which the app's task updates as
//! the app is deployed, started and stopped. See [`lifecycle`] for starting, stopping and adopting apps.

mod backup;
mod lifecycle;
mod shared;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::BuildHasher;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use kn_bundle::manifest::{Arch, is_valid_app_name};
use kn_bundle::{BundleError, CheckOptions, CheckedBundle, Problems};
use kn_proto::api::{AppInfo, AppSummary, ContainerInfo, LogsQuery, SharedMountInfo};
use kn_proto::backup::Restore;
use kn_proto::state::{AppStatus, SharedMode};
use serde::{Deserialize, Serialize};
use tokio::process::Child;
use tokio::sync::{Mutex as AsyncMutex, OwnedRwLockReadGuard, RwLock, mpsc, oneshot};

use crate::NodeConfig;
use crate::gateway::gate::{Gate, Phase};
use crate::runtime::{Docker, LogsOptions, Project, Runtime, RuntimeError};
use crate::volumes::{Backend, Btrfs, VolumeError, Volumes};

pub use backup::{StaticTargets, TargetSource};
pub use shared::LoadSource;

/// Version 2 added subvolume IDs and shared volumes.
const STATE_VERSION: u32 = 2;

/// What the node records for one app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRecord {
    pub name: String,
    pub status: AppStatus,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub project: String,
    pub port: u16,
    pub domain: String,
    pub service: String,
    pub container_port: u16,
    pub idle_timeout_secs: u64,
    pub bundle_digest: String,
    pub services: Vec<String>,
    pub volumes: Vec<String>,
    /// The subvolume ID of each named volume that's in place.
    #[serde(default)]
    pub volume_ids: BTreeMap<String, u64>,
    /// The shared volumes it mounts.
    #[serde(default)]
    pub shared: BTreeMap<String, SharedMountRecord>,
    /// The app whose volumes this one's started as snapshots of.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The backup this one's volumes were restored from, on its first deploy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore: Option<Restore>,
    /// Set while the app's containers are paused for a snapshot, so a restarted node unpauses them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    /// Unix seconds.
    pub created_at: u64,
    pub updated_at: u64,
}

/// A shared volume that an app mounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedMountRecord {
    pub target: String,
    pub mode: SharedMode,
    /// In `snapshot` mode, the ID of the app's snapshot, once it's in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<u64>,
}

/// What the node records for one shared volume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedRecord {
    pub name: String,
    /// The subvolume ID of its `data`.
    pub id: u64,
    /// While `volume load` swaps in new contents, their subvolume ID, so a crash in the middle can be told apart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loading: Option<u64>,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded_at: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    apps: BTreeMap<String, AppRecord>,
    #[serde(default)]
    shared: BTreeMap<String, SharedRecord>,
}

#[derive(Debug, thiserror::Error)]
pub enum AppsError {
    #[error("`{0}` isn't a valid app name: use lowercase letters, digits and `-`, at most 50 characters")]
    InvalidName(String),
    #[error("{0}")]
    Bundle(#[from] BundleError),
    #[error("{0}")]
    Refused(Problems),
    #[error("{0}")]
    Unsupported(String),
    #[error("app `{0}` doesn't exist")]
    NotFound(String),
    #[error("{0}")]
    NotFoundOther(String),
    #[error("app `{0}` already exists")]
    Exists(String),
    #[error("domain `{domain}` is already used by app `{app}`")]
    DomainTaken { domain: String, app: String },
    /// Refused because of what else is on the node, such as deleting a volume that an app mounts.
    #[error("{0}")]
    Conflict(String),
    /// A mount isn't what kiwi-nest made, so the app isn't started.
    #[error("{0}")]
    Mount(String),
    #[error("deploying `{name}` failed: {message}")]
    DeployFailed { name: String, message: String },
    #[error("starting `{name}` failed: {message}")]
    StartFailed { name: String, message: String },
    #[error("{0}")]
    Busy(String),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    #[error(transparent)]
    Volume(#[from] VolumeError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

type Result<T, E = AppsError> = std::result::Result<T, E>;

/// A bundle that has been extracted and checked, ready to deploy.
pub struct Staged {
    dir: tempfile::TempDir,
    digest: String,
    bundle: CheckedBundle,
}

impl Staged {
    pub fn bundle(&self) -> &CheckedBundle {
        &self.bundle
    }

    pub fn digest(&self) -> &str {
        &self.digest
    }
}

/// How to deploy.
#[derive(Debug, Clone, Default)]
pub struct DeployOptions {
    /// Refuse if the app exists.
    pub create_only: bool,
    /// On the first deploy, snapshot the named volumes this app shares with that one.
    pub from: Option<String>,
    /// On the first deploy, restore the app's volumes from this backup.
    pub restore: Option<Restore>,
    /// The generation to record, as the server numbers it; by default the previous one plus one.
    pub generation: Option<u64>,
}

/// An app's logs: all of it, or a running `docker compose logs --follow` to stream.
pub enum Logs {
    Complete(Vec<u8>),
    Follow(Child),
}

enum Op {
    Deploy {
        staged: Box<Staged>,
        opts: DeployOptions,
        reply: oneshot::Sender<Result<AppInfo>>,
    },
    Inspect {
        reply: oneshot::Sender<Result<AppInfo>>,
    },
    Logs {
        query: LogsQuery,
        reply: oneshot::Sender<Result<Logs>>,
    },
    Delete {
        keep_data: bool,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Snapshot this app's volumes named in `volumes` (those it has) into the directory `dest`, at one instant.
    SnapshotFor {
        dest: PathBuf,
        volumes: Vec<String>,
        reply: oneshot::Sender<Result<BTreeMap<String, u64>>>,
    },
    /// Snapshot all of this app's data (its volumes, and its snapshots of shared volumes as `@NAME`) read-only
    /// into the directory `dest`, at one instant, for a backup.
    BackupSnapshot {
        dest: PathBuf,
        reply: oneshot::Sender<Result<Vec<String>>>,
    },
    /// Start the app: for a request (`explicit` is false, and the gateway has set the gate to `Starting`), or for
    /// a resume, which also starts a suspended or failed app and skips the cooldown after a failed start.
    Start {
        explicit: bool,
        reply: Option<oneshot::Sender<Result<AppInfo>>>,
    },
    /// Stop the app for being idle (the reaper has set the gate to `Stopping`).
    IdleStop,
    Suspend {
        reply: oneshot::Sender<Result<AppInfo>>,
    },
    /// Find out what the app's containers are doing after the node agent started.
    Adopt,
    /// Answers once everything sent before it is done; see [`Apps::close`].
    Barrier {
        reply: oneshot::Sender<()>,
    },
}

/// The node's apps.
#[derive(Clone)]
pub struct Apps {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: NodeConfig,
    runtime: Arc<dyn Runtime>,
    volumes: Volumes,
    state: AsyncMutex<StateFile>,
    /// Each app's task. Senders are only used with this lock held, so a task can retire without losing an op.
    actors: Mutex<HashMap<String, mpsc::UnboundedSender<Op>>>,
    /// Each shared volume's lock: held shared by deploys and starts that mount it, exclusively by `volume` commands.
    shared_locks: Mutex<HashMap<String, Arc<RwLock<()>>>>,
    gates: Mutex<Gates>,
    /// Set by [`Apps::close`]: no operation is taken any more. Changed with `actors` locked.
    closed: std::sync::atomic::AtomicBool,
    /// Where backups go and restores come from.
    targets: std::sync::RwLock<Arc<dyn TargetSource>>,
    /// Held while an app or volume is backed up, so its backups (and their snapshots) never overlap.
    backup_locks: Mutex<HashMap<BackupKey, Arc<AsyncMutex<()>>>>,
}

/// An app or a shared volume, for its backup lock.
type BackupKey = (kn_proto::backup::Kind, String);

/// Every app's gate, by name and by domain.
#[derive(Default)]
struct Gates {
    by_name: HashMap<String, Arc<Gate>>,
    by_domain: HashMap<String, Arc<Gate>>,
}

/// The gate's phase for an app that isn't known to be running: what a request should do with it.
fn resting_phase(status: AppStatus) -> Phase {
    match status {
        AppStatus::Running | AppStatus::Idle => Phase::Stopped,
        AppStatus::Suspended => Phase::Suspended,
        AppStatus::Failed => Phase::Broken,
        AppStatus::Deploying => Phase::Busy,
    }
}

impl Inner {
    fn gates(&self) -> std::sync::MutexGuard<'_, Gates> {
        self.gates.lock().expect("gates lock")
    }

    fn gate(&self, name: &str) -> Option<Arc<Gate>> {
        self.gates().by_name.get(name).cloned()
    }

    /// Routes `domain` to the app's gate, creating it (busy) if needed.
    fn set_route(&self, name: &str, domain: &str, port: u16, idle_timeout_secs: u64) -> Arc<Gate> {
        let idle = Duration::from_secs(idle_timeout_secs);
        let mut gates = self.gates();
        let gate = gates
            .by_name
            .entry(name.to_string())
            .or_insert_with(|| Gate::new(name, port, idle, Phase::Busy))
            .clone();
        gate.configure(port, idle);
        gates.by_domain.retain(|_, g| g.name() != name);
        gates.by_domain.insert(domain.to_ascii_lowercase(), gate.clone());
        gate
    }

    fn remove_route(&self, name: &str) -> Option<Arc<Gate>> {
        let mut gates = self.gates();
        gates.by_domain.retain(|_, g| g.name() != name);
        gates.by_name.remove(name)
    }

    /// Sends an op to an app's task, starting the task if needed.
    fn send(self: &Arc<Self>, name: &str, op: Op) -> Result<()> {
        if !is_valid_app_name(name) {
            return Err(AppsError::InvalidName(name.to_string()));
        }
        let mut actors = self.actors.lock().expect("actors lock");
        if self.closed.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AppsError::Busy(
                "the server forgot this node, so it runs nothing any more".into(),
            ));
        }
        let mut op = Some(op);
        if let Some(tx) = actors.get(name) {
            match tx.send(op.take().expect("op")) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::SendError(back)) => op = Some(back),
            }
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(op.take().expect("op")).expect("a new channel is open");
        actors.insert(name.to_string(), tx);
        tokio::spawn(actor(self.clone(), name.to_string(), rx));
        Ok(())
    }

    fn shared_lock(&self, name: &str) -> Arc<RwLock<()>> {
        self.shared_locks
            .lock()
            .expect("shared locks")
            .entry(name.to_string())
            .or_default()
            .clone()
    }

    /// Takes the shared locks of some volumes, in name order so that two takers never deadlock.
    async fn lock_shared<'a>(&self, names: impl IntoIterator<Item = &'a String>) -> Vec<OwnedRwLockReadGuard<()>> {
        let mut names: Vec<&String> = names.into_iter().collect();
        names.sort();
        names.dedup();
        let mut guards = Vec::new();
        for n in names {
            guards.push(self.shared_lock(n).read_owned().await);
        }
        guards
    }

    async fn persist(&self, state: &StateFile) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state).map_err(|e| AppsError::Internal(e.into()))?;
        let path = self.cfg.state_dir.join("state.json");
        blocking(move || write_atomic(&path, &bytes))
            .await?
            .map_err(|e| AppsError::Internal(e.into()))
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn rfc3339(secs: u64) -> String {
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_secs(secs)).to_string()
}

/// Writes a file atomically: a temporary file beside it, synced, then renamed over it.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    std::fs::File::open(dir)?.sync_all()
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| AppsError::Internal(e.into()))
}

fn remove_dir_if_exists(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn shared_mode(m: kn_bundle::manifest::SharedMode) -> SharedMode {
    match m {
        kn_bundle::manifest::SharedMode::Snapshot => SharedMode::Snapshot,
        kn_bundle::manifest::SharedMode::Ro => SharedMode::Ro,
        kn_bundle::manifest::SharedMode::Rw => SharedMode::Rw,
    }
}

/// The error for a mount that isn't what kiwi-nest made.
fn refuse_start(app: &str, problems: &[String]) -> AppsError {
    AppsError::Mount(format!(
        "refusing to start `{app}`, because a mount isn't the subvolume kiwi-nest made: {}",
        problems.join("; ")
    ))
}

fn list(names: &[String]) -> String {
    names.iter().map(|n| format!("`{n}`")).collect::<Vec<_>>().join(", ")
}

impl Apps {
    /// Opens the node's state, creating its directories if needed, with btrfs volumes.
    ///
    /// Refuses a volumes directory that isn't on btrfs.
    pub async fn open(cfg: NodeConfig) -> anyhow::Result<Apps> {
        Self::open_with(cfg, Arc::new(Btrfs)).await
    }

    /// Opens the node's state with a given subvolume backend, and Docker.
    pub async fn open_with(cfg: NodeConfig, backend: Arc<dyn Backend>) -> anyhow::Result<Apps> {
        let runtime = Arc::new(Docker::new(&cfg.docker));
        Self::open_with_runtime(cfg, backend, runtime).await
    }

    /// Opens the node's state with a given subvolume backend and runtime.
    ///
    /// Recovers from a crash: a deploy that was cut off is recorded as failed (its containers are left as they are),
    /// a `volume load` cut off is finished or undone, staging leftovers are destroyed, shared volumes in place but
    /// not recorded are adopted, and apps left paused for a snapshot are unpaused. Then every app is adopted as its
    /// containers are found (see [`lifecycle`]), and the idle reaper starts.
    pub async fn open_with_runtime(
        cfg: NodeConfig,
        backend: Arc<dyn Backend>,
        runtime: Arc<dyn Runtime>,
    ) -> anyhow::Result<Apps> {
        cfg.validate()?;
        let targets: Arc<dyn TargetSource> = match &cfg.backup_targets {
            Some(path) => Arc::new(StaticTargets::new(kn_backup::target::load(path)?, "--backup-targets")),
            None => Arc::new(StaticTargets::new(Vec::new(), "--backup-targets")),
        };
        let d = cfg.state_dir.join("apps");
        std::fs::create_dir_all(&d).map_err(|e| anyhow::anyhow!("creating {}: {e}", d.display()))?;
        let staging = cfg.state_dir.join("staging");
        remove_dir_if_exists(&staging)?;
        std::fs::create_dir_all(&staging)?;
        let volumes = Volumes::open(cfg.volumes_dir.clone(), backend)?;

        let path = cfg.state_dir.join("state.json");
        let mut state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice::<StateFile>(&bytes)
                .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => StateFile {
                version: STATE_VERSION,
                ..Default::default()
            },
            Err(e) => anyhow::bail!("reading {}: {e}", path.display()),
        };
        if state.version > STATE_VERSION {
            anyhow::bail!(
                "{} was written by a newer kiwi-nest (version {})",
                path.display(),
                state.version
            );
        }
        let mut changed = state.version != STATE_VERSION;
        state.version = STATE_VERSION;
        for app in state.apps.values_mut() {
            if app.status == AppStatus::Deploying {
                app.status = AppStatus::Failed;
                app.error = Some("the deploy was cut off by a node restart".into());
                changed = true;
            }
        }
        changed |= recover_volumes(&mut state, &volumes)?;
        if changed {
            write_atomic(&path, &serde_json::to_vec_pretty(&state)?)?;
        }
        let paused: Vec<AppRecord> = state.apps.values().filter(|a| a.paused).cloned().collect();
        let records: Vec<AppRecord> = state.apps.values().cloned().collect();
        let idle_check = cfg.idle_check_interval;
        let apps = Apps {
            inner: Arc::new(Inner {
                cfg,
                runtime,
                volumes,
                state: AsyncMutex::new(state),
                actors: Mutex::new(HashMap::new()),
                shared_locks: Mutex::new(HashMap::new()),
                gates: Mutex::new(Gates::default()),
                closed: std::sync::atomic::AtomicBool::new(false),
                targets: std::sync::RwLock::new(targets),
                backup_locks: Mutex::new(HashMap::new()),
            }),
        };
        for r in paused {
            let app = App {
                inner: &apps.inner,
                name: &r.name,
            };
            let project = app.project(&r);
            match app.unpause(&project).await {
                Ok(()) => {
                    tracing::warn!(
                        app = r.name,
                        "unpaused the app, left paused for a snapshot when the node stopped"
                    );
                    app.set_paused(false).await?;
                }
                Err(e) => {
                    tracing::error!(app = r.name, error = %e, "couldn't unpause the app, left paused for a snapshot")
                }
            }
        }
        // Requests wait until each app has been adopted.
        for r in &records {
            apps.inner.set_route(&r.name, &r.domain, r.port, r.idle_timeout_secs);
            apps.inner.send(&r.name, Op::Adopt)?;
        }
        tokio::spawn(reaper(Arc::downgrade(&apps.inner), idle_check));
        Ok(apps)
    }

    pub fn config(&self) -> &NodeConfig {
        &self.inner.cfg
    }

    /// Stops for good, for a node the server forgot: from now on every operation is refused (deploys, starts for
    /// requests, idle stops, …), every route is removed, and this returns once each app's task has finished what it
    /// was doing and what was queued before. Its containers are left as they are, for the caller to take down.
    pub async fn close(&self) {
        let tasks: Vec<mpsc::UnboundedSender<Op>> = {
            let actors = self.inner.actors.lock().expect("actors lock");
            self.inner.closed.store(true, std::sync::atomic::Ordering::SeqCst);
            actors.values().cloned().collect()
        };
        for tx in tasks {
            let (reply, done) = oneshot::channel();
            if tx.send(Op::Barrier { reply }).is_ok() {
                let _ = done.await;
            }
        }
        let gates: Vec<Arc<Gate>> = {
            let mut g = self.inner.gates();
            g.by_domain.clear();
            g.by_name.drain().map(|(_, gate)| gate).collect()
        };
        for g in gates {
            g.break_leases();
            g.set_phase(Phase::Gone);
        }
    }

    pub fn runtime(&self) -> &dyn Runtime {
        &*self.inner.runtime
    }

    /// The gate of the app serving `host` (a domain in lowercase, without a port).
    pub fn route(&self, host: &str) -> Option<Arc<Gate>> {
        self.inner.gates().by_domain.get(host).cloned()
    }

    /// Starts the app behind `gate` for a request, unless it's already starting or can't be started now; returns
    /// whether it did. The start runs in the app's task, so it carries on even if every request waiting for it
    /// goes away.
    pub fn wake(&self, gate: &Gate) -> bool {
        let woken = gate.begin_start(Instant::now());
        if woken {
            tracing::debug!(app = gate.name(), "waking for a request");
            if let Err(e) = self.inner.send(
                gate.name(),
                Op::Start {
                    explicit: false,
                    reply: None,
                },
            ) {
                tracing::warn!(app = gate.name(), error = %e, "couldn't start the app");
                gate.set_phase(Phase::Stopped);
            }
        }
        woken
    }

    pub fn volumes(&self) -> &Volumes {
        &self.inner.volumes
    }

    /// A new file in the staging directory, for an upload.
    pub fn upload_file(&self) -> Result<tempfile::NamedTempFile> {
        tempfile::Builder::new()
            .prefix("upload-")
            .tempfile_in(self.inner.cfg.state_dir.join("staging"))
            .map_err(|e| AppsError::Internal(e.into()))
    }

    /// Extracts and checks an uploaded ZIP.
    pub async fn stage_zip(&self, zip: PathBuf) -> Result<Staged> {
        let limits = self.inner.cfg.limits.clone();
        self.stage(move |dest| {
            let f = std::fs::File::open(&zip).map_err(|e| BundleError::Io("the upload".into(), e))?;
            kn_bundle::zip::extract(std::io::BufReader::new(f), dest, &limits)
        })
        .await
    }

    /// Copies and checks a bundle directory on this machine.
    pub async fn stage_dir(&self, src: PathBuf) -> Result<Staged> {
        let limits = self.inner.cfg.limits.clone();
        self.stage(move |dest| kn_bundle::dir::copy(&src, dest, &limits)).await
    }

    async fn stage(
        &self,
        fill: impl FnOnce(&Path) -> Result<kn_bundle::FileIndex, BundleError> + Send + 'static,
    ) -> Result<Staged> {
        let staging = self.inner.cfg.state_dir.join("staging");
        let options = CheckOptions {
            allowed_host_paths: self.inner.cfg.allowed_host_paths.clone(),
        };
        let staged = blocking(move || -> Result<Staged> {
            let dir = tempfile::Builder::new()
                .prefix("bundle-")
                .tempdir_in(&staging)
                .map_err(|e| AppsError::Internal(e.into()))?;
            let index = fill(dir.path())?;
            let bundle = kn_bundle::load(dir.path(), &index, &options).map_err(AppsError::Refused)?;
            Ok(Staged {
                digest: index.digest(),
                dir,
                bundle,
            })
        })
        .await??;
        let m = &staged.bundle.manifest;
        if let Some(arch) = m.arch
            && Some(arch) != Arch::current()
        {
            return Err(AppsError::Unsupported(format!(
                "the bundle needs arch `{}`, and this node is {}",
                arch.as_str(),
                std::env::consts::ARCH
            )));
        }
        Ok(staged)
    }

    fn send(&self, name: &str, op: Op) -> Result<()> {
        self.inner.send(name, op)
    }

    async fn ask<T>(&self, name: &str, op: impl FnOnce(oneshot::Sender<Result<T>>) -> Op) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.send(name, op(tx))?;
        rx.await
            .map_err(|_| AppsError::Internal(anyhow::anyhow!("app `{name}`'s task stopped")))?
    }

    /// Deploys a staged bundle as `name`, replacing the app if it exists (unless `create_only`).
    pub async fn deploy(&self, name: &str, staged: Staged, opts: DeployOptions) -> Result<AppInfo> {
        let staged = Box::new(staged);
        self.ask(name, |reply| Op::Deploy { staged, opts, reply }).await
    }

    pub async fn inspect(&self, name: &str) -> Result<AppInfo> {
        self.ask(name, |reply| Op::Inspect { reply }).await
    }

    pub async fn logs(&self, name: &str, query: LogsQuery) -> Result<Logs> {
        self.ask(name, |reply| Op::Logs { query, reply }).await
    }

    /// Removes the app's containers and its bundle, and its subvolumes unless `keep_data`, cutting off its open
    /// requests.
    pub async fn delete(&self, name: &str, keep_data: bool) -> Result<()> {
        self.ask(name, |reply| Op::Delete { keep_data, reply }).await
    }

    /// Stops the app and keeps it stopped until a resume. Refused while it has open requests.
    pub async fn suspend(&self, name: &str) -> Result<AppInfo> {
        self.ask(name, |reply| Op::Suspend { reply }).await
    }

    /// Starts the app, whatever stopped it, and returns once it's ready. Skips the cooldown after a failed start.
    pub async fn resume(&self, name: &str) -> Result<AppInfo> {
        self.ask(name, |reply| Op::Start {
            explicit: true,
            reply: Some(reply),
        })
        .await
    }

    /// Every app, with what Docker reports for its containers.
    pub async fn list(&self) -> Result<Vec<AppSummary>> {
        let records: Vec<AppRecord> = self.inner.state.lock().await.apps.values().cloned().collect();
        let live: HashMap<String, String> = self
            .inner
            .runtime
            .projects(&self.inner.cfg.project_prefix)
            .await?
            .into_iter()
            .map(|p| (p.name, p.status))
            .collect();
        Ok(records
            .into_iter()
            .map(|r| AppSummary {
                containers: live.get(&r.project).cloned().unwrap_or_else(|| "absent".into()),
                name: r.name,
                status: r.status,
                generation: r.generation,
                domain: r.domain,
                port: r.port,
                updated_at: rfc3339(r.updated_at),
                node: None,
                nest: None,
                pending: None,
            })
            .collect())
    }

    /// Every app as the node records it, for the server, with what Docker reports for its containers (or
    /// `unknown`, if Docker can't be asked within a few seconds, so a stuck Docker never holds up a report).
    pub async fn reports(&self) -> Vec<kn_proto::agent::AppReport> {
        let records: Vec<AppRecord> = self.inner.state.lock().await.apps.values().cloned().collect();
        let projects = tokio::time::timeout(
            Duration::from_secs(5),
            self.inner.runtime.projects(&self.inner.cfg.project_prefix),
        )
        .await;
        let live: Option<HashMap<String, String>> = match projects {
            Ok(Ok(ps)) => Some(ps.into_iter().map(|p| (p.name, p.status)).collect()),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "couldn't list the apps' containers");
                None
            }
            Err(_) => {
                tracing::warn!("listing the apps' containers timed out");
                None
            }
        };
        records
            .into_iter()
            .map(|r| kn_proto::agent::AppReport {
                containers: match &live {
                    Some(l) => l.get(&r.project).cloned().unwrap_or_else(|| "absent".into()),
                    None => "unknown".into(),
                },
                name: r.name,
                status: r.status,
                generation: r.generation,
                error: r.error,
                domain: r.domain,
                port: r.port,
                bundle_digest: r.bundle_digest,
            })
            .collect()
    }
}

/// Reconciles the recorded volumes with the disk when the node starts; returns whether the state changed.
fn recover_volumes(state: &mut StateFile, volumes: &Volumes) -> anyhow::Result<bool> {
    let mut changed = false;
    // A `volume load` cut off: if the new contents were swapped in, keep them; otherwise the old ones stay.
    for s in state.shared.values_mut() {
        if let Some(new) = s.loading.take() {
            if volumes.check(&volumes.shared_data(&s.name), Some(new)).is_ok() {
                tracing::warn!(volume = s.name, "finished a `volume load` cut off by a node restart");
                s.id = new;
                s.loaded_at = Some(now());
            } else {
                tracing::warn!(volume = s.name, "undid a `volume load` cut off by a node restart");
            }
            changed = true;
        }
    }
    for p in volumes.sweep()? {
        tracing::warn!(path = %p.display(), "removed a half-made volume left by a node restart");
    }
    // A shared volume moved into place just before a crash, before it was recorded.
    for name in Volumes::names(&volumes.root().join("shared"))? {
        if state.shared.contains_key(&name) {
            continue;
        }
        if !kn_bundle::compose::is_valid_object_name(&name) {
            tracing::warn!(
                name,
                "ignoring a directory in the shared volumes that isn't a volume name"
            );
            continue;
        }
        match volumes.check(&volumes.shared_data(&name), None) {
            Ok(id) => {
                tracing::warn!(volume = name, "adopted a shared volume that wasn't recorded");
                state.shared.insert(
                    name.clone(),
                    SharedRecord {
                        name,
                        id,
                        loading: None,
                        created_at: now(),
                        loaded_at: None,
                    },
                );
                changed = true;
            }
            Err(e) => tracing::warn!(volume = name, error = %e, "ignoring a shared volume directory"),
        }
    }
    Ok(changed)
}

async fn actor(inner: Arc<Inner>, name: String, mut rx: mpsc::UnboundedReceiver<Op>) {
    loop {
        let op = match rx.try_recv() {
            Ok(op) => op,
            Err(mpsc::error::TryRecvError::Disconnected) => return,
            Err(mpsc::error::TryRecvError::Empty) => {
                if inner.state.lock().await.apps.contains_key(&name) {
                    match rx.recv().await {
                        Some(op) => op,
                        None => return,
                    }
                } else {
                    // The app doesn't exist (any more): retire, unless an op arrived meanwhile.
                    let mut actors = inner.actors.lock().expect("actors lock");
                    match rx.try_recv() {
                        Ok(op) => op,
                        Err(_) => {
                            actors.remove(&name);
                            return;
                        }
                    }
                }
            }
        };
        let app = App {
            inner: &inner,
            name: &name,
        };
        match op {
            Op::Deploy { staged, opts, reply } => {
                let _ = reply.send(app.deploy(*staged, opts).await);
            }
            Op::Inspect { reply } => {
                let _ = reply.send(app.inspect().await);
            }
            Op::Logs { query, reply } => {
                let _ = reply.send(app.logs(query).await);
            }
            Op::Delete { keep_data, reply } => {
                let _ = reply.send(app.delete(keep_data).await);
            }
            Op::SnapshotFor { dest, volumes, reply } => {
                let _ = reply.send(app.snapshot_for(dest, volumes).await);
            }
            Op::BackupSnapshot { dest, reply } => {
                let _ = reply.send(app.backup_snapshot(dest).await);
            }
            Op::Start { explicit, reply } => {
                let result = app.start(explicit).await;
                if let Some(reply) = reply {
                    let _ = reply.send(result);
                }
            }
            Op::IdleStop => app.idle_stop().await,
            Op::Suspend { reply } => {
                let _ = reply.send(app.suspend().await);
            }
            Op::Adopt => app.adopt().await,
            Op::Barrier { reply } => {
                let _ = reply.send(());
            }
        }
    }
}

/// Stops apps that have been idle for their timeout, checking every `every`.
async fn reaper(inner: Weak<Inner>, every: Duration) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(inner) = inner.upgrade() else { return };
        let now = Instant::now();
        let due: Vec<String> = inner
            .gates()
            .by_name
            .values()
            .filter(|g| g.begin_idle_stop(now))
            .map(|g| g.name().to_string())
            .collect();
        for name in due {
            if let Err(e) = inner.send(&name, Op::IdleStop) {
                tracing::warn!(app = name, error = %e, "couldn't stop an idle app");
            }
        }
    }
}

/// How a deploy failed: before the new generation was started (the previous one may still be running), or after.
enum Failure {
    NotStarted(AppsError),
    Started(AppsError),
}

/// One app's operations, run by its task.
struct App<'a> {
    inner: &'a Arc<Inner>,
    name: &'a str,
}

impl App<'_> {
    fn app_dir(&self) -> PathBuf {
        self.inner.cfg.state_dir.join("apps").join(self.name)
    }

    fn project(&self, r: &AppRecord) -> Project {
        let dir = self.app_dir();
        Project {
            name: r.project.clone(),
            file: dir.join(format!("compose-{}.yaml", r.generation)),
            dir,
        }
    }

    async fn record(&self) -> Result<AppRecord> {
        self.inner
            .state
            .lock()
            .await
            .apps
            .get(self.name)
            .cloned()
            .ok_or_else(|| AppsError::NotFound(self.name.into()))
    }

    /// Updates the app's record and writes the state file.
    async fn update(&self, f: impl FnOnce(&mut AppRecord)) -> Result<AppRecord> {
        let mut state = self.inner.state.lock().await;
        let r = state
            .apps
            .get_mut(self.name)
            .ok_or_else(|| AppsError::NotFound(self.name.into()))?;
        f(r);
        r.updated_at = now();
        let r = r.clone();
        self.inner.persist(&state).await?;
        Ok(r)
    }

    async fn set_paused(&self, paused: bool) -> Result<()> {
        let mut state = self.inner.state.lock().await;
        if let Some(r) = state.apps.get_mut(self.name) {
            r.paused = paused;
        }
        self.inner.persist(&state).await
    }

    async fn deploy(&self, staged: Staged, opts: DeployOptions) -> Result<AppInfo> {
        let cfg = &self.inner.cfg;
        let m = &staged.bundle.manifest;
        if opts.from.is_some() && opts.restore.is_some() {
            return Err(AppsError::Unsupported(
                "an app's volumes start either `--from` another app or `--restore`d from a backup, not both".into(),
            ));
        }
        if let Some(from) = &opts.from {
            if !is_valid_app_name(from) {
                return Err(AppsError::InvalidName(from.clone()));
            }
            if from == self.name {
                return Err(AppsError::Unsupported(
                    "an app can't be deployed `--from` itself".into(),
                ));
            }
        }
        // The first deploy is the one that makes the app's volume directory. An app with recorded volumes has had
        // its first deploy, even if the directory has gone (removed, or the filesystem not mounted): then its volumes
        // are refused as missing, never made again empty or from `--from`. Only this app's task changes its record,
        // so it can be read before the reservation.
        let recorded = self
            .inner
            .state
            .lock()
            .await
            .apps
            .get(self.name)
            .is_some_and(|p| !p.volume_ids.is_empty());
        let dir = self.inner.volumes.app_dir(self.name);
        let first = !recorded && !blocking(move || Volumes::exists(&dir)).await??;
        // Reserve the generation and port, and record the deploy before touching anything.
        let (record, previous) = {
            let mut state = self.inner.state.lock().await;
            let previous = state.apps.get(self.name).cloned();
            if opts.create_only && previous.is_some() {
                return Err(AppsError::Exists(self.name.into()));
            }
            if let Some(other) = state
                .apps
                .values()
                .find(|a| a.domain == m.domain && a.name != self.name)
            {
                return Err(AppsError::DomainTaken {
                    domain: m.domain.clone(),
                    app: other.name.clone(),
                });
            }
            let missing: Vec<String> = m
                .shared
                .keys()
                .filter(|n| !state.shared.contains_key(*n))
                .cloned()
                .collect();
            if !missing.is_empty() {
                return Err(AppsError::NotFoundOther(format!(
                    "the bundle mounts shared volume {}, which doesn't exist on this node; create it with \
                     `kiwi-nest volume create`",
                    list(&missing)
                )));
            }
            let from = match (&opts.from, first) {
                (Some(from), true) => {
                    let Some(source) = state.apps.get(from) else {
                        return Err(AppsError::NotFoundOther(format!(
                            "app `{from}` (given as --from) doesn't exist"
                        )));
                    };
                    // A first deploy may itself be waiting on another app's snapshots (its `--from`), so waiting on
                    // it could deadlock: two apps' first deploys `--from` each other. A redeploy never waits.
                    if source.status == AppStatus::Deploying && !Volumes::exists(&self.inner.volumes.app_dir(from))? {
                        return Err(AppsError::Conflict(format!(
                            "app `{from}` (given as --from) is being deployed for the first time; try again once \
                             it's done"
                        )));
                    }
                    Some(from.clone())
                }
                (None, true) => None,
                (_, false) => previous.as_ref().and_then(|p| p.from.clone()),
            };
            let restore = match (&opts.restore, first) {
                (Some(r), true) => Some(r.clone()),
                (None, true) => None,
                (_, false) => previous.as_ref().and_then(|p| p.restore.clone()),
            };
            let port = match &previous {
                Some(p) => p.port,
                None => allocate_port(&state, cfg, self.name)?,
            };
            let volumes: Vec<String> = staged.bundle.volumes.clone();
            let volume_ids = previous
                .as_ref()
                .map(|p| {
                    p.volume_ids
                        .iter()
                        .filter(|(k, _)| volumes.contains(k))
                        .map(|(k, v)| (k.clone(), *v))
                        .collect()
                })
                .unwrap_or_default();
            let shared = m
                .shared
                .iter()
                .map(|(name, s)| {
                    let mode = shared_mode(s.mode);
                    let snapshot_id = previous
                        .as_ref()
                        .and_then(|p| p.shared.get(name))
                        .filter(|p| p.mode == SharedMode::Snapshot && mode == SharedMode::Snapshot)
                        .and_then(|p| p.snapshot_id);
                    (
                        name.clone(),
                        SharedMountRecord {
                            target: s.target.clone(),
                            mode,
                            snapshot_id,
                        },
                    )
                })
                .collect();
            let t = now();
            let record = AppRecord {
                name: self.name.to_string(),
                status: AppStatus::Deploying,
                generation: opts
                    .generation
                    .unwrap_or_else(|| previous.as_ref().map_or(1, |p| p.generation + 1)),
                error: None,
                project: format!("{}{}", cfg.project_prefix, self.name),
                port,
                domain: m.domain.clone(),
                service: m.service.clone(),
                container_port: m.port,
                idle_timeout_secs: m.idle_timeout_secs,
                bundle_digest: staged.digest.clone(),
                services: staged.bundle.services.iter().map(|s| s.name.clone()).collect(),
                volumes,
                volume_ids,
                shared,
                from,
                restore,
                paused: false,
                created_at: previous.as_ref().map_or(t, |p| p.created_at),
                updated_at: t,
            };
            state.apps.insert(self.name.to_string(), record.clone());
            self.inner.persist(&state).await?;
            (record, previous)
        };
        // Requests wait for the deploy.
        let gate = self
            .inner
            .set_route(self.name, &record.domain, record.port, record.idle_timeout_secs);
        gate.set_phase(Phase::Busy);
        tracing::info!(app = self.name, generation = record.generation, digest = %record.bundle_digest, first, "deploying");

        let generation = record.generation;
        let mut notes = Vec::new();
        if opts.from.is_some() && !first {
            notes.push("--from only applies to an app's first deploy, so its volumes were kept".to_string());
        }
        if opts.restore.is_some() && !first {
            notes.push("--restore only applies to an app's first deploy, so its volumes were kept".to_string());
        }
        let failed = match self.apply(&record, previous.as_ref(), staged, first, &mut notes).await {
            Ok(()) => {
                self.cleanup_generations(generation).await;
                let record = self.update(|r| r.status = AppStatus::Running).await;
                gate.set_phase(Phase::Ready);
                tracing::info!(app = self.name, generation, "deployed");
                let mut info = self.info(record?).await?;
                info.notes = notes;
                return Ok(info);
            }
            // The previous generation was stopped, so the new one is the app now, even though it failed.
            Err(Failure::Started(e)) | Err(Failure::NotStarted(e)) if previous.is_none() => e,
            Err(Failure::Started(e)) => {
                self.cleanup_generations(generation).await;
                e
            }
            // The previous generation may still be running from its own bundle and file: keep them, drop the new
            // generation's, and put the previous record back.
            Err(Failure::NotStarted(e)) => {
                self.remove_generation(generation).await;
                let mut prev = previous.expect("a previous generation");
                let message = e.to_string();
                tracing::warn!(app = self.name, generation, %message, "redeploy failed; the previous generation stays");
                prev.error = Some(format!(
                    "redeploying failed, so generation {} is still in place: {message}",
                    prev.generation
                ));
                prev.updated_at = now();
                // Its containers may still be running: the next request checks, and starts them if needed.
                self.inner
                    .set_route(self.name, &prev.domain, prev.port, prev.idle_timeout_secs);
                gate.set_phase(resting_phase(prev.status));
                let mut state = self.inner.state.lock().await;
                state.apps.insert(self.name.to_string(), prev);
                self.inner.persist(&state).await?;
                return Err(AppsError::DeployFailed {
                    name: self.name.into(),
                    message,
                });
            }
        };
        let message = failed.to_string();
        gate.set_phase(Phase::Broken);
        self.update(|r| {
            r.status = AppStatus::Failed;
            r.error = Some(message.clone());
        })
        .await?;
        tracing::warn!(app = self.name, generation, %message, "deploy failed");
        Err(AppsError::DeployFailed {
            name: self.name.into(),
            message,
        })
    }

    /// Puts the new generation's files and volumes in place, checks the mounts, stops the old project and starts the
    /// new one.
    async fn apply(
        &self,
        record: &AppRecord,
        previous: Option<&AppRecord>,
        staged: Staged,
        first: bool,
        notes: &mut Vec<String>,
    ) -> std::result::Result<(), Failure> {
        let cfg = &self.inner.cfg;
        let vols = &self.inner.volumes;
        let app_dir = self.app_dir();
        let bundle_dir = app_dir.join(format!("bundle-{}", record.generation));
        let volume_dirs: BTreeMap<String, PathBuf> = record
            .volumes
            .iter()
            .map(|v| (v.clone(), vols.app_volume(self.name, v)))
            .collect();
        let shared_dirs: BTreeMap<String, PathBuf> = record
            .shared
            .iter()
            .map(|(name, m)| {
                let dir = match m.mode {
                    SharedMode::Snapshot => vols.snapshot_path(name, self.name),
                    SharedMode::Ro | SharedMode::Rw => vols.shared_data(name),
                };
                (name.clone(), dir)
            })
            .collect();
        let labels: BTreeMap<String, String> = [
            ("kiwi-nest.app".to_string(), self.name.to_string()),
            ("kiwi-nest.generation".to_string(), record.generation.to_string()),
        ]
        .into_iter()
        .collect();
        let generated = kn_bundle::generate::generate(
            &staged.bundle,
            &kn_bundle::generate::Params {
                project: &record.project,
                bundle_dir: &bundle_dir,
                volume_dirs: &volume_dirs,
                shared_dirs: &shared_dirs,
                host_port: record.port,
                labels: &labels,
            },
        )
        .map_err(|e| Failure::NotStarted(AppsError::Internal(e.into())))?;
        let project = self.project(record);
        let file = project.file.clone();
        blocking(move || -> std::io::Result<()> {
            std::fs::create_dir_all(&app_dir)?;
            remove_dir_if_exists(&bundle_dir)?;
            std::fs::rename(staged.dir.keep(), &bundle_dir)?;
            write_atomic(&file, generated.as_bytes())
        })
        .await
        .map_err(Failure::NotStarted)?
        .map_err(|e| Failure::NotStarted(AppsError::Internal(anyhow::anyhow!("putting the bundle in place: {e}"))))?;

        // The volumes, while any previous generation keeps running. `--from` waits on the source app's task, so no
        // shared volume's lock is held yet.
        let restored = self.prepare_volumes(first, notes).await.map_err(Failure::NotStarted)?;
        let _locks = self.inner.lock_shared(record.shared.keys()).await;
        self.prepare_snapshots(&restored, notes)
            .await
            .map_err(Failure::NotStarted)?;
        self.verify_mounts().await.map_err(Failure::NotStarted)?;

        if let Some(prev) = previous {
            tracing::info!(
                app = self.name,
                generation = prev.generation,
                "stopping the previous generation"
            );
            self.inner
                .runtime
                .down(&self.project(prev))
                .await
                .map_err(|e| Failure::NotStarted(e.into()))?;
        }
        // Check again right before starting.
        self.verify_mounts().await.map_err(Failure::Started)?;
        self.inner
            .runtime
            .up(&project, cfg.up_timeout)
            .await
            .map_err(|e| Failure::Started(e.into()))
    }

    /// Makes sure each of the app's named volumes is in place, and records their IDs.
    ///
    /// On the first deploy the whole set is made in a staging directory, as snapshots of the `--from` app's volumes,
    /// restored from a backup, or empty, and moved into place together, so a failure part way leaves nothing. Later
    /// deploys check the recorded volumes, adopt any in place but not recorded, and create the new ones.
    ///
    /// Returns the shared volumes whose snapshots a restore staged, for [`App::prepare_snapshots`] to move into place.
    async fn prepare_volumes(&self, first: bool, notes: &mut Vec<String>) -> Result<BTreeSet<String>> {
        let r = self.record().await?;
        let vols = self.inner.volumes.clone();
        let app = self.name.to_string();
        let mut restored = BTreeSet::new();
        let ids = if first {
            let dest = vols.app_dir(&app);
            let staging = {
                let (v, d) = (vols.clone(), dest.clone());
                blocking(move || v.stage_dir(&d)).await??
            };
            let made = self.make_first_volumes(&r, &staging, &mut restored, notes).await;
            if made.is_err() {
                self.unstage_restored_snapshots(&restored).await;
                let v = vols.clone();
                match blocking(move || v.remove(&staging)).await {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => tracing::warn!(app, error = %e, "couldn't remove half-made volumes"),
                    Err(e) => tracing::warn!(app, error = %e, "couldn't remove half-made volumes"),
                }
            }
            made?
        } else {
            let dir = vols.app_dir(&app);
            if !blocking(move || Volumes::exists(&dir)).await?? {
                return Err(refuse_start(
                    &app,
                    &[format!(
                        "its volumes directory `{}` is missing, though it had volumes; put it back, or delete the app \
                         to start again without its data",
                        vols.app_dir(&app).display()
                    )],
                ));
            }
            let volumes = r.volumes.clone();
            let recorded = r.volume_ids.clone();
            let (ids, created, kept) = blocking(move || -> Result<_> {
                let (mut ids, mut created, mut kept) = (BTreeMap::new(), Vec::new(), Vec::new());
                for vol in volumes {
                    let path = vols.app_volume(&app, &vol);
                    let refuse = |e| refuse_start(&app, &[format!("volume `{vol}`: {e}")]);
                    let id = match recorded.get(&vol) {
                        Some(id) => vols.check(&path, Some(*id)).map_err(refuse)?,
                        None if Volumes::exists(&path)? => {
                            kept.push(vol.clone());
                            vols.check(&path, None).map_err(refuse)?
                        }
                        None => {
                            created.push(vol.clone());
                            vols.create_in_place(&path)?
                        }
                    };
                    ids.insert(vol, id);
                }
                Ok((ids, created, kept))
            })
            .await??;
            if !created.is_empty() {
                notes.push(format!("created volume {}", list(&created)));
            }
            if !kept.is_empty() {
                notes.push(format!("kept volume {} from before", list(&kept)));
            }
            ids
        };
        self.update(|r| r.volume_ids = ids).await?;
        Ok(restored)
    }

    /// Makes the first deploy's volumes in `staging`, then moves them into place. A restore also stages the app's
    /// snapshots of shared volumes it held, and adds their names to `restored`.
    async fn make_first_volumes(
        &self,
        r: &AppRecord,
        staging: &Path,
        restored: &mut BTreeSet<String>,
        notes: &mut Vec<String>,
    ) -> Result<BTreeMap<String, u64>> {
        let mut ids = BTreeMap::new();
        if let Some(restore) = &r.restore {
            ids.extend(self.restore_volumes(r, restore, staging, restored, notes).await?);
        }
        if let Some(from) = &r.from {
            let source = Apps {
                inner: self.inner.clone(),
            };
            let snapped = source
                .ask(from, |reply| Op::SnapshotFor {
                    dest: staging.to_path_buf(),
                    volumes: r.volumes.clone(),
                    reply,
                })
                .await?;
            if snapped.is_empty() {
                notes.push(format!("`{from}` has none of this app's volumes, so they start empty"));
            } else {
                let names: Vec<String> = snapped.keys().cloned().collect();
                notes.push(format!("snapshotted volume {} from `{from}`", list(&names)));
            }
            ids.extend(snapped);
        }
        let todo: Vec<String> = r.volumes.iter().filter(|v| !ids.contains_key(*v)).cloned().collect();
        let (vols, staging, dest) = (
            self.inner.volumes.clone(),
            staging.to_path_buf(),
            self.inner.volumes.app_dir(self.name),
        );
        let created = blocking(move || -> Result<Vec<(String, u64)>, VolumeError> {
            let made = todo
                .into_iter()
                .map(|v| vols.create(&staging.join(&v)).map(|id| (v, id)))
                .collect::<Result<Vec<_>, _>>()?;
            vols.commit(&dest)?;
            Ok(made)
        })
        .await??;
        ids.extend(created);
        Ok(ids)
    }

    /// Makes the app's snapshots of the shared volumes it mounts in `snapshot` mode, the first time it mounts each
    /// (or moves into place the ones a restore staged, in `restored`), and records their IDs. The caller holds the
    /// volumes' locks.
    async fn prepare_snapshots(&self, restored: &BTreeSet<String>, notes: &mut Vec<String>) -> Result<()> {
        let r = self.record().await?;
        let wanted: Vec<(String, Option<u64>)> = r
            .shared
            .iter()
            .filter(|(_, m)| m.mode == SharedMode::Snapshot)
            .map(|(n, m)| (n.clone(), m.snapshot_id))
            .collect();
        if wanted.is_empty() {
            return Ok(());
        }
        let sources = self.shared_ids(&r).await?;
        let (vols, app, restored) = (self.inner.volumes.clone(), self.name.to_string(), restored.clone());
        let (ids, made, kept) = blocking(move || -> Result<_> {
            let (mut ids, mut made, mut kept) = (BTreeMap::new(), Vec::new(), Vec::new());
            for (name, recorded) in wanted {
                let path = vols.snapshot_path(&name, &app);
                let refuse = |e| refuse_start(&app, &[format!("the snapshot of shared volume `{name}`: {e}")]);
                let id = match recorded {
                    Some(id) => vols.check(&path, Some(id)).map_err(refuse)?,
                    None if Volumes::exists(&path)? => {
                        kept.push(name.clone());
                        vols.check(&path, None).map_err(refuse)?
                    }
                    None if restored.contains(&name) => {
                        let staged = Volumes::staging_for(&path);
                        let id = vols.check(&staged, None).map_err(refuse)?;
                        vols.commit(&path)?;
                        id
                    }
                    None => {
                        let source = sources[&name]
                            .ok_or_else(|| AppsError::NotFoundOther(format!("shared volume `{name}` doesn't exist")))?;
                        made.push(name.clone());
                        vols.snapshot_in_place(&vols.shared_data(&name), source, &path)?
                    }
                };
                ids.insert(name, id);
            }
            Ok((ids, made, kept))
        })
        .await??;
        if !made.is_empty() {
            notes.push(format!("snapshotted shared volume {}", list(&made)));
        }
        if !kept.is_empty() {
            notes.push(format!(
                "kept the snapshot of shared volume {} from before",
                list(&kept)
            ));
        }
        self.update(|r| {
            for (name, id) in ids {
                if let Some(m) = r.shared.get_mut(&name) {
                    m.snapshot_id = Some(id);
                }
            }
        })
        .await?;
        Ok(())
    }

    /// The recorded data subvolume of each shared volume the app mounts, if it exists.
    async fn shared_ids(&self, r: &AppRecord) -> Result<BTreeMap<String, Option<u64>>> {
        let state = self.inner.state.lock().await;
        Ok(r.shared
            .keys()
            .map(|n| (n.clone(), state.shared.get(n).map(|s| s.id)))
            .collect())
    }

    /// The check before every start: every mount is the subvolume recorded for it, reached without symlinks.
    async fn verify_mounts(&self) -> Result<()> {
        let r = self.record().await?;
        let shared_ids = self.shared_ids(&r).await?;
        let (vols, app) = (self.inner.volumes.clone(), self.name.to_string());
        let problems = blocking(move || {
            let mut problems = Vec::new();
            for vol in &r.volumes {
                let path = vols.app_volume(&app, vol);
                match r.volume_ids.get(vol) {
                    None => problems.push(format!("volume `{vol}` has no recorded subvolume")),
                    Some(id) => {
                        if let Err(e) = vols.check(&path, Some(*id)) {
                            problems.push(format!("volume `{vol}`: {e}"));
                        }
                    }
                }
            }
            for (name, m) in &r.shared {
                let (path, id) = match m.mode {
                    SharedMode::Snapshot => (vols.snapshot_path(name, &app), m.snapshot_id),
                    SharedMode::Ro | SharedMode::Rw => (vols.shared_data(name), shared_ids[name]),
                };
                match id {
                    None => problems.push(format!("shared volume `{name}` has no recorded subvolume")),
                    Some(id) => {
                        if let Err(e) = vols.check(&path, Some(id)) {
                            problems.push(format!("shared volume `{name}`: {e}"));
                        }
                    }
                }
            }
            problems
        })
        .await?;
        if problems.is_empty() {
            return Ok(());
        }
        tracing::error!(
            app = self.name,
            ?problems,
            "refusing to start: a mount isn't what kiwi-nest made"
        );
        Err(refuse_start(self.name, &problems))
    }

    /// Snapshots this app's volumes named in `wanted` into `dest`, with its containers paused around them so they're
    /// all taken at one instant. The containers are always unpaused afterwards; if that fails, the app's record says
    /// so, and a restarted node tries again.
    async fn snapshot_for(&self, dest: PathBuf, wanted: Vec<String>) -> Result<BTreeMap<String, u64>> {
        let r = self
            .record()
            .await
            .map_err(|_| AppsError::NotFoundOther(format!("app `{}` (given as --from) doesn't exist", self.name)))?;
        let common = r
            .volumes
            .iter()
            .filter(|v| wanted.contains(v))
            .map(|v| match r.volume_ids.get(v) {
                Some(id) => Ok((v.clone(), self.inner.volumes.app_volume(self.name, v), *id)),
                None => Err(AppsError::Mount(format!(
                    "`{}`'s volume `{v}` has no recorded subvolume; redeploy `{}` first",
                    self.name, self.name
                ))),
            })
            .collect::<Result<Vec<_>>>()?;
        if common.is_empty() {
            return Ok(BTreeMap::new());
        }
        self.snapshot_paused(&r, common, dest, false).await
    }

    /// Snapshots each of `items` (a name, the subvolume and its recorded ID) as `dest/NAME`, with the app's
    /// containers paused around them so they're all taken at one instant. The sources are checked before anything
    /// is paused. The containers are always unpaused afterwards; if that fails, the app's record says so, and a
    /// restarted node tries again.
    async fn snapshot_paused(
        &self,
        r: &AppRecord,
        items: Vec<(String, PathBuf, u64)>,
        dest: PathBuf,
        read_only: bool,
    ) -> Result<BTreeMap<String, u64>> {
        let (vols, app, c) = (self.inner.volumes.clone(), self.name.to_string(), items.clone());
        blocking(move || -> Result<()> {
            for (name, path, id) in &c {
                vols.check(path, Some(*id))
                    .map_err(|e| AppsError::Mount(format!("`{app}`'s volume `{name}`: {e}")))?;
            }
            Ok(())
        })
        .await??;

        let project = self.project(r);
        let running = self
            .inner
            .runtime
            .ps(&project)
            .await?
            .iter()
            .any(|c| c.state == "running");
        if running {
            self.set_paused(true).await?;
            if let Err(e) = self.inner.runtime.pause(&project).await {
                // Some of its containers may be paused even so.
                let unpaused = self.unpause(&project).await;
                return Err(match unpaused {
                    Ok(()) => {
                        self.set_paused(false).await?;
                        AppsError::Runtime(e)
                    }
                    Err(u) => self.left_paused(e.to_string(), u).await,
                });
            }
        }
        let started = std::time::Instant::now();
        let (vols, d) = (self.inner.volumes.clone(), dest);
        let snapped = blocking(move || -> Result<BTreeMap<String, u64>, VolumeError> {
            let mut ids = BTreeMap::new();
            for (name, source, _) in items {
                let to = d.join(&name);
                let id = if read_only {
                    vols.snapshot_read_only(&source, &to)?
                } else {
                    vols.snapshot_unchecked(&source, &to)?
                };
                ids.insert(name, id);
            }
            Ok(ids)
        })
        .await;
        if running {
            let paused_for = started.elapsed();
            match self.unpause(&project).await {
                Ok(()) => {
                    tracing::info!(app = self.name, ?paused_for, "paused for snapshots");
                    self.set_paused(false).await?;
                }
                Err(u) => {
                    let done = match &snapped {
                        Ok(Ok(_)) => "the snapshots were taken".to_string(),
                        Ok(Err(e)) => e.to_string(),
                        Err(e) => e.to_string(),
                    };
                    return Err(self.left_paused(done, u).await);
                }
            }
        }
        Ok(snapped??)
    }

    /// Unpauses the app's containers, trying a few times.
    async fn unpause(&self, project: &Project) -> Result<(), RuntimeError> {
        let mut attempt = 0;
        loop {
            match self.inner.runtime.unpause(project).await {
                Ok(()) => return Ok(()),
                Err(e) if attempt >= 2 => return Err(e),
                Err(e) => {
                    attempt += 1;
                    tracing::warn!(app = self.name, error = %e, "unpausing failed; trying again");
                    tokio::time::sleep(Duration::from_millis(250 * attempt)).await;
                }
            }
        }
    }

    /// Records that the app's containers were left paused, and returns the error to report.
    async fn left_paused(&self, what: String, e: RuntimeError) -> AppsError {
        let message = format!(
            "`{}`'s containers may still be paused after snapshotting it ({what}): unpausing failed: {e}",
            self.name
        );
        tracing::error!(app = self.name, %message);
        if let Err(e) = self.update(|r| r.error = Some(message.clone())).await {
            tracing::error!(app = self.name, error = %e, "couldn't record the failure");
        }
        AppsError::Internal(anyhow::anyhow!(message))
    }

    /// Removes one generation's bundle and generated file.
    async fn remove_generation(&self, generation: u64) {
        let dir = self.app_dir();
        let r = blocking(move || -> std::io::Result<()> {
            remove_dir_if_exists(&dir.join(format!("bundle-{generation}")))?;
            match std::fs::remove_file(dir.join(format!("compose-{generation}.yaml"))) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            }
        })
        .await;
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!(app = self.name, generation, error = %e, "couldn't remove a failed generation")
            }
            Err(e) => tracing::warn!(app = self.name, generation, error = %e, "couldn't remove a failed generation"),
        }
    }

    /// Removes the bundles and generated files of every generation but `keep`.
    async fn cleanup_generations(&self, keep: u64) {
        let dir = self.app_dir();
        let r = blocking(move || -> std::io::Result<()> {
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let generation = name
                    .strip_prefix("bundle-")
                    .or_else(|| name.strip_prefix("compose-").and_then(|n| n.strip_suffix(".yaml")))
                    .and_then(|n| n.parse::<u64>().ok());
                match generation {
                    Some(g) if g != keep && entry.file_type()?.is_dir() => std::fs::remove_dir_all(entry.path())?,
                    Some(g) if g != keep => std::fs::remove_file(entry.path())?,
                    _ => {}
                }
            }
            Ok(())
        })
        .await;
        match r {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(app = self.name, error = %e, "couldn't remove old generations"),
            Err(e) => tracing::warn!(app = self.name, error = %e, "couldn't remove old generations"),
        }
    }

    async fn info(&self, r: AppRecord) -> Result<AppInfo> {
        let open_requests = self.inner.gate(self.name).map_or(0, |g| g.open_leases());
        let containers = self
            .inner
            .runtime
            .ps(&self.project(&r))
            .await?
            .into_iter()
            .map(|c| ContainerInfo {
                service: c.service,
                name: c.name,
                state: c.state,
                health: c.health,
                status: c.status,
            })
            .collect();
        Ok(AppInfo {
            name: r.name,
            status: r.status,
            generation: r.generation,
            error: r.error,
            domain: r.domain,
            service: r.service,
            container_port: r.container_port,
            port: r.port,
            idle_timeout_secs: r.idle_timeout_secs,
            project: r.project,
            bundle_digest: r.bundle_digest,
            volumes: r.volumes,
            shared: r
                .shared
                .into_iter()
                .map(|(name, m)| SharedMountInfo {
                    name,
                    target: m.target,
                    mode: m.mode,
                })
                .collect(),
            from: r.from,
            restored_from: r.restore.map(|x| format!("{}:{}", x.target, x.key)),
            notes: Vec::new(),
            created_at: rfc3339(r.created_at),
            updated_at: rfc3339(r.updated_at),
            containers,
            open_requests,
            node: None,
            nest: None,
            placement: None,
            pending: None,
            stale: false,
        })
    }

    async fn inspect(&self) -> Result<AppInfo> {
        let r = self.record().await?;
        self.info(r).await
    }

    async fn logs(&self, q: LogsQuery) -> Result<Logs> {
        let r = self.record().await?;
        if let Some(s) = &q.service
            && !r.services.contains(s)
        {
            return Err(AppsError::NotFoundOther(format!(
                "app `{}` has no service `{s}`",
                self.name
            )));
        }
        let opts = LogsOptions {
            tail: q.tail,
            follow: q.follow,
            timestamps: q.timestamps,
            service: q.service,
        };
        let child = self.inner.runtime.logs(&self.project(&r), &opts)?;
        if q.follow {
            return Ok(Logs::Follow(child));
        }
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| AppsError::Internal(anyhow::anyhow!("reading logs: {e}")))?;
        if !out.status.success() {
            return Err(AppsError::Runtime(RuntimeError::Failed {
                command: "docker compose logs".into(),
                status: out.status.to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            }));
        }
        Ok(Logs::Complete(out.stdout))
    }

    /// Removes the app's containers and files, and its subvolumes (its volumes, and its snapshots of shared volumes)
    /// unless `keep_data`. Nothing is removed if its containers can't be.
    async fn delete(&self, keep_data: bool) -> Result<()> {
        let r = self.record().await?;
        tracing::info!(app = self.name, keep_data, "deleting");
        let gate = self.inner.gate(self.name);
        if let Some(g) = &gate {
            g.set_phase(Phase::Busy);
            g.break_leases();
        }
        if let Err(e) = self.inner.runtime.down(&self.project(&r)).await {
            if let Some(g) = &gate {
                g.set_phase(resting_phase(r.status));
            }
            return Err(e.into());
        }
        // Its containers are gone: if removing its files fails, it's unavailable until the delete is tried again.
        let removed = self.remove_files(keep_data).await;
        if removed.is_err()
            && let Some(g) = &gate
        {
            g.set_phase(Phase::Broken);
        }
        removed?;
        let mut state = self.inner.state.lock().await;
        state.apps.remove(self.name);
        if let Some(g) = self.inner.remove_route(self.name) {
            g.set_phase(Phase::Gone);
        }
        self.inner.persist(&state).await?;
        tracing::info!(app = self.name, keep_data, "deleted");
        Ok(())
    }

    /// The files of a deleted app: its subvolumes unless `keep_data`, and its bundles and generated files.
    async fn remove_files(&self, keep_data: bool) -> Result<()> {
        if !keep_data {
            let names: Vec<String> = self.inner.state.lock().await.shared.keys().cloned().collect();
            let _locks = self.inner.lock_shared(&names).await;
            let (vols, app) = (self.inner.volumes.clone(), self.name.to_string());
            blocking(move || -> Result<(), VolumeError> {
                for name in &names {
                    let snap = vols.snapshot_path(name, &app);
                    vols.remove(&Volumes::staging_for(&snap))?;
                    vols.remove(&snap)?;
                }
                let dir = vols.app_dir(&app);
                vols.remove(&Volumes::staging_for(&dir))?;
                vols.remove(&dir)?;
                // Its backups' snapshots too: set aside at once (a backup's send may still hold them), and destroyed
                // later. A new app of the same name never continues this one's chains.
                let backups = vols.backups_dir(kn_proto::backup::Kind::App, &app);
                if Volumes::exists(&backups)? {
                    let gone = backups.with_file_name(format!(
                        "{}{app}-{}",
                        crate::volumes::GONE_PREFIX,
                        SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
                    ));
                    kn_btrfs::rename_noreplace(&backups, &gone).map_err(|e| VolumeError {
                        context: format!("setting aside {}", backups.display()),
                        source: e.into(),
                    })?;
                }
                Ok(())
            })
            .await??;
        }
        let app_dir = self.app_dir();
        blocking(move || remove_dir_if_exists(&app_dir))
            .await?
            .map_err(|e| AppsError::Internal(anyhow::anyhow!("removing the app's files: {e}")))
    }
}

/// Picks a free loopback port in the node's range for a new app: not used by another app, and not bound by
/// anything else right now.
fn allocate_port(state: &StateFile, cfg: &NodeConfig, name: &str) -> Result<u16> {
    let used: HashSet<u16> = state.apps.values().map(|a| a.port).collect();
    let (start, end) = (*cfg.ports.start(), *cfg.ports.end());
    let len = u32::from(end - start) + 1;
    // Start somewhere random, so nodes sharing a machine rarely race for the same port.
    let offset = (std::collections::hash_map::RandomState::new().hash_one((name, now())) % u64::from(len)) as u32;
    for i in 0..len {
        let port = start + ((offset + i) % len) as u16;
        if !used.contains(&port) && TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return Ok(port);
        }
    }
    Err(AppsError::Unsupported(format!("no free port left in {start}-{end}")))
}

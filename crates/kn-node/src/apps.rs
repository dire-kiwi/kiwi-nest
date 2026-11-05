//! Each app's lifecycle on the node.
//!
//! Every app has one task, and every operation on the app (deploy, inspect, logs, delete) goes through it, so an
//! app's operations never overlap. Different apps proceed in parallel.
//!
//! What the node has applied is kept in `STATE/state.json`, written atomically. On disk:
//!
//! ```text
//! STATE/state.json                 the apps, their generations, ports and state
//! STATE/staging/                   uploads and bundles being checked
//! STATE/apps/APP/bundle-GEN/       the extracted bundle of APP's generation GEN
//! STATE/apps/APP/compose-GEN.yaml  the Compose file generated from it, outside the bundle
//! VOLUMES/apps/APP/VOL/            APP's named volume VOL (a plain directory, for now)
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::BuildHasher;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kn_bundle::manifest::{Arch, is_valid_app_name};
use kn_bundle::{BundleError, CheckOptions, CheckedBundle, Problems};
use kn_proto::api::{AppInfo, AppSummary, ContainerInfo, LogsQuery};
use kn_proto::state::AppStatus;
use serde::{Deserialize, Serialize};
use tokio::process::Child;
use tokio::sync::{Mutex as AsyncMutex, mpsc, oneshot};

use crate::NodeConfig;
use crate::runtime::{LogsOptions, Project, Runtime, RuntimeError};

const STATE_VERSION: u32 = 1;

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
    /// Unix seconds.
    pub created_at: u64,
    pub updated_at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StateFile {
    version: u32,
    apps: BTreeMap<String, AppRecord>,
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
    #[error("deploying `{name}` failed: {message}")]
    DeployFailed { name: String, message: String },
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
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

/// An app's logs: all of it, or a running `docker compose logs --follow` to stream.
pub enum Logs {
    Complete(Vec<u8>),
    Follow(Child),
}

enum Op {
    Deploy {
        staged: Box<Staged>,
        create_only: bool,
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
        reply: oneshot::Sender<Result<()>>,
    },
}

/// The node's apps.
#[derive(Clone)]
pub struct Apps {
    inner: Arc<Inner>,
}

struct Inner {
    cfg: NodeConfig,
    runtime: Runtime,
    state: AsyncMutex<StateFile>,
    /// Each app's task. Senders are only used with this lock held, so a task can retire without losing an op.
    actors: Mutex<HashMap<String, mpsc::UnboundedSender<Op>>>,
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

impl Apps {
    /// Opens the node's state, creating its directories if needed.
    ///
    /// A deploy that was cut off by a restart is recorded as failed; its containers are left as they are.
    pub async fn open(cfg: NodeConfig) -> anyhow::Result<Apps> {
        cfg.validate()?;
        for d in [cfg.state_dir.join("apps"), cfg.volumes_dir.join("apps")] {
            std::fs::create_dir_all(&d).map_err(|e| anyhow::anyhow!("creating {}: {e}", d.display()))?;
        }
        let staging = cfg.state_dir.join("staging");
        remove_dir_if_exists(&staging)?;
        std::fs::create_dir_all(&staging)?;

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
        let mut changed = false;
        for app in state.apps.values_mut() {
            if app.status == AppStatus::Deploying {
                app.status = AppStatus::Failed;
                app.error = Some("the deploy was cut off by a node restart".into());
                changed = true;
            }
        }
        if changed {
            write_atomic(&path, &serde_json::to_vec_pretty(&state)?)?;
        }
        let runtime = Runtime::new(&cfg.docker);
        Ok(Apps {
            inner: Arc::new(Inner {
                cfg,
                runtime,
                state: AsyncMutex::new(state),
                actors: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn config(&self) -> &NodeConfig {
        &self.inner.cfg
    }

    pub fn runtime(&self) -> &Runtime {
        &self.inner.runtime
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
        if !m.shared.is_empty() {
            return Err(AppsError::Unsupported(
                "this node doesn't support shared volumes yet".into(),
            ));
        }
        Ok(staged)
    }

    fn send(&self, name: &str, op: Op) -> Result<()> {
        if !is_valid_app_name(name) {
            return Err(AppsError::InvalidName(name.to_string()));
        }
        let mut actors = self.inner.actors.lock().expect("actors lock");
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
        tokio::spawn(actor(self.inner.clone(), name.to_string(), rx));
        Ok(())
    }

    async fn ask<T>(&self, name: &str, op: impl FnOnce(oneshot::Sender<Result<T>>) -> Op) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.send(name, op(tx))?;
        rx.await
            .map_err(|_| AppsError::Internal(anyhow::anyhow!("app `{name}`'s task stopped")))?
    }

    /// Deploys a staged bundle as `name`, replacing the app if it exists (unless `create_only`).
    pub async fn deploy(&self, name: &str, staged: Staged, create_only: bool) -> Result<AppInfo> {
        let staged = Box::new(staged);
        self.ask(name, |reply| Op::Deploy {
            staged,
            create_only,
            reply,
        })
        .await
    }

    pub async fn inspect(&self, name: &str) -> Result<AppInfo> {
        self.ask(name, |reply| Op::Inspect { reply }).await
    }

    pub async fn logs(&self, name: &str, query: LogsQuery) -> Result<Logs> {
        self.ask(name, |reply| Op::Logs { query, reply }).await
    }

    /// Removes the app's containers, its bundle and its volumes.
    pub async fn delete(&self, name: &str) -> Result<()> {
        self.ask(name, |reply| Op::Delete { reply }).await
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
            })
            .collect())
    }
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
            Op::Deploy {
                staged,
                create_only,
                reply,
            } => {
                let _ = reply.send(app.deploy(*staged, create_only).await);
            }
            Op::Inspect { reply } => {
                let _ = reply.send(app.inspect().await);
            }
            Op::Logs { query, reply } => {
                let _ = reply.send(app.logs(query).await);
            }
            Op::Delete { reply } => {
                let _ = reply.send(app.delete().await);
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
    inner: &'a Inner,
    name: &'a str,
}

impl App<'_> {
    fn app_dir(&self) -> PathBuf {
        self.inner.cfg.state_dir.join("apps").join(self.name)
    }

    fn volumes_dir(&self) -> PathBuf {
        self.inner.cfg.volumes_dir.join("apps").join(self.name)
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

    async fn persist(&self, state: &StateFile) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state).map_err(|e| AppsError::Internal(e.into()))?;
        let path = self.inner.cfg.state_dir.join("state.json");
        blocking(move || write_atomic(&path, &bytes))
            .await?
            .map_err(|e| AppsError::Internal(e.into()))
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
        self.persist(&state).await?;
        Ok(r)
    }

    async fn deploy(&self, staged: Staged, create_only: bool) -> Result<AppInfo> {
        let cfg = &self.inner.cfg;
        let m = &staged.bundle.manifest;
        // Reserve the generation and port, and record the deploy before touching anything.
        let (record, previous) = {
            let mut state = self.inner.state.lock().await;
            let previous = state.apps.get(self.name).cloned();
            if create_only && previous.is_some() {
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
            let port = match &previous {
                Some(p) => p.port,
                None => allocate_port(&state, cfg, self.name)?,
            };
            let t = now();
            let record = AppRecord {
                name: self.name.to_string(),
                status: AppStatus::Deploying,
                generation: previous.as_ref().map_or(1, |p| p.generation + 1),
                error: None,
                project: format!("{}{}", cfg.project_prefix, self.name),
                port,
                domain: m.domain.clone(),
                service: m.service.clone(),
                container_port: m.port,
                idle_timeout_secs: m.idle_timeout_secs,
                bundle_digest: staged.digest.clone(),
                services: staged.bundle.services.iter().map(|s| s.name.clone()).collect(),
                volumes: staged.bundle.volumes.clone(),
                created_at: previous.as_ref().map_or(t, |p| p.created_at),
                updated_at: t,
            };
            state.apps.insert(self.name.to_string(), record.clone());
            self.persist(&state).await?;
            (record, previous)
        };
        tracing::info!(app = self.name, generation = record.generation, digest = %record.bundle_digest, "deploying");

        let generation = record.generation;
        let failed = match self.apply(&record, previous.as_ref(), staged).await {
            Ok(()) => {
                self.cleanup_generations(generation).await;
                let record = self.update(|r| r.status = AppStatus::Running).await?;
                tracing::info!(app = self.name, generation, "deployed");
                return self.info(record).await;
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
                let mut state = self.inner.state.lock().await;
                state.apps.insert(self.name.to_string(), prev);
                self.persist(&state).await?;
                return Err(AppsError::DeployFailed {
                    name: self.name.into(),
                    message,
                });
            }
        };
        let message = failed.to_string();
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

    /// Puts the new generation's files in place, stops the old project and starts the new one.
    async fn apply(
        &self,
        record: &AppRecord,
        previous: Option<&AppRecord>,
        staged: Staged,
    ) -> std::result::Result<(), Failure> {
        let cfg = &self.inner.cfg;
        let app_dir = self.app_dir();
        let bundle_dir = app_dir.join(format!("bundle-{}", record.generation));
        let volumes_dir = self.volumes_dir();
        let volume_dirs: BTreeMap<String, PathBuf> = record
            .volumes
            .iter()
            .map(|v| (v.clone(), volumes_dir.join(v)))
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
            for dir in volume_dirs.values() {
                std::fs::create_dir_all(dir)?;
            }
            write_atomic(&file, generated.as_bytes())
        })
        .await
        .map_err(Failure::NotStarted)?
        .map_err(|e| Failure::NotStarted(AppsError::Internal(anyhow::anyhow!("putting the bundle in place: {e}"))))?;

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
        self.inner
            .runtime
            .up(&project, cfg.up_timeout)
            .await
            .map_err(|e| Failure::Started(e.into()))
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
            created_at: rfc3339(r.created_at),
            updated_at: rfc3339(r.updated_at),
            containers,
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

    async fn delete(&self) -> Result<()> {
        let r = self.record().await?;
        tracing::info!(app = self.name, "deleting");
        self.inner.runtime.down(&self.project(&r)).await?;
        let (app_dir, volumes_dir) = (self.app_dir(), self.volumes_dir());
        blocking(move || -> std::io::Result<()> {
            remove_dir_if_exists(&app_dir)?;
            remove_dir_if_exists(&volumes_dir)
        })
        .await?
        .map_err(|e| AppsError::Internal(anyhow::anyhow!("removing the app's files: {e}")))?;
        let mut state = self.inner.state.lock().await;
        state.apps.remove(self.name);
        self.persist(&state).await?;
        tracing::info!(app = self.name, "deleted");
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in for `docker` that succeeds, unless `fail-down` or `fail-up` exists beside it.
    const FAKE_DOCKER: &str = r#"#!/bin/sh
dir=$(dirname "$0")
case " $* " in
  *" down "*) if [ -e "$dir/fail-down" ]; then echo "down failed" >&2; exit 1; fi ;;
  *" up "*) if [ -e "$dir/fail-up" ]; then echo "up failed" >&2; exit 1; fi ;;
esac
exit 0
"#;

    struct Env {
        dir: tempfile::TempDir,
        apps: Apps,
    }

    impl Env {
        async fn new() -> Env {
            let dir = tempfile::tempdir().unwrap();
            let docker = dir.path().join("docker");
            std::fs::write(&docker, FAKE_DOCKER).unwrap();
            std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut cfg = NodeConfig::new(dir.path().join("state"));
            cfg.docker = docker;
            let apps = Apps::open(cfg).await.unwrap();
            Env { dir, apps }
        }

        fn path(&self, p: &str) -> PathBuf {
            self.dir.path().join(p)
        }

        async fn deploy(&self, page: &str) -> Result<AppInfo> {
            let b = self.path("bundle");
            std::fs::create_dir_all(b.join("site")).unwrap();
            std::fs::write(b.join("site/index.html"), page).unwrap();
            std::fs::write(
                b.join("compose.yaml"),
                "x-kiwi-nest: { domain: a.localhost, service: web, port: 80 }\n\
                 services:\n  web:\n    image: x\n    volumes: [\"./site:/www\", \"data:/data\"]\n\
                 volumes:\n  data: {}\n",
            )
            .unwrap();
            let staged = self.apps.stage_dir(b).await?;
            self.apps.deploy("app", staged, false).await
        }

        fn generation_files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.path("state/apps/app"))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    #[tokio::test]
    async fn a_redeploy_that_fails_before_stopping_keeps_the_running_generation() {
        let env = Env::new().await;
        assert_eq!(env.deploy("one").await.unwrap().generation, 1);
        std::fs::write(env.path("state/volumes/apps/app/data/db"), "data").unwrap();

        // `down` of generation 1 fails: its containers may still be running from its bundle, so that stays.
        std::fs::write(env.path("fail-down"), "").unwrap();
        let e = env.deploy("two").await.unwrap_err();
        assert!(
            matches!(&e, AppsError::DeployFailed { message, .. } if message.contains("down failed")),
            "{e}"
        );
        assert_eq!(env.generation_files(), ["bundle-1", "compose-1.yaml"]);
        let page = env.path("state/apps/app/bundle-1/site/index.html");
        assert_eq!(std::fs::read_to_string(&page).unwrap(), "one");
        let info = env.apps.inspect("app").await.unwrap();
        assert_eq!(info.generation, 1);
        assert_eq!(info.status, AppStatus::Running);
        assert!(
            info.error
                .as_deref()
                .unwrap()
                .contains("generation 1 is still in place"),
            "{:?}",
            info.error
        );

        // `up` of the new generation fails after generation 1 was stopped: the new one is the app now.
        std::fs::remove_file(env.path("fail-down")).unwrap();
        std::fs::write(env.path("fail-up"), "").unwrap();
        env.deploy("three").await.unwrap_err();
        assert_eq!(env.generation_files(), ["bundle-2", "compose-2.yaml"]);
        let info = env.apps.inspect("app").await.unwrap();
        assert_eq!((info.generation, info.status), (2, AppStatus::Failed));

        // And a working deploy after that, with the data still there.
        std::fs::remove_file(env.path("fail-up")).unwrap();
        let info = env.deploy("four").await.unwrap();
        assert_eq!(
            (info.generation, info.status, info.error),
            (3, AppStatus::Running, None)
        );
        assert_eq!(env.generation_files(), ["bundle-3", "compose-3.yaml"]);
        assert_eq!(
            std::fs::read_to_string(env.path("state/volumes/apps/app/data/db")).unwrap(),
            "data"
        );
    }

    #[tokio::test]
    async fn a_first_deploy_that_fails_is_recorded() {
        let env = Env::new().await;
        std::fs::write(env.path("fail-up"), "").unwrap();
        env.deploy("one").await.unwrap_err();
        let info = env.apps.inspect("app").await.unwrap();
        assert_eq!((info.generation, info.status), (1, AppStatus::Failed));
        assert!(info.error.unwrap().contains("up failed"));
        env.apps.delete("app").await.unwrap();
        assert!(matches!(env.apps.inspect("app").await, Err(AppsError::NotFound(_))));
    }
}

//! What a node does once the server says it was forgotten (`node forget`): it never runs its apps again.
//!
//! 1. It records that it was forgotten in `agent.json`, and drops its token, so it never polls as that node again.
//! 2. It takes every app down (`docker compose down`), so no container is left for Docker to restart.
//! 3. It moves its data aside, by renames within each filesystem, so nothing is copied or deleted:
//!
//!    ```text
//!    VOLUMES/forgotten/TIMESTAMP/apps/        was VOLUMES/apps: the apps' subvolumes
//!    VOLUMES/forgotten/TIMESTAMP/shared/      was VOLUMES/shared: the shared volumes and their snapshots
//!    VOLUMES/forgotten/TIMESTAMP/state.json   was STATE/state.json: what the node had, with its subvolume IDs
//!    VOLUMES/forgotten/TIMESTAMP/bundles/     was STATE/apps: bundles and generated Compose files
//!    VOLUMES/forgotten/TIMESTAMP/forgotten.json  who the node was, and when it was forgotten
//!    ```
//!
//!    (If the state directory is on another filesystem, `STATE/apps` goes to `STATE/forgotten/TIMESTAMP/apps`
//!    instead, and `state.json`, a few kilobytes, is copied and then removed.)
//! 4. It records that all that is done, and the agent stops. It refuses to start again until it's given a fresh
//!    join token, which makes it a new node with an empty desired state.
//!
//! Every step can be run again: a node cut off in the middle finishes the job when it next starts, before it opens
//! its apps, so it never adopts (and starts) an app it should have taken down.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::NodeConfig;
use crate::apps::write_atomic;
use crate::runtime::{Project, Runtime};

/// The record of a forgotten node, kept in `agent.json` (and in its set-aside directory).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forgotten {
    pub server: String,
    pub node_id: u64,
    pub name: String,
    /// When the server forgot it, in Unix seconds.
    pub forgotten_at: u64,
    /// When the node heard, in Unix seconds.
    pub noticed_at: u64,
    /// Where its data is set aside: `VOLUMES/forgotten/TIMESTAMP`.
    pub aside: PathBuf,
    /// Every app is down and the data is set aside.
    #[serde(default)]
    pub done: bool,
}

fn rfc3339(secs: u64) -> String {
    humantime::format_rfc3339_seconds(std::time::UNIX_EPOCH + Duration::from_secs(secs)).to_string()
}

impl Forgotten {
    pub fn new(cfg: &NodeConfig, server: &str, node_id: u64, name: &str, forgotten_at: u64, now: u64) -> Self {
        let stamp = rfc3339(now).replace([':', '-'], "");
        Forgotten {
            server: server.into(),
            node_id,
            name: name.into(),
            forgotten_at,
            noticed_at: now,
            aside: cfg.volumes_dir.join("forgotten").join(stamp),
            done: false,
        }
    }

    /// Why the agent won't start, for people.
    pub fn describe(&self) -> String {
        format!(
            "the server at {} forgot this node (`{}`, node {}) at {}; its apps were taken down and their data set \
             aside in {}, which is deleted only by hand. It won't start again as that node: to use this machine \
             as a new one, start it with a fresh join token (--join-token)",
            self.server,
            self.name,
            self.node_id,
            rfc3339(self.forgotten_at),
            self.aside.display()
        )
    }
}

/// What [`set_aside`] needs of `state.json`: each app's Compose project.
#[derive(Debug, Default, Deserialize)]
struct State {
    #[serde(default)]
    apps: BTreeMap<String, AppRecord>,
}

#[derive(Debug, Deserialize)]
struct AppRecord {
    project: String,
    generation: u64,
}

/// Renames `from` to `to`, unless `from` is gone already (moved by an earlier try). Refuses to replace anything.
fn rename(from: &Path, to: &Path) -> std::io::Result<bool> {
    if std::fs::symlink_metadata(from).is_err() {
        return Ok(false);
    }
    if std::fs::symlink_metadata(to).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("{} is already there", to.display()),
        ));
    }
    std::fs::rename(from, to)?;
    Ok(true)
}

fn sync_dir(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
}

/// Takes every app down and sets the node's data aside (see the module's docs). Returns the record, done.
pub async fn set_aside(cfg: &NodeConfig, runtime: &dyn Runtime, mut rec: Forgotten) -> anyhow::Result<Forgotten> {
    let state_file = cfg.state_dir.join("state.json");
    let moved_state = rec.aside.join("state.json");
    let state: State = match std::fs::read(&state_file).or_else(|_| std::fs::read(&moved_state)) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("reading the node's state file")?,
        Err(_) => State::default(),
    };

    // Every app down first: nothing is moved while a container might still use it.
    let mut failed = Vec::new();
    for (name, app) in &state.apps {
        let dir = cfg.state_dir.join("apps").join(name);
        let project = Project {
            name: app.project.clone(),
            file: dir.join(format!("compose-{}.yaml", app.generation)),
            dir,
        };
        let mut result = Ok(());
        for attempt in 0..3 {
            result = runtime.down(&project).await;
            if result.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500 * (attempt + 1))).await;
        }
        match result {
            Ok(()) => tracing::warn!(app = name, "took the app down: the server forgot this node"),
            Err(e) => failed.push(format!("`{name}`: {e}")),
        }
    }
    if !failed.is_empty() {
        anyhow::bail!(
            "the server forgot this node, and taking its apps down failed ({}); nothing was set aside, and the node \
             tries again when it next starts",
            failed.join("; ")
        );
    }

    let aside = rec.aside.clone();
    let state_dir = cfg.state_dir.clone();
    let volumes_dir = cfg.volumes_dir.clone();
    let record = rec.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        std::fs::create_dir_all(&aside).with_context(|| format!("creating {}", aside.display()))?;
        for d in ["apps", "shared", "backups"] {
            rename(&volumes_dir.join(d), &aside.join(d))
                .with_context(|| format!("moving {} aside", volumes_dir.join(d).display()))?;
        }
        let bundles = state_dir.join("apps");
        match rename(&bundles, &aside.join("bundles")) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                let stamp = aside.file_name().unwrap_or_default();
                let local = state_dir.join("forgotten").join(stamp);
                std::fs::create_dir_all(&local)?;
                rename(&bundles, &local.join("apps")).with_context(|| format!("moving {} aside", bundles.display()))?;
                sync_dir(&local);
            }
            Err(e) => return Err(e).with_context(|| format!("moving {} aside", bundles.display())),
        }
        let state_file = state_dir.join("state.json");
        let moved = aside.join("state.json");
        match rename(&state_file, &moved) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::CrossesDevices => {
                write_atomic(&moved, &std::fs::read(&state_file)?)?;
                std::fs::remove_file(&state_file)?;
            }
            Err(e) => return Err(e).with_context(|| format!("moving {} aside", state_file.display())),
        }
        let done = Forgotten { done: true, ..record };
        write_atomic(&aside.join("forgotten.json"), &serde_json::to_vec_pretty(&done)?)?;
        sync_dir(&aside);
        sync_dir(&volumes_dir);
        sync_dir(&state_dir);
        Ok(())
    })
    .await??;
    rec.done = true;
    tracing::warn!(aside = %rec.aside.display(), "set the node's data aside: the server forgot this node");
    Ok(rec)
}

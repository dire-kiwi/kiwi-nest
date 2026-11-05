//! The server's side of backups: the targets (from `--backup-targets`, handed to nodes too), the schedule, and
//! backups of its own state.
//!
//! **The schedule.** Every `--backup-check` (a minute), for each target with `every`, the server lists what the
//! target holds, and backs up whatever's due there: each app (unless its bundle says `backups: false`) and shared
//! volume on an online node, by sending its node a backup command naming the due targets, and the server itself.
//! The target's listing is the record, so nothing about the schedule is kept in the database: a restored or
//! restarted server picks up where the targets say it was. A backup that fails is tried again after a while.
//!
//! **The server's own state** is always a tar archive (`server/STAMP.tar.zst`) of its database and the bundles it
//! refers to:
//!
//! 1. Bundle files stop being deleted (deletes that come up meanwhile are left for later), so every bundle the copy
//!    refers to is still there to be linked.
//! 2. SQLite's online backup API copies the database, in one step, into a staging directory beside the bundles,
//!    while the server keeps serving (in WAL mode, a reader never blocks a writer). The copy is checked with
//!    `PRAGMA integrity_check`.
//! 3. Each bundle the copy refers to is hard-linked into the staging directory. Bundles are never changed once
//!    written, so a link is a copy.
//! 4. Bundle deletion resumes, and what it skipped is swept.
//! 5. The staging directory is archived to each target. Only this part takes long, and nothing waits on it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use kn_backup::{Key, Listed, Stamp, Target};
use kn_proto::agent::Action;
use kn_proto::backup::{BackupResult, Format, Kind, TargetConfig, TargetError};
use tokio_util::sync::CancellationToken;

use crate::Server;
use crate::error::{Error, Result};
use crate::store::{self, apps, nodes};

/// The database's file name, in the state directory and in a backup.
pub const DATABASE: &str = "server.sqlite";

/// The targets, and what the schedule keeps in memory.
pub struct Backups {
    /// As configured, with the files they name read in: what nodes are given.
    pub configs: Vec<TargetConfig>,
    pub targets: Vec<Target>,
    /// Held while the server backs itself up, so two never overlap.
    own: tokio::sync::Mutex<()>,
    /// What a scheduled backup is running for now.
    running: Mutex<HashSet<(Kind, String)>>,
    /// When a scheduled backup last failed, by target, kind and name.
    failed: Mutex<HashMap<(String, Kind, String), Instant>>,
    /// Run right after the database is copied, before the bundles are linked (tests race deletes against it).
    #[cfg(test)]
    pub(crate) after_copy: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Backups {
    /// Opens the targets, with the files they name read in first: nodes get the configurations with what those
    /// files hold, so the files need be only on the server.
    pub fn new(configs: Vec<TargetConfig>) -> anyhow::Result<Backups> {
        let configs: Vec<TargetConfig> = configs
            .into_iter()
            .map(kn_backup::target::inline_files)
            .collect::<anyhow::Result<_>>()?;
        let targets = kn_backup::target::open_all(configs.clone())?;
        Ok(Backups {
            configs,
            targets,
            own: tokio::sync::Mutex::new(()),
            running: Mutex::new(HashSet::new()),
            failed: Mutex::new(HashMap::new()),
            #[cfg(test)]
            after_copy: Mutex::new(None),
        })
    }

    /// The targets named (every target if none is).
    pub fn chosen(&self, only: &[String]) -> Result<Vec<&Target>> {
        if self.targets.is_empty() {
            return Err(Error::BadRequest(
                "the server has no backup targets: give it --backup-targets".into(),
            ));
        }
        if only.is_empty() {
            return Ok(self.targets.iter().collect());
        }
        only.iter()
            .map(|n| {
                self.targets
                    .iter()
                    .find(|t| t.name() == n)
                    .ok_or_else(|| Error::NotFound(format!("there's no backup target `{n}`")))
            })
            .collect()
    }
}

/// How long a scheduled backup that failed waits before it's tried again.
fn retry_after(every: Duration) -> Duration {
    every.min(Duration::from_secs(15 * 60))
}

impl Server {
    /// Backs up the server's own state to the targets named (every target if none is).
    pub async fn backup_self(&self, only: &[String]) -> Result<BackupResult> {
        let targets = self.backups.chosen(only)?;
        let _one = self.backups.own.lock().await;
        let mut result = BackupResult::default();
        let mut listings: Vec<(&Target, Vec<Listed>)> = Vec::new();
        for t in targets {
            match t.list(Some((Kind::Server, ""))).await {
                Ok(l) => listings.push((t, l)),
                Err(e) => result.failed.push(TargetError {
                    target: t.name().into(),
                    error: format!("{e:#}"),
                }),
            }
        }
        if listings.is_empty() {
            return Ok(result);
        }
        let all: Vec<Listed> = listings.iter().flat_map(|(_, l)| l.clone()).collect();
        let stamp = Target::next_stamp(&all, None);

        let staging = self.bundles.temp_dir()?;
        let (copied, bundles) = self.snapshot_state(staging.path()).await?;
        result.notes.push(format!(
            "copied the database and {bundles} bundle{}",
            if bundles == 1 { "" } else { "s" }
        ));
        result.notes.extend(copied);

        for (t, _) in &listings {
            let key = Key::new(Kind::Server, "", stamp, None, Format::Tar);
            let dir = staging.path().to_path_buf();
            let produce: kn_backup::Producer = Box::new(move |out| {
                let mut w = kn_backup::tarball::TarWriter::new(out);
                w.add_tree("", &dir)?;
                w.finish()?;
                Ok(())
            });
            match t.upload(&key, produce).await {
                Ok(size) => {
                    tracing::info!(target = t.name(), key = %key, size, "backed up the server");
                    result.made.push(key.info(t.name(), Some(size)));
                    match t.prune(Kind::Server, "", None).await {
                        Ok(gone) if !gone.is_empty() => result.notes.push(format!(
                            "retention removed {} older backup{} from target `{}`",
                            gone.len(),
                            if gone.len() == 1 { "" } else { "s" },
                            t.name()
                        )),
                        Ok(_) => {}
                        Err(e) => result
                            .notes
                            .push(format!("retention on target `{}` failed: {e:#}", t.name())),
                    }
                }
                Err(e) => {
                    tracing::warn!(target = t.name(), error = %format!("{e:#}"), "backing up the server failed");
                    result.failed.push(TargetError {
                        target: t.name().into(),
                        error: format!("{e:#}"),
                    });
                }
            }
        }
        Ok(result)
    }

    /// Steps 1 to 4 of the module's docs: a consistent copy of the database and the bundles it refers to, in
    /// `dest`. Returns notes, and how many bundles it linked.
    async fn snapshot_state(&self, dest: &Path) -> Result<(Vec<String>, usize)> {
        let hold = self.bundles.hold();
        let (db, dest_owned, bundles) = (
            self.cfg.state_dir.join(DATABASE),
            dest.to_path_buf(),
            self.bundles.clone(),
        );
        #[cfg(test)]
        let hook = self.backups.after_copy.lock().expect("hook").take();
        let copied = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<String>, usize)> {
            let copy = dest_owned.join(DATABASE);
            copy_database(&db, &copy)?;
            #[cfg(test)]
            if let Some(hook) = hook {
                hook();
            }
            let conn = rusqlite::Connection::open(&copy)?;
            let digests: Vec<String> = conn
                .prepare("SELECT digest FROM bundles ORDER BY digest")?
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            drop(conn);
            let dir = dest_owned.join("bundles");
            std::fs::create_dir(&dir)?;
            let mut notes = Vec::new();
            let mut linked = 0;
            for d in digests {
                let Some(from) = bundles.path(&d) else { continue };
                let to = dir.join(from.file_name().context("a bundle has a file name")?);
                match std::fs::hard_link(&from, &to) {
                    Ok(()) => linked += 1,
                    Err(e) => notes.push(format!("bundle {d} couldn't be copied: {e}")),
                }
            }
            Ok((notes, linked))
        })
        .await
        .map_err(|e| Error::Internal(e.into()))?;
        drop(hold);
        // What was kept back while the copy was made.
        let b = self.bundles.clone();
        if let Err(e) = self.store.call(move |c| apps::sweep(c, &b)).await {
            tracing::warn!(error = %e, "couldn't remove the bundles kept for a backup");
        }
        copied.map_err(Error::Internal)
    }
}

/// Copies a live database with SQLite's online backup API, in one step (so it's one consistent read, which in
/// WAL mode never blocks the server's writes), and checks the copy.
pub fn copy_database(from: &Path, to: &Path) -> anyhow::Result<()> {
    let src = rusqlite::Connection::open_with_flags(
        from,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", from.display()))?;
    src.busy_timeout(Duration::from_secs(30))?;
    let mut dst = rusqlite::Connection::open(to).with_context(|| format!("creating {}", to.display()))?;
    {
        use rusqlite::backup::StepResult;
        let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
        // Every page in one step: one read transaction, so the copy is of one instant. A step that finds the
        // database locked (it can't, in WAL mode, but a checkpoint might briefly) is tried again.
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            match backup.step(-1).context("copying the database")? {
                StepResult::Done => break,
                StepResult::More => {}
                StepResult::Busy | StepResult::Locked if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                r => anyhow::bail!("copying the database: {r:?}"),
            }
        }
    }
    // A plain file, without a WAL beside it.
    dst.pragma_update(None, "journal_mode", "DELETE")?;
    let check: String = dst.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if check != "ok" {
        anyhow::bail!("the copy of the database fails its integrity check: {check}");
    }
    Ok(())
}

/// Restores a server's state from a backup into `state_dir`, which must not hold a database yet.
pub async fn restore(state_dir: &Path, targets: &[Target], target: Option<&str>, key: &str) -> anyhow::Result<String> {
    let k = kn_backup::parse_restore_key(key, Kind::Server)?;
    let t = kn_backup::find(targets, target, &k).await?;
    std::fs::create_dir_all(state_dir).with_context(|| format!("creating {}", state_dir.display()))?;
    let db = state_dir.join(DATABASE);
    if db.exists() {
        anyhow::bail!(
            "{} already has a database; restore into an empty state directory",
            state_dir.display()
        );
    }
    let staging = tempfile::Builder::new()
        .prefix(".restore-")
        .tempdir_in(state_dir)
        .with_context(|| format!("making a directory in {}", state_dir.display()))?;
    let into = staging.path().to_path_buf();
    t.download(&k, move |r| kn_backup::tarball::extract_plain(r, &into))
        .await?;
    let restored = staging.path().join(DATABASE);
    let conn = rusqlite::Connection::open(&restored).context("opening the restored database")?;
    let check: String = conn.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    if check != "ok" {
        anyhow::bail!("the restored database fails its integrity check: {check}");
    }
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version as usize > store::MIGRATIONS.len() {
        anyhow::bail!("the backup is from a newer kiwi-nest (database version {version})");
    }
    let bundles: i64 = conn.query_row("SELECT count(*) FROM bundles", [], |r| r.get(0))?;
    drop(conn);
    let dir = state_dir.join("bundles");
    std::fs::create_dir_all(&dir)?;
    let mut moved = 0;
    if let Ok(entries) = std::fs::read_dir(staging.path().join("bundles")) {
        for e in entries {
            let e = e?;
            let to = dir.join(e.file_name());
            if !to.exists() {
                std::fs::rename(e.path(), &to)?;
                moved += 1;
            }
        }
    }
    std::fs::rename(&restored, &db)?;
    std::fs::File::open(state_dir)?.sync_all()?;
    Ok(format!(
        "restored the server's state from `{k}` (target `{}`) into {}: the database and {moved} of its {bundles} \
         bundles",
        t.name(),
        state_dir.display()
    ))
}

/// What the schedule sends one node: the targets due for each app or volume.
type Due = BTreeMap<(Kind, String), (u64, String, Vec<String>)>;

/// Runs the schedule until `stop`.
pub async fn schedule(server: Server, stop: CancellationToken) {
    if !server.backups.targets.iter().any(|t| t.every().is_some()) {
        return;
    }
    let every = server.cfg.backup_check_interval;
    loop {
        tokio::select! {
            _ = stop.cancelled() => return,
            _ = tokio::time::sleep(every) => {}
        }
        if let Err(e) = tick(&server).await {
            tracing::warn!(error = %e, "checking for backups that are due failed");
        }
    }
}

/// One look at what's due, starting what is.
async fn tick(s: &Server) -> Result<()> {
    let (rows, vols, nodes) = s
        .store
        .call(|c| Ok((apps::apps(c)?, apps::volumes(c)?, nodes::list(c)?)))
        .await?;
    let nodes: HashMap<u64, nodes::Node> = nodes.into_iter().map(|n| (n.id, n)).collect();
    let now = Stamp::now();
    let mut due: Due = BTreeMap::new();
    let mut server_due = Vec::new();
    for t in s.backups.targets.iter() {
        let Some(every) = t.every() else { continue };
        let listing = match t.list(None).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(target = t.name(), error = %format!("{e:#}"), "couldn't list a backup target");
                continue;
            }
        };
        let mut latest: HashMap<(Kind, String), Stamp> = HashMap::new();
        for l in listing {
            let e = latest.entry((l.key.kind, l.key.name.clone())).or_insert(l.key.stamp);
            *e = (*e).max(l.key.stamp);
        }
        let failed = s.backups.failed.lock().expect("failed").clone();
        let is_due = |kind: Kind, name: &str| {
            let fresh = latest
                .get(&(kind, name.to_string()))
                .is_some_and(|at| now.0.saturating_sub(at.0) < every.as_secs());
            let backing_off = failed
                .get(&(t.name().to_string(), kind, name.to_string()))
                .is_some_and(|at| at.elapsed() < retry_after(every));
            !fresh && !backing_off
        };
        if is_due(Kind::Server, "") {
            server_due.push(t.name().to_string());
        }
        // Online, and new enough for backups.
        let online = |id: Option<u64>| {
            id.and_then(|i| nodes.get(&i))
                .filter(|n| s.hub.is_online(n.id) && nodes::has_backups(n))
        };
        for a in rows.iter().filter(|a| a.state == apps::State::Active && a.backups) {
            let Some(n) = online(a.node_id) else { continue };
            // Only once the node has it.
            let on_node = n
                .report
                .as_ref()
                .is_some_and(|r| r.apps.iter().any(|x| x.name == a.name));
            if on_node && is_due(Kind::App, &a.name) {
                due.entry((Kind::App, a.name.clone()))
                    .or_insert_with(|| (n.id, n.name.clone(), Vec::new()))
                    .2
                    .push(t.name().to_string());
            }
        }
        for v in vols.iter().filter(|v| v.state == apps::State::Active) {
            let Some(n) = online(v.node_id) else { continue };
            let on_node = n
                .report
                .as_ref()
                .is_some_and(|r| r.volumes.iter().any(|x| x.name == v.name));
            if on_node && is_due(Kind::Volume, &v.name) {
                due.entry((Kind::Volume, v.name.clone()))
                    .or_insert_with(|| (n.id, n.name.clone(), Vec::new()))
                    .2
                    .push(t.name().to_string());
            }
        }
    }
    if !server_due.is_empty() && claim(s, Kind::Server, "") {
        let s = s.clone();
        tokio::spawn(async move {
            let r = s.backup_self(&server_due).await;
            settle(&s, Kind::Server, "", &server_due, r);
        });
    }
    for ((kind, name), (node_id, node, targets)) in due {
        if !claim(s, kind, &name) {
            continue;
        }
        let s = s.clone();
        tokio::spawn(async move {
            tracing::info!(kind = %kind, name, node, ?targets, "scheduled backup");
            let action = Action::Backup {
                what: kind,
                name: name.clone(),
                targets: targets.clone(),
                full: false,
            };
            let r = match s
                .command_with(node_id, &node, action, "schedule", s.cfg.backup_timeout)
                .await
            {
                Ok(o) => crate::api::result::<BackupResult>(o),
                Err(e) => Err(e),
            };
            settle(&s, kind, &name, &targets, r);
        });
    }
    Ok(())
}

/// Marks a scheduled backup as running, unless one is.
fn claim(s: &Server, kind: Kind, name: &str) -> bool {
    s.backups
        .running
        .lock()
        .expect("running")
        .insert((kind, name.to_string()))
}

/// Records how a scheduled backup went.
fn settle(s: &Server, kind: Kind, name: &str, targets: &[String], r: Result<BackupResult>) {
    s.backups
        .running
        .lock()
        .expect("running")
        .remove(&(kind, name.to_string()));
    let mut failed = s.backups.failed.lock().expect("failed");
    let failures: Vec<(String, String)> = match r {
        Ok(res) => {
            for m in &res.made {
                failed.remove(&(m.target.clone(), kind, name.to_string()));
            }
            res.failed.into_iter().map(|f| (f.target, f.error)).collect()
        }
        Err(e) => targets.iter().map(|t| (t.clone(), e.to_string())).collect(),
    };
    for (t, e) in failures {
        tracing::warn!(kind = %kind, name, target = t, error = e, "a scheduled backup failed; it's tried again later");
        failed.insert((t, kind, name.to_string()), Instant::now());
    }
}

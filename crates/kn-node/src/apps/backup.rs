//! Backups of apps and shared volumes, and restores into a first deploy or a new volume.
//!
//! A backup starts from a set of read-only snapshots, taken at one instant (an app's containers are paused only
//! for that), in `VOLUMES/backups/apps/APP/STAMP` (or `…/volumes/NAME/STAMP`). Then, for each target:
//!
//! - `send`: the set goes up as one `btrfs send` stream per volume, incremental against the newest earlier set
//!   that's both still on the node and in the target with its whole chain, unless that chain's full backup is older
//!   than the target's `full_every` (or `--full` is given), when it's a full one again.
//! - `tar`: the set goes up as one archive.
//!
//! Then each target's retention runs, and the node keeps only the sets that some `send` target could continue a
//! chain from: for each one, the newest set it has. A set no target took is destroyed, so a failed backup never
//! becomes a parent. Backups of one app or volume never overlap.

use std::collections::BTreeSet;
use std::sync::Arc;

use futures_util::future::BoxFuture;
use kn_backup::{Key, Listed, Stamp, Target};
use kn_proto::backup::{BackupInfo, BackupResult, Format, Kind, ListQuery, Restore, TargetError};

use super::*;

/// Where a node gets its backup targets: a file of its own (standalone), or the server.
pub trait TargetSource: Send + Sync {
    fn targets(&self) -> BoxFuture<'_, anyhow::Result<Arc<Vec<Target>>>>;
}

/// Targets known up front.
pub struct StaticTargets {
    targets: Arc<Vec<Target>>,
    /// Where they'd be configured, for the error when there are none.
    hint: &'static str,
}

impl StaticTargets {
    pub fn new(targets: Vec<Target>, hint: &'static str) -> StaticTargets {
        StaticTargets {
            targets: Arc::new(targets),
            hint,
        }
    }
}

impl TargetSource for StaticTargets {
    fn targets(&self) -> BoxFuture<'_, anyhow::Result<Arc<Vec<Target>>>> {
        Box::pin(async move {
            if self.targets.is_empty() {
                anyhow::bail!("no backup targets are configured ({})", self.hint);
            }
            Ok(self.targets.clone())
        })
    }
}

/// kiwi-nest's subvolume backend, for restores.
struct Subvols(Arc<dyn Backend>);

impl kn_backup::Subvolumes for Subvols {
    fn create(&self, path: &Path) -> anyhow::Result<()> {
        Ok(self.0.create(path)?)
    }

    fn snapshot(&self, source: &Path, dest: &Path) -> anyhow::Result<()> {
        Ok(self.0.snapshot(source, dest)?)
    }
}

fn internal(e: anyhow::Error) -> AppsError {
    AppsError::Internal(e)
}

/// The names a backup of an app may hold: its volumes, and its snapshots of shared volumes as `@NAME`.
fn app_set_name(n: &str) -> bool {
    let n = n.strip_prefix('@').unwrap_or(n);
    kn_bundle::compose::is_valid_object_name(n)
}

impl Apps {
    /// Uses these targets from now on (the agent's, from the server).
    pub fn set_targets(&self, targets: Arc<dyn TargetSource>) {
        *self.inner.targets.write().expect("targets") = targets;
    }

    async fn targets(&self) -> Result<Arc<Vec<Target>>> {
        let source = self.inner.targets.read().expect("targets").clone();
        source
            .targets()
            .await
            .map_err(|e| AppsError::Unsupported(format!("{e:#}")))
    }

    fn backup_lock(&self, kind: Kind, name: &str) -> Arc<AsyncMutex<()>> {
        self.inner
            .backup_locks
            .lock()
            .expect("backup locks")
            .entry((kind, name.to_string()))
            .or_default()
            .clone()
    }

    /// Backs up an app or a shared volume to the targets named in `only` (every target, if it's empty), and applies
    /// their retention.
    pub async fn backup(&self, kind: Kind, name: &str, only: &[String], full: bool) -> Result<BackupResult> {
        match kind {
            Kind::App if !is_valid_app_name(name) => return Err(AppsError::InvalidName(name.into())),
            Kind::Volume if !kn_bundle::compose::is_valid_object_name(name) => {
                return Err(AppsError::Unsupported(format!("`{name}` isn't a valid volume name")));
            }
            Kind::Server => return Err(AppsError::Unsupported("a node doesn't back up the server".into())),
            _ => {}
        }
        let all = self.targets().await?;
        let mut chosen: Vec<&Target> = Vec::new();
        for t in only {
            chosen.push(
                all.iter()
                    .find(|x| x.name() == t)
                    .ok_or_else(|| AppsError::NotFoundOther(format!("there's no backup target `{t}`")))?,
            );
        }
        if only.is_empty() {
            chosen = all.iter().collect();
        }
        if chosen.iter().any(|t| t.format() == Format::Send) && !kn_btrfs::can_send() {
            return Err(AppsError::Unsupported(
                "backing up to a `send` target needs btrfs send, which needs the node to run as root".into(),
            ));
        }
        // When it was created: retention leaves earlier apps' (or volumes') backups of the same name alone.
        let since = {
            let state = self.inner.state.lock().await;
            let created = match kind {
                Kind::App => state.apps.get(name).map(|a| a.created_at),
                _ => state.shared.get(name).map(|s| s.created_at),
            };
            match created {
                Some(c) => Stamp(c),
                None => {
                    return Err(match kind {
                        Kind::App => AppsError::NotFound(name.into()),
                        _ => AppsError::NotFoundOther(format!("shared volume `{name}` doesn't exist")),
                    });
                }
            }
        };
        let lock = self.backup_lock(kind, name);
        let _held = lock.lock().await;
        let mut result = BackupResult::default();

        // What each target has, and what the node keeps.
        let mut listings: Vec<(&Target, Vec<Listed>)> = Vec::new();
        for t in &chosen {
            match t.list(Some((kind, name))).await {
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
        let base = self.inner.volumes.backups_dir(kind, name);
        let local = local_sets(&base).await?;
        let all_listed: Vec<Listed> = listings.iter().flat_map(|(_, l)| l.clone()).collect();
        let stamp = Target::next_stamp(&all_listed, local.iter().next_back().copied());

        // The snapshot set, at one instant.
        let set = base.join(stamp.to_string());
        let names = match self.snapshot_set(kind, name, &base, stamp).await {
            Ok(n) => n,
            Err(e) => {
                for (t, _) in &listings {
                    result.failed.push(TargetError {
                        target: t.name().into(),
                        error: format!("snapshotting: {e}"),
                    });
                }
                return Ok(result);
            }
        };
        result.notes.push(if names.is_empty() {
            format!("{kind} `{name}` has no volumes, so its backups are empty")
        } else {
            format!("backed up {}", list(&names))
        });

        // Up to each target.
        let notes = Arc::new(Mutex::new(Vec::new()));
        let mut uploaded: Vec<(&Target, Stamp)> = Vec::new();
        for (t, listing) in &listings {
            let (key, producer) = match t.format() {
                Format::Tar => (
                    Key::new(kind, name, stamp, None, Format::Tar),
                    kn_backup::tar_producer(set.clone(), names.clone(), notes.clone()),
                ),
                Format::Send => {
                    let parent = if full {
                        None
                    } else {
                        t.parent_for(listing, &local, stamp)
                    };
                    (
                        Key::new(kind, name, stamp, parent, Format::Send),
                        kn_backup::send_producer(set.clone(), names.clone(), parent.map(|p| base.join(p.to_string()))),
                    )
                }
            };
            tracing::info!(target = t.name(), key = %key, "backing up");
            match t.upload(&key, producer).await {
                Ok(size) => {
                    tracing::info!(target = t.name(), key = %key, size, "backed up");
                    result.made.push(key.info(t.name(), Some(size)));
                    uploaded.push((t, stamp));
                    match t.prune(kind, name, Some(since)).await {
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
                    tracing::warn!(target = t.name(), key = %key, error = %format!("{e:#}"), "backup failed");
                    result.failed.push(TargetError {
                        target: t.name().into(),
                        error: format!("{e:#}"),
                    })
                }
            }
        }
        result.notes.extend(notes.lock().expect("notes").drain(..));

        // Keep only the sets a `send` target could continue a chain from.
        if let Err(e) = self.collect_sets(kind, name, &all, &base).await {
            tracing::warn!(kind = %kind, name, error = %e, "couldn't clean up backup snapshots; they're kept");
        }
        Ok(result)
    }

    /// Makes the snapshot set for a backup in `base/STAMP` (made under its staging name, then moved into place);
    /// returns the names in it.
    async fn snapshot_set(&self, kind: Kind, name: &str, base: &Path, stamp: Stamp) -> Result<Vec<String>> {
        let vols = self.inner.volumes.clone();
        let dest = base.join(stamp.to_string());
        let staging = {
            let (v, b, d) = (vols.clone(), base.to_path_buf(), dest.clone());
            blocking(move || -> Result<PathBuf, VolumeError> {
                std::fs::create_dir_all(&b).map_err(|e| VolumeError {
                    context: format!("creating {}", b.display()),
                    source: e.into(),
                })?;
                v.stage_dir(&d)
            })
            .await??
        };
        let made = match kind {
            Kind::App => {
                self.ask(name, |reply| Op::BackupSnapshot {
                    dest: staging.clone(),
                    reply,
                })
                .await
            }
            _ => self.volume_snapshot(name, &staging).await,
        };
        let (v, s, d) = (vols.clone(), staging.clone(), dest.clone());
        let committed = match made {
            Ok(names) => blocking(move || v.commit(&d).map(|()| names))
                .await?
                .map_err(AppsError::from),
            Err(e) => Err(e),
        };
        if committed.is_err() {
            let _ = blocking(move || vols.remove(&s)).await;
        }
        committed
    }

    /// A read-only snapshot of a shared volume's data as `dest/data`, under the volume's lock (so no load or delete
    /// runs meanwhile).
    async fn volume_snapshot(&self, name: &str, dest: &Path) -> Result<Vec<String>> {
        let lock = self.inner.shared_lock(name);
        let _guard = lock.read().await;
        let id = self
            .inner
            .state
            .lock()
            .await
            .shared
            .get(name)
            .map(|s| s.id)
            .ok_or_else(|| AppsError::NotFoundOther(format!("shared volume `{name}` doesn't exist")))?;
        let (vols, n, d) = (self.inner.volumes.clone(), name.to_string(), dest.to_path_buf());
        blocking(move || -> Result<Vec<String>> {
            let data = vols.shared_data(&n);
            vols.check(&data, Some(id))
                .map_err(|e| AppsError::Mount(format!("shared volume `{n}`: {e}")))?;
            vols.snapshot_read_only(&data, &d.join("data"))?;
            Ok(vec!["data".to_string()])
        })
        .await?
    }

    /// Destroys the snapshot sets in `base` that no `send` target could continue a chain from, and deleted apps'
    /// sets. If any `send` target can't be listed, everything is kept.
    async fn collect_sets(&self, kind: Kind, name: &str, all: &[Target], base: &Path) -> anyhow::Result<()> {
        let local = local_sets(base).await?;
        let mut keep = BTreeSet::new();
        for t in all.iter().filter(|t| t.format() == Format::Send) {
            let listed: BTreeSet<Stamp> = t
                .list(Some((kind, name)))
                .await?
                .into_iter()
                .map(|l| l.key.stamp)
                .collect();
            if let Some(newest) = local.iter().rev().find(|s| listed.contains(s)) {
                keep.insert(*newest);
            }
        }
        let vols = self.inner.volumes.clone();
        let base = base.to_path_buf();
        blocking(move || -> anyhow::Result<()> {
            for s in local.difference(&keep) {
                vols.remove(&base.join(s.to_string()))?;
            }
            // Deleted apps' sets, set aside.
            if let Some(parent) = base.parent()
                && let Ok(entries) = std::fs::read_dir(parent)
            {
                for e in entries.flatten() {
                    if e.file_name().to_string_lossy().starts_with(crate::volumes::GONE_PREFIX)
                        && let Err(err) = vols.remove(&e.path())
                    {
                        tracing::debug!(path = %e.path().display(), error = %err, "couldn't remove old snapshots yet");
                    }
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?
    }

    /// Works out where `--restore KEY` (with `--target`, or not) comes from: the key is checked, and the target is
    /// the one named, or else the first that has the key.
    pub async fn resolve_restore(&self, kind: Kind, key: &str, target: Option<&str>) -> Result<Restore> {
        let k = kn_backup::parse_restore_key(key, kind).map_err(|e| AppsError::Unsupported(format!("{e:#}")))?;
        let all = self.targets().await?;
        let t = kn_backup::find(&all, target, &k)
            .await
            .map_err(|e| AppsError::NotFoundOther(format!("{e:#}")))?;
        Ok(Restore {
            target: t.name().to_string(),
            key: k.to_string(),
        })
    }

    /// The backups in every target (or the one named), of everything or what the query names.
    pub async fn backup_list(&self, q: &ListQuery) -> Result<Vec<BackupInfo>> {
        let all = self.targets().await?;
        list_backups(&all, q).await.map_err(internal)
    }

    /// Restores a backup into a fresh directory `staging/.restore`; returns what it restored, by name.
    async fn restore_set(
        &self,
        restore: &Restore,
        kind: Kind,
        staging: &Path,
        check_name: kn_backup::receive::CheckName,
    ) -> Result<BTreeMap<String, PathBuf>> {
        let all = self.targets().await?;
        let key =
            kn_backup::parse_restore_key(&restore.key, kind).map_err(|e| AppsError::Unsupported(format!("{e:#}")))?;
        let target = all
            .iter()
            .find(|t| t.name() == restore.target)
            .ok_or_else(|| AppsError::NotFoundOther(format!("there's no backup target `{}`", restore.target)))?;
        let dest = staging.join(".restore");
        let vols = self.inner.volumes.clone();
        let d = dest.clone();
        blocking(move || {
            vols.remove(&d)?;
            std::fs::create_dir(&d).map_err(|e| VolumeError {
                context: format!("creating {}", d.display()),
                source: e.into(),
            })
        })
        .await??;
        let vols = self.inner.volumes.clone();
        let to = kn_backup::RestoreTo {
            subvols: Arc::new(Subvols(vols.backend())),
            remove: Arc::new(move |p: &Path| Ok(vols.remove(p)?)),
            check_name,
        };
        tracing::info!(target = target.name(), key = %key, "restoring");
        kn_backup::restore(target, &key, dest, to)
            .await
            .map_err(|e| AppsError::Internal(e.context(format!("restoring `{key}` from target `{}`", target.name()))))
    }

    /// Makes the data of a new shared volume in `staging/data` from a backup.
    pub(super) async fn restore_volume_data(&self, restore: &Restore, staging: &Path) -> Result<u64> {
        let got = self
            .restore_set(restore, Kind::Volume, staging, Arc::new(|n: &str| n == "data"))
            .await?;
        let (vols, s) = (self.inner.volumes.clone(), staging.to_path_buf());
        blocking(move || -> Result<u64> {
            let data = got
                .get("data")
                .ok_or_else(|| AppsError::Unsupported("the backup holds no `data`".into()))?;
            kn_btrfs::rename_noreplace(data, &s.join("data")).map_err(|e| AppsError::Internal(e.into()))?;
            vols.remove(&s.join(".restore"))?;
            vols.check(&s.join("data"), None)
                .map_err(|e| AppsError::Mount(format!("the restored volume: {e}")))
        })
        .await?
    }
}

/// The stamps of the snapshot sets in `base`.
async fn local_sets(base: &Path) -> Result<BTreeSet<Stamp>> {
    let b = base.to_path_buf();
    let names = blocking(move || Volumes::names(&b)).await??;
    Ok(names.iter().filter_map(|n| Stamp::parse(n)).collect())
}

/// Lists backups in targets: every one, or the one `q` names, of everything or what `q` names.
pub async fn list_backups(all: &[Target], q: &ListQuery) -> anyhow::Result<Vec<BackupInfo>> {
    let chosen: Vec<&Target> = match &q.target {
        Some(t) => vec![
            all.iter()
                .find(|x| x.name() == t)
                .ok_or_else(|| anyhow::anyhow!("there's no backup target `{t}`"))?,
        ],
        None => all.iter().collect(),
    };
    let mut out = Vec::new();
    for t in chosen {
        let scope = match (q.kind, &q.name) {
            (Some(Kind::Server), _) => Some((Kind::Server, "")),
            (Some(k), Some(n)) => Some((k, n.as_str())),
            _ => None,
        };
        let infos = t.infos(scope).await?;
        out.extend(
            infos
                .into_iter()
                .filter(|i| q.kind.is_none_or(|k| k == i.kind))
                .filter(|i| q.name.as_ref().is_none_or(|n| *n == i.name)),
        );
    }
    Ok(out)
}

impl App<'_> {
    /// Snapshots all of the app's data read-only into `dest`, at one instant: its volumes, and its snapshots of
    /// shared volumes (as `@NAME`). Returns the names.
    pub(super) async fn backup_snapshot(&self, dest: PathBuf) -> Result<Vec<String>> {
        let r = self.record().await?;
        let vols = &self.inner.volumes;
        let mut items = Vec::new();
        for v in &r.volumes {
            let id = r.volume_ids.get(v).ok_or_else(|| {
                AppsError::Mount(format!(
                    "`{}`'s volume `{v}` has no recorded subvolume; redeploy `{}` first",
                    self.name, self.name
                ))
            })?;
            items.push((v.clone(), vols.app_volume(self.name, v), *id));
        }
        for (n, m) in &r.shared {
            if m.mode == SharedMode::Snapshot
                && let Some(id) = m.snapshot_id
            {
                items.push((format!("@{n}"), vols.snapshot_path(n, self.name), id));
            }
        }
        let names: Vec<String> = items.iter().map(|(n, _, _)| n.clone()).collect();
        if !items.is_empty() {
            self.snapshot_paused(&r, items, dest, true).await?;
        }
        Ok(names)
    }

    /// Restores the first deploy's volumes from a backup into `staging`, and stages its snapshots of the shared
    /// volumes the bundle mounts in `snapshot` mode beside their places (adding them to `restored`). What the
    /// backup has that the bundle doesn't use is dropped, with a note.
    pub(super) async fn restore_volumes(
        &self,
        r: &AppRecord,
        restore: &Restore,
        staging: &Path,
        restored: &mut BTreeSet<String>,
        notes: &mut Vec<String>,
    ) -> Result<BTreeMap<String, u64>> {
        let apps = Apps {
            inner: self.inner.clone(),
        };
        let got = apps
            .restore_set(restore, Kind::App, staging, Arc::new(app_set_name))
            .await?;
        let (vols, app, s, rec) = (
            self.inner.volumes.clone(),
            self.name.to_string(),
            staging.to_path_buf(),
            r.clone(),
        );
        let (ids, used, dropped) = blocking(move || -> Result<_> {
            let (mut ids, mut used, mut dropped) = (BTreeMap::new(), Vec::new(), Vec::new());
            for (n, path) in &got {
                if let Some(shared) = n.strip_prefix('@') {
                    let snapshot = rec.shared.get(shared).is_some_and(|m| m.mode == SharedMode::Snapshot);
                    if snapshot {
                        let place = vols.snapshot_path(shared, &app);
                        if Volumes::exists(&place)? {
                            dropped.push(format!(
                                "its snapshot of shared volume `{shared}` (the app has one already)"
                            ));
                        } else {
                            let staged = Volumes::staging_for(&place);
                            vols.remove(&staged)?;
                            kn_btrfs::rename_noreplace(path, &staged).map_err(|e| AppsError::Internal(e.into()))?;
                            used.push(n.clone());
                            continue;
                        }
                    } else {
                        dropped.push(format!(
                            "its snapshot of shared volume `{shared}` (the bundle doesn't mount it in snapshot mode)"
                        ));
                    }
                } else if rec.volumes.contains(n) {
                    let to = s.join(n);
                    kn_btrfs::rename_noreplace(path, &to).map_err(|e| AppsError::Internal(e.into()))?;
                    let id = vols
                        .check(&to, None)
                        .map_err(|e| AppsError::Mount(format!("restored volume `{n}`: {e}")))?;
                    ids.insert(n.clone(), id);
                    used.push(n.clone());
                    continue;
                } else {
                    dropped.push(format!("volume `{n}` (the bundle doesn't declare it)"));
                }
            }
            vols.remove(&s.join(".restore"))?;
            Ok((ids, used, dropped))
        })
        .await??;
        for n in &used {
            if let Some(shared) = n.strip_prefix('@') {
                restored.insert(shared.to_string());
            }
        }
        notes.push(format!(
            "restored {} from `{}` (target `{}`)",
            if used.is_empty() {
                "nothing".to_string()
            } else {
                list(&used)
            },
            restore.key,
            restore.target
        ));
        for d in dropped {
            notes.push(format!("not restored from the backup: {d}"));
        }
        Ok(ids)
    }

    /// Removes the shared-volume snapshots a failed first deploy's restore may have staged (all of the app's: the
    /// staging names are its own).
    pub(super) async fn unstage_restored_snapshots(&self, r: &AppRecord) {
        let (vols, app) = (self.inner.volumes.clone(), self.name.to_string());
        let names: Vec<String> = r.shared.keys().cloned().collect();
        let _ = blocking(move || {
            for n in names {
                let _ = vols.remove(&Volumes::staging_for(&vols.snapshot_path(&n, &app)));
            }
        })
        .await;
    }
}

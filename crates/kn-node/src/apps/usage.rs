//! Measuring the space the node's subvolumes take, now and then (`--usage-interval`), for the reports and the UI.
//!
//! Each app volume, shared volume and snapshot of one is measured on its own ([`kn_btrfs::usage`]): what its files
//! take, and what only it holds. Subvolumes that can share data are measured together too, counting what they share
//! once: an app, the apps made `--from` it, the shared volumes any of them mount, and those volumes' snapshots. Data
//! is only ever shared through those snapshots (and the backups' read-only sets, measured on their own), so each such
//! group is within one nest, and a nest's space is the sum of its groups'.
//!
//! The walk reads only metadata, but on a large volume it still takes a while, so it runs on a blocking thread
//! without holding any lock: the state is read first, and a subvolume that goes away meanwhile is skipped.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kn_proto::agent::{Usage, UsageGroup};
use kn_proto::api::Space;
use kn_proto::backup::Kind;
use tokio_util::sync::CancellationToken;

use super::{Apps, now};
use crate::volumes::Volumes;

/// The last measurement.
#[derive(Debug, Clone, Default)]
pub struct Measured {
    pub usage: Usage,
    /// Each app's named volumes.
    pub apps: HashMap<String, BTreeMap<String, Space>>,
    /// Each shared volume's data, and each app's snapshot of it.
    pub shared: HashMap<String, (Space, BTreeMap<String, Space>)>,
}

/// What a measurement covers, read from the state.
struct Plan {
    /// App → its named volumes, and the app it was made `--from`.
    apps: BTreeMap<String, (Vec<String>, Option<String>)>,
    /// App → the shared volumes it mounts.
    mounts: BTreeMap<String, Vec<String>>,
    shared: Vec<String>,
}

impl Apps {
    /// The last measurement, if there's been one.
    pub fn usage(&self) -> Option<Arc<Measured>> {
        self.inner.usage.lock().expect("usage lock").clone()
    }

    /// Measures every subvolume now, and keeps the result for the reports.
    pub async fn measure_usage(&self) -> Arc<Measured> {
        let plan = {
            let state = self.inner.state.lock().await;
            Plan {
                apps: state
                    .apps
                    .values()
                    .map(|a| {
                        let vols = a
                            .volumes
                            .iter()
                            .filter(|v| a.volume_ids.contains_key(*v))
                            .cloned()
                            .collect();
                        (a.name.clone(), (vols, a.from.clone()))
                    })
                    .collect(),
                mounts: state
                    .apps
                    .values()
                    .map(|a| (a.name.clone(), a.shared.keys().cloned().collect()))
                    .collect(),
                shared: state.shared.keys().cloned().collect(),
            }
        };
        let volumes = self.inner.volumes.clone();
        let measured = tokio::task::spawn_blocking(move || measure(&volumes, plan))
            .await
            .unwrap_or_default();
        let measured = Arc::new(measured);
        *self.inner.usage.lock().expect("usage lock") = Some(measured.clone());
        measured
    }

    /// Measures every `interval` (the first time soon after the node starts), and again soon after the state
    /// changes (a deploy, a delete, a volume made or loaded), but never more often than every [`MIN_GAP`], until
    /// `stop`.
    pub fn spawn_usage(&self, interval: Duration, stop: CancellationToken) {
        if interval.is_zero() {
            return;
        }
        let apps = self.clone();
        tokio::spawn(async move {
            let mut wait = Duration::from_secs(5).min(interval);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = apps.inner.usage_changed.notified() => {
                        // Let a burst of changes (a deploy and its previews) settle first.
                        tokio::time::sleep(SETTLE).await;
                    }
                    _ = stop.cancelled() => return,
                }
                let started = std::time::Instant::now();
                let m = apps.measure_usage().await;
                tracing::debug!(
                    took = ?started.elapsed(),
                    groups = m.usage.groups.len(),
                    "measured the volumes' space"
                );
                tokio::select! {
                    _ = tokio::time::sleep(MIN_GAP.min(interval)) => {}
                    _ = stop.cancelled() => return,
                }
                wait = interval.saturating_sub(MIN_GAP);
            }
        });
    }
}

/// How long after a change the space is measured.
const SETTLE: Duration = Duration::from_secs(5);
/// The least time between two measurements.
const MIN_GAP: Duration = Duration::from_secs(60);

/// One subvolume's measurement, with its shared extents.
fn one(path: PathBuf) -> Option<kn_btrfs::TreeUsage> {
    match kn_btrfs::usage(&path) {
        Ok(u) => {
            if u.skipped > 0 {
                tracing::debug!(path = %path.display(), skipped = u.skipped, "couldn't measure some files");
            }
            Some(u)
        }
        Err(kn_btrfs::Error::NotFound(_)) => None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "couldn't measure a volume");
            None
        }
    }
}

fn space(u: &kn_btrfs::TreeUsage) -> Space {
    Space {
        bytes: u.bytes,
        exclusive_bytes: u.exclusive_bytes,
    }
}

/// Union-find over apps and shared volumes.
struct Groups {
    parent: HashMap<String, String>,
}

impl Groups {
    fn find(&mut self, x: &str) -> String {
        let p = self.parent.get(x).cloned().unwrap_or_else(|| x.to_string());
        if p == x {
            self.parent.insert(x.to_string(), p.clone());
            return p;
        }
        let root = self.find(&p);
        self.parent.insert(x.to_string(), root.clone());
        root
    }

    fn join(&mut self, a: &str, b: &str) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent.insert(ra, rb);
        }
    }
}

/// Apps and volumes get keys of their own, so an app and a volume of the same name stay apart.
fn app_key(name: &str) -> String {
    format!("a:{name}")
}

fn volume_key(name: &str) -> String {
    format!("v:{name}")
}

fn measure(volumes: &Volumes, plan: Plan) -> Measured {
    let mut groups = Groups { parent: HashMap::new() };
    for (app, (_, from)) in &plan.apps {
        groups.find(&app_key(app));
        if let Some(f) = from.as_ref().filter(|f| plan.apps.contains_key(*f)) {
            groups.join(&app_key(app), &app_key(f));
        }
    }
    for name in &plan.shared {
        groups.find(&volume_key(name));
    }
    for (app, mounted) in &plan.mounts {
        for v in mounted.iter().filter(|v| plan.shared.contains(v)) {
            groups.join(&app_key(app), &volume_key(v));
        }
    }

    #[derive(Default)]
    struct Acc {
        apps: BTreeSet<String>,
        volumes: BTreeSet<String>,
        exclusive: u64,
        shared: Vec<(u64, u64)>,
    }
    let mut accs: BTreeMap<String, Acc> = BTreeMap::new();
    let mut out = Measured::default();
    let add = |acc: &mut Acc, u: kn_btrfs::TreeUsage| {
        acc.exclusive += u.exclusive_bytes;
        acc.shared.extend(u.shared);
    };
    for (app, (vols, _)) in &plan.apps {
        let root = groups.find(&app_key(app));
        let acc = accs.entry(root).or_default();
        acc.apps.insert(app.clone());
        let mut spaces = BTreeMap::new();
        for v in vols {
            if let Some(u) = one(volumes.app_volume(app, v)) {
                spaces.insert(v.clone(), space(&u));
                add(acc, u);
            }
        }
        out.apps.insert(app.clone(), spaces);
    }
    for name in &plan.shared {
        let root = groups.find(&volume_key(name));
        let acc = accs.entry(root).or_default();
        acc.volumes.insert(name.clone());
        let data = one(volumes.shared_data(name));
        let data_space = data.as_ref().map(space).unwrap_or_default();
        if let Some(u) = data {
            add(acc, u);
        }
        let mut snaps = BTreeMap::new();
        for app in Volumes::names(&volumes.snapshots_dir(name)).unwrap_or_default() {
            if let Some(u) = one(volumes.snapshot_path(name, &app)) {
                snaps.insert(app, space(&u));
                add(acc, u);
            }
        }
        out.shared.insert(name.clone(), (data_space, snaps));
    }
    let mut data_groups = Vec::new();
    for (_, mut acc) in accs {
        let bytes = acc.exclusive + kn_btrfs::union_bytes(&mut acc.shared);
        data_groups.push(UsageGroup {
            apps: acc.apps.into_iter().collect(),
            volumes: acc.volumes.into_iter().collect(),
            bytes,
        });
    }
    out.usage = Usage {
        measured_at: now(),
        groups: data_groups,
        backups: backups(volumes),
    };
    out
}

/// The backups' read-only snapshot sets: `backups/{apps,volumes}/NAME/STAMP/VOL`, each a subvolume.
fn backups(volumes: &Volumes) -> Space {
    let mut total = Space::default();
    for kind in [Kind::App, Kind::Volume] {
        let Some(dir) = volumes.backups_dir(kind, "x").parent().map(|p| p.to_path_buf()) else {
            continue;
        };
        for name in Volumes::names(&dir).unwrap_or_default() {
            let sets = dir.join(&name);
            for stamp in Volumes::names(&sets).unwrap_or_default() {
                for vol in Volumes::names(&sets.join(&stamp)).unwrap_or_default() {
                    if let Some(u) = one(sets.join(&stamp).join(vol)) {
                        total.bytes += u.bytes;
                        total.exclusive_bytes += u.exclusive_bytes;
                    }
                }
            }
        }
    }
    total
}

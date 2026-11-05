//! Shared volumes: `volume create|list|load|delete|delete-snapshot`.
//!
//! Each command holds the volume's lock exclusively, so it never overlaps a deploy that mounts the volume (which
//! holds it shared) or another command on it. The rules:
//!
//! - `load` replaces the volume's contents, and is refused while an app mounts it `ro` or `rw`, since those apps
//!   would see it change under them. Apps' snapshots keep the contents they were made from.
//! - `delete` is refused while any app mounts the volume, or any snapshot of it exists (even one kept with
//!   `--keep-data`).
//! - `delete-snapshot` is refused while the snapshot's app is deployed and mounts it.

use std::path::PathBuf;

use kn_proto::api::{VolumeInfo, VolumeMount};

use super::*;
use crate::volumes::LOAD_NAME;

/// Where `volume load` reads the new contents from.
pub enum LoadSource {
    /// An uploaded ZIP.
    Zip(PathBuf),
    /// A directory on this machine.
    Dir(PathBuf),
}

fn check_name(name: &str) -> Result<()> {
    if kn_bundle::compose::is_valid_object_name(name) {
        Ok(())
    } else {
        Err(AppsError::Unsupported(format!(
            "`{name}` isn't a valid volume name: use letters, digits, `_`, `.` and `-`, starting with a letter or \
             digit, at most 63 characters"
        )))
    }
}

fn missing(name: &str) -> AppsError {
    AppsError::NotFoundOther(format!("shared volume `{name}` doesn't exist"))
}

impl Apps {
    /// Creates an empty shared volume.
    pub async fn volume_create(&self, name: &str) -> Result<VolumeInfo> {
        check_name(name)?;
        let lock = self.inner.shared_lock(name);
        let _guard = lock.write().await;
        if self.inner.state.lock().await.shared.contains_key(name) {
            return Err(AppsError::Conflict(format!("shared volume `{name}` already exists")));
        }
        let (vols, n) = (self.inner.volumes.clone(), name.to_string());
        let id = blocking(move || -> Result<u64, VolumeError> {
            // `data` and `snaps` are made in a staging directory, then moved into place together.
            let dest = vols.shared_dir(&n);
            let staging = vols.stage_dir(&dest)?;
            let made = std::fs::create_dir(staging.join("snaps"))
                .map_err(|e| VolumeError {
                    context: format!("creating {}", staging.display()),
                    source: e.into(),
                })
                .and_then(|()| vols.create(&staging.join("data")))
                .and_then(|id| vols.commit(&dest).map(|()| id));
            if made.is_err() {
                let _ = vols.remove(&staging);
            }
            made
        })
        .await??;
        {
            let mut state = self.inner.state.lock().await;
            state.shared.insert(
                name.to_string(),
                SharedRecord {
                    name: name.to_string(),
                    id,
                    loading: None,
                    created_at: now(),
                    loaded_at: None,
                },
            );
            self.inner.persist(&state).await?;
        }
        tracing::info!(volume = name, id, "created a shared volume");
        self.volume_info(name).await
    }

    /// Every shared volume.
    pub async fn volume_list(&self) -> Result<Vec<VolumeInfo>> {
        let names: Vec<String> = self.inner.state.lock().await.shared.keys().cloned().collect();
        let mut out = Vec::new();
        for n in names {
            match self.volume_info(&n).await {
                Ok(info) => out.push(info),
                // Deleted meanwhile.
                Err(AppsError::NotFoundOther(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// One shared volume: who mounts it, its snapshots, and whether its data is in place.
    pub async fn volume_info(&self, name: &str) -> Result<VolumeInfo> {
        check_name(name)?;
        let (rec, mounts) = {
            let state = self.inner.state.lock().await;
            let rec = state.shared.get(name).cloned().ok_or_else(|| missing(name))?;
            let mounts: Vec<VolumeMount> = state
                .apps
                .values()
                .filter_map(|a| {
                    a.shared.get(name).map(|m| VolumeMount {
                        app: a.name.clone(),
                        mode: m.mode,
                    })
                })
                .collect();
            (rec, mounts)
        };
        let (vols, n, id) = (self.inner.volumes.clone(), name.to_string(), rec.id);
        let (snapshots, problem) = blocking(move || -> Result<_> {
            let snapshots = Volumes::names(&vols.snapshots_dir(&n))?;
            let problem = vols.check(&vols.shared_data(&n), Some(id)).err().map(|e| e.to_string());
            Ok((snapshots, problem))
        })
        .await??;
        Ok(VolumeInfo {
            name: rec.name,
            created_at: rfc3339(rec.created_at),
            loaded_at: rec.loaded_at.map(rfc3339),
            mounts,
            snapshots,
            problem,
        })
    }

    /// Replaces a shared volume's contents with a bundle-like tree of plain files and directories.
    ///
    /// The new contents are written to a fresh subvolume beside `data`, synced, and swapped in atomically, so the
    /// volume is always either all old or all new, even across a crash.
    pub async fn volume_load(&self, name: &str, source: LoadSource) -> Result<VolumeInfo> {
        check_name(name)?;
        let lock = self.inner.shared_lock(name);
        let _guard = lock.write().await;
        let old = {
            let state = self.inner.state.lock().await;
            let rec = state.shared.get(name).ok_or_else(|| missing(name))?;
            let users: Vec<String> = state
                .apps
                .values()
                .filter(|a| a.shared.get(name).is_some_and(|m| m.mode != SharedMode::Snapshot))
                .map(|a| a.name.clone())
                .collect();
            if !users.is_empty() {
                return Err(AppsError::Conflict(format!(
                    "can't load shared volume `{name}` while app {} mounts it (`ro` or `rw`)",
                    list(&users)
                )));
            }
            rec.id
        };
        let (vols, n, limits) = (
            self.inner.volumes.clone(),
            name.to_string(),
            self.inner.cfg.load_limits.clone(),
        );
        let new = blocking(move || -> Result<u64> {
            let data = vols.shared_data(&n);
            vols.check(&data, Some(old))
                .map_err(|e| AppsError::Mount(format!("shared volume `{n}`: {e}")))?;
            let load = vols.shared_dir(&n).join(LOAD_NAME);
            vols.remove(&load)?;
            let id = vols.create(&load)?;
            let filled = match source {
                LoadSource::Zip(zip) => std::fs::File::open(&zip)
                    .map_err(|e| BundleError::Io("the upload".into(), e))
                    .and_then(|f| kn_bundle::zip::extract(std::io::BufReader::new(f), &load, &limits)),
                LoadSource::Dir(src) => kn_bundle::dir::copy(&src, &load, &limits),
            }
            .map_err(AppsError::Bundle)
            .and_then(|_| {
                kn_btrfs::sync_filesystem(&load)
                    .map_err(|e| AppsError::Internal(anyhow::anyhow!("syncing the new contents: {e}")))
            });
            if let Err(e) = filled {
                let _ = vols.remove(&load);
                return Err(e);
            }
            Ok(id)
        })
        .await??;

        // Record the new subvolume before the swap, so a restart can tell whether it happened.
        self.set_loading(name, Some(new), None).await?;
        let (vols, n) = (self.inner.volumes.clone(), name.to_string());
        let swapped = blocking(move || {
            let load = vols.shared_dir(&n).join(LOAD_NAME);
            let r = vols.exchange(&load, &vols.shared_data(&n));
            if r.is_err() {
                let _ = vols.remove(&load);
            }
            r
        })
        .await?;
        if let Err(e) = swapped {
            self.set_loading(name, None, None).await?;
            return Err(e.into());
        }
        self.set_loading(name, None, Some(new)).await?;
        tracing::info!(volume = name, old, new, "loaded a shared volume");
        // The old contents are now at the load name; a failure here is swept when the node next starts.
        let (vols, n) = (self.inner.volumes.clone(), name.to_string());
        if let Err(e) = blocking(move || vols.remove(&vols.shared_dir(&n).join(LOAD_NAME))).await? {
            tracing::warn!(volume = name, error = %e, "couldn't remove the old contents");
        }
        self.volume_info(name).await
    }

    /// Records a load in progress (`loading`), or its result (`done`: the new data's ID).
    async fn set_loading(&self, name: &str, loading: Option<u64>, done: Option<u64>) -> Result<()> {
        let mut state = self.inner.state.lock().await;
        let rec = state.shared.get_mut(name).ok_or_else(|| missing(name))?;
        rec.loading = loading;
        if let Some(id) = done {
            rec.id = id;
            rec.loaded_at = Some(now());
        }
        self.inner.persist(&state).await
    }

    /// Deletes a shared volume that no app mounts and that has no snapshots.
    pub async fn volume_delete(&self, name: &str) -> Result<()> {
        check_name(name)?;
        let lock = self.inner.shared_lock(name);
        let _guard = lock.write().await;
        {
            let state = self.inner.state.lock().await;
            if !state.shared.contains_key(name) {
                return Err(missing(name));
            }
            let users: Vec<String> = state
                .apps
                .values()
                .filter(|a| a.shared.contains_key(name))
                .map(|a| a.name.clone())
                .collect();
            if !users.is_empty() {
                return Err(AppsError::Conflict(format!(
                    "can't delete shared volume `{name}` while app {} mounts it",
                    list(&users)
                )));
            }
        }
        let (vols, n) = (self.inner.volumes.clone(), name.to_string());
        blocking(move || -> Result<()> {
            let snapshots = Volumes::names(&vols.snapshots_dir(&n))?;
            if !snapshots.is_empty() {
                return Err(AppsError::Conflict(format!(
                    "can't delete shared volume `{n}` while snapshots of it exist, for app {}; delete them first \
                     with `kiwi-nest volume delete-snapshot`",
                    list(&snapshots)
                )));
            }
            vols.remove(&vols.shared_data(&n))?;
            vols.remove(&vols.shared_dir(&n))?;
            Ok(())
        })
        .await??;
        let mut state = self.inner.state.lock().await;
        state.shared.remove(name);
        self.inner.persist(&state).await?;
        tracing::info!(volume = name, "deleted a shared volume");
        Ok(())
    }

    /// Deletes an app's snapshot of a shared volume, unless the app is deployed and mounts it.
    pub async fn volume_delete_snapshot(&self, name: &str, app: &str) -> Result<()> {
        check_name(name)?;
        if !is_valid_app_name(app) {
            return Err(AppsError::InvalidName(app.to_string()));
        }
        let lock = self.inner.shared_lock(name);
        let _guard = lock.write().await;
        {
            let state = self.inner.state.lock().await;
            if !state.shared.contains_key(name) {
                return Err(missing(name));
            }
            if state
                .apps
                .get(app)
                .is_some_and(|a| a.shared.get(name).is_some_and(|m| m.mode == SharedMode::Snapshot))
            {
                return Err(AppsError::Conflict(format!(
                    "can't delete app `{app}`'s snapshot of `{name}` while the app is deployed and mounts it; \
                     deleting the app deletes its snapshot too"
                )));
            }
        }
        let (vols, n, a) = (self.inner.volumes.clone(), name.to_string(), app.to_string());
        blocking(move || -> Result<()> {
            let path = vols.snapshot_path(&n, &a);
            if !Volumes::exists(&path)? {
                return Err(AppsError::NotFoundOther(format!(
                    "app `{a}` has no snapshot of shared volume `{n}`"
                )));
            }
            vols.check(&path, None)
                .map_err(|e| AppsError::Mount(format!("app `{a}`'s snapshot of `{n}`: {e}")))?;
            vols.remove(&path)?;
            Ok(())
        })
        .await??;
        tracing::info!(volume = name, app, "deleted a snapshot");
        Ok(())
    }
}

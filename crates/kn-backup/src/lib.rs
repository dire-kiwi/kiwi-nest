//! kiwi-nest's backups: read-only snapshots, as `btrfs send` streams or tar archives, through zstd, to S3 (or R2,
//! MinIO, …), a directory over SSH, or a directory on the machine; and restores from them.
//!
//! - [`key`]: what a backup's key says (what it's of, when, its format, the backup it follows), chains, choosing a
//!   parent, and retention.
//! - [`target`]: a configured target, and uploads and downloads through zstd. [`store`] is what a target keeps
//!   backups in ([`s3`], [`ssh`], or a directory), and never leaves a failed upload looking complete.
//! - [`sendstream`] and [`receive`]: the `btrfs send` format, and receiving it as file operations ([`fsops`])
//!   confined to a fresh subvolume. Sending is the kernel's, through [`kn_btrfs::send`].
//! - [`tarball`]: tar archives of snapshot sets, and extracting them.
//!
//! A backup of an app or a volume is one object holding every volume of its snapshot set: for `send`, one stream
//! per volume, one after another (as `btrfs receive` reads them too); for `tar`, one top-level directory per volume.

#[cfg(test)]
mod btrfs_tests;
pub mod fsops;
pub mod key;
pub mod receive;
pub mod s3;
pub mod sendstream;
pub mod ssh;
pub mod store;
pub mod tarball;
pub mod target;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, bail};
pub use key::{Key, Stamp};
use kn_proto::backup::{Format, Kind};
pub use receive::Subvolumes;
pub use target::{Listed, Producer, Target};

/// A producer for a `send` backup of the read-only snapshots `dir/NAME`, each against `parent/NAME` where the
/// parent has it (a volume new since the parent is sent whole).
pub fn send_producer(dir: PathBuf, names: Vec<String>, parent: Option<PathBuf>) -> Producer {
    Box::new(move |out| {
        for n in &names {
            let p = parent.as_ref().map(|p| p.join(n)).filter(|p| p.exists());
            kn_btrfs::send(&dir.join(n), p.as_deref(), out).with_context(|| format!("sending volume `{n}`"))?;
        }
        Ok(())
    })
}

/// A producer for a `tar` backup of the snapshots `dir/NAME`, each as the archive's directory `NAME`. Notes (such
/// as skipped sockets) go to `notes`.
pub fn tar_producer(dir: PathBuf, names: Vec<String>, notes: Arc<std::sync::Mutex<Vec<String>>>) -> Producer {
    Box::new(move |out| {
        let mut w = tarball::TarWriter::new(out);
        for n in &names {
            w.add_tree(n, &dir.join(n))
                .with_context(|| format!("archiving volume `{n}`"))?;
        }
        let made = std::mem::take(&mut w.notes);
        w.finish()?;
        notes.lock().expect("notes").extend(made);
        Ok(())
    })
}

/// The backups that restoring `key` from `target` needs, full first; fails if the key isn't there, or its chain
/// is broken.
pub async fn chain_for(target: &Target, key: &Key) -> anyhow::Result<Vec<Key>> {
    let listing: Vec<Key> = target
        .list(Some((key.kind, &key.name)))
        .await?
        .into_iter()
        .map(|l| l.key)
        .collect();
    if !listing.contains(key) {
        bail!("there's no backup `{key}` in target {}", target.describe());
    }
    key::chain(&listing, key).map_err(anyhow::Error::msg)
}

/// How a restore makes and removes subvolumes, and which volume names it may restore.
#[derive(Clone)]
pub struct RestoreTo {
    pub subvols: Arc<dyn Subvolumes>,
    pub remove: receive::Remove,
    pub check_name: receive::CheckName,
}

/// Restores `key` (and the chain it needs) from `target` into `dest`, an existing, empty directory; returns the
/// restored subvolumes, `dest/NAME`, by name. On failure, `dest` may hold leftovers for the caller to remove.
pub async fn restore(
    target: &Target,
    key: &Key,
    dest: PathBuf,
    to: RestoreTo,
) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    if key.kind == Kind::Server {
        bail!("`{key}` is a backup of the server; restore it with `kiwi-nest server restore`");
    }
    let chain = chain_for(target, key).await?;
    match key.format {
        Format::Tar => {
            let (subvols, check) = (to.subvols.clone(), to.check_name.clone());
            target
                .download(key, move |r| tarball::extract(r, &dest, &*subvols, &*check))
                .await
        }
        Format::Send => {
            let mut rc = receive::Chain::new(dest, to.subvols.clone(), to.remove.clone(), to.check_name.clone());
            for k in &chain {
                tracing::info!(target = target.name(), key = %k, "receiving");
                rc = target
                    .download(k, move |r| {
                        rc.step(r)?;
                        Ok(rc)
                    })
                    .await
                    .with_context(|| format!("receiving {k}"))?;
            }
            tokio::task::spawn_blocking(move || rc.finish()).await?
        }
    }
}

/// Picks the target that has `key`: the one named, or else the first that has it.
pub async fn find<'a>(targets: &'a [Target], name: Option<&str>, key: &Key) -> anyhow::Result<&'a Target> {
    if let Some(n) = name {
        return targets
            .iter()
            .find(|t| t.name() == n)
            .with_context(|| format!("there's no backup target `{n}`"));
    }
    if targets.is_empty() {
        bail!("no backup targets are configured (--backup-targets)");
    }
    let mut errors = Vec::new();
    for t in targets {
        match t.list(Some((key.kind, &key.name))).await {
            Ok(l) if l.iter().any(|l| &l.key == key) => return Ok(t),
            Ok(_) => {}
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    let mut msg = format!("no backup target has `{key}`");
    if !errors.is_empty() {
        msg.push_str(&format!(" (and {})", errors.join("; ")));
    }
    bail!(msg)
}

/// Parses a key given to `--restore`, checking it's a backup of what's being restored.
pub fn parse_restore_key(key: &str, kind: Kind) -> anyhow::Result<Key> {
    let k = Key::parse(key).with_context(|| {
        format!(
            "`{key}` isn't a backup key: they look like apps/NAME/20261106T030000Z.full.send.zst or \
             volumes/NAME/20261106T030000Z.tar.zst (see `kiwi-nest backup list`)"
        )
    })?;
    if k.kind != kind {
        bail!("`{key}` is a backup of {} {}, not of {}", k.kind, k.name, kind);
    }
    Ok(k)
}

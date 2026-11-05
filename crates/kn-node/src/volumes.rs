//! The node's volumes: every byte of app data is a btrfs subvolume.
//!
//! ```text
//! VOLUMES/apps/APP/VOL/            subvolume: APP's named volume VOL
//! VOLUMES/shared/NAME/data/        subvolume: shared volume NAME
//! VOLUMES/shared/NAME/snaps/APP/   subvolume: APP's writable snapshot of NAME
//! ```
//!
//! This module is the layout and the disk operations; [`crate::apps`] decides when to use them and records what they
//! made.
//!
//! - **Nothing half-made is ever in place.** Each new subvolume, or set of them, is made under a staging name
//!   beside its final place (`.new-…`, which no app or volume name can be), then renamed into place without replacing
//!   anything. A crash leaves at most a staging leftover, which is destroyed before that name is staged again and when
//!   the node starts.
//! - **[`Volumes::check`]** is the check before every start: the path is reached without symlinks, and is a subvolume
//!   of this node's btrfs filesystem, with the ID kiwi-nest recorded for it.
//! - **The backend** does the subvolume operations: [`Btrfs`] on a node, and a fake one of plain directories in unit
//!   tests.

use std::path::{Path, PathBuf};
use std::sync::Arc;

pub use kn_btrfs::{Error as BtrfsError, FsId, Subvolume};

/// The prefix of staging names. App and volume names start with a letter or digit, so they never collide.
pub const STAGING_PREFIX: &str = ".new-";

/// Where `volume load` builds the new contents of a shared volume, beside `data`.
pub const LOAD_NAME: &str = ".load";

/// The subvolume operations, on paths that must be absolute and are opened without following symlinks.
pub trait Backend: Send + Sync + 'static {
    /// Checks the volumes directory when the node starts, and returns its filesystem.
    fn filesystem(&self, root: &Path) -> Result<FsId, BtrfsError>;
    /// Creates an empty subvolume at `path`, whose parent exists.
    fn create(&self, path: &Path) -> Result<(), BtrfsError>;
    /// Snapshots the subvolume `source` as a writable `dest`, whose parent exists.
    fn snapshot(&self, source: &Path, dest: &Path) -> Result<(), BtrfsError>;
    /// Destroys the subvolume at `path`.
    fn destroy(&self, path: &Path) -> Result<(), BtrfsError>;
    /// Checks `path` is a subvolume, reached without symlinks, and says which one.
    fn inspect(&self, path: &Path) -> Result<Subvolume, BtrfsError>;
}

/// The real backend: btrfs ioctls through [`kn_btrfs`].
pub struct Btrfs;

impl Backend for Btrfs {
    fn filesystem(&self, root: &Path) -> Result<FsId, BtrfsError> {
        kn_btrfs::filesystem(root)
    }

    fn create(&self, path: &Path) -> Result<(), BtrfsError> {
        kn_btrfs::create(path)
    }

    fn snapshot(&self, source: &Path, dest: &Path) -> Result<(), BtrfsError> {
        kn_btrfs::snapshot(source, dest, false)
    }

    fn destroy(&self, path: &Path) -> Result<(), BtrfsError> {
        kn_btrfs::destroy(path)
    }

    fn inspect(&self, path: &Path) -> Result<Subvolume, BtrfsError> {
        kn_btrfs::inspect(path)
    }
}

/// Why a mount was refused.
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("{0}")]
    Btrfs(#[from] BtrfsError),
    #[error("`{0}` is on another filesystem than the node's volumes")]
    OtherFilesystem(PathBuf),
    #[error("`{path}` is subvolume {found}, not subvolume {expected} as recorded")]
    WrongSubvolume { path: PathBuf, expected: u64, found: u64 },
    #[error("`{0}` is read-only")]
    ReadOnly(PathBuf),
}

/// An error from a volume operation, with what it was doing.
#[derive(Debug, thiserror::Error)]
#[error("{context}: {source}")]
pub struct VolumeError {
    pub context: String,
    #[source]
    pub source: anyhow::Error,
}

fn ctx<E: Into<anyhow::Error>>(context: impl std::fmt::Display) -> impl FnOnce(E) -> VolumeError {
    move |e| VolumeError {
        context: context.to_string(),
        source: e.into(),
    }
}

pub type Result<T, E = VolumeError> = std::result::Result<T, E>;

/// The volumes directory of a node.
#[derive(Clone)]
pub struct Volumes {
    root: PathBuf,
    fs: FsId,
    backend: Arc<dyn Backend>,
}

impl std::fmt::Debug for Volumes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Volumes")
            .field("root", &self.root)
            .field("fs", &self.fs.to_string())
            .finish()
    }
}

fn exists(p: &Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

impl Volumes {
    /// Opens the volumes directory, creating it and its `apps` and `shared` directories if needed. Refuses one that
    /// isn't on btrfs (for the real backend) or is reached through a symlink.
    pub fn open(root: PathBuf, backend: Arc<dyn Backend>) -> anyhow::Result<Volumes> {
        std::fs::create_dir_all(&root).map_err(|e| anyhow::anyhow!("creating {}: {e}", root.display()))?;
        let fs = backend.filesystem(&root).map_err(|e| match e {
            BtrfsError::NotBtrfs(_) => anyhow::anyhow!(
                "the volumes directory {} isn't on btrfs: every app volume is a btrfs subvolume, so give \
                 --volumes-dir a directory on a btrfs filesystem (a loop-mounted file works)",
                root.display()
            ),
            e => anyhow::anyhow!("the volumes directory: {e}"),
        })?;
        for d in ["apps", "shared"] {
            let d = root.join(d);
            std::fs::create_dir_all(&d).map_err(|e| anyhow::anyhow!("creating {}: {e}", d.display()))?;
            kn_btrfs::open_dir(&d).map_err(|e| anyhow::anyhow!("the volumes directory: {e}"))?;
        }
        Ok(Volumes { root, fs, backend })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn filesystem(&self) -> FsId {
        self.fs
    }

    /// `VOLUMES/apps/APP`: a plain directory holding APP's subvolumes.
    pub fn app_dir(&self, app: &str) -> PathBuf {
        self.root.join("apps").join(app)
    }

    /// `VOLUMES/apps/APP/VOL`.
    pub fn app_volume(&self, app: &str, volume: &str) -> PathBuf {
        self.app_dir(app).join(volume)
    }

    /// `VOLUMES/shared/NAME`: a plain directory holding `data` and `snaps`.
    pub fn shared_dir(&self, name: &str) -> PathBuf {
        self.root.join("shared").join(name)
    }

    /// `VOLUMES/shared/NAME/data`.
    pub fn shared_data(&self, name: &str) -> PathBuf {
        self.shared_dir(name).join("data")
    }

    /// `VOLUMES/shared/NAME/snaps`.
    pub fn snapshots_dir(&self, name: &str) -> PathBuf {
        self.shared_dir(name).join("snaps")
    }

    /// `VOLUMES/shared/NAME/snaps/APP`.
    pub fn snapshot_path(&self, name: &str, app: &str) -> PathBuf {
        self.snapshots_dir(name).join(app)
    }

    /// The staging name beside `path`.
    pub fn staging_for(path: &Path) -> PathBuf {
        let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        path.with_file_name(format!("{STAGING_PREFIX}{name}"))
    }

    /// Whether anything (even a broken symlink) is at `path`.
    pub fn exists(path: &Path) -> Result<bool> {
        exists(path).map_err(ctx(format!("checking {}", path.display())))
    }

    /// The check before every start: `path` is reached without symlinks, and is a writable subvolume of this node's
    /// filesystem, with the ID recorded for it if there is one. Returns its ID.
    pub fn check(&self, path: &Path, expected: Option<u64>) -> Result<u64, CheckError> {
        let sub = self.backend.inspect(path)?;
        if sub.fs != self.fs {
            return Err(CheckError::OtherFilesystem(path.into()));
        }
        if let Some(expected) = expected
            && sub.id != expected
        {
            return Err(CheckError::WrongSubvolume {
                path: path.into(),
                expected,
                found: sub.id,
            });
        }
        if sub.read_only {
            return Err(CheckError::ReadOnly(path.into()));
        }
        Ok(sub.id)
    }

    /// Whether `path` is a subvolume of this filesystem (without following symlinks).
    fn is_subvolume(&self, path: &Path) -> bool {
        self.backend.inspect(path).is_ok_and(|s| s.fs == self.fs)
    }

    /// Removes whatever is at `path`: subvolumes are destroyed, and directories are emptied the same way. Nothing
    /// there is fine.
    pub fn remove(&self, path: &Path) -> Result<()> {
        let what = || format!("removing {}", path.display());
        // Never through a symlink: that could reach far outside the volumes.
        let parent = path.parent().ok_or_else(|| ctx(what())(anyhow::anyhow!("no parent")))?;
        kn_btrfs::open_dir(parent).map_err(ctx(what()))?;
        let meta = match std::fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(ctx(what())(e)),
        };
        if !meta.is_dir() {
            return std::fs::remove_file(path).map_err(ctx(what()));
        }
        if self.is_subvolume(path) {
            return self.backend.destroy(path).map_err(ctx(what()));
        }
        for entry in std::fs::read_dir(path).map_err(ctx(what()))? {
            let entry = entry.map_err(ctx(what()))?;
            self.remove(&entry.path())?;
        }
        std::fs::remove_dir(path).map_err(ctx(what()))
    }

    /// Makes a fresh, empty staging directory for `dest` (removing any leftover), for a set of subvolumes that
    /// [`Volumes::commit`] then moves into place together.
    pub fn stage_dir(&self, dest: &Path) -> Result<PathBuf> {
        let staging = Self::staging_for(dest);
        self.remove(&staging)?;
        std::fs::create_dir(&staging).map_err(ctx(format!("creating {}", staging.display())))?;
        Ok(staging)
    }

    /// Moves a staged directory or subvolume into place, refusing to replace anything there.
    pub fn commit(&self, dest: &Path) -> Result<()> {
        let staging = Self::staging_for(dest);
        kn_btrfs::rename_noreplace(&staging, dest).map_err(ctx(format!("moving {} into place", dest.display())))
    }

    /// Creates an empty subvolume at `path` and returns its ID.
    pub fn create(&self, path: &Path) -> Result<u64> {
        let what = || format!("creating subvolume {}", path.display());
        self.backend.create(path).map_err(ctx(what()))?;
        self.check(path, None).map_err(ctx(what()))
    }

    /// Snapshots `source`, which must be the subvolume `source_id`, as `dest`, and returns the snapshot's ID.
    pub fn snapshot(&self, source: &Path, source_id: u64, dest: &Path) -> Result<u64> {
        self.check(source, Some(source_id))
            .map_err(ctx(format!("snapshotting {}", source.display())))?;
        self.snapshot_unchecked(source, dest)
    }

    /// Snapshots `source`, already checked, as `dest`.
    pub fn snapshot_unchecked(&self, source: &Path, dest: &Path) -> Result<u64> {
        let what = || format!("snapshotting {} as {}", source.display(), dest.display());
        self.backend.snapshot(source, dest).map_err(ctx(what()))?;
        self.check(dest, None).map_err(ctx(what()))
    }

    /// Creates an empty subvolume at `path` atomically: under its staging name, then moved into place.
    pub fn create_in_place(&self, path: &Path) -> Result<u64> {
        let staging = Self::staging_for(path);
        self.remove(&staging)?;
        let made = self.create(&staging).and_then(|id| self.commit(path).map(|()| id));
        if made.is_err() {
            let _ = self.remove(&staging);
        }
        made
    }

    /// Snapshots `source` (the subvolume `source_id`) as `dest` atomically: under its staging name, then moved into
    /// place.
    pub fn snapshot_in_place(&self, source: &Path, source_id: u64, dest: &Path) -> Result<u64> {
        let staging = Self::staging_for(dest);
        self.remove(&staging)?;
        let made = self
            .snapshot(source, source_id, &staging)
            .and_then(|id| self.commit(dest).map(|()| id));
        if made.is_err() {
            let _ = self.remove(&staging);
        }
        made
    }

    /// Swaps two paths atomically.
    pub fn exchange(&self, a: &Path, b: &Path) -> Result<()> {
        kn_btrfs::exchange(a, b).map_err(ctx(format!("swapping {} and {}", a.display(), b.display())))
    }

    /// The names in a directory that aren't staging leftovers, sorted; none if it doesn't exist.
    pub fn names(dir: &Path) -> Result<Vec<String>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(ctx(format!("reading {}", dir.display()))(e)),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(ctx(format!("reading {}", dir.display())))?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// Removes every staging leftover (and half-made load) under the volumes directory: what a crash in the middle of
    /// making something leaves. Only run when nothing is being made.
    pub fn sweep(&self) -> Result<Vec<PathBuf>> {
        let mut dirs = vec![self.root.join("apps"), self.root.join("shared")];
        for app in Self::names(&self.root.join("apps"))? {
            dirs.push(self.app_dir(&app));
        }
        let mut loads = Vec::new();
        for name in Self::names(&self.root.join("shared"))? {
            dirs.push(self.snapshots_dir(&name));
            loads.push(self.shared_dir(&name).join(LOAD_NAME));
        }
        let mut removed = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(STAGING_PREFIX) {
                    self.remove(&entry.path())?;
                    removed.push(entry.path());
                }
            }
        }
        for load in loads {
            if Self::exists(&load)? {
                self.remove(&load)?;
                removed.push(load);
            }
        }
        Ok(removed)
    }
}

/// A backend of plain directories for unit tests: a "subvolume" is a directory it made, known by its device and
/// inode, so it survives renames as a real one does, and a snapshot is a copy.
#[cfg(test)]
pub mod fake {
    use std::collections::{HashMap, HashSet};
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::sync::Mutex;

    use super::{Backend, BtrfsError, FsId, Subvolume};

    #[derive(Default)]
    pub struct Fake {
        subvolumes: Mutex<HashMap<(u64, u64), u64>>,
        next: Mutex<u64>,
        /// Snapshots whose destination has one of these names fail.
        pub fail_snapshots: Mutex<HashSet<String>>,
        /// Every snapshot made, as (source, destination).
        pub snapshots: Mutex<Vec<(std::path::PathBuf, std::path::PathBuf)>>,
    }

    fn key(path: &Path) -> Result<(u64, u64), BtrfsError> {
        kn_btrfs::open_dir(path)?;
        let m = std::fs::symlink_metadata(path).map_err(|e| BtrfsError::Io {
            op: "checking",
            path: path.into(),
            source: e,
        })?;
        Ok((m.dev(), m.ino()))
    }

    fn io(op: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> BtrfsError {
        move |e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => BtrfsError::Exists(path.into()),
            std::io::ErrorKind::NotFound => BtrfsError::NotFound(path.into()),
            _ => BtrfsError::Io {
                op,
                path: path.into(),
                source: e,
            },
        }
    }

    fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let (src, dst) = (entry.path(), to.join(entry.file_name()));
            let ft = entry.file_type()?;
            if ft.is_symlink() {
                std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
            } else if ft.is_dir() {
                std::fs::create_dir(&dst)?;
                copy_tree(&src, &dst)?;
            } else {
                std::fs::copy(&src, &dst)?;
            }
        }
        Ok(())
    }

    impl Fake {
        fn register(&self, path: &Path) -> Result<(), BtrfsError> {
            let k = key(path)?;
            let mut next = self.next.lock().unwrap();
            *next += 1;
            self.subvolumes.lock().unwrap().insert(k, 256 + *next);
            Ok(())
        }
    }

    impl Backend for Fake {
        fn filesystem(&self, root: &Path) -> Result<FsId, BtrfsError> {
            kn_btrfs::open_dir(root)?;
            Ok(FsId([7; 16]))
        }

        fn create(&self, path: &Path) -> Result<(), BtrfsError> {
            kn_btrfs::open_dir(path.parent().unwrap())?;
            std::fs::create_dir(path).map_err(io("creating", path))?;
            self.register(path)
        }

        fn snapshot(&self, source: &Path, dest: &Path) -> Result<(), BtrfsError> {
            self.inspect(source)?;
            let name = dest.file_name().unwrap().to_string_lossy().into_owned();
            if self.fail_snapshots.lock().unwrap().contains(&name) {
                return Err(BtrfsError::Io {
                    op: "snapshotting into",
                    path: dest.into(),
                    source: std::io::Error::other("injected failure"),
                });
            }
            kn_btrfs::open_dir(dest.parent().unwrap())?;
            std::fs::create_dir(dest).map_err(io("snapshotting into", dest))?;
            copy_tree(source, dest).map_err(io("snapshotting into", dest))?;
            self.snapshots.lock().unwrap().push((source.into(), dest.into()));
            self.register(dest)
        }

        fn destroy(&self, path: &Path) -> Result<(), BtrfsError> {
            let k = key(path)?;
            if self.subvolumes.lock().unwrap().remove(&k).is_none() {
                return Err(BtrfsError::NotSubvolume(path.into()));
            }
            std::fs::remove_dir_all(path).map_err(io("destroying", path))
        }

        fn inspect(&self, path: &Path) -> Result<Subvolume, BtrfsError> {
            let k = key(path)?;
            match self.subvolumes.lock().unwrap().get(&k) {
                Some(id) => Ok(Subvolume {
                    id: *id,
                    fs: FsId([7; 16]),
                    read_only: false,
                }),
                None => Err(BtrfsError::NotSubvolume(path.into())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volumes() -> (tempfile::TempDir, Volumes, Arc<fake::Fake>) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap().join("volumes");
        let fake = Arc::new(fake::Fake::default());
        let v = Volumes::open(root, fake.clone()).unwrap();
        (dir, v, fake)
    }

    #[test]
    fn refuses_a_volumes_directory_off_btrfs() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        if kn_btrfs::filesystem(&root).is_ok() {
            return; // The temporary directory is on btrfs here.
        }
        let e = Volumes::open(root.join("volumes"), Arc::new(Btrfs))
            .unwrap_err()
            .to_string();
        assert!(e.contains("isn't on btrfs"), "{e}");
    }

    #[test]
    fn refuses_a_volumes_directory_through_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
        let e = Volumes::open(root.join("link"), Arc::new(fake::Fake::default()))
            .unwrap_err()
            .to_string();
        assert!(e.contains("is a symlink"), "{e}");
    }

    #[test]
    fn checks_mounts() {
        let (_d, v, _) = volumes();
        std::fs::create_dir_all(v.app_dir("blog")).unwrap();
        let data = v.app_volume("blog", "data");
        let id = v.create_in_place(&data).unwrap();
        assert_eq!(v.check(&data, Some(id)).unwrap(), id);
        assert_eq!(v.check(&data, None).unwrap(), id);

        // Another subvolume in its place.
        let other = v.create_in_place(&v.app_volume("blog", "other")).unwrap();
        std::fs::rename(&data, v.app_volume("blog", "aside")).unwrap();
        std::fs::rename(v.app_volume("blog", "other"), &data).unwrap();
        let e = v.check(&data, Some(id)).unwrap_err();
        assert!(
            matches!(e, CheckError::WrongSubvolume { expected, found, .. } if expected == id && found == other),
            "{e}"
        );

        // A symlink in its place, even to the right subvolume.
        std::fs::remove_dir(&data).unwrap();
        std::os::unix::fs::symlink(v.app_volume("blog", "aside"), &data).unwrap();
        let e = v.check(&data, Some(id)).unwrap_err();
        assert!(matches!(e, CheckError::Btrfs(BtrfsError::Symlink(_))), "{e}");

        // A plain directory, or nothing.
        std::fs::remove_file(&data).unwrap();
        std::fs::create_dir(&data).unwrap();
        let e = v.check(&data, Some(id)).unwrap_err();
        assert!(matches!(e, CheckError::Btrfs(BtrfsError::NotSubvolume(_))), "{e}");
        std::fs::remove_dir(&data).unwrap();
        let e = v.check(&data, Some(id)).unwrap_err();
        assert!(matches!(e, CheckError::Btrfs(BtrfsError::NotFound(_))), "{e}");

        // A symlink further up.
        std::fs::rename(v.app_dir("blog"), v.app_dir("blog-real")).unwrap();
        std::os::unix::fs::symlink(v.app_dir("blog-real"), v.app_dir("blog")).unwrap();
        let e = v.check(&v.app_volume("blog", "aside"), None).unwrap_err();
        assert!(
            matches!(&e, CheckError::Btrfs(BtrfsError::Symlink(p)) if p == &v.app_dir("blog")),
            "{e}"
        );
    }

    #[test]
    fn makes_things_in_place_without_replacing_and_sweeps_leftovers() {
        let (_d, v, fake) = volumes();
        std::fs::create_dir_all(v.app_dir("blog")).unwrap();
        let data = v.app_volume("blog", "data");
        let id = v.create_in_place(&data).unwrap();
        std::fs::write(data.join("f"), "x").unwrap();
        // Something there already is never replaced.
        assert!(v.create_in_place(&data).is_err());
        assert_eq!(v.check(&data, Some(id)).unwrap(), id);
        assert!(!Volumes::exists(&Volumes::staging_for(&data)).unwrap());

        // A snapshot that fails leaves nothing behind.
        let copy = v.app_volume("blog", "copy");
        fake.fail_snapshots.lock().unwrap().insert(".new-copy".into());
        assert!(v.snapshot_in_place(&data, id, &copy).is_err());
        assert!(!Volumes::exists(&copy).unwrap() && !Volumes::exists(&Volumes::staging_for(&copy)).unwrap());
        fake.fail_snapshots.lock().unwrap().clear();
        // Nor does one of the wrong source.
        assert!(v.snapshot_in_place(&data, id + 100, &copy).is_err());
        let cid = v.snapshot_in_place(&data, id, &copy).unwrap();
        assert_ne!(cid, id);
        assert_eq!(std::fs::read_to_string(copy.join("f")).unwrap(), "x");

        // What a crash leaves, staging names, is swept, and nothing else.
        let staged = v.stage_dir(&v.app_dir("new")).unwrap();
        v.create(&staged.join("data")).unwrap();
        v.create(&Volumes::staging_for(&v.app_volume("blog", "more"))).unwrap();
        std::fs::create_dir_all(v.snapshots_dir("corpus")).unwrap();
        v.create(&v.shared_dir("corpus").join(LOAD_NAME)).unwrap();
        let mut swept = v.sweep().unwrap();
        swept.sort();
        assert_eq!(swept.len(), 3, "{swept:?}");
        assert!(!Volumes::exists(&staged).unwrap());
        assert_eq!(Volumes::names(&v.app_dir("blog")).unwrap(), ["copy", "data"]);

        // Removing an app's directory destroys its subvolumes.
        v.remove(&v.app_dir("blog")).unwrap();
        assert!(!Volumes::exists(&v.app_dir("blog")).unwrap());
    }

    #[test]
    fn never_removes_through_a_symlink() {
        let (d, v, _) = volumes();
        let outside = std::fs::canonicalize(d.path()).unwrap().join("outside");
        std::fs::create_dir_all(outside.join("blog/data")).unwrap();
        std::fs::write(outside.join("blog/data/precious"), "x").unwrap();
        let apps = v.root().join("apps");
        std::fs::remove_dir(&apps).unwrap();
        std::os::unix::fs::symlink(&outside, &apps).unwrap();
        let e = v.remove(&v.app_dir("blog")).unwrap_err().to_string();
        assert!(e.contains("is a symlink"), "{e}");
        assert!(v.sweep().is_err() || outside.join("blog/data/precious").exists());
        assert!(outside.join("blog/data/precious").exists());
        // A symlink itself is removed, not what it points to.
        std::fs::remove_file(&apps).unwrap();
        std::fs::create_dir(&apps).unwrap();
        std::os::unix::fs::symlink(outside.join("blog"), v.app_dir("blog")).unwrap();
        v.remove(&v.app_dir("blog")).unwrap();
        assert!(outside.join("blog/data/precious").exists());
    }
}

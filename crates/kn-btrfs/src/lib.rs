//! btrfs subvolumes, through the kernel's ioctls rather than `btrfs-progs`.
//!
//! - [`create`] makes an empty subvolume, [`snapshot`] a writable or read-only snapshot of one, and [`destroy`]
//!   deletes one.
//! - [`inspect`] checks that a path is a subvolume on btrfs, reached without symlinks, and returns its ID and its
//!   filesystem's UUID.
//! - [`rename_noreplace`] and [`exchange`] move subvolumes (or anything else) into place atomically.
//! - [`send`] writes a read-only subvolume as a `btrfs send` stream, whole or against a parent (receiving one is
//!   plain file operations, in `kn-backup`).
//!
//! Every path is opened one component at a time with `O_NOFOLLOW`, starting from `/`, so a symlink anywhere on the
//! way is refused rather than followed, and the ioctls act on those file descriptors rather than on paths. Paths
//! must be absolute, with no `.` or `..`.
//!
//! The node agent runs as root. Without root (in tests, or a development node), creating subvolumes and
//! snapshotting your own still work, but the kernel only lets an unprivileged user destroy a subvolume when the
//! filesystem is mounted with `user_subvol_rm_allowed`; otherwise [`destroy`] empties the subvolume and removes it
//! with `rmdir`, which the kernel allows for an empty subvolume.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

/// The inode number of every subvolume's root directory.
pub const SUBVOLUME_ROOT_INODE: u64 = 256;

/// `statfs`'s `f_type` for btrfs.
pub const SUPER_MAGIC: u64 = 0x9123_683e;

const IOCTL_MAGIC: u64 = 0x94;
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;

/// The generic Linux ioctl encoding, as used on x86_64 and aarch64.
const fn ioc(dir: u64, nr: u64, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | (IOCTL_MAGIC << 8) | nr
}

/// `struct btrfs_ioctl_vol_args`.
#[repr(C)]
struct VolArgs {
    fd: i64,
    name: [u8; 4088],
}

/// `struct btrfs_ioctl_vol_args_v2`.
#[repr(C)]
struct VolArgsV2 {
    fd: i64,
    transid: u64,
    flags: u64,
    unused: [u64; 4],
    name: [u8; 4040],
}

/// `struct btrfs_ioctl_ino_lookup_args`.
#[repr(C)]
struct InoLookupArgs {
    treeid: u64,
    objectid: u64,
    name: [u8; 4080],
}

/// `struct btrfs_ioctl_fs_info_args`, up to the UUID; the kernel fills the rest.
#[repr(C)]
struct FsInfoArgs {
    max_id: u64,
    num_devices: u64,
    fsid: [u8; 16],
    rest: [u8; 992],
}

const IOC_SUBVOL_CREATE: u64 = ioc(IOC_WRITE, 14, size_of::<VolArgs>());
const IOC_SNAP_DESTROY: u64 = ioc(IOC_WRITE, 15, size_of::<VolArgs>());
const IOC_INO_LOOKUP: u64 = ioc(IOC_READ | IOC_WRITE, 18, size_of::<InoLookupArgs>());
const IOC_SNAP_CREATE_V2: u64 = ioc(IOC_WRITE, 23, size_of::<VolArgsV2>());
const IOC_SUBVOL_GETFLAGS: u64 = ioc(IOC_READ, 25, size_of::<u64>());
const IOC_SUBVOL_SETFLAGS: u64 = ioc(IOC_WRITE, 26, size_of::<u64>());
const IOC_FS_INFO: u64 = ioc(IOC_READ, 31, size_of::<FsInfoArgs>());
const IOC_DEV_INFO: u64 = ioc(IOC_READ | IOC_WRITE, 30, size_of::<DevInfoArgs>());
const IOC_SEND: u64 = ioc(IOC_WRITE, 38, size_of::<SendArgs>());

/// `struct btrfs_ioctl_send_args`.
#[repr(C)]
struct SendArgs {
    send_fd: i64,
    clone_sources_count: u64,
    clone_sources: *const u64,
    parent_root: u64,
    flags: u64,
    version: u32,
    reserved: [u8; 28],
}

/// `struct btrfs_ioctl_dev_info_args`, up to the sizes; the kernel fills the rest.
#[repr(C)]
struct DevInfoArgs {
    devid: u64,
    uuid: [u8; 16],
    bytes_used: u64,
    total_bytes: u64,
    rest: [u8; 4056],
}

const SUBVOL_RDONLY: u64 = 1 << 1;

/// The UUID of a btrfs filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FsId(pub [u8; 16]);

impl std::fmt::Display for FsId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (i, b) in self.0.iter().enumerate() {
            if matches!(i, 4 | 6 | 8 | 10) {
                f.write_str("-")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// What [`inspect`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subvolume {
    /// The subvolume's ID, unique within its filesystem and never reused.
    pub id: u64,
    /// Its filesystem.
    pub fs: FsId,
    pub read_only: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("`{0}` must be an absolute path without `.` or `..`")]
    BadPath(PathBuf),
    #[error("`{0}` doesn't exist")]
    NotFound(PathBuf),
    #[error("`{0}` is a symlink")]
    Symlink(PathBuf),
    #[error("`{0}` isn't a directory")]
    NotDirectory(PathBuf),
    #[error("`{0}` isn't on btrfs")]
    NotBtrfs(PathBuf),
    #[error("`{0}` isn't a btrfs subvolume")]
    NotSubvolume(PathBuf),
    #[error("`{0}` already exists")]
    Exists(PathBuf),
    #[error("{op} `{path}`: {source}")]
    Io {
        op: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

impl Error {
    fn io(op: &'static str, path: &Path, source: io::Error) -> Error {
        match source.raw_os_error() {
            Some(libc::EEXIST) => Error::Exists(path.into()),
            Some(libc::ENOENT) => Error::NotFound(path.into()),
            _ => Error::Io {
                op,
                path: path.into(),
                source,
            },
        }
    }

    /// The underlying OS error, if there is one.
    pub fn raw_os_error(&self) -> Option<i32> {
        match self {
            Error::Io { source, .. } => source.raw_os_error(),
            Error::NotFound(_) => Some(libc::ENOENT),
            Error::Exists(_) => Some(libc::EEXIST),
            _ => None,
        }
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn cstr(s: &[u8]) -> CString {
    CString::new(s).expect("names are checked for NUL bytes")
}

/// Checks that `name` is one path component that can be created in a directory.
fn check_name(path: &Path, name: &[u8]) -> Result<()> {
    if name.is_empty() || name.len() > 255 || name == b"." || name == b".." || name.contains(&b'/') || name.contains(&0)
    {
        return Err(Error::BadPath(path.into()));
    }
    Ok(())
}

/// Splits an absolute, normal path into its parent and its last component.
fn split(path: &Path) -> Result<(&Path, &[u8])> {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) if path.is_absolute() => {
            check_name(path, name.as_bytes())?;
            Ok((parent, name.as_bytes()))
        }
        _ => Err(Error::BadPath(path.into())),
    }
}

fn openat(dir: BorrowedFd, name: &[u8], flags: libc::c_int) -> io::Result<OwnedFd> {
    let name = cstr(name);
    // SAFETY: `name` is a valid C string and `dir` an open descriptor; the result is checked.
    let fd = unsafe { libc::openat(dir.as_raw_fd(), name.as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just opened and is owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn lstat_at(dir: BorrowedFd, name: &[u8]) -> io::Result<libc::stat> {
    let name = cstr(name);
    // SAFETY: a zeroed `stat` is valid, and is only read after the call succeeds.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, C string and out-pointer.
    let r = unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

fn fstat(fd: BorrowedFd) -> io::Result<libc::stat> {
    // SAFETY: as in `lstat_at`.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor and out-pointer.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st)
}

/// Opens a directory without following symlinks: each component is opened with `O_NOFOLLOW`, starting from `/`.
///
/// Refuses a relative path, or one with `.` or `..`, and says which component is a symlink or not a directory.
pub fn open_dir(path: &Path) -> Result<OwnedFd> {
    let mut components = path.components();
    if components.next() != Some(Component::RootDir) || !components.clone().all(|c| matches!(c, Component::Normal(_))) {
        return Err(Error::BadPath(path.into()));
    }
    let root = cstr(b"/");
    // SAFETY: a valid C string; the result is checked.
    let fd = unsafe { libc::open(root.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(Error::io("opening", Path::new("/"), io::Error::last_os_error()));
    }
    // SAFETY: just opened.
    let mut dir = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut at = PathBuf::from("/");
    for c in components {
        let Component::Normal(name) = c else {
            return Err(Error::BadPath(path.into()));
        };
        at.push(name);
        let name = name.as_bytes();
        match openat(dir.as_fd(), name, libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW) {
            Ok(next) => dir = next,
            Err(e) => {
                return Err(match e.raw_os_error() {
                    Some(libc::ELOOP) => Error::Symlink(at),
                    Some(libc::ENOTDIR) => match lstat_at(dir.as_fd(), name) {
                        Ok(st) if st.st_mode & libc::S_IFMT == libc::S_IFLNK => Error::Symlink(at),
                        _ => Error::NotDirectory(at),
                    },
                    _ => Error::io("opening", &at, e),
                });
            }
        }
    }
    Ok(dir)
}

/// Whether an open file is on btrfs.
pub fn is_btrfs_fd(fd: BorrowedFd) -> io::Result<bool> {
    // SAFETY: a zeroed `statfs` is valid, and is only read after the call succeeds.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor and out-pointer.
    if unsafe { libc::fstatfs(fd.as_raw_fd(), &mut st) } < 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    Ok(st.f_type as u64 == SUPER_MAGIC)
}

/// Checks that a directory, reached without symlinks, is on btrfs, and returns its filesystem's UUID.
pub fn filesystem(path: &Path) -> Result<FsId> {
    let fd = open_dir(path)?;
    if !is_btrfs_fd(fd.as_fd()).map_err(|e| Error::io("checking", path, e))? {
        return Err(Error::NotBtrfs(path.into()));
    }
    fs_id_fd(fd.as_fd()).map_err(|e| Error::io("reading the filesystem of", path, e))
}

fn ioctl<T>(fd: BorrowedFd, request: u64, arg: *mut T) -> io::Result<()> {
    // SAFETY: each caller passes the request's own argument type, which the kernel reads and writes within its
    // size (encoded in `request`).
    let r = unsafe { libc::ioctl(fd.as_raw_fd(), request as _, arg) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn fs_id_fd(fd: BorrowedFd) -> io::Result<FsId> {
    // SAFETY: plain integers and bytes, all valid when zeroed.
    let mut args: Box<FsInfoArgs> = Box::new(unsafe { std::mem::zeroed() });
    ioctl(fd, IOC_FS_INFO, &mut *args)?;
    Ok(FsId(args.fsid))
}

/// The space on a btrfs filesystem's devices that no chunk has been allocated from yet, summed over its devices.
///
/// Free space as `statvfs` reports it counts only what's left in allocated chunks plus an estimate, and a
/// filesystem whose chunks are all allocated can fail writes while it shows space free; this is the number to
/// watch. It needs no privilege.
pub fn unallocated(path: &Path) -> Result<u64> {
    let fd = open_dir(path)?;
    if !is_btrfs_fd(fd.as_fd()).map_err(|e| Error::io("checking", path, e))? {
        return Err(Error::NotBtrfs(path.into()));
    }
    // SAFETY: plain integers and bytes, all valid when zeroed.
    let mut fs: Box<FsInfoArgs> = Box::new(unsafe { std::mem::zeroed() });
    ioctl(fd.as_fd(), IOC_FS_INFO, &mut *fs).map_err(|e| Error::io("reading the filesystem of", path, e))?;
    let mut free = 0u64;
    for devid in 1..=fs.max_id {
        // SAFETY: as above.
        let mut dev: Box<DevInfoArgs> = Box::new(unsafe { std::mem::zeroed() });
        dev.devid = devid;
        match ioctl(fd.as_fd(), IOC_DEV_INFO, &mut *dev) {
            Ok(()) => free += dev.total_bytes.saturating_sub(dev.bytes_used),
            // A device ID that was removed.
            Err(e) if e.raw_os_error() == Some(libc::ENODEV) => {}
            Err(e) => return Err(Error::io("reading the devices of", path, e)),
        }
    }
    Ok(free)
}

/// A filesystem's size and the space available to unprivileged users, from `statvfs`, in bytes.
pub fn disk_space(path: &Path) -> io::Result<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
    // SAFETY: all integers, valid when zeroed; `statvfs` fills it.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid C string and `st` a valid out-pointer.
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } < 0 {
        return Err(io::Error::last_os_error());
    }
    #[allow(clippy::unnecessary_cast)]
    let frsize = st.f_frsize as u64;
    #[allow(clippy::unnecessary_cast)]
    Ok((st.f_blocks as u64 * frsize, st.f_bavail as u64 * frsize))
}

fn subvolume_id_fd(fd: BorrowedFd) -> io::Result<u64> {
    // SAFETY: as in `fs_id_fd`.
    let mut args: Box<InoLookupArgs> = Box::new(unsafe { std::mem::zeroed() });
    // Tree 0 and the root directory's inode ask for the ID of the subvolume holding `fd`, which needs no privilege.
    args.objectid = SUBVOLUME_ROOT_INODE;
    ioctl(fd, IOC_INO_LOOKUP, &mut *args)?;
    Ok(args.treeid)
}

fn flags_fd(fd: BorrowedFd) -> io::Result<u64> {
    let mut flags: u64 = 0;
    ioctl(fd, IOC_SUBVOL_GETFLAGS, &mut flags)?;
    Ok(flags)
}

/// Checks an open directory is a subvolume's root on btrfs.
fn inspect_fd(fd: BorrowedFd, path: &Path) -> Result<Subvolume> {
    if !is_btrfs_fd(fd).map_err(|e| Error::io("checking", path, e))? {
        return Err(Error::NotBtrfs(path.into()));
    }
    let st = fstat(fd).map_err(|e| Error::io("checking", path, e))?;
    #[allow(clippy::unnecessary_cast)]
    if st.st_ino as u64 != SUBVOLUME_ROOT_INODE {
        return Err(Error::NotSubvolume(path.into()));
    }
    Ok(Subvolume {
        id: subvolume_id_fd(fd).map_err(|e| Error::io("reading the subvolume ID of", path, e))?,
        fs: fs_id_fd(fd).map_err(|e| Error::io("reading the filesystem of", path, e))?,
        read_only: flags_fd(fd).map_err(|e| Error::io("reading the flags of", path, e))? & SUBVOL_RDONLY != 0,
    })
}

/// Checks that `path`, reached without symlinks, is a subvolume on btrfs, and says which one.
pub fn inspect(path: &Path) -> Result<Subvolume> {
    let fd = open_dir(path)?;
    inspect_fd(fd.as_fd(), path)
}

fn vol_args(name: &[u8]) -> Box<VolArgs> {
    // SAFETY: as in `fs_id_fd`.
    let mut args: Box<VolArgs> = Box::new(unsafe { std::mem::zeroed() });
    args.name[..name.len()].copy_from_slice(name);
    args
}

/// Creates an empty subvolume at `path`, whose parent must exist. Fails if anything is already there.
pub fn create(path: &Path) -> Result<()> {
    let (parent, name) = split(path)?;
    let dir = open_dir(parent)?;
    let mut args = vol_args(name);
    ioctl(dir.as_fd(), IOC_SUBVOL_CREATE, &mut *args).map_err(|e| Error::io("creating subvolume", path, e))
}

/// Snapshots the subvolume `source` as `dest`, whose parent must exist, on the same filesystem.
///
/// The snapshot is taken at one instant, and is durable when this returns. A read-only one can't be changed,
/// which is what backups send from.
pub fn snapshot(source: &Path, dest: &Path, read_only: bool) -> Result<()> {
    let src = open_dir(source)?;
    inspect_fd(src.as_fd(), source)?;
    let (parent, name) = split(dest)?;
    let dir = open_dir(parent)?;
    // SAFETY: as in `fs_id_fd`.
    let mut args: Box<VolArgsV2> = Box::new(unsafe { std::mem::zeroed() });
    args.fd = i64::from(src.as_raw_fd());
    args.flags = if read_only { SUBVOL_RDONLY } else { 0 };
    args.name[..name.len()].copy_from_slice(name);
    ioctl(dir.as_fd(), IOC_SNAP_CREATE_V2, &mut *args).map_err(|e| Error::io("snapshotting into", dest, e))
}

/// Makes a subvolume read-only, or writable again.
pub fn set_read_only(path: &Path, read_only: bool) -> Result<()> {
    let fd = open_dir(path)?;
    inspect_fd(fd.as_fd(), path)?;
    let flags = flags_fd(fd.as_fd()).map_err(|e| Error::io("reading the flags of", path, e))?;
    let mut new = if read_only {
        flags | SUBVOL_RDONLY
    } else {
        flags & !SUBVOL_RDONLY
    };
    ioctl(fd.as_fd(), IOC_SUBVOL_SETFLAGS, &mut new).map_err(|e| Error::io("setting the flags of", path, e))
}

/// Destroys the subvolume at `path`, with any subvolumes nested inside it. Refuses anything that isn't a subvolume.
pub fn destroy(path: &Path) -> Result<()> {
    let (parent, name) = split(path)?;
    let dir = open_dir(parent)?;
    let sub = open_dir(path)?;
    let info = inspect_fd(sub.as_fd(), path)?;
    drop(sub);
    // SAFETY: `geteuid` has no preconditions.
    let root = unsafe { libc::geteuid() } == 0;
    if info.read_only && !root {
        // Without root, the kernel checks write access to the subvolume, which a read-only one refuses.
        set_read_only(path, false)?;
    }
    let mut args = vol_args(name);
    let Err(e) = ioctl(dir.as_fd(), IOC_SNAP_DESTROY, &mut *args) else {
        return Ok(());
    };
    match e.raw_os_error() {
        // Subvolumes nested inside it: destroy those first.
        Some(libc::ENOTEMPTY) => {
            for nested in nested_subvolumes(path)? {
                destroy(&nested)?;
            }
            ioctl(dir.as_fd(), IOC_SNAP_DESTROY, &mut *args).map_err(|e| Error::io("destroying subvolume", path, e))
        }
        Some(libc::EPERM) if !root => {
            // Not root, and no `user_subvol_rm_allowed`: empty it, then `rmdir` (allowed for an empty subvolume).
            std::fs::remove_dir_all(path).map_err(|e| Error::io("removing", path, e))
        }
        _ => Err(Error::io("destroying subvolume", path, e)),
    }
}

/// The subvolumes directly nested inside `path` (at any depth, but not inside each other), without following
/// symlinks.
fn nested_subvolumes(path: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| Error::io("reading", &dir, e))?;
        for entry in entries {
            let entry = entry.map_err(|e| Error::io("reading", &dir, e))?;
            let ft = entry.file_type().map_err(|e| Error::io("reading", &entry.path(), e))?;
            if !ft.is_dir() {
                continue;
            }
            let p = entry.path();
            let st = std::fs::symlink_metadata(&p).map_err(|e| Error::io("reading", &p, e))?;
            if std::os::unix::fs::MetadataExt::ino(&st) == SUBVOLUME_ROOT_INODE {
                found.push(p);
            } else {
                stack.push(p);
            }
        }
    }
    Ok(found)
}

/// Writes everything on the filesystem holding `path` to disk (`syncfs`), such as files written before they're
/// swapped into place.
pub fn sync_filesystem(path: &Path) -> Result<()> {
    let fd = open_dir(path)?;
    // SAFETY: a valid descriptor; called through `syscall` because not every libc has a wrapper.
    if unsafe { libc::syscall(libc::SYS_syncfs, fd.as_raw_fd()) } < 0 {
        return Err(Error::io("syncing", path, io::Error::last_os_error()));
    }
    Ok(())
}

const RENAME_NOREPLACE: libc::c_uint = 1;
const RENAME_EXCHANGE: libc::c_uint = 2;

fn renameat2(from: &Path, to: &Path, flags: libc::c_uint, op: &'static str) -> Result<()> {
    let (from_parent, from_name) = split(from)?;
    let (to_parent, to_name) = split(to)?;
    let from_dir = open_dir(from_parent)?;
    let to_dir = open_dir(to_parent)?;
    let (a, b) = (cstr(from_name), cstr(to_name));
    // SAFETY: valid descriptors and C strings; the result is checked. `renameat2` is called through `syscall`
    // because not every libc has a wrapper.
    let r = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from_dir.as_raw_fd(),
            a.as_ptr(),
            to_dir.as_raw_fd(),
            b.as_ptr(),
            flags,
        )
    };
    if r < 0 {
        let e = io::Error::last_os_error();
        return Err(match e.raw_os_error() {
            Some(libc::EEXIST) => Error::Exists(to.into()),
            Some(libc::ENOENT) => Error::NotFound(from.into()),
            _ => Error::Io {
                op,
                path: from.into(),
                source: e,
            },
        });
    }
    for dir in [&from_dir, &to_dir] {
        // SAFETY: a valid descriptor.
        if unsafe { libc::fsync(dir.as_raw_fd()) } < 0 {
            return Err(Error::io("syncing the parent of", to, io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// Renames `from` to `to` atomically, failing with [`Error::Exists`] rather than replacing anything at `to`.
pub fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    renameat2(from, to, RENAME_NOREPLACE, "renaming")
}

/// Swaps `a` and `b` atomically: each path names the other's contents afterwards.
pub fn exchange(a: &Path, b: &Path) -> Result<()> {
    renameat2(a, b, RENAME_EXCHANGE, "exchanging")
}

/// Writes the read-only subvolume `path` to `out` as a `btrfs send` stream (version 1): all of it, or with
/// `parent` (another read-only subvolume on the same filesystem, normally an earlier snapshot of the same data) only
/// what changed since. The stream names the subvolume by its own name (the last component of `path`).
///
/// The kernel only lets root send (`CAP_SYS_ADMIN`). It writes the stream into a pipe from a thread of its own while
/// this reads it; if `out` fails, the pipe is closed, which stops the kernel too.
pub fn send(path: &Path, parent: Option<&Path>, out: &mut dyn io::Write) -> Result<()> {
    let fd = open_dir(path)?;
    let sub = inspect_fd(fd.as_fd(), path)?;
    if !sub.read_only {
        return Err(Error::Io {
            op: "sending",
            path: path.into(),
            source: io::Error::other("only a read-only subvolume can be sent"),
        });
    }
    let parent_root = match parent {
        Some(p) => {
            let ps = inspect(p)?;
            if !ps.read_only || ps.fs != sub.fs {
                return Err(Error::Io {
                    op: "sending against",
                    path: p.into(),
                    source: io::Error::other("the parent must be a read-only subvolume on the same filesystem"),
                });
            }
            ps.id
        }
        None => 0,
    };
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors; the result is checked.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return Err(Error::io("making a pipe for", path, io::Error::last_os_error()));
    }
    // SAFETY: both were just opened, and are owned by nothing else.
    let (reader, writer) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    let target = path.to_path_buf();
    let kernel = std::thread::Builder::new()
        .name("kn-btrfs-send".into())
        .spawn(move || -> io::Result<()> {
            let mut args = SendArgs {
                send_fd: i64::from(writer.as_raw_fd()),
                clone_sources_count: 0,
                clone_sources: std::ptr::null(),
                parent_root,
                flags: 0,
                version: 0,
                reserved: [0; 28],
            };
            let r = ioctl(fd.as_fd(), IOC_SEND, &mut args);
            // The reader sees the end of the stream once this end is closed.
            drop(writer);
            r
        })
        .map_err(|e| Error::io("starting to send", &target, e))?;
    let mut pipe = std::fs::File::from(reader);
    let copied = io::copy(&mut pipe, out);
    // If writing failed, closing the pipe makes the kernel's next write fail, so it stops.
    drop(pipe);
    let sent = kernel
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("the send thread panicked")));
    match (sent, copied) {
        (Err(e), _) if e.raw_os_error() == Some(libc::EPERM) => Err(Error::Io {
            op: "sending",
            path: path.into(),
            source: io::Error::new(e.kind(), "btrfs send needs root (CAP_SYS_ADMIN)"),
        }),
        // The kernel's EPIPE is only the echo of a failed write.
        (_, Err(e)) => Err(Error::io("writing the send stream of", path, e)),
        (Err(e), Ok(_)) => Err(Error::io("sending", path, e)),
        (Ok(()), Ok(_)) => Ok(()),
    }
}

/// Whether this process can send subvolumes: whether it runs as root.
pub fn can_send() -> bool {
    // SAFETY: `geteuid` has no preconditions.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(test)]
mod tests;

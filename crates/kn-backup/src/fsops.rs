//! File operations confined to one directory tree, for writing what a backup holds (a send stream's commands, a
//! tar archive's entries) into a fresh subvolume.
//!
//! A backup comes from outside the node, so its paths are untrusted: every path must be relative, of plain
//! components; each directory on the way is opened with `O_NOFOLLOW` from the tree's root, so a symlink the backup
//! made can never lead outside; and the last component is acted on without following it (`*at` calls with
//! `AT_SYMLINK_NOFOLLOW`, or opened with `O_NOFOLLOW`). The empty path is the root itself.
//!
//! Without root, changing a file's owner to someone else, and setting `trusted.` and `security.` attributes, aren't
//! allowed: those are skipped rather than failing, so tests and development nodes can restore their own files.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

fn bad(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

fn cstr(b: &[u8]) -> io::Result<CString> {
    CString::new(b).map_err(|_| bad("a path with a NUL byte"))
}

fn check(r: libc::c_int) -> io::Result<()> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// Whether this process runs as root.
pub fn is_root() -> bool {
    // SAFETY: no preconditions.
    unsafe { libc::geteuid() == 0 }
}

/// The components of a relative path, refusing anything but plain names.
pub fn components(path: &[u8]) -> io::Result<Vec<&[u8]>> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let p = Path::new(OsStr::from_bytes(path));
    let mut out = Vec::new();
    for c in p.components() {
        match c {
            Component::Normal(n) => out.push(n.as_bytes()),
            Component::CurDir => {}
            _ => {
                return Err(bad(format!(
                    "the path {:?} isn't relative to the volume",
                    String::from_utf8_lossy(path)
                )));
            }
        }
    }
    if out.is_empty() || path.starts_with(b"/") {
        return Err(bad(format!(
            "the path {:?} isn't a file in the volume",
            String::from_utf8_lossy(path)
        )));
    }
    Ok(out)
}

/// A directory tree, every operation confined to it.
pub struct Root {
    fd: OwnedFd,
    path: PathBuf,
    root: bool,
}

/// A directory in the tree and a name in it, for an operation on that name.
pub struct At {
    dir: OwnedFd,
    name: CString,
    path: String,
}

impl At {
    fn dir(&self) -> BorrowedFd<'_> {
        self.dir.as_fd()
    }

    /// `/proc/self/fd/DIR/NAME`, for the calls that have no `*at` form (extended attributes).
    fn proc_path(&self) -> io::Result<CString> {
        let mut p = format!("/proc/self/fd/{}/", self.dir.as_raw_fd()).into_bytes();
        p.extend_from_slice(self.name.as_bytes());
        cstr(&p)
    }
}

fn openat(dir: BorrowedFd, name: &CString, flags: libc::c_int, mode: libc::mode_t) -> io::Result<OwnedFd> {
    // SAFETY: valid descriptor and C string; the result is checked.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just opened, owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn fstatat(dir: BorrowedFd, name: &CString) -> io::Result<libc::stat> {
    // SAFETY: zeroed is a valid `stat`, filled by the call.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid descriptor, C string and out-pointer.
    check(unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(st)
}

/// Skips what only root may do, when not root.
fn unless_unprivileged(root: bool, r: io::Result<()>) -> io::Result<()> {
    match r {
        Err(e) if !root && matches!(e.raw_os_error(), Some(libc::EPERM) | Some(libc::EACCES)) => Ok(()),
        r => r,
    }
}

impl Root {
    /// Opens a directory (reached without symlinks) as a tree's root.
    pub fn open(path: &Path) -> io::Result<Root> {
        let fd = kn_btrfs::open_dir(path).map_err(io::Error::other)?;
        Ok(Root {
            fd,
            path: path.into(),
            root: is_root(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The directory holding `path`, opened without following symlinks, and its last component.
    pub fn at(&self, path: &[u8]) -> io::Result<At> {
        let parts = components(path)?;
        let Some((last, dirs)) = parts.split_last() else {
            return Err(bad("an operation on the volume itself"));
        };
        let dot = cstr(b".")?;
        let mut dir = openat(self.fd.as_fd(), &dot, libc::O_PATH | libc::O_DIRECTORY, 0)?;
        for d in dirs {
            let name = cstr(d)?;
            let next = openat(
                dir.as_fd(),
                &name,
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )
            .map_err(|e| self.err(path, "opening a directory of", e))?;
            // `O_PATH | O_NOFOLLOW` opens a symlink itself rather than failing: make sure it's a directory.
            // SAFETY: zeroed is valid; filled by the call.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: valid descriptor and out-pointer.
            check(unsafe { libc::fstat(next.as_raw_fd(), &mut st) })?;
            if st.st_mode & libc::S_IFMT != libc::S_IFDIR {
                return Err(bad(format!(
                    "{:?} goes through something that isn't a directory",
                    String::from_utf8_lossy(path)
                )));
            }
            dir = next;
        }
        Ok(At {
            dir,
            name: cstr(last)?,
            path: String::from_utf8_lossy(path).into_owned(),
        })
    }

    fn err(&self, path: &[u8], what: &str, e: io::Error) -> io::Error {
        io::Error::new(
            e.kind(),
            format!(
                "{what} {:?} in {}: {e}",
                String::from_utf8_lossy(path),
                self.path.display()
            ),
        )
    }

    fn wrap<T>(&self, at: &At, what: &str, r: io::Result<T>) -> io::Result<T> {
        r.map_err(|e| self.err(at.path.as_bytes(), what, e))
    }

    pub fn mkdir(&self, path: &[u8], mode: u32) -> io::Result<()> {
        let at = self.at(path)?;
        // SAFETY: valid descriptor and C string.
        let r = check(unsafe { libc::mkdirat(at.dir().as_raw_fd(), at.name.as_ptr(), mode) });
        self.wrap(&at, "making directory", r)
    }

    /// Creates a new, empty file, open for writing.
    pub fn create(&self, path: &[u8], mode: u32) -> io::Result<File> {
        let at = self.at(path)?;
        let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW;
        let fd = self.wrap(&at, "creating", openat(at.dir(), &at.name, flags, mode))?;
        Ok(File::from(fd))
    }

    /// Opens an existing regular file (not through a symlink) for writing.
    pub fn open_write(&self, path: &[u8]) -> io::Result<File> {
        let at = self.at(path)?;
        let fd = self.wrap(
            &at,
            "opening",
            openat(at.dir(), &at.name, libc::O_WRONLY | libc::O_NOFOLLOW, 0),
        )?;
        Ok(File::from(fd))
    }

    /// Opens an existing regular file (not through a symlink) for reading.
    pub fn open_read(&self, path: &[u8]) -> io::Result<File> {
        let at = self.at(path)?;
        let fd = self.wrap(
            &at,
            "opening",
            openat(at.dir(), &at.name, libc::O_RDONLY | libc::O_NOFOLLOW, 0),
        )?;
        Ok(File::from(fd))
    }

    /// A device node, FIFO or socket; `mode` includes the file type.
    pub fn mknod(&self, path: &[u8], mode: u32, rdev: u64) -> io::Result<()> {
        let at = self.at(path)?;
        // SAFETY: valid descriptor and C string.
        let r = check(unsafe { libc::mknodat(at.dir().as_raw_fd(), at.name.as_ptr(), mode, rdev as libc::dev_t) });
        self.wrap(&at, "making node", r)
    }

    pub fn symlink(&self, path: &[u8], target: &[u8]) -> io::Result<()> {
        let at = self.at(path)?;
        let target = cstr(target)?;
        // SAFETY: valid descriptor and C strings.
        let r = check(unsafe { libc::symlinkat(target.as_ptr(), at.dir().as_raw_fd(), at.name.as_ptr()) });
        self.wrap(&at, "making symlink", r)
    }

    /// A hard link at `path` to the existing `existing` (itself, if it's a symlink).
    pub fn link(&self, existing: &[u8], path: &[u8]) -> io::Result<()> {
        let from = self.at(existing)?;
        let to = self.at(path)?;
        // SAFETY: valid descriptors and C strings; flags 0 doesn't follow a symlink.
        let r = check(unsafe {
            libc::linkat(
                from.dir().as_raw_fd(),
                from.name.as_ptr(),
                to.dir().as_raw_fd(),
                to.name.as_ptr(),
                0,
            )
        });
        self.wrap(&to, "linking", r)
    }

    pub fn rename(&self, from: &[u8], to: &[u8]) -> io::Result<()> {
        let a = self.at(from)?;
        let b = self.at(to)?;
        // SAFETY: valid descriptors and C strings.
        let r = check(unsafe {
            libc::renameat(
                a.dir().as_raw_fd(),
                a.name.as_ptr(),
                b.dir().as_raw_fd(),
                b.name.as_ptr(),
            )
        });
        self.wrap(&a, "renaming", r)
    }

    pub fn unlink(&self, path: &[u8]) -> io::Result<()> {
        let at = self.at(path)?;
        // SAFETY: valid descriptor and C string.
        let r = check(unsafe { libc::unlinkat(at.dir().as_raw_fd(), at.name.as_ptr(), 0) });
        self.wrap(&at, "removing", r)
    }

    pub fn rmdir(&self, path: &[u8]) -> io::Result<()> {
        let at = self.at(path)?;
        // SAFETY: valid descriptor and C string.
        let r = check(unsafe { libc::unlinkat(at.dir().as_raw_fd(), at.name.as_ptr(), libc::AT_REMOVEDIR) });
        self.wrap(&at, "removing directory", r)
    }

    /// The file type and mode of `path`, without following it.
    pub fn lstat(&self, path: &[u8]) -> io::Result<libc::stat> {
        if path.is_empty() {
            // SAFETY: as in `fstatat`.
            let mut st: libc::stat = unsafe { std::mem::zeroed() };
            // SAFETY: valid descriptor and out-pointer.
            check(unsafe { libc::fstat(self.fd.as_raw_fd(), &mut st) })?;
            return Ok(st);
        }
        let at = self.at(path)?;
        self.wrap(&at, "checking", fstatat(at.dir(), &at.name))
    }

    /// Sets the permission bits (and setuid, setgid and sticky) of anything but a symlink, which has none.
    pub fn chmod(&self, path: &[u8], mode: u32) -> io::Result<()> {
        let mode = mode & 0o7777;
        if path.is_empty() {
            // SAFETY: valid descriptor.
            return check(unsafe { libc::fchmod(self.fd.as_raw_fd(), mode) });
        }
        let at = self.at(path)?;
        let st = self.wrap(&at, "checking", fstatat(at.dir(), &at.name))?;
        if st.st_mode & libc::S_IFMT == libc::S_IFLNK {
            return Ok(());
        }
        // Not a symlink, so following the last component is safe: nothing else changes the tree meanwhile.
        // SAFETY: valid descriptor and C string.
        let r = check(unsafe { libc::fchmodat(at.dir().as_raw_fd(), at.name.as_ptr(), mode, 0) });
        self.wrap(&at, "changing the mode of", r)
    }

    pub fn chown(&self, path: &[u8], uid: u32, gid: u32) -> io::Result<()> {
        let r = if path.is_empty() {
            // SAFETY: valid descriptor.
            check(unsafe { libc::fchown(self.fd.as_raw_fd(), uid, gid) })
        } else {
            let at = self.at(path)?;
            // SAFETY: valid descriptor and C string.
            let r = check(unsafe {
                libc::fchownat(
                    at.dir().as_raw_fd(),
                    at.name.as_ptr(),
                    uid,
                    gid,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            });
            let r = unless_unprivileged(self.root, r);
            self.wrap(&at, "changing the owner of", r)
        };
        unless_unprivileged(self.root, r)
    }

    /// Sets the access and modification times, as (seconds, nanoseconds).
    pub fn utimes(&self, path: &[u8], atime: (i64, u32), mtime: (i64, u32)) -> io::Result<()> {
        let ts = [
            libc::timespec {
                tv_sec: atime.0 as libc::time_t,
                tv_nsec: atime.1 as _,
            },
            libc::timespec {
                tv_sec: mtime.0 as libc::time_t,
                tv_nsec: mtime.1 as _,
            },
        ];
        if path.is_empty() {
            // SAFETY: valid descriptor and two timespecs.
            return check(unsafe { libc::futimens(self.fd.as_raw_fd(), ts.as_ptr()) });
        }
        let at = self.at(path)?;
        // SAFETY: valid descriptor, C string and two timespecs.
        let r = check(unsafe {
            libc::utimensat(
                at.dir().as_raw_fd(),
                at.name.as_ptr(),
                ts.as_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        });
        self.wrap(&at, "setting the times of", r)
    }

    fn xattr_path(&self, path: &[u8]) -> io::Result<(Option<At>, CString)> {
        if path.is_empty() {
            return Ok((None, cstr(format!("/proc/self/fd/{}", self.fd.as_raw_fd()).as_bytes())?));
        }
        let at = self.at(path)?;
        let p = at.proc_path()?;
        Ok((Some(at), p))
    }

    pub fn set_xattr(&self, path: &[u8], name: &[u8], value: &[u8]) -> io::Result<()> {
        let (_at, p) = self.xattr_path(path)?;
        let n = cstr(name)?;
        // SAFETY: valid C strings and a buffer of `value.len()` bytes. `lsetxattr` doesn't follow the last
        // component, and the `/proc` link before it is the directory already opened safely.
        let r = check(unsafe { libc::lsetxattr(p.as_ptr(), n.as_ptr(), value.as_ptr().cast(), value.len(), 0) });
        let privileged = name.starts_with(b"trusted.") || name.starts_with(b"security.");
        let r = if privileged {
            unless_unprivileged(self.root, r)
        } else {
            r
        };
        r.map_err(|e| self.err(path, "setting an extended attribute of", e))
    }

    pub fn remove_xattr(&self, path: &[u8], name: &[u8]) -> io::Result<()> {
        let (_at, p) = self.xattr_path(path)?;
        let n = cstr(name)?;
        // SAFETY: valid C strings.
        let r = check(unsafe { libc::lremovexattr(p.as_ptr(), n.as_ptr()) });
        unless_unprivileged(self.root, r).map_err(|e| self.err(path, "removing an extended attribute of", e))
    }
}

/// The extended attributes of a file, without following it, by name.
pub fn xattrs(path: &Path) -> io::Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let p = cstr(path.as_os_str().as_bytes())?;
    let mut names = vec![0u8; 1024];
    let n = loop {
        // SAFETY: valid C string and buffer.
        let n = unsafe { libc::llistxattr(p.as_ptr(), names.as_mut_ptr().cast(), names.len()) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::ERANGE) => names.resize(names.len() * 4, 0),
            Some(libc::ENOTSUP) => return Ok(Vec::new()),
            _ => return Err(e),
        }
    };
    let mut out = Vec::new();
    for name in names[..n].split(|b| *b == 0).filter(|n| !n.is_empty()) {
        let cname = cstr(name)?;
        let mut value = vec![0u8; 256];
        loop {
            // SAFETY: valid C strings and buffer.
            let m = unsafe { libc::lgetxattr(p.as_ptr(), cname.as_ptr(), value.as_mut_ptr().cast(), value.len()) };
            if m >= 0 {
                value.truncate(m as usize);
                break;
            }
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::ERANGE) => value.resize(value.len() * 4, 0),
                // Removed meanwhile.
                Some(libc::ENODATA) => {
                    value.clear();
                    break;
                }
                _ => return Err(e),
            }
        }
        out.push((name.to_vec(), value));
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_must_stay_inside() {
        for bad in [&b"/etc/passwd"[..], b"../x", b"a/../../x", b"a/..", b"."] {
            assert!(components(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        assert_eq!(components(b"a/./b").unwrap(), [&b"a"[..], b"b"]);
        assert!(components(b"").unwrap().is_empty());
    }

    #[test]
    fn symlinks_are_never_followed() {
        let d = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(d.path()).unwrap();
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("precious"), "x").unwrap();
        let tree = base.join("tree");
        std::fs::create_dir(&tree).unwrap();
        let r = Root::open(&tree).unwrap();
        r.symlink(b"escape", outside.as_os_str().as_bytes()).unwrap();
        r.symlink(b"file", b"../outside/precious").unwrap();
        // Through a symlinked directory: refused.
        assert!(r.create(b"escape/new", 0o600).is_err());
        assert!(r.unlink(b"escape/precious").is_err());
        assert!(r.set_xattr(b"escape/precious", b"user.x", b"1").is_err());
        // At a symlink: acts on the link, or refuses.
        assert!(r.open_write(b"file").is_err());
        r.chmod(b"file", 0o777).unwrap();
        r.chown(b"file", 0, 0).unwrap();
        r.utimes(b"file", (1, 0), (2, 0)).unwrap();
        r.unlink(b"file").unwrap();
        assert!(outside.join("precious").exists());
        assert!(!outside.join("new").exists());
        use std::os::unix::fs::PermissionsExt;
        assert_ne!(
            std::fs::metadata(outside.join("precious"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o777
        );
        // The ordinary operations work.
        r.mkdir(b"d", 0o700).unwrap();
        let mut f = r.create(b"d/f", 0o600).unwrap();
        std::io::Write::write_all(&mut f, b"hi").unwrap();
        drop(f);
        r.link(b"d/f", b"d/g").unwrap();
        r.rename(b"d/g", b"h").unwrap();
        r.chmod(b"h", 0o640).unwrap();
        r.utimes(b"h", (1_000_000, 5), (2_000_000, 7)).unwrap();
        let m = std::fs::metadata(tree.join("h")).unwrap();
        assert_eq!(m.permissions().mode() & 0o777, 0o640);
        assert_eq!(std::os::unix::fs::MetadataExt::mtime(&m), 2_000_000);
        assert_eq!(std::os::unix::fs::MetadataExt::nlink(&m), 2);
        if r.set_xattr(b"h", b"user.kn", b"v").is_ok() {
            assert_eq!(xattrs(&tree.join("h")).unwrap(), [(b"user.kn".to_vec(), b"v".to_vec())]);
            r.remove_xattr(b"h", b"user.kn").unwrap();
        }
        r.mknod(b"fifo", libc::S_IFIFO | 0o600, 0).unwrap();
        r.chmod(b"", 0o750).unwrap();
        r.unlink(b"d/f").unwrap();
        r.rmdir(b"d").unwrap();
    }
}

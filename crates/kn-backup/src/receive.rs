//! Receiving `btrfs send` streams: each one's commands, applied as file operations to a fresh subvolume.
//!
//! A full stream (`SUBVOL`) makes an empty subvolume; an incremental one (`SNAPSHOT`) makes a writable snapshot of
//! its parent, which must be one received earlier (found by the UUID it was sent with), and applies the changes. A
//! `CLONE` copies extents from the subvolume itself or its parent (reflinked, so they still share space). Paths are
//! confined to the subvolume ([`crate::fsops`]), so a damaged or hostile stream can't write outside it.
//!
//! This is what `btrfs receive` does, without the parts kiwi-nest doesn't need: received subvolumes aren't marked
//! with the UUID they came from, since the chain being received is known.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, bail};

use crate::fsops::Root;
use crate::sendstream::{Command, Reader, attr, cmd};

/// Making subvolumes: btrfs on a node, plain directories in unit tests.
pub trait Subvolumes: Send + Sync {
    fn create(&self, path: &Path) -> anyhow::Result<()>;
    /// A writable snapshot of `source` at `dest`.
    fn snapshot(&self, source: &Path, dest: &Path) -> anyhow::Result<()>;
}

/// The real thing.
pub struct Btrfs;

impl Subvolumes for Btrfs {
    fn create(&self, path: &Path) -> anyhow::Result<()> {
        Ok(kn_btrfs::create(path)?)
    }

    fn snapshot(&self, source: &Path, dest: &Path) -> anyhow::Result<()> {
        Ok(kn_btrfs::snapshot(source, dest, false)?)
    }
}

/// A subvolume a stream made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    /// Its name in the stream: a volume's name.
    pub name: String,
    /// The UUID it was sent from, which a later incremental stream names as its parent.
    pub uuid: [u8; 16],
    pub path: PathBuf,
}

/// Receives every stream in `input` into `dest`, an existing directory, each as `dest/NAME`. An incremental
/// stream's parent must be in `parents`, by UUID. `check_name` says which names a stream may have.
pub fn receive(
    input: &mut dyn Read,
    dest: &Path,
    parents: &HashMap<[u8; 16], PathBuf>,
    subvols: &dyn Subvolumes,
    check_name: &dyn Fn(&str) -> bool,
) -> anyhow::Result<Vec<Received>> {
    let mut reader = Reader::new(input);
    let mut out: Vec<Received> = Vec::new();
    while reader.next_stream()? {
        let first = reader.next_command()?;
        let (name, uuid, parent) = match first.cmd {
            cmd::SUBVOL => (first.path(attr::PATH)?.to_vec(), first.uuid(attr::UUID)?, None),
            cmd::SNAPSHOT => (
                first.path(attr::PATH)?.to_vec(),
                first.uuid(attr::UUID)?,
                Some(first.uuid(attr::CLONE_UUID)?),
            ),
            c => bail!("a send stream starts with command {c}, not a subvolume"),
        };
        let name = String::from_utf8(name)
            .ok()
            .filter(|n| check_name(n))
            .with_context(|| {
                format!(
                    "the backup holds a subvolume named {:?}, which isn't one of its volumes",
                    String::from_utf8_lossy(first.path(attr::PATH).unwrap_or_default())
                )
            })?;
        if out.iter().any(|r| r.name == name) {
            bail!("the backup holds volume `{name}` twice");
        }
        let path = dest.join(&name);
        let parent_path = match parent {
            None => {
                subvols.create(&path)?;
                None
            }
            Some(p) => {
                let source = parents.get(&p).with_context(|| {
                    format!("volume `{name}` is an incremental backup whose parent wasn't restored before it")
                })?;
                subvols.snapshot(source, &path)?;
                Some(source.clone())
            }
        };
        let root = Root::open(&path).with_context(|| format!("opening {}", path.display()))?;
        let parent_root = parent_path
            .as_deref()
            .map(Root::open)
            .transpose()
            .context("opening the parent")?;
        let mut apply = Apply {
            root: &root,
            parent: parent_root.as_ref().zip(parent),
            uuid,
            open: None,
        };
        loop {
            let c = reader.next_command()?;
            if c.cmd == cmd::END {
                break;
            }
            apply
                .command(&c)
                .with_context(|| format!("receiving volume `{name}`: command {}", c.cmd))?;
        }
        drop(apply);
        kn_btrfs::sync_filesystem(&path).ok();
        out.push(Received { name, uuid, path });
    }
    Ok(out)
}

/// Applies one stream's commands to its subvolume.
struct Apply<'a> {
    root: &'a Root,
    /// The parent subvolume and its UUID, for an incremental stream.
    parent: Option<(&'a Root, [u8; 16])>,
    /// The subvolume's own UUID in the stream, which clones from itself name.
    uuid: [u8; 16],
    /// The file last written, kept open for the writes that usually follow.
    open: Option<(Vec<u8>, File)>,
}

impl Apply<'_> {
    fn file(&mut self, path: &[u8]) -> io::Result<&File> {
        if self.open.as_ref().is_none_or(|(p, _)| p != path) {
            self.open = Some((path.to_vec(), self.root.open_write(path)?));
        }
        Ok(&self.open.as_ref().expect("just opened").1)
    }

    fn command(&mut self, c: &Command) -> anyhow::Result<()> {
        let r = self.root;
        if !matches!(c.cmd, cmd::WRITE | cmd::CLONE | cmd::TRUNCATE) {
            self.open = None;
        }
        let path = || c.path(attr::PATH);
        match c.cmd {
            cmd::MKFILE => drop(r.create(path()?, 0o600)?),
            cmd::MKDIR => r.mkdir(path()?, 0o700)?,
            cmd::MKNOD => {
                let mode = c.u64(attr::MODE)? as u32;
                r.mknod(path()?, (mode & libc::S_IFMT) | 0o600, c.u64(attr::RDEV)?)?
            }
            cmd::MKFIFO => r.mknod(path()?, libc::S_IFIFO | 0o600, 0)?,
            cmd::MKSOCK => r.mknod(path()?, libc::S_IFSOCK | 0o600, 0)?,
            cmd::SYMLINK => r.symlink(path()?, c.path(attr::PATH_LINK)?)?,
            cmd::RENAME => {
                self.open = None;
                r.rename(path()?, c.path(attr::PATH_TO)?)?
            }
            cmd::LINK => r.link(c.path(attr::PATH_LINK)?, path()?)?,
            cmd::UNLINK => r.unlink(path()?)?,
            cmd::RMDIR => r.rmdir(path()?)?,
            cmd::SET_XATTR => r.set_xattr(path()?, c.bytes(attr::XATTR_NAME)?, c.bytes(attr::XATTR_DATA)?)?,
            cmd::REMOVE_XATTR => r.remove_xattr(path()?, c.bytes(attr::XATTR_NAME)?)?,
            cmd::WRITE => {
                let offset = c.u64(attr::FILE_OFFSET)?;
                let data = c.bytes(attr::DATA)?.to_vec();
                self.file(path()?)?.write_all_at(&data, offset)?;
            }
            cmd::CLONE => self.clone_range(c)?,
            cmd::TRUNCATE => self.file(path()?)?.set_len(c.u64(attr::SIZE)?)?,
            cmd::CHMOD => r.chmod(path()?, c.u64(attr::MODE)? as u32)?,
            cmd::CHOWN => r.chown(path()?, c.u64(attr::UID)? as u32, c.u64(attr::GID)? as u32)?,
            cmd::UTIMES => r.utimes(path()?, c.time(attr::ATIME)?, c.time(attr::MTIME)?)?,
            // Only in streams without file data, which kiwi-nest never asks for.
            cmd::UPDATE_EXTENT => bail!("the stream has no file data"),
            cmd::SUBVOL | cmd::SNAPSHOT => bail!("a second subvolume inside one stream"),
            other => bail!("unknown command {other} (a newer stream version?)"),
        }
        Ok(())
    }

    /// Copies a range of a file from the subvolume itself or its parent, sharing the extents.
    fn clone_range(&mut self, c: &Command) -> anyhow::Result<()> {
        let src_uuid = c.uuid(attr::CLONE_UUID)?;
        let src_path = c.path(attr::CLONE_PATH)?;
        let source = if src_uuid == self.uuid {
            self.root.open_read(src_path)?
        } else {
            match self.parent {
                Some((p, u)) if u == src_uuid => p.open_read(src_path)?,
                _ => bail!("a clone from a subvolume that isn't this one or its parent"),
            }
        };
        let (offset, len, src_offset) = (
            c.u64(attr::FILE_OFFSET)?,
            c.u64(attr::CLONE_LEN)?,
            c.u64(attr::CLONE_OFFSET)?,
        );
        let dest = self.file(c.path(attr::PATH)?)?;
        let range = FileCloneRange {
            src_fd: i64::from(source.as_raw_fd()),
            src_offset,
            src_length: len,
            dest_offset: offset,
        };
        // SAFETY: valid descriptors and a `file_clone_range`; the result is checked.
        let r = unsafe { libc::ioctl(dest.as_raw_fd(), FICLONERANGE as _, &range) };
        if r < 0 {
            let e = io::Error::last_os_error();
            // Without reflinks (a different filesystem in tests), copy the bytes.
            if matches!(
                e.raw_os_error(),
                Some(libc::EOPNOTSUPP) | Some(libc::EXDEV) | Some(libc::EINVAL)
            ) {
                let mut buf = vec![0u8; len.min(1 << 20) as usize];
                let mut done = 0u64;
                while done < len {
                    let n = (len - done).min(buf.len() as u64) as usize;
                    source.read_exact_at(&mut buf[..n], src_offset + done)?;
                    dest.write_all_at(&buf[..n], offset + done)?;
                    done += n as u64;
                }
                return Ok(());
            }
            return Err(e.into());
        }
        Ok(())
    }
}

/// `struct file_clone_range`.
#[repr(C)]
struct FileCloneRange {
    src_fd: i64,
    src_offset: u64,
    src_length: u64,
    dest_offset: u64,
}

/// `FICLONERANGE`: `_IOW(0x94, 13, struct file_clone_range)`.
const FICLONERANGE: u64 = (1 << 30) | ((size_of::<FileCloneRange>() as u64) << 16) | (0x94 << 8) | 13;

/// Removes a subvolume, or a directory of them.
pub type Remove = Arc<dyn Fn(&Path) -> anyhow::Result<()> + Send + Sync>;

/// Says which volume names a backup may hold.
pub type CheckName = Arc<dyn Fn(&str) -> bool + Send + Sync>;

/// Receives a chain of backups, oldest (full) first, into `dest`: each backup's subvolumes are made from the
/// previous one's, in a directory of their own (`dest/.step-N`), which are then removed. [`Chain::finish`] moves the
/// last backup's subvolumes to `dest/NAME`.
pub struct Chain {
    dest: PathBuf,
    step: usize,
    previous: Vec<Received>,
    subvols: Arc<dyn Subvolumes + Send + Sync>,
    remove: Remove,
    check_name: CheckName,
}

impl Chain {
    pub fn new(
        dest: PathBuf,
        subvols: Arc<dyn Subvolumes + Send + Sync>,
        remove: Remove,
        check_name: CheckName,
    ) -> Chain {
        Chain {
            dest,
            step: 0,
            previous: Vec::new(),
            subvols,
            remove,
            check_name,
        }
    }

    fn step_dir(&self, i: usize) -> PathBuf {
        self.dest.join(format!(".step-{i}"))
    }

    /// Receives the next backup of the chain.
    pub fn step(&mut self, input: &mut dyn Read) -> anyhow::Result<()> {
        let dir = self.step_dir(self.step);
        std::fs::create_dir(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let parents: HashMap<[u8; 16], PathBuf> = self.previous.iter().map(|r| (r.uuid, r.path.clone())).collect();
        let got = receive(input, &dir, &parents, &*self.subvols, &*self.check_name)?;
        if self.step > 0 {
            (self.remove)(&self.step_dir(self.step - 1))?;
        }
        self.previous = got;
        self.step += 1;
        Ok(())
    }

    /// Moves the last backup's subvolumes into `dest`, and returns them by name.
    pub fn finish(self) -> anyhow::Result<BTreeMap<String, PathBuf>> {
        let mut out = BTreeMap::new();
        for r in &self.previous {
            let to = self.dest.join(&r.name);
            std::fs::rename(&r.path, &to).with_context(|| format!("moving {} into place", r.name))?;
            out.insert(r.name.clone(), to);
        }
        if self.step > 0 {
            (self.remove)(&self.step_dir(self.step - 1))?;
        }
        Ok(out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::os::unix::fs::MetadataExt;

    use super::*;
    use crate::sendstream::Writer;

    /// Plain directories as subvolumes; a snapshot is a copy.
    pub(crate) struct Dirs;

    pub(crate) fn copy_tree(from: &Path, to: &Path) -> io::Result<()> {
        std::fs::create_dir(to)?;
        for e in std::fs::read_dir(from)? {
            let e = e?;
            let (s, d) = (e.path(), to.join(e.file_name()));
            let ft = e.file_type()?;
            if ft.is_symlink() {
                std::os::unix::fs::symlink(std::fs::read_link(&s)?, &d)?;
            } else if ft.is_dir() {
                copy_tree(&s, &d)?;
            } else {
                std::fs::copy(&s, &d)?;
            }
        }
        Ok(())
    }

    impl Subvolumes for Dirs {
        fn create(&self, path: &Path) -> anyhow::Result<()> {
            Ok(std::fs::create_dir(path)?)
        }
        fn snapshot(&self, source: &Path, dest: &Path) -> anyhow::Result<()> {
            Ok(copy_tree(source, dest)?)
        }
    }

    fn le(n: u64) -> [u8; 8] {
        n.to_le_bytes()
    }

    fn ts(s: i64) -> [u8; 12] {
        let mut b = [0u8; 12];
        b[..8].copy_from_slice(&s.to_le_bytes());
        b
    }

    /// A full stream like the kernel's for a small tree.
    fn full(uuid: [u8; 16]) -> Vec<u8> {
        let mut w = Writer::new(Vec::new()).unwrap();
        w.command(
            cmd::SUBVOL,
            &[(attr::PATH, b"data"), (attr::UUID, &uuid), (attr::CTRANSID, &le(5))],
        )
        .unwrap();
        w.command(cmd::MKDIR, &[(attr::PATH, b"o257-5-0")]).unwrap();
        w.command(cmd::RENAME, &[(attr::PATH, b"o257-5-0"), (attr::PATH_TO, b"dir")])
            .unwrap();
        w.command(cmd::MKFILE, &[(attr::PATH, b"o258-5-0")]).unwrap();
        w.command(cmd::RENAME, &[(attr::PATH, b"o258-5-0"), (attr::PATH_TO, b"dir/file")])
            .unwrap();
        w.command(
            cmd::WRITE,
            &[
                (attr::PATH, b"dir/file"),
                (attr::FILE_OFFSET, &le(0)),
                (attr::DATA, b"hello"),
            ],
        )
        .unwrap();
        w.command(cmd::LINK, &[(attr::PATH, b"hard"), (attr::PATH_LINK, b"dir/file")])
            .unwrap();
        w.command(cmd::SYMLINK, &[(attr::PATH, b"link"), (attr::PATH_LINK, b"dir/file")])
            .unwrap();
        w.command(
            cmd::CLONE,
            &[
                (attr::PATH, b"hard"),
                (attr::FILE_OFFSET, &le(5)),
                (attr::CLONE_LEN, &le(4)),
                (attr::CLONE_UUID, &uuid),
                (attr::CLONE_CTRANSID, &le(5)),
                (attr::CLONE_PATH, b"dir/file"),
                (attr::CLONE_OFFSET, &le(1)),
            ],
        )
        .unwrap();
        w.command(cmd::CHMOD, &[(attr::PATH, b"dir/file"), (attr::MODE, &le(0o640))])
            .unwrap();
        w.command(
            cmd::UTIMES,
            &[
                (attr::PATH, b"dir/file"),
                (attr::ATIME, &ts(1_000)),
                (attr::MTIME, &ts(2_000)),
                (attr::CTIME, &ts(3_000)),
            ],
        )
        .unwrap();
        w.command(cmd::END, &[]).unwrap();
        w.into_inner()
    }

    fn incremental(uuid: [u8; 16], parent: [u8; 16]) -> Vec<u8> {
        let mut w = Writer::new(Vec::new()).unwrap();
        w.command(
            cmd::SNAPSHOT,
            &[
                (attr::PATH, b"data"),
                (attr::UUID, &uuid),
                (attr::CTRANSID, &le(9)),
                (attr::CLONE_UUID, &parent),
                (attr::CLONE_CTRANSID, &le(5)),
            ],
        )
        .unwrap();
        w.command(
            cmd::WRITE,
            &[
                (attr::PATH, b"dir/file"),
                (attr::FILE_OFFSET, &le(0)),
                (attr::DATA, b"HE"),
            ],
        )
        .unwrap();
        w.command(cmd::UNLINK, &[(attr::PATH, b"link")]).unwrap();
        w.command(cmd::TRUNCATE, &[(attr::PATH, b"dir/file"), (attr::SIZE, &le(4))])
            .unwrap();
        w.command(
            cmd::UTIMES,
            &[
                (attr::PATH, b"dir/file"),
                (attr::ATIME, &ts(3_000)),
                (attr::MTIME, &ts(3_000)),
                (attr::CTIME, &ts(3_000)),
            ],
        )
        .unwrap();
        w.command(cmd::END, &[]).unwrap();
        w.into_inner()
    }

    #[test]
    fn receives_a_chain() {
        let d = tempfile::tempdir().unwrap();
        let dest = std::fs::canonicalize(d.path()).unwrap();
        let (u1, u2) = ([1u8; 16], [2u8; 16]);
        let mut chain = Chain::new(
            dest.clone(),
            Arc::new(Dirs),
            Arc::new(|p: &Path| Ok(std::fs::remove_dir_all(p)?)),
            Arc::new(|n: &str| n == "data"),
        );
        for s in [full(u1), incremental(u2, u1)] {
            chain.step(&mut &s[..]).unwrap();
        }
        let got = chain.finish().unwrap();
        let data = &got["data"];
        assert_eq!(std::fs::read(data.join("dir/file")).unwrap(), b"HEll");
        // `hard` was the same file as `dir/file` when the clone wrote into it (the stand-in snapshot copies it, so
        // the incremental step's writes don't reach it), and the symlink is gone.
        assert_eq!(std::fs::read(data.join("hard")).unwrap(), b"helloello");
        assert!(std::fs::symlink_metadata(data.join("link")).is_err());
        let m = std::fs::metadata(data.join("dir/file")).unwrap();
        assert_eq!((m.mode() & 0o777, m.mtime()), (0o640, 3_000));
        // Only the result is left.
        let names: Vec<_> = std::fs::read_dir(&dest)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["data"]);
    }

    #[test]
    fn refuses_streams_that_reach_outside_or_lack_their_parent() {
        let d = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(d.path()).unwrap();
        let dest = base.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let uuid = [3u8; 16];
        type Cmds = [(u16, Vec<(u16, Vec<u8>)>)];
        let evil = |cmds: &Cmds| {
            let mut w = Writer::new(Vec::new()).unwrap();
            w.command(
                cmd::SUBVOL,
                &[(attr::PATH, b"data"), (attr::UUID, &uuid), (attr::CTRANSID, &le(1))],
            )
            .unwrap();
            for (c, attrs) in cmds {
                let a: Vec<(u16, &[u8])> = attrs.iter().map(|(t, v)| (*t, v.as_slice())).collect();
                w.command(*c, &a).unwrap();
            }
            w.command(cmd::END, &[]).unwrap();
            w.into_inner()
        };
        let p = |s: &str| (attr::PATH, s.as_bytes().to_vec());
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        for (i, cmds) in [
            vec![(cmd::MKFILE, vec![p("../outside/x")])],
            vec![(cmd::MKFILE, vec![p("/tmp/kn-x")])],
            vec![
                (
                    cmd::SYMLINK,
                    vec![p("esc"), (attr::PATH_LINK, outside.as_os_str().as_bytes().to_vec())],
                ),
                (cmd::MKFILE, vec![p("esc/x")]),
            ],
            vec![
                (cmd::SYMLINK, vec![p("esc2"), (attr::PATH_LINK, b"../outside".to_vec())]),
                (cmd::RENAME, vec![p("esc2/whatever"), (attr::PATH_TO, b"y".to_vec())]),
            ],
            // A FIFO (or, as root, a device node) written to, truncated or cloned from: never opened.
            vec![
                (cmd::MKFIFO, vec![p("fifo")]),
                (
                    cmd::WRITE,
                    vec![
                        p("fifo"),
                        (attr::FILE_OFFSET, le(0).to_vec()),
                        (attr::DATA, b"x".to_vec()),
                    ],
                ),
            ],
            vec![
                (cmd::MKFIFO, vec![p("fifo")]),
                (cmd::TRUNCATE, vec![p("fifo"), (attr::SIZE, le(0).to_vec())]),
            ],
            vec![
                (cmd::MKFIFO, vec![p("fifo")]),
                (cmd::MKFILE, vec![p("f")]),
                (
                    cmd::CLONE,
                    vec![
                        p("f"),
                        (attr::FILE_OFFSET, le(0).to_vec()),
                        (attr::CLONE_LEN, le(1).to_vec()),
                        (attr::CLONE_UUID, uuid.to_vec()),
                        (attr::CLONE_CTRANSID, le(1).to_vec()),
                        (attr::CLONE_PATH, b"fifo".to_vec()),
                        (attr::CLONE_OFFSET, le(0).to_vec()),
                    ],
                ),
            ],
            vec![
                (
                    cmd::MKNOD,
                    vec![
                        p("disk"),
                        (attr::MODE, le(u64::from(libc::S_IFBLK | 0o600)).to_vec()),
                        (attr::RDEV, le(0x800).to_vec()),
                    ],
                ),
                (
                    cmd::WRITE,
                    vec![
                        p("disk"),
                        (attr::FILE_OFFSET, le(0).to_vec()),
                        (attr::DATA, b"x".to_vec()),
                    ],
                ),
            ],
        ]
        .into_iter()
        .enumerate()
        {
            let sub = dest.join(format!("t{i}"));
            std::fs::create_dir(&sub).unwrap();
            let stream = evil(&cmds);
            let r = receive(&mut &stream[..], &sub, &HashMap::new(), &Dirs, &|n| n == "data");
            assert!(r.is_err(), "case {i}");
        }
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        assert!(!Path::new("/tmp/kn-x").exists());

        // A subvolume name that isn't a volume, and a parent that isn't there.
        let mut w = Writer::new(Vec::new()).unwrap();
        w.command(
            cmd::SUBVOL,
            &[(attr::PATH, b"../x"), (attr::UUID, &uuid), (attr::CTRANSID, &le(1))],
        )
        .unwrap();
        w.command(cmd::END, &[]).unwrap();
        let s = w.into_inner();
        assert!(receive(&mut &s[..], &dest, &HashMap::new(), &Dirs, &|n| n == "data").is_err());
        let s = incremental([4; 16], [5; 16]);
        let e = receive(&mut &s[..], &dest, &HashMap::new(), &Dirs, &|n| n == "data").unwrap_err();
        assert!(format!("{e:#}").contains("parent"), "{e:#}");
    }

    use std::os::unix::ffi::OsStrExt;
}

//! The `tar` format: an archive of a snapshot set, one top-level directory per volume.
//!
//! Entries keep numeric owners (no user or group names, so `tar` restores the numbers), modes (with setuid, setgid
//! and sticky), modification times (to the nanosecond, in PAX headers), symlinks, hard links (within a volume),
//! device nodes and FIFOs, and extended attributes (as `SCHILY.xattr.` PAX records, which GNU tar reads with
//! `--xattrs`). Sockets can't be archived, and are skipped with a note. Long names use GNU extensions. So
//! `tar --zstd -xpf BACKUP.tar.zst` as root restores it file for file, without kiwi-nest or btrfs.
//!
//! Extracting ([`extract`]) makes a fresh subvolume per top-level directory, and writes into it through
//! [`crate::fsops`], so no entry can land outside it.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tar::{EntryType, Header};

use crate::fsops::{self, Root};
use crate::receive::Subvolumes;

/// Writes trees into a tar archive.
pub struct TarWriter<W: Write> {
    b: tar::Builder<W>,
    /// The first path of each file with more than one link, by (device, inode).
    links: HashMap<(u64, u64), PathBuf>,
    pub notes: Vec<String>,
}

fn header(meta: &std::fs::Metadata, kind: EntryType) -> Header {
    let mut h = Header::new_gnu();
    h.set_entry_type(kind);
    h.set_mode(meta.mode() & 0o7777);
    h.set_uid(u64::from(meta.uid()));
    h.set_gid(u64::from(meta.gid()));
    h.set_mtime(meta.mtime().max(0) as u64);
    h.set_size(0);
    h
}

impl<W: Write> TarWriter<W> {
    pub fn new(w: W) -> Self {
        let mut b = tar::Builder::new(w);
        b.follow_symlinks(false);
        TarWriter {
            b,
            links: HashMap::new(),
            notes: Vec::new(),
        }
    }

    /// Adds the tree at `dir` under `prefix` (with an entry for `prefix` itself), or at the top if `prefix` is
    /// empty.
    pub fn add_tree(&mut self, prefix: &str, dir: &Path) -> anyhow::Result<()> {
        let meta = std::fs::symlink_metadata(dir).with_context(|| format!("reading {}", dir.display()))?;
        if !meta.is_dir() {
            bail!("{} isn't a directory", dir.display());
        }
        if prefix.is_empty() {
            self.add_children(dir, Path::new(""))
        } else {
            self.add(dir, Path::new(prefix), &meta)
        }
    }

    fn add_children(&mut self, dir: &Path, name: &Path) -> anyhow::Result<()> {
        let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .collect::<io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let path = e.path();
            let meta = std::fs::symlink_metadata(&path).with_context(|| format!("reading {}", path.display()))?;
            self.add(&path, &name.join(e.file_name()), &meta)?;
        }
        Ok(())
    }

    /// PAX records for an entry: its time to the nanosecond, and its extended attributes.
    fn pax(&mut self, path: &Path, meta: &std::fs::Metadata) -> anyhow::Result<()> {
        let mut records: Vec<(String, Vec<u8>)> = Vec::new();
        if meta.mtime_nsec() != 0 {
            records.push((
                "mtime".into(),
                format!("{}.{:09}", meta.mtime(), meta.mtime_nsec()).into_bytes(),
            ));
        }
        for (name, value) in
            fsops::xattrs(path).with_context(|| format!("reading the attributes of {}", path.display()))?
        {
            records.push((format!("SCHILY.xattr.{}", String::from_utf8_lossy(&name)), value));
        }
        if !records.is_empty() {
            self.b
                .append_pax_extensions(records.iter().map(|(k, v)| (k.as_str(), v.as_slice())))?;
        }
        Ok(())
    }

    fn add(&mut self, path: &Path, name: &Path, meta: &std::fs::Metadata) -> anyhow::Result<()> {
        let ft = meta.file_type();
        if ft.is_socket() {
            self.notes.push(format!("skipped socket {}", name.display()));
            return Ok(());
        }
        if !ft.is_dir() && meta.nlink() > 1 {
            let id = (meta.dev(), meta.ino());
            if let Some(first) = self.links.get(&id) {
                let mut h = header(meta, EntryType::Link);
                let first = first.clone();
                self.b.append_link(&mut h, name, &first)?;
                return Ok(());
            }
            self.links.insert(id, name.to_path_buf());
        }
        self.pax(path, meta)?;
        if ft.is_dir() {
            let mut h = header(meta, EntryType::Directory);
            self.b.append_data(&mut h, name, io::empty())?;
            return self.add_children(path, name);
        }
        if ft.is_symlink() {
            let target = std::fs::read_link(path)?;
            let mut h = header(meta, EntryType::Symlink);
            self.b.append_link(&mut h, name, &target)?;
            return Ok(());
        }
        if ft.is_file() {
            let f = std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(path)
                .with_context(|| format!("opening {}", path.display()))?;
            let mut h = header(meta, EntryType::Regular);
            h.set_size(meta.len());
            // The snapshot is read-only, so the file can't change size while it's read.
            self.b.append_data(&mut h, name, f.take(meta.len()))?;
            return Ok(());
        }
        let kind = if ft.is_char_device() {
            EntryType::Char
        } else if ft.is_block_device() {
            EntryType::Block
        } else if ft.is_fifo() {
            EntryType::Fifo
        } else {
            bail!("{} is of an unknown type", path.display());
        };
        let mut h = header(meta, kind);
        if kind != EntryType::Fifo {
            let rdev = meta.rdev() as libc::dev_t;
            h.set_device_major(libc::major(rdev))?;
            h.set_device_minor(libc::minor(rdev))?;
        }
        self.b.append_data(&mut h, name, io::empty())?;
        Ok(())
    }

    /// Ends the archive, and returns the writer.
    pub fn finish(self) -> anyhow::Result<W> {
        Ok(self.b.into_inner()?)
    }
}

/// What an entry's metadata is to become.
struct Meta {
    uid: u32,
    gid: u32,
    mode: u32,
    mtime: (i64, u32),
    xattrs: Vec<(Vec<u8>, Vec<u8>)>,
}

fn meta_of<R: Read>(e: &mut tar::Entry<'_, R>) -> anyhow::Result<Meta> {
    let h = e.header();
    let mut m = Meta {
        uid: h.uid()? as u32,
        gid: h.gid()? as u32,
        mode: h.mode()?,
        mtime: (h.mtime()? as i64, 0),
        xattrs: Vec::new(),
    };
    if let Some(pax) = e.pax_extensions()? {
        for ext in pax {
            let ext = ext?;
            let key = ext.key_bytes();
            if key == b"mtime" {
                let v = String::from_utf8_lossy(ext.value_bytes()).into_owned();
                let (s, ns) = v.split_once('.').unwrap_or((&v, "0"));
                let ns = format!("{ns:0<9}");
                m.mtime = (s.parse().unwrap_or(m.mtime.0), ns[..9].parse().unwrap_or(0));
            } else if let Some(name) = key.strip_prefix(b"SCHILY.xattr.") {
                m.xattrs.push((name.to_vec(), ext.value_bytes().to_vec()));
            }
        }
    }
    Ok(m)
}

/// Sets an entry's owner, mode, attributes and times, in that order (changing the owner clears setuid).
fn apply(root: &Root, path: &[u8], m: &Meta, symlink: bool) -> io::Result<()> {
    root.chown(path, m.uid, m.gid)?;
    if !symlink {
        root.chmod(path, m.mode)?;
    }
    for (name, value) in &m.xattrs {
        root.set_xattr(path, name, value)?;
    }
    root.utimes(path, m.mtime, m.mtime)
}

/// Extracts an archive of volumes into `dest`: each top-level directory becomes a fresh subvolume `dest/NAME`,
/// where `check_name` allows NAME. Returns them by name.
pub fn extract(
    input: &mut dyn Read,
    dest: &Path,
    subvols: &dyn Subvolumes,
    check_name: &dyn Fn(&str) -> bool,
) -> anyhow::Result<BTreeMap<String, PathBuf>> {
    let mut archive = tar::Archive::new(input);
    let mut roots: BTreeMap<String, Root> = BTreeMap::new();
    // Directories' metadata is set once everything in them is written, deepest first.
    let mut dirs: Vec<(String, Vec<u8>, Meta)> = Vec::new();
    for entry in archive.entries()? {
        let mut e = entry?;
        let path = e.path_bytes().into_owned();
        let parts = fsops::components(&path).context("an entry in the archive")?;
        let (first, rest) = parts.split_first().context("an empty path in the archive")?;
        let vol = String::from_utf8(first.to_vec())
            .ok()
            .filter(|n| check_name(n))
            .with_context(|| {
                format!(
                    "the archive holds {:?}, which isn't a volume",
                    String::from_utf8_lossy(first)
                )
            })?;
        let rel: Vec<u8> = rest.join(&b'/');
        if !roots.contains_key(&vol) {
            let p = dest.join(&vol);
            subvols.create(&p)?;
            roots.insert(vol.clone(), Root::open(&p)?);
        }
        let root = &roots[&vol];
        let kind = e.header().entry_type();
        let meta = meta_of(&mut e)?;
        let what = || format!("extracting {:?}", String::from_utf8_lossy(&path));
        match kind {
            EntryType::Directory => {
                if !rel.is_empty() {
                    root.mkdir(&rel, 0o700).with_context(what)?;
                }
                dirs.push((vol, rel, meta));
            }
            _ if rel.is_empty() => bail!("the archive's volume {vol:?} isn't a directory"),
            EntryType::Regular | EntryType::Continuous => {
                let mut f = root.create(&rel, 0o600).with_context(what)?;
                io::copy(&mut e, &mut f).with_context(what)?;
                drop(f);
                apply(root, &rel, &meta, false).with_context(what)?;
            }
            EntryType::Symlink => {
                let target = e.link_name_bytes().context("a symlink without a target")?.into_owned();
                root.symlink(&rel, &target).with_context(what)?;
                apply(root, &rel, &meta, true).with_context(what)?;
            }
            EntryType::Link => {
                let target = e
                    .link_name_bytes()
                    .context("a hard link without a target")?
                    .into_owned();
                let tparts = fsops::components(&target)?;
                let (tvol, trest) = tparts.split_first().context("a hard link to nothing")?;
                if *tvol != vol.as_bytes() || trest.is_empty() {
                    bail!(
                        "a hard link from {:?} outside its volume",
                        String::from_utf8_lossy(&path)
                    );
                }
                root.link(&trest.join(&b'/'), &rel).with_context(what)?;
            }
            EntryType::Char | EntryType::Block | EntryType::Fifo => {
                let (t, rdev) = match kind {
                    EntryType::Fifo => (libc::S_IFIFO, 0),
                    _ => {
                        let h = e.header();
                        let (ma, mi) = (h.device_major()?.unwrap_or(0), h.device_minor()?.unwrap_or(0));
                        let t = if kind == EntryType::Char {
                            libc::S_IFCHR
                        } else {
                            libc::S_IFBLK
                        };
                        (t, libc::makedev(ma, mi))
                    }
                };
                #[allow(clippy::unnecessary_cast)]
                root.mknod(&rel, t | 0o600, rdev as u64).with_context(what)?;
                apply(root, &rel, &meta, false).with_context(what)?;
            }
            other => bail!(
                "the archive has an entry {:?} of a kind kiwi-nest doesn't restore ({other:?})",
                String::from_utf8_lossy(&path)
            ),
        }
    }
    for (vol, rel, meta) in dirs.iter().rev() {
        apply(&roots[vol], rel, meta, false)
            .with_context(|| format!("setting the metadata of {vol}/{}", String::from_utf8_lossy(rel)))?;
    }
    let out = roots.into_iter().map(|(n, r)| (n, r.path().to_path_buf())).collect();
    Ok(out)
}

/// Extracts an archive of plain files (the server's) into `dest`, an empty directory.
pub fn extract_plain(input: &mut dyn Read, dest: &Path) -> anyhow::Result<()> {
    let mut archive = tar::Archive::new(input);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(false);
    for entry in archive.entries()? {
        let mut e = entry?;
        if !matches!(e.header().entry_type(), EntryType::Regular | EntryType::Directory) {
            bail!("the server's backup holds something other than files and directories");
        }
        let path = e.path_bytes().into_owned();
        fsops::components(&path)?;
        if !e.unpack_in(dest)? {
            bail!("an entry outside the backup: {:?}", String::from_utf8_lossy(&path));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::receive::tests::Dirs;

    /// A tree with the kinds of things a volume holds.
    pub(crate) fn sample(root: &Path) {
        std::fs::create_dir_all(root.join("sub/deep")).unwrap();
        // An attribute on the top directory itself, where the filesystem takes them.
        let c = std::ffi::CString::new(root.as_os_str().as_bytes()).unwrap();
        // SAFETY: valid C strings and a one-byte buffer. (Not supported everywhere, which is fine.)
        let _ = unsafe { libc::lsetxattr(c.as_ptr(), c"user.top".as_ptr(), b"t".as_ptr().cast(), 1, 0) };
        std::fs::write(root.join("sub/file"), "contents").unwrap();
        std::fs::set_permissions(root.join("sub/file"), std::fs::Permissions::from_mode(0o4751)).unwrap();
        std::fs::hard_link(root.join("sub/file"), root.join("hard")).unwrap();
        std::os::unix::fs::symlink("sub/file", root.join("link")).unwrap();
        let long = "n".repeat(120);
        std::fs::write(root.join("sub/deep").join(&long), vec![9u8; 100_000]).unwrap();
        std::fs::write(root.join("empty"), "").unwrap();
        let c = std::ffi::CString::new(root.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: a valid C string.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o640) }, 0);
        std::fs::set_permissions(root.join("sub/deep"), std::fs::Permissions::from_mode(0o750)).unwrap();
    }

    fn walk(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        fn go(root: &Path, dir: &Path, out: &mut Vec<String>) {
            let mut es: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
            es.sort_by_key(|e| e.file_name());
            for e in es {
                let p = e.path();
                let m = std::fs::symlink_metadata(&p).unwrap();
                let rel = p.strip_prefix(root).unwrap().display().to_string();
                let content = if m.is_file() {
                    format!("{:x}", m.len())
                } else if m.file_type().is_symlink() {
                    std::fs::read_link(&p).unwrap().display().to_string()
                } else {
                    String::new()
                };
                out.push(format!(
                    "{rel} {:o} {}:{} {} {content}",
                    m.mode(),
                    m.uid(),
                    m.gid(),
                    if m.is_dir() { 0 } else { m.mtime() }
                ));
                if m.is_dir() {
                    go(root, &p, out);
                }
            }
        }
        go(root, root, &mut out);
        out
    }

    #[test]
    fn archives_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(d.path()).unwrap();
        let src = base.join("src");
        std::fs::create_dir(&src).unwrap();
        sample(&src);
        let mut w = TarWriter::new(Vec::new());
        w.add_tree("data", &src).unwrap();
        let bytes = w.finish().unwrap();
        let dest = base.join("dest");
        std::fs::create_dir(&dest).unwrap();
        let got = extract(&mut &bytes[..], &dest, &Dirs, &|n| n == "data").unwrap();
        assert_eq!(got.keys().collect::<Vec<_>>(), ["data"]);
        assert_eq!(walk(&src), walk(&got["data"]));
        // The volume's top directory keeps its attributes.
        let top = crate::fsops::xattrs(&src).unwrap();
        assert_eq!(crate::fsops::xattrs(&got["data"]).unwrap(), top);
        if crate::receive::tests::xattrs_supported(&base) {
            assert_eq!(top, [(b"user.top".to_vec(), b"t".to_vec())]);
        }
        let m = std::fs::metadata(got["data"].join("hard")).unwrap();
        assert_eq!(m.nlink(), 2);
        // And with plain tar, where there is one.
        let out = base.join("plain");
        std::fs::create_dir(&out).unwrap();
        let ok = std::process::Command::new("tar")
            .args(["-xpf", "-", "-C"])
            .arg(&out)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut c| {
                c.stdin.take().unwrap().write_all(&bytes)?;
                c.wait()
            });
        if let Ok(status) = ok {
            assert!(status.success());
            assert_eq!(walk(&src), walk(&out.join("data")));
        }
    }

    #[test]
    fn hostile_archives_are_refused() {
        let d = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(d.path()).unwrap();
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let entries: Vec<Vec<(EntryType, &str, &str)>> = vec![
            vec![
                (EntryType::Directory, "data", ""),
                (EntryType::Regular, "data/../../outside/x", ""),
            ],
            vec![(EntryType::Directory, "other", "")],
            vec![
                (EntryType::Directory, "data", ""),
                (EntryType::Symlink, "data/esc", "../../outside"),
                (EntryType::Regular, "data/esc/x", ""),
            ],
            vec![
                (EntryType::Directory, "data", ""),
                (EntryType::Directory, "two", ""),
                (EntryType::Link, "data/l", "two/x"),
            ],
        ];
        for (i, es) in entries.into_iter().enumerate() {
            let mut b = tar::Builder::new(Vec::new());
            for (t, p, l) in es {
                let mut h = Header::new_gnu();
                h.set_entry_type(t);
                h.set_mode(0o644);
                h.set_size(0);
                // Written raw, as a hostile archive would be (the builder refuses `..` itself).
                let name = h.as_old_mut().name.as_mut();
                name[..p.len()].copy_from_slice(p.as_bytes());
                if !l.is_empty() {
                    h.set_link_name(l).unwrap();
                }
                h.set_cksum();
                b.append(&h, io::empty()).unwrap();
            }
            let bytes = b.into_inner().unwrap();
            let dest = base.join(format!("dest{i}"));
            std::fs::create_dir(&dest).unwrap();
            // (Case 1: `other` isn't a volume name the caller allows.)
            let r = extract(&mut &bytes[..], &dest, &Dirs, &|n| n == "data" || n == "two");
            assert!(r.is_err(), "case {i}");
        }
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }
}

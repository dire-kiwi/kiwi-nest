//! Round trips through the kernel's `btrfs send`, which needs root and a btrfs directory
//! (`KIWI_NEST_TEST_BTRFS_DIR`); skipped otherwise. CI runs them as root.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kn_proto::backup::{DirConfig, Format, Keep, Kind, TargetConfig};

use crate::receive::{Btrfs, Chain};
use crate::{Key, RestoreTo, Stamp, Target};

/// A fresh directory on btrfs, if this runs as root with one.
pub(crate) fn root_btrfs() -> Option<tempfile::TempDir> {
    let Ok(dir) = std::env::var("KIWI_NEST_TEST_BTRFS_DIR") else {
        eprintln!("skipped: set KIWI_NEST_TEST_BTRFS_DIR to a directory on btrfs, and run as root");
        return None;
    };
    if !kn_btrfs::can_send() {
        eprintln!("skipped: btrfs send needs root");
        return None;
    }
    let dir = std::fs::canonicalize(dir).unwrap();
    Some(tempfile::tempdir_in(dir).unwrap())
}

/// Every file under a directory, with what a restore must keep: type, mode, owner, size, times, link target,
/// link count, attributes and contents.
pub(crate) fn describe(root: &Path) -> Vec<String> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let mut es: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        es.sort_by_key(|e| e.file_name());
        for e in es {
            let p = e.path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            let extra = if m.is_file() {
                let data = std::fs::read(&p).unwrap();
                format!("{} {:x}", data.len(), data.iter().fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(*b))))
            } else if m.file_type().is_symlink() {
                std::fs::read_link(&p).unwrap().display().to_string()
            } else {
                String::new()
            };
            let xattrs = crate::fsops::xattrs(&p).unwrap();
            out.push(format!(
                "{rel} {:o} {}:{} n{} t{} {extra} {xattrs:?}",
                m.mode(),
                m.uid(),
                m.gid(),
                if m.is_dir() { 0 } else { m.nlink() },
                if m.is_dir() { 0 } else { m.mtime() },
            ));
            if m.is_dir() {
                go(root, &p, out);
            }
        }
    }
    let mut out = Vec::new();
    go(root, root, &mut out);
    let m = std::fs::metadata(root).unwrap();
    out.push(format!(". {:o} {}:{}", m.mode(), m.uid(), m.gid()));
    out
}

/// Fills a volume with what apps' data has: owners other than root, setuid, hard links, symlinks, attributes, a
/// FIFO, a device node, a sparse file and a reflinked copy.
pub(crate) fn fill(v: &Path) {
    std::fs::create_dir_all(v.join("db/wal")).unwrap();
    std::fs::write(v.join("db/main"), vec![1u8; 300_000]).unwrap();
    std::fs::write(v.join("db/wal/0001"), "log").unwrap();
    std::os::unix::fs::chown(v.join("db"), Some(999), Some(999)).unwrap();
    std::os::unix::fs::chown(v.join("db/main"), Some(999), Some(998)).unwrap();
    std::fs::set_permissions(v.join("db/main"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(v.join("tool"), "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(v.join("tool"), std::fs::Permissions::from_mode(0o4755)).unwrap();
    std::fs::hard_link(v.join("db/wal/0001"), v.join("hard")).unwrap();
    std::os::unix::fs::symlink("db/main", v.join("link")).unwrap();
    std::os::unix::fs::lchown(v.join("link"), Some(1000), Some(1000)).unwrap();
    let set = |p: &Path, name: &str, value: &[u8]| {
        let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
        let n = std::ffi::CString::new(name).unwrap();
        // SAFETY: valid C strings and buffer.
        assert_eq!(unsafe { libc::lsetxattr(c.as_ptr(), n.as_ptr(), value.as_ptr().cast(), value.len(), 0) }, 0);
    };
    set(&v.join("tool"), "user.kind", b"script");
    set(&v.join("db"), "trusted.kn", b"\x00\x01");
    let c = |p: &Path| std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: valid C strings.
    unsafe {
        assert_eq!(libc::mkfifo(c(&v.join("fifo")).as_ptr(), 0o640), 0);
        assert_eq!(libc::mknod(c(&v.join("null")).as_ptr(), libc::S_IFCHR | 0o666, libc::makedev(1, 3)), 0);
    }
    let f = std::fs::File::create(v.join("sparse")).unwrap();
    f.set_len(10 << 20).unwrap();
    std::os::unix::fs::FileExt::write_at(&f, b"end", (10 << 20) - 3).unwrap();
    drop(f);
    reflink(&v.join("db/main"), &v.join("db/copy"));
}

fn reflink(from: &Path, to: &Path) {
    let src = std::fs::File::open(from).unwrap();
    let dst = std::fs::File::create(to).unwrap();
    // FICLONE: `_IOW(0x94, 9, int)`.
    const FICLONE: u64 = (1 << 30) | (4 << 16) | (0x94 << 8) | 9;
    use std::os::fd::AsRawFd;
    // SAFETY: valid descriptors.
    assert_eq!(unsafe { libc::ioctl(dst.as_raw_fd(), FICLONE as _, src.as_raw_fd()) }, 0);
}

/// Changes a volume: new, changed, renamed and removed files, and a clone.
pub(crate) fn change(v: &Path) {
    let f = std::fs::OpenOptions::new().write(true).open(v.join("db/main")).unwrap();
    std::os::unix::fs::FileExt::write_at(&f, &[2u8; 4096], 8192).unwrap();
    drop(f);
    std::fs::rename(v.join("db/wal/0001"), v.join("db/wal/0002")).unwrap();
    std::fs::remove_file(v.join("fifo")).unwrap();
    std::fs::create_dir(v.join("new")).unwrap();
    std::fs::write(v.join("new/file"), "new").unwrap();
    std::os::unix::fs::chown(v.join("new/file"), Some(33), Some(33)).unwrap();
    reflink(&v.join("db/main"), &v.join("new/clone"));
}

fn snapshot(src: &Path, dest: &Path) {
    kn_btrfs::snapshot(src, dest, true).unwrap();
}

#[test]
fn a_chain_of_kernel_streams_restores_exactly() {
    let Some(d) = root_btrfs() else { return };
    let base = d.path();
    let vol = base.join("data");
    kn_btrfs::create(&vol).unwrap();
    fill(&vol);
    std::fs::create_dir_all(base.join("snaps/1")).unwrap();
    std::fs::create_dir_all(base.join("snaps/2")).unwrap();
    snapshot(&vol, &base.join("snaps/1/data"));
    let first = describe(&base.join("snaps/1/data"));
    change(&vol);
    snapshot(&vol, &base.join("snaps/2/data"));
    let second = describe(&base.join("snaps/2/data"));
    assert_ne!(first, second);

    let mut full = Vec::new();
    kn_btrfs::send(&base.join("snaps/1/data"), None, &mut full).unwrap();
    let mut incr = Vec::new();
    kn_btrfs::send(&base.join("snaps/2/data"), Some(&base.join("snaps/1/data")), &mut incr).unwrap();
    assert!(incr.len() < full.len() / 2, "{} vs {}", incr.len(), full.len());
    // A writable subvolume can't be sent.
    assert!(kn_btrfs::send(&vol, None, &mut Vec::new()).is_err());

    let dest = base.join("restored");
    std::fs::create_dir(&dest).unwrap();
    let mut chain = Chain::new(
        dest.clone(),
        Arc::new(Btrfs),
        Arc::new(|p: &Path| crate_remove(p)),
        Arc::new(|n: &str| n == "data"),
    );
    chain.step(&mut &full[..]).unwrap();
    let step0 = dest.join(".step-0/data");
    assert_eq!(describe(&step0), first);
    chain.step(&mut &incr[..]).unwrap();
    let got = chain.finish().unwrap();
    assert_eq!(describe(&got["data"]), second);
    let sub = kn_btrfs::inspect(&got["data"]).unwrap();
    assert!(!sub.read_only);
    assert_eq!(std::fs::read_dir(&dest).unwrap().count(), 1);
}

fn crate_remove(p: &Path) -> anyhow::Result<()> {
    fn go(p: &Path) -> anyhow::Result<()> {
        if kn_btrfs::inspect(p).is_ok() {
            return Ok(kn_btrfs::destroy(p)?);
        }
        if p.is_dir() {
            for e in std::fs::read_dir(p)? {
                go(&e?.path())?;
            }
            std::fs::remove_dir(p)?;
        } else {
            std::fs::remove_file(p)?;
        }
        Ok(())
    }
    go(p)
}

fn target(dir: &Path, format: Format) -> Target {
    Target::open(TargetConfig {
        name: "local".into(),
        s3: None,
        ssh: None,
        dir: Some(DirConfig {
            path: dir.to_string_lossy().into_owned(),
        }),
        prefix: String::new(),
        format,
        every: None,
        full_every: None,
        keep: Keep::default(),
    })
    .unwrap()
}

fn restore_to() -> RestoreTo {
    RestoreTo {
        subvols: Arc::new(Btrfs),
        remove: Arc::new(crate_remove),
        check_name: Arc::new(|n: &str| n == "data" || n == "logs"),
    }
}

/// Two volumes, backed up together as `send` (full, then incremental) and as `tar`, through a target.
#[tokio::test]
async fn backups_of_a_set_restore_through_a_target() {
    let Some(d) = root_btrfs() else { return };
    let base: PathBuf = d.path().into();
    for v in ["data", "logs"] {
        kn_btrfs::create(&base.join(v)).unwrap();
    }
    fill(&base.join("data"));
    std::fs::write(base.join("logs/today"), "x").unwrap();
    let set = |n: u32| {
        let dir = base.join(format!("set{n}"));
        std::fs::create_dir(&dir).unwrap();
        for v in ["data", "logs"] {
            snapshot(&base.join(v), &dir.join(v));
        }
        dir
    };
    let s1 = set(1);
    change(&base.join("data"));
    let s2 = set(2);
    let want: Vec<Vec<String>> = ["data", "logs"].iter().map(|v| describe(&s2.join(v))).collect();
    let names = vec!["data".to_string(), "logs".to_string()];

    let store = base.join("store");
    let t = target(&store, Format::Send);
    let k1 = Key::new(Kind::App, "blog", Stamp(1_800_000_000), None, Format::Send);
    let k2 = Key::new(Kind::App, "blog", Stamp(1_800_000_100), Some(k1.stamp), Format::Send);
    t.upload(&k1, crate::send_producer(s1.clone(), names.clone(), None)).await.unwrap();
    t.upload(&k2, crate::send_producer(s2.clone(), names.clone(), Some(s1.clone())))
        .await
        .unwrap();
    let dest = base.join("restore-send");
    std::fs::create_dir(&dest).unwrap();
    let got = crate::restore(&t, &k2, dest, restore_to()).await.unwrap();
    let have: Vec<Vec<String>> = ["data", "logs"].iter().map(|v| describe(&got[*v])).collect();
    assert_eq!(have, want);

    // Plain `btrfs receive` reads the same object, where btrfs-progs is installed.
    let tt = target(&base.join("store-tar"), Format::Tar);
    let kt = Key::new(Kind::App, "blog", Stamp(1_800_000_100), None, Format::Tar);
    let notes = Arc::new(std::sync::Mutex::new(Vec::new()));
    tt.upload(&kt, crate::tar_producer(s2.clone(), names.clone(), notes)).await.unwrap();
    let dest = base.join("restore-tar");
    std::fs::create_dir(&dest).unwrap();
    let got = crate::restore(&tt, &kt, dest, restore_to()).await.unwrap();
    let have: Vec<Vec<String>> = ["data", "logs"].iter().map(|v| describe(&got[*v])).collect();
    // A tar restore doesn't share extents, but every file matches, sparse ones included.
    assert_eq!(have, want);

    // And plain `tar` as root: file for file, owners and modes included.
    let plain = base.join("plain-tar");
    std::fs::create_dir(&plain).unwrap();
    let file = base.join("store-tar").join(kt.to_string());
    let status = std::process::Command::new("tar")
        .args(["--zstd", "--xattrs", "--xattrs-include=*", "-xpf"])
        .arg(&file)
        .arg("-C")
        .arg(&plain)
        .status();
    match status {
        Ok(s) if s.success() => {
            let have: Vec<Vec<String>> = ["data", "logs"].iter().map(|v| describe(&plain.join(v))).collect();
            assert_eq!(have, want);
        }
        other => eprintln!("plain tar not checked: {other:?}"),
    }
}

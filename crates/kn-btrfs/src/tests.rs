use super::*;

/// A fresh directory inside `KIWI_NEST_TEST_BTRFS_DIR`, or `None` (and a note) if it isn't set.
fn btrfs_dir() -> Option<tempfile::TempDir> {
    let Ok(dir) = std::env::var("KIWI_NEST_TEST_BTRFS_DIR") else {
        eprintln!("skipped: set KIWI_NEST_TEST_BTRFS_DIR to a directory on btrfs to run the btrfs tests");
        return None;
    };
    let dir = std::fs::canonicalize(dir).expect("KIWI_NEST_TEST_BTRFS_DIR should exist");
    Some(tempfile::tempdir_in(dir).unwrap())
}

/// A directory that isn't on btrfs (the system temporary directory, unless it happens to be btrfs).
fn plain_dir() -> Option<tempfile::TempDir> {
    let d = tempfile::tempdir().unwrap();
    let real = std::fs::canonicalize(d.path()).unwrap();
    match filesystem(&real) {
        Err(Error::NotBtrfs(_)) => Some(d),
        _ => None,
    }
}

#[test]
fn ioctl_numbers_match_the_kernel_headers() {
    // From <linux/btrfs.h>, as compiled on x86_64.
    assert_eq!(IOC_SUBVOL_CREATE, 0x5000_940e);
    assert_eq!(IOC_SNAP_DESTROY, 0x5000_940f);
    assert_eq!(IOC_INO_LOOKUP, 0xd000_9412);
    assert_eq!(IOC_SNAP_CREATE_V2, 0x5000_9417);
    assert_eq!(IOC_SUBVOL_GETFLAGS, 0x8008_9419);
    assert_eq!(IOC_DEV_INFO, 0xd000_941e);
    assert_eq!(size_of::<DevInfoArgs>(), 4096);
    assert_eq!(IOC_SUBVOL_SETFLAGS, 0x4008_941a);
    assert_eq!(IOC_FS_INFO, 0x8400_941f);
    assert_eq!(std::mem::offset_of!(VolArgsV2, flags), 16);
    assert_eq!(std::mem::offset_of!(VolArgsV2, name), 56);
    assert_eq!(std::mem::offset_of!(FsInfoArgs, fsid), 16);
}

#[test]
fn refuses_relative_and_unnormal_paths() {
    // (`Path` drops a `.` in the middle by itself, which is harmless.)
    for bad in ["relative", "/a/../b", "./a", ""] {
        assert!(matches!(open_dir(Path::new(bad)), Err(Error::BadPath(_))), "{bad}");
    }
    assert!(matches!(create(Path::new("/")), Err(Error::BadPath(_))));
    assert!(matches!(create(Path::new("x")), Err(Error::BadPath(_))));
}

#[test]
fn open_dir_refuses_symlinks_on_the_way() {
    let d = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(d.path()).unwrap();
    std::fs::create_dir_all(root.join("real/inner")).unwrap();
    std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
    std::fs::write(root.join("file"), "x").unwrap();

    open_dir(&root.join("real/inner")).unwrap();
    let e = open_dir(&root.join("link/inner")).unwrap_err();
    assert!(matches!(&e, Error::Symlink(p) if p == &root.join("link")), "{e}");
    let e = open_dir(&root.join("link")).unwrap_err();
    assert!(matches!(&e, Error::Symlink(p) if p == &root.join("link")), "{e}");
    let e = open_dir(&root.join("file")).unwrap_err();
    assert!(matches!(&e, Error::NotDirectory(_)), "{e}");
    let e = open_dir(&root.join("missing/x")).unwrap_err();
    assert!(matches!(&e, Error::NotFound(p) if p == &root.join("missing")), "{e}");
}

#[test]
fn refuses_what_isnt_on_btrfs() {
    let Some(d) = plain_dir() else { return };
    let root = std::fs::canonicalize(d.path()).unwrap();
    assert!(matches!(filesystem(&root), Err(Error::NotBtrfs(_))));
    assert!(matches!(inspect(&root), Err(Error::NotBtrfs(_))));
    std::fs::create_dir(root.join("sub")).unwrap();
    assert!(matches!(destroy(&root.join("sub")), Err(Error::NotBtrfs(_))));
    assert!(root.join("sub").exists(), "nothing is removed");
}

#[test]
fn renames_without_replacing_and_exchanges() {
    let d = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(d.path()).unwrap();
    for (dir, file) in [("a", "in-a"), ("b", "in-b")] {
        std::fs::create_dir(root.join(dir)).unwrap();
        std::fs::write(root.join(dir).join(file), "").unwrap();
    }
    std::fs::create_dir(root.join("empty")).unwrap();
    // A plain rename would replace the empty directory; this one refuses.
    assert!(matches!(
        rename_noreplace(&root.join("a"), &root.join("empty")),
        Err(Error::Exists(_))
    ));
    assert!(matches!(
        rename_noreplace(&root.join("nothing"), &root.join("c")),
        Err(Error::NotFound(_))
    ));
    rename_noreplace(&root.join("a"), &root.join("c")).unwrap();
    assert!(root.join("c/in-a").exists() && !root.join("a").exists());
    exchange(&root.join("b"), &root.join("c")).unwrap();
    assert!(root.join("b/in-a").exists() && root.join("c/in-b").exists());
}

#[test]
fn subvolumes_snapshots_and_destroy() {
    let Some(d) = btrfs_dir() else { return };
    let root = std::fs::canonicalize(d.path()).unwrap();
    let fs = filesystem(&root).unwrap();

    // A plain directory on btrfs isn't a subvolume.
    std::fs::create_dir(root.join("plain")).unwrap();
    assert!(matches!(inspect(&root.join("plain")), Err(Error::NotSubvolume(_))));
    assert!(matches!(destroy(&root.join("plain")), Err(Error::NotSubvolume(_))));

    let a = root.join("a");
    create(&a).unwrap();
    assert!(matches!(create(&a), Err(Error::Exists(_))));
    let info = inspect(&a).unwrap();
    assert_eq!(info.fs, fs);
    assert!(!info.read_only);
    std::fs::write(a.join("data"), "one").unwrap();

    // A writable snapshot is a new subvolume with the same contents, and the two diverge.
    let b = root.join("b");
    snapshot(&a, &b, false).unwrap();
    let binfo = inspect(&b).unwrap();
    assert_ne!(binfo.id, info.id);
    assert_eq!(std::fs::read_to_string(b.join("data")).unwrap(), "one");
    std::fs::write(b.join("data"), "two").unwrap();
    assert_eq!(std::fs::read_to_string(a.join("data")).unwrap(), "one");

    // A read-only snapshot can't be written.
    let ro = root.join("ro");
    snapshot(&a, &ro, true).unwrap();
    assert!(inspect(&ro).unwrap().read_only);
    assert!(std::fs::write(ro.join("data"), "x").is_err());
    // Only a subvolume can be snapshotted, and nothing is replaced.
    assert!(matches!(
        snapshot(&root.join("plain"), &root.join("c"), false),
        Err(Error::NotSubvolume(_))
    ));
    assert!(matches!(snapshot(&a, &b, false), Err(Error::Exists(_))));

    // A symlink to a subvolume isn't one.
    std::os::unix::fs::symlink(&a, root.join("link")).unwrap();
    assert!(matches!(inspect(&root.join("link")), Err(Error::Symlink(_))));
    assert!(matches!(destroy(&root.join("link")), Err(Error::Symlink(_))));

    // Destroy, including a read-only snapshot and a subvolume with another nested inside it.
    create(&a.join("nested")).unwrap();
    std::fs::write(a.join("nested/x"), "x").unwrap();
    for p in [&a, &b, &ro] {
        destroy(p).unwrap();
        assert!(!p.exists(), "{}", p.display());
    }
}

#[test]
fn reports_space() {
    let (total, free) = disk_space(Path::new("/")).unwrap();
    assert!(total > 0 && free <= total);
    if let Some(p) = plain_dir() {
        assert!(matches!(
            unallocated(&std::fs::canonicalize(p.path()).unwrap()),
            Err(Error::NotBtrfs(_))
        ));
    }
    let Some(dir) = btrfs_dir() else { return };
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let (total, _) = disk_space(&root).unwrap();
    let unallocated = unallocated(&root).unwrap();
    assert!(unallocated <= total * 2, "{unallocated} unallocated of {total}");
}

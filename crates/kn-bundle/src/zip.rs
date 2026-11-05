//! Extracting a bundle's ZIP safely.
//!
//! Every entry's name is checked before anything is written: no absolute paths, no `..`, no duplicates, nothing
//! that is both a file and a directory. Only plain files and directories are accepted: symlinks, devices, FIFOs
//! and sockets are refused, as are encrypted entries. Sizes are counted as the data is decompressed, so a ZIP
//! that lies about its sizes is caught too.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use zip::ZipArchive;

use crate::files::{check_name, copy_limited};
use crate::{BundleError, FileIndex, Limits};

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

enum Kind {
    Dir,
    File { executable: bool },
}

fn zip_error(e: zip::result::ZipError) -> BundleError {
    BundleError::Zip(e.to_string())
}

/// Extracts a ZIP into `dest`, which must be an empty directory, and returns the index of what it holds.
///
/// On error, `dest` may hold part of the bundle; the caller removes it.
pub fn extract<R: Read + Seek>(mut reader: R, dest: &Path, limits: &Limits) -> Result<FileIndex, BundleError> {
    let size = reader
        .seek(SeekFrom::End(0))
        .and_then(|n| reader.seek(SeekFrom::Start(0)).map(|_| n))
        .map_err(|e| BundleError::io("reading the ZIP", e))?;
    if size > limits.max_archive_size {
        return Err(BundleError::TooLarge(format!(
            "the ZIP is {size} bytes; the limit is {}",
            limits.max_archive_size
        )));
    }
    let mut archive = ZipArchive::new(reader).map_err(zip_error)?;
    if archive.has_duplicate_names() {
        return Err(BundleError::DuplicateInZip);
    }
    if archive.len() > limits.max_files {
        return Err(BundleError::TooLarge(format!(
            "the ZIP has {} entries; the limit is {}",
            archive.len(),
            limits.max_files
        )));
    }

    // First, check every entry without decompressing anything.
    let mut index = FileIndex::new();
    let mut files = Vec::new();
    let mut declared: u64 = 0;
    for i in 0..archive.len() {
        let entry = archive.by_index_raw(i).map_err(zip_error)?;
        let raw = entry.name().map_err(zip_error)?.into_owned();
        let (path, dir_marker) = check_name(&raw, limits)?;
        if entry.encrypted() {
            return Err(BundleError::Encrypted(path));
        }
        if entry.is_symlink() {
            return Err(BundleError::Symlink(path));
        }
        let kind = match entry.unix_mode().map(|m| m & S_IFMT) {
            None | Some(0) if dir_marker => Kind::Dir,
            None | Some(0) => Kind::File { executable: false },
            Some(S_IFDIR) => Kind::Dir,
            Some(S_IFREG) if !dir_marker => Kind::File {
                executable: entry.unix_mode().unwrap_or(0) & 0o111 != 0,
            },
            Some(S_IFREG) => return Err(BundleError::Conflict(path)),
            Some(S_IFLNK) => return Err(BundleError::Symlink(path)),
            Some(_) => return Err(BundleError::SpecialFile(path)),
        };
        match kind {
            Kind::Dir => index.add_dir(&path)?,
            Kind::File { executable } => {
                if entry.size() > limits.max_file_size {
                    return Err(BundleError::TooLarge(format!(
                        "`{path}` is {} bytes; the limit is {}",
                        entry.size(),
                        limits.max_file_size
                    )));
                }
                declared = declared.saturating_add(entry.size());
                if declared > limits.max_expanded_size {
                    return Err(BundleError::TooLarge(format!(
                        "the files add up to more than {} bytes",
                        limits.max_expanded_size
                    )));
                }
                index.add_file(&path, entry.size(), executable, [0; 32])?;
                files.push((i, path, executable));
            }
        }
    }

    // Then create the directories, and write the files, counting what is really there.
    for (path, entry) in index.iter() {
        if matches!(entry, crate::Entry::Dir) {
            let full = dest.join(path);
            fs::create_dir_all(&full).map_err(|e| BundleError::io(path, e))?;
            fs::set_permissions(&full, fs::Permissions::from_mode(0o755)).map_err(|e| BundleError::io(path, e))?;
        }
    }
    let mut written: u64 = 0;
    for (i, path, executable) in files {
        let mut entry = archive.by_index(i).map_err(zip_error)?;
        let mode = if executable { 0o755 } else { 0o644 };
        let full = dest.join(&path);
        let mut out = fs::File::options()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&full)
            .map_err(|e| BundleError::io(&path, e))?;
        let left = limits.max_expanded_size - written;
        let (limit, what) = if limits.max_file_size <= left {
            (limits.max_file_size, "for one file")
        } else {
            (left, "for the whole bundle")
        };
        let (n, sha) = copy_limited(&mut entry, &mut out, limit, &path, what)?;
        out.set_permissions(fs::Permissions::from_mode(mode))
            .map_err(|e| BundleError::io(&path, e))?;
        written += n;
        index.set_contents(&path, n, sha);
    }
    Ok(index)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::Entry;
    use std::io::Cursor;

    /// One entry of a hand-made ZIP: a name, a Unix mode (`0` for none) and stored (uncompressed) data.
    pub(crate) struct Raw<'a> {
        pub name: &'a str,
        pub mode: u32,
        pub data: &'a [u8],
        /// The size to claim in the headers, if not the data's real size.
        pub claimed_size: Option<u32>,
    }

    pub(crate) fn file<'a>(name: &'a str, data: &'a [u8]) -> Raw<'a> {
        Raw {
            name,
            mode: 0o100644,
            data,
            claimed_size: None,
        }
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for &b in data {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    /// Writes a ZIP by hand, so tests can make entries that a well-behaved writer wouldn't (duplicates, `..`,
    /// symlinks, devices, lying sizes).
    pub(crate) fn raw_zip(entries: &[Raw]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for e in entries {
            let offset = out.len() as u32;
            let crc = crc32(e.data);
            let size = e.claimed_size.unwrap_or(e.data.len() as u32);
            let name = e.name.as_bytes();
            let local = |v: &mut Vec<u8>| {
                v.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
                v.extend_from_slice(&20u16.to_le_bytes()); // version needed
                v.extend_from_slice(&0u16.to_le_bytes()); // flags
                v.extend_from_slice(&0u16.to_le_bytes()); // stored
                v.extend_from_slice(&0u32.to_le_bytes()); // time and date
                v.extend_from_slice(&crc.to_le_bytes());
                v.extend_from_slice(&size.to_le_bytes());
                v.extend_from_slice(&size.to_le_bytes());
                v.extend_from_slice(&(name.len() as u16).to_le_bytes());
                v.extend_from_slice(&0u16.to_le_bytes());
                v.extend_from_slice(name);
            };
            local(&mut out);
            out.extend_from_slice(e.data);

            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            let made_by: u16 = if e.mode == 0 { 20 } else { (3 << 8) | 20 }; // 3 = Unix
            central.extend_from_slice(&made_by.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&crc.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&size.to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes()); // extra
            central.extend_from_slice(&0u16.to_le_bytes()); // comment
            central.extend_from_slice(&0u16.to_le_bytes()); // disk
            central.extend_from_slice(&0u16.to_le_bytes()); // internal attributes
            central.extend_from_slice(&(e.mode << 16).to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name);
        }
        let cd_offset = out.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(central.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn try_extract(entries: &[Raw], limits: &Limits) -> (tempfile::TempDir, Result<FileIndex, BundleError>) {
        let dir = tempfile::tempdir().unwrap();
        let r = extract(Cursor::new(raw_zip(entries)), dir.path(), limits);
        (dir, r)
    }

    fn refused(entries: &[Raw]) -> BundleError {
        let (_dir, r) = try_extract(entries, &Limits::default());
        r.expect_err("should be refused")
    }

    #[test]
    fn extracts_files_and_directories() {
        let (dir, r) = try_extract(
            &[
                file("compose.yaml", b"services: {}\n"),
                Raw {
                    name: "site/",
                    mode: 0o040755,
                    data: b"",
                    claimed_size: None,
                },
                file("site/index.html", b"<h1>hi</h1>"),
                Raw {
                    name: "bin/run.sh",
                    mode: 0o100755,
                    data: b"#!/bin/sh\n",
                    claimed_size: None,
                },
                Raw {
                    name: "empty/",
                    mode: 0,
                    data: b"",
                    claimed_size: None,
                },
                Raw {
                    name: "plain.txt",
                    mode: 0,
                    data: b"x",
                    claimed_size: None,
                },
            ],
            &Limits::default(),
        );
        let index = r.unwrap();
        assert!(index.is_file("compose.yaml"));
        assert!(index.is_file("site/index.html"));
        assert_eq!(index.get("bin"), Some(&Entry::Dir));
        assert_eq!(index.get("empty"), Some(&Entry::Dir));
        assert!(matches!(
            index.get("bin/run.sh"),
            Some(Entry::File {
                executable: true,
                size: 10,
                ..
            })
        ));
        assert_eq!(
            fs::read_to_string(dir.path().join("site/index.html")).unwrap(),
            "<h1>hi</h1>"
        );
        let mode = |p: &str| fs::metadata(dir.path().join(p)).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode("bin/run.sh"), 0o755);
        assert_eq!(mode("site/index.html"), 0o644);
        assert_eq!(mode("empty"), 0o755);
    }

    #[test]
    fn same_files_give_the_same_digest_as_a_directory() {
        let (_dir, r) = try_extract(&[file("compose.yaml", b"a"), file("x/y", b"b")], &Limits::default());
        let from_zip = r.unwrap();
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("compose.yaml"), "a").unwrap();
        fs::create_dir(src.path().join("x")).unwrap();
        fs::write(src.path().join("x/y"), "b").unwrap();
        let dest = tempfile::tempdir().unwrap();
        let from_dir = crate::dir::copy(src.path(), dest.path(), &Limits::default()).unwrap();
        assert_eq!(from_zip.digest(), from_dir.digest());
    }

    #[test]
    fn refuses_path_traversal() {
        assert!(matches!(
            refused(&[file("../evil", b"x")]),
            BundleError::UnsafePath { .. }
        ));
        assert!(matches!(
            refused(&[file("site/../../evil", b"x")]),
            BundleError::UnsafePath { .. }
        ));
    }

    #[test]
    fn refuses_absolute_paths() {
        assert!(matches!(
            refused(&[file("/etc/cron.d/evil", b"x")]),
            BundleError::UnsafePath { .. }
        ));
        assert!(matches!(
            refused(&[file("C:/evil", b"x")]),
            BundleError::UnsafePath { .. }
        ));
        assert!(matches!(
            refused(&[file("site\\..\\evil", b"x")]),
            BundleError::UnsafePath { .. }
        ));
    }

    #[test]
    fn refuses_symlinks() {
        let e = refused(&[Raw {
            name: "link",
            mode: 0o120777,
            data: b"/etc/passwd",
            claimed_size: None,
        }]);
        assert!(matches!(e, BundleError::Symlink(_)), "{e}");
    }

    #[test]
    fn refuses_special_files() {
        for mode in [0o020644, 0o060644, 0o010644, 0o140644] {
            let e = refused(&[Raw {
                name: "dev",
                mode,
                data: b"",
                claimed_size: None,
            }]);
            assert!(matches!(e, BundleError::SpecialFile(_)), "mode {mode:o}: {e}");
        }
    }

    #[test]
    fn refuses_duplicates() {
        let e = refused(&[file("a.txt", b"1"), file("a.txt", b"2")]);
        assert!(matches!(e, BundleError::DuplicateInZip), "{e}");
        let e = refused(&[
            Raw {
                name: "d/",
                mode: 0o040755,
                data: b"",
                claimed_size: None,
            },
            Raw {
                name: "d/",
                mode: 0o040755,
                data: b"",
                claimed_size: None,
            },
        ]);
        assert!(matches!(e, BundleError::DuplicateInZip), "{e}");
    }

    #[test]
    fn refuses_a_file_that_is_also_a_directory() {
        let e = refused(&[file("a", b"1"), file("a/b", b"2")]);
        assert!(matches!(e, BundleError::Conflict(_)), "{e}");
        let e = refused(&[file("a/b", b"2"), file("a", b"1")]);
        assert!(matches!(e, BundleError::Conflict(_)), "{e}");
    }

    #[test]
    fn refuses_too_many_files() {
        let limits = Limits {
            max_files: 2,
            ..Limits::default()
        };
        let (_d, r) = try_extract(&[file("a", b""), file("b", b""), file("c", b"")], &limits);
        assert!(matches!(r, Err(BundleError::TooLarge(_))));
    }

    #[test]
    fn refuses_a_large_archive() {
        let limits = Limits {
            max_archive_size: 100,
            ..Limits::default()
        };
        let (_d, r) = try_extract(&[file("a", &[b'x'; 200])], &limits);
        assert!(matches!(r, Err(BundleError::TooLarge(_))));
    }

    #[test]
    fn refuses_a_large_file() {
        let limits = Limits {
            max_file_size: 10,
            ..Limits::default()
        };
        let (_d, r) = try_extract(&[file("a", &[b'x'; 11])], &limits);
        assert!(matches!(r, Err(BundleError::TooLarge(_))));
    }

    #[test]
    fn refuses_a_large_expanded_size() {
        let limits = Limits {
            max_expanded_size: 15,
            ..Limits::default()
        };
        let (_d, r) = try_extract(&[file("a", &[b'x'; 10]), file("b", &[b'x'; 10])], &limits);
        assert!(matches!(r, Err(BundleError::TooLarge(_))));
    }

    #[test]
    fn refuses_data_beyond_the_declared_size() {
        // The headers claim 1 byte, but 64 follow: whatever the ZIP reader makes of it, nothing over the limit
        // may be written.
        let limits = Limits {
            max_file_size: 8,
            max_expanded_size: 8,
            ..Limits::default()
        };
        let (dir, r) = try_extract(
            &[Raw {
                name: "a",
                mode: 0o100644,
                data: &[b'x'; 64],
                claimed_size: Some(1),
            }],
            &limits,
        );
        if r.is_ok() {
            assert!(fs::metadata(dir.path().join("a")).unwrap().len() <= 8);
        }
    }

    #[test]
    fn refuses_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let r = extract(Cursor::new(b"not a zip".to_vec()), dir.path(), &Limits::default());
        assert!(matches!(r, Err(BundleError::Zip(_))));
    }
}

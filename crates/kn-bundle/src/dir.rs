//! Reading a bundle from a directory: copying it into place, or packing it into a ZIP to send.
//!
//! The same rules apply as to a ZIP: only plain files and directories, under [`Limits`].

use std::fs;
use std::io::{Cursor, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::files::{check_name, copy_limited, open_regular};
use crate::{BundleError, FileIndex, Limits};

enum Item<'a> {
    Dir,
    File { file: &'a mut fs::File, executable: bool },
}

struct Walk<'l> {
    limits: &'l Limits,
    entries: usize,
    total: u64,
}

impl Walk<'_> {
    fn walk(
        &mut self,
        root: &Path,
        rel: &str,
        visit: &mut dyn FnMut(&str, Item) -> Result<(), BundleError>,
    ) -> Result<(), BundleError> {
        let here = if rel.is_empty() {
            root.to_path_buf()
        } else {
            root.join(rel)
        };
        let mut names = Vec::new();
        for entry in fs::read_dir(&here).map_err(|e| BundleError::io(here.display(), e))? {
            let entry = entry.map_err(|e| BundleError::io(here.display(), e))?;
            let name = entry.file_name().into_string().map_err(|n| BundleError::UnsafePath {
                path: format!("{rel}/{}", n.to_string_lossy()),
                reason: "isn't valid UTF-8",
            })?;
            names.push(name);
        }
        names.sort();
        for name in names {
            let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let (path, _) = check_name(&path, self.limits)?;
            self.entries += 1;
            if self.entries > self.limits.max_files {
                return Err(BundleError::TooLarge(format!(
                    "the directory has more than {} entries",
                    self.limits.max_files
                )));
            }
            let full = root.join(&path);
            let meta = fs::symlink_metadata(&full).map_err(|e| BundleError::io(&path, e))?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                return Err(BundleError::Symlink(path));
            } else if ft.is_dir() {
                visit(&path, Item::Dir)?;
                self.walk(root, &path, visit)?;
            } else if ft.is_file() {
                let mut file = open_regular(&full).map_err(|e| match e {
                    BundleError::Symlink(_) => BundleError::Symlink(path.clone()),
                    BundleError::SpecialFile(_) => BundleError::SpecialFile(path.clone()),
                    e => e,
                })?;
                let size = file.metadata().map_err(|e| BundleError::io(&path, e))?.len();
                if size > self.limits.max_file_size {
                    return Err(BundleError::TooLarge(format!(
                        "`{path}` is {size} bytes; the limit is {}",
                        self.limits.max_file_size
                    )));
                }
                self.total += size;
                if self.total > self.limits.max_expanded_size {
                    return Err(BundleError::TooLarge(format!(
                        "the files add up to more than {} bytes",
                        self.limits.max_expanded_size
                    )));
                }
                let executable = meta.permissions().mode() & 0o111 != 0;
                visit(
                    &path,
                    Item::File {
                        file: &mut file,
                        executable,
                    },
                )?;
            } else {
                return Err(BundleError::SpecialFile(path));
            }
        }
        Ok(())
    }
}

fn check_root(src: &Path) -> Result<(), BundleError> {
    let meta = fs::metadata(src).map_err(|e| BundleError::io(src.display(), e))?;
    if !meta.is_dir() {
        return Err(BundleError::io(
            src.display(),
            std::io::Error::new(std::io::ErrorKind::NotADirectory, "not a directory"),
        ));
    }
    Ok(())
}

/// Copies the bundle in `src` into `dest`, which must be an empty directory, and returns its index.
///
/// On error, `dest` may hold part of the bundle; the caller removes it.
pub fn copy(src: &Path, dest: &Path, limits: &Limits) -> Result<FileIndex, BundleError> {
    check_root(src)?;
    let mut index = FileIndex::new();
    let mut written: u64 = 0;
    Walk {
        limits,
        entries: 0,
        total: 0,
    }
    .walk(src, "", &mut |path, item| {
        let full = dest.join(path);
        match item {
            Item::Dir => {
                fs::create_dir(&full).map_err(|e| BundleError::io(path, e))?;
                fs::set_permissions(&full, fs::Permissions::from_mode(0o755)).map_err(|e| BundleError::io(path, e))?;
                index.add_dir(path)
            }
            Item::File { file, executable } => {
                let mode = if executable { 0o755 } else { 0o644 };
                let mut out = fs::File::options()
                    .write(true)
                    .create_new(true)
                    .mode(mode)
                    .open(&full)
                    .map_err(|e| BundleError::io(path, e))?;
                // The file may have grown since it was measured, so count again.
                let left = limits.max_expanded_size - written;
                let (limit, what) = if limits.max_file_size <= left {
                    (limits.max_file_size, "for one file")
                } else {
                    (left, "for the whole bundle")
                };
                let (n, sha) = copy_limited(file, &mut out, limit, path, what)?;
                out.set_permissions(fs::Permissions::from_mode(mode))
                    .map_err(|e| BundleError::io(path, e))?;
                written += n;
                index.add_file(path, n, executable, sha)
            }
        }
    })?;
    Ok(index)
}

/// Packs the bundle in `src` into a ZIP, refusing what a node would refuse.
pub fn pack(src: &Path, limits: &Limits) -> Result<Vec<u8>, BundleError> {
    check_root(src)?;
    let mut zip = ZipWriter::new(Cursor::new(Vec::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    let zip_err = |e: zip::result::ZipError| BundleError::Zip(e.to_string());
    let mut written: u64 = 0;
    Walk {
        limits,
        entries: 0,
        total: 0,
    }
    .walk(src, "", &mut |path, item| match item {
        Item::Dir => zip
            .add_directory(format!("{path}/"), opts.unix_permissions(0o755))
            .map_err(zip_err),
        Item::File { file, executable } => {
            let mode = if executable { 0o755 } else { 0o644 };
            zip.start_file(path, opts.unix_permissions(mode)).map_err(zip_err)?;
            let left = limits.max_expanded_size - written;
            let (n, _) = copy_limited(file, &mut zip, limits.max_file_size.min(left), path, "")?;
            written += n;
            Ok(())
        }
    })?;
    let mut out = zip.finish().map_err(zip_err)?;
    out.flush().map_err(|e| BundleError::io("packing the bundle", e))?;
    let bytes = out.into_inner();
    if bytes.len() as u64 > limits.max_archive_size {
        return Err(BundleError::TooLarge(format!(
            "the packed bundle is {} bytes; the limit is {}",
            bytes.len(),
            limits.max_archive_size
        )));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    fn bundle() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        fs::write(d.path().join("compose.yaml"), "services: {}\n").unwrap();
        fs::create_dir_all(d.path().join("site/css")).unwrap();
        fs::write(d.path().join("site/index.html"), "hi").unwrap();
        fs::create_dir(d.path().join("empty")).unwrap();
        d
    }

    fn copy_err(src: &Path) -> BundleError {
        let dest = tempfile::tempdir().unwrap();
        copy(src, dest.path(), &Limits::default()).expect_err("should be refused")
    }

    #[test]
    fn copies_files_and_directories() {
        let src = bundle();
        let dest = tempfile::tempdir().unwrap();
        let index = copy(src.path(), dest.path(), &Limits::default()).unwrap();
        assert!(index.is_file("site/index.html"));
        assert!(index.contains("site/css"));
        assert!(index.contains("empty"));
        assert_eq!(fs::read_to_string(dest.path().join("site/index.html")).unwrap(), "hi");
    }

    #[test]
    fn pack_round_trips_through_extract() {
        let src = bundle();
        let zip = pack(src.path(), &Limits::default()).unwrap();
        let dest = tempfile::tempdir().unwrap();
        let from_zip = crate::zip::extract(Cursor::new(zip), dest.path(), &Limits::default()).unwrap();
        let dest2 = tempfile::tempdir().unwrap();
        let from_dir = copy(src.path(), dest2.path(), &Limits::default()).unwrap();
        assert_eq!(from_zip, from_dir);
    }

    #[test]
    fn refuses_symlinks() {
        let src = bundle();
        std::os::unix::fs::symlink("/etc/passwd", src.path().join("site/passwd")).unwrap();
        assert!(matches!(copy_err(src.path()), BundleError::Symlink(_)));
        assert!(matches!(
            pack(src.path(), &Limits::default()),
            Err(BundleError::Symlink(_))
        ));
    }

    #[test]
    fn refuses_symlinked_directories() {
        let src = bundle();
        std::os::unix::fs::symlink("/etc", src.path().join("etc")).unwrap();
        assert!(matches!(copy_err(src.path()), BundleError::Symlink(_)));
    }

    #[test]
    fn refuses_special_files() {
        let src = bundle();
        let fifo = CString::new(src.path().join("fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        assert!(matches!(copy_err(src.path()), BundleError::SpecialFile(_)));
    }

    #[test]
    fn refuses_unsafe_names() {
        let src = bundle();
        fs::write(src.path().join("a\\b"), "x").unwrap();
        assert!(matches!(copy_err(src.path()), BundleError::UnsafePath { .. }));
    }

    #[test]
    fn enforces_limits() {
        let src = bundle();
        let dest = tempfile::tempdir().unwrap();
        let r = copy(
            src.path(),
            dest.path(),
            &Limits {
                max_files: 2,
                ..Limits::default()
            },
        );
        assert!(matches!(r, Err(BundleError::TooLarge(_))));
        let dest = tempfile::tempdir().unwrap();
        let r = copy(
            src.path(),
            dest.path(),
            &Limits {
                max_file_size: 5,
                ..Limits::default()
            },
        );
        assert!(matches!(r, Err(BundleError::TooLarge(_))));
        let dest = tempfile::tempdir().unwrap();
        let r = copy(
            src.path(),
            dest.path(),
            &Limits {
                max_expanded_size: 14,
                ..Limits::default()
            },
        );
        assert!(matches!(r, Err(BundleError::TooLarge(_))));
    }
}

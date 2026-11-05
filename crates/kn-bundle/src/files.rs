//! The index of a bundle's files, and the rules for their names.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::{BundleError, Limits};

/// One file or directory in a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Dir,
    File {
        size: u64,
        executable: bool,
        sha256: [u8; 32],
    },
}

/// Every file and directory in a bundle, by path relative to its root (`/`-separated, no leading `/`).
///
/// The root itself is the empty path, and is always a directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileIndex {
    entries: BTreeMap<String, Entry>,
    /// Paths that were listed explicitly (as opposed to parents implied by a deeper path).
    listed: BTreeSet<String>,
}

impl FileIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a directory, and any parents not yet present.
    pub fn add_dir(&mut self, path: &str) -> Result<(), BundleError> {
        self.list(path)?;
        self.add_parents(path)?;
        match self.entries.get(path) {
            Some(Entry::File { .. }) => Err(BundleError::Conflict(path.to_string())),
            _ => {
                self.entries.insert(path.to_string(), Entry::Dir);
                Ok(())
            }
        }
    }

    /// Adds a file, and any parents not yet present.
    pub fn add_file(&mut self, path: &str, size: u64, executable: bool, sha256: [u8; 32]) -> Result<(), BundleError> {
        self.list(path)?;
        self.add_parents(path)?;
        if self.entries.contains_key(path) {
            return Err(BundleError::Conflict(path.to_string()));
        }
        self.entries.insert(
            path.to_string(),
            Entry::File {
                size,
                executable,
                sha256,
            },
        );
        Ok(())
    }

    /// Replaces an existing file's size and hash, once its contents have been read.
    pub(crate) fn set_contents(&mut self, path: &str, new_size: u64, new_sha256: [u8; 32]) {
        if let Some(Entry::File { size, sha256, .. }) = self.entries.get_mut(path) {
            *size = new_size;
            *sha256 = new_sha256;
        }
    }

    fn list(&mut self, path: &str) -> Result<(), BundleError> {
        if path.is_empty() || !self.listed.insert(path.to_string()) {
            return Err(BundleError::Duplicate(path.to_string()));
        }
        Ok(())
    }

    fn add_parents(&mut self, path: &str) -> Result<(), BundleError> {
        let mut end = 0;
        while let Some(i) = path[end..].find('/') {
            let parent = &path[..end + i];
            match self.entries.get(parent) {
                Some(Entry::File { .. }) => return Err(BundleError::Conflict(parent.to_string())),
                Some(Entry::Dir) => {}
                None => {
                    self.entries.insert(parent.to_string(), Entry::Dir);
                }
            }
            end += i + 1;
        }
        Ok(())
    }

    /// The entry at `path`; the empty path is the root directory.
    pub fn get(&self, path: &str) -> Option<&Entry> {
        if path.is_empty() {
            return Some(&Entry::Dir);
        }
        self.entries.get(path)
    }

    pub fn is_file(&self, path: &str) -> bool {
        matches!(self.get(path), Some(Entry::File { .. }))
    }

    pub fn contains(&self, path: &str) -> bool {
        self.get(path).is_some()
    }

    /// Every entry, in path order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Entry)> {
        self.entries.iter().map(|(p, e)| (p.as_str(), e))
    }

    /// The number of entries, directories included.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The total size of all files.
    pub fn total_size(&self) -> u64 {
        self.entries
            .values()
            .map(|e| match e {
                Entry::File { size, .. } => *size,
                Entry::Dir => 0,
            })
            .sum()
    }

    /// A digest of the bundle's contents (`sha256:` and hex): its paths, file contents and executable bits.
    ///
    /// The same files give the same digest whether they came from a ZIP or a directory.
    pub fn digest(&self) -> String {
        let mut h = Sha256::new();
        for (path, entry) in &self.entries {
            match entry {
                Entry::Dir => h.update(b"d"),
                Entry::File { .. } => h.update(b"f"),
            }
            h.update((path.len() as u64).to_be_bytes());
            h.update(path.as_bytes());
            if let Entry::File {
                size,
                executable,
                sha256,
            } = entry
            {
                h.update([u8::from(*executable)]);
                h.update(size.to_be_bytes());
                h.update(sha256);
            }
        }
        format!("sha256:{}", hex::encode(h.finalize()))
    }
}

/// Checks an entry's name and returns it normalized (no trailing `/`), with whether it ended in `/`.
pub(crate) fn check_name(raw: &str, limits: &Limits) -> Result<(String, bool), BundleError> {
    let unsafe_path = |reason| BundleError::UnsafePath {
        path: raw.to_string(),
        reason,
    };
    if raw.len() > limits.max_path_len {
        return Err(BundleError::TooLarge(format!(
            "a path is {} bytes long; the limit is {}",
            raw.len(),
            limits.max_path_len
        )));
    }
    if raw.contains('\0') {
        return Err(unsafe_path("contains a NUL byte"));
    }
    if raw.chars().any(char::is_control) {
        return Err(unsafe_path("contains a control character"));
    }
    if raw.contains('\\') {
        return Err(unsafe_path("contains a backslash"));
    }
    if raw.starts_with('/') {
        return Err(unsafe_path("is an absolute path"));
    }
    let b = raw.as_bytes();
    if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
        return Err(unsafe_path("is a drive path"));
    }
    let is_dir = raw.ends_with('/');
    let name = raw.strip_suffix('/').unwrap_or(raw);
    if name.is_empty() {
        return Err(unsafe_path("is empty"));
    }
    let mut depth = 0;
    for part in name.split('/') {
        match part {
            "" => return Err(unsafe_path("has an empty path component")),
            "." => return Err(unsafe_path("has a `.` path component")),
            ".." => {
                return Err(unsafe_path(
                    "has a `..` path component, which could reach outside the bundle",
                ));
            }
            _ => depth += 1,
        }
    }
    if depth > limits.max_depth {
        return Err(BundleError::TooLarge(format!(
            "`{raw}` is {depth} levels deep; the limit is {}",
            limits.max_depth
        )));
    }
    Ok((name.to_string(), is_dir))
}

/// Opens a file for reading, refusing a symlink and anything but a regular file.
pub(crate) fn open_regular(path: &Path) -> Result<File, BundleError> {
    let f = File::options()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| {
            if e.raw_os_error() == Some(libc::ELOOP) {
                BundleError::Symlink(path.display().to_string())
            } else {
                BundleError::io(path.display(), e)
            }
        })?;
    let meta = f.metadata().map_err(|e| BundleError::io(path.display(), e))?;
    if !meta.is_file() {
        return Err(BundleError::SpecialFile(path.display().to_string()));
    }
    Ok(f)
}

/// Copies at most `limit` bytes from `from` into `to`, hashing them. Errors if `from` has more.
pub(crate) fn copy_limited(
    from: &mut impl std::io::Read,
    to: &mut impl std::io::Write,
    limit: u64,
    name: &str,
    what: &str,
) -> Result<(u64, [u8; 32]), BundleError> {
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut n: u64 = 0;
    loop {
        let read = from.read(&mut buf).map_err(|e| BundleError::io(name, e))?;
        if read == 0 {
            break;
        }
        n += read as u64;
        if n > limit {
            return Err(BundleError::TooLarge(format!(
                "`{name}` goes past the limit of {limit} bytes {what}"
            )));
        }
        h.update(&buf[..read]);
        to.write_all(&buf[..read]).map_err(|e| BundleError::io(name, e))?;
    }
    Ok((n, h.finalize().into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(raw: &str) -> Result<(String, bool), BundleError> {
        check_name(raw, &Limits::default())
    }

    #[test]
    fn accepts_plain_names() {
        assert_eq!(name("compose.yaml").unwrap(), ("compose.yaml".into(), false));
        assert_eq!(name("site/").unwrap(), ("site".into(), true));
        assert_eq!(name("site/a b/ü.html").unwrap(), ("site/a b/ü.html".into(), false));
        assert_eq!(name("..hidden").unwrap(), ("..hidden".into(), false));
    }

    #[test]
    fn refuses_unsafe_names() {
        for raw in [
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a/./b",
            "a//b",
            "",
            "/",
            "a\\b",
            "C:/x",
            "a\0b",
            "a\nb",
        ] {
            assert!(
                matches!(name(raw), Err(BundleError::UnsafePath { .. })),
                "{raw:?} should be refused"
            );
        }
    }

    #[test]
    fn refuses_long_and_deep_names() {
        let limits = Limits {
            max_path_len: 10,
            max_depth: 2,
            ..Limits::default()
        };
        assert!(matches!(
            check_name("abcdefghijk", &limits),
            Err(BundleError::TooLarge(_))
        ));
        assert!(matches!(check_name("a/b/c", &limits), Err(BundleError::TooLarge(_))));
        assert!(check_name("a/b", &limits).is_ok());
    }

    #[test]
    fn index_refuses_duplicates_and_conflicts() {
        let mut i = FileIndex::new();
        i.add_file("a/b", 1, false, [0; 32]).unwrap();
        i.add_dir("a").unwrap(); // listed after being implied: fine
        assert!(matches!(i.add_dir("a"), Err(BundleError::Duplicate(_))));
        assert!(matches!(
            i.add_file("a/b", 1, false, [0; 32]),
            Err(BundleError::Duplicate(_))
        ));
        assert!(matches!(
            i.add_file("a", 1, false, [0; 32]),
            Err(BundleError::Duplicate(_))
        ));
        assert!(matches!(i.add_dir("a/b/c"), Err(BundleError::Conflict(_))));
        i.add_file("x", 1, false, [0; 32]).unwrap();
        assert!(matches!(
            i.add_dir("x/"),
            Err(BundleError::Conflict(_)) | Err(BundleError::Duplicate(_))
        ));
    }

    #[test]
    fn digest_depends_on_contents() {
        let mut a = FileIndex::new();
        a.add_file("f", 1, false, [1; 32]).unwrap();
        let mut b = FileIndex::new();
        b.add_file("f", 1, false, [2; 32]).unwrap();
        let mut c = FileIndex::new();
        c.add_file("f", 1, true, [1; 32]).unwrap();
        assert_ne!(a.digest(), b.digest());
        assert_ne!(a.digest(), c.digest());
        assert_eq!(a.digest(), a.clone().digest());
        assert!(a.digest().starts_with("sha256:"));
    }
}

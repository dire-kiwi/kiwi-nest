//! Where kiwi-nest is installed: every release side by side, and a link to the current one.
//!
//! ```text
//! /usr/local/lib/kiwi-nest/releases/VERSION/kiwi-nest   each release's binary, for this machine; old ones are kept
//! /usr/local/lib/kiwi-nest/releases/VERSION/dist/       on the server's host: the release's files for every Linux
//!                                                       target and its SHA256SUMS, offered to the nodes
//! /usr/local/lib/kiwi-nest/current -> releases/VERSION  the release in use, swapped by one rename
//! /usr/local/bin/kiwi-nest -> /usr/local/lib/kiwi-nest/current/kiwi-nest   what the units run
//! ```
//!
//! A process can tell it runs from an installed release by its own path (`/proc/self/exe`, links resolved), which
//! is how a node knows it may upgrade itself, and where.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};
use kn_proto::version::Version;

/// The default root of the installed releases.
pub const ROOT: &str = "/usr/local/lib/kiwi-nest";

/// The link on the PATH to the current release's binary, which the systemd units run.
pub const BIN: &str = "/usr/local/bin/kiwi-nest";

/// The binary's name in each release's directory.
pub const BINARY: &str = "kiwi-nest";

/// The releases installed under a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Layout {
        Layout { root: root.into() }
    }

    /// `/usr/local/lib/kiwi-nest`.
    pub fn system() -> Layout {
        Layout::new(ROOT)
    }

    /// The layout a binary at `exe` (links resolved) was run from, if it's an installed release:
    /// `ROOT/releases/VERSION/kiwi-nest`.
    pub fn of_binary(exe: &Path) -> Option<Layout> {
        if exe.file_name()? != BINARY {
            return None;
        }
        let version = exe.parent()?;
        Version::parse(version.file_name()?.to_str()?)?;
        let releases = version.parent()?;
        if releases.file_name()? != "releases" {
            return None;
        }
        Some(Layout::new(releases.parent()?))
    }

    /// The layout this process runs from, if it runs from an installed release.
    pub fn running() -> Option<Layout> {
        let exe = std::env::current_exe().ok()?;
        Layout::of_binary(&std::fs::canonicalize(&exe).unwrap_or(exe))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn releases(&self) -> PathBuf {
        self.root.join("releases")
    }

    pub fn release_dir(&self, version: &str) -> PathBuf {
        self.releases().join(version)
    }

    /// A release's binary, for this machine.
    pub fn binary(&self, version: &str) -> PathBuf {
        self.release_dir(version).join(BINARY)
    }

    /// A release's files for every target, on the server's host.
    pub fn dist(&self, version: &str) -> PathBuf {
        self.release_dir(version).join("dist")
    }

    /// The link to the current release.
    pub fn current_link(&self) -> PathBuf {
        self.root.join("current")
    }

    /// The current release's binary, through the link.
    pub fn current_binary(&self) -> PathBuf {
        self.current_link().join(BINARY)
    }

    /// The current release's version, if one is set.
    pub fn current(&self) -> Option<String> {
        let target = std::fs::read_link(self.current_link()).ok()?;
        Some(target.file_name()?.to_str()?.to_string())
    }

    /// The installed releases' versions, oldest first.
    pub fn installed(&self) -> anyhow::Result<Vec<String>> {
        let dir = self.releases();
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", dir.display())),
        };
        let mut found: Vec<(Version, String)> = Vec::new();
        for e in entries {
            let e = e?;
            let Some(name) = e.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(v) = Version::parse(&name) else { continue };
            if self.binary(&name).is_file() {
                found.push((v, name));
            }
        }
        found.sort();
        Ok(found.into_iter().map(|(_, n)| n).collect())
    }

    /// Where a release's binary is written before it's put in place: `.kiwi-nest.partial` in its directory, made
    /// if needed. A leftover from a cut-off download is replaced.
    pub fn staging(&self, version: &str) -> anyhow::Result<PathBuf> {
        check_version(version)?;
        let dir = self.release_dir(version);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let p = dir.join(format!(".{BINARY}.partial"));
        match std::fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", p.display())),
        }
        Ok(p)
    }

    /// Puts a staged binary in place as a release's: executable, synced, then renamed. If the release already has
    /// a binary, the new one replaces it only if it differs (same checksum: nothing to do).
    pub fn commit(&self, version: &str, staged: &Path) -> anyhow::Result<PathBuf> {
        let dest = self.binary(version);
        let f = std::fs::File::open(staged).with_context(|| format!("opening {}", staged.display()))?;
        f.set_permissions(std::fs::Permissions::from_mode(0o755))?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(staged, &dest).with_context(|| format!("installing {}", dest.display()))?;
        sync_dir(dest.parent().expect("a release's directory"))?;
        Ok(dest)
    }

    /// Copies a binary in as a release's (`kiwi-nest server init` and `node join` install themselves this way).
    /// Copying a release's own binary onto itself does nothing.
    pub fn install_file(&self, version: &str, src: &Path) -> anyhow::Result<PathBuf> {
        let dest = self.binary(version);
        if let (Ok(a), Ok(b)) = (std::fs::canonicalize(src), std::fs::canonicalize(&dest))
            && a == b
        {
            return Ok(dest);
        }
        let staged = self.staging(version)?;
        std::fs::copy(src, &staged).with_context(|| format!("copying {} to {}", src.display(), staged.display()))?;
        self.commit(version, &staged)
    }

    /// Makes a release the current one, replacing the link with one rename, so the link is always one release
    /// or the other.
    pub fn activate(&self, version: &str) -> anyhow::Result<()> {
        check_version(version)?;
        if !self.binary(version).is_file() {
            bail!("release {version} isn't installed in {}", self.releases().display());
        }
        let link = self.current_link();
        let tmp = self.root.join(".current.new");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(Path::new("releases").join(version), &tmp)
            .with_context(|| format!("making {}", tmp.display()))?;
        std::fs::rename(&tmp, &link).with_context(|| format!("replacing {}", link.display()))?;
        sync_dir(&self.root)
    }

    /// Points `bin` (such as `/usr/local/bin/kiwi-nest`) at the current release's binary.
    pub fn link_bin(&self, bin: &Path) -> anyhow::Result<()> {
        let want = self.current_binary();
        if std::fs::read_link(bin).ok().as_deref() == Some(want.as_path()) {
            return Ok(());
        }
        if let Ok(meta) = std::fs::symlink_metadata(bin)
            && !meta.file_type().is_symlink()
        {
            bail!(
                "{} is a file, not a link to an installed release; move it out of the way first",
                bin.display()
            );
        }
        let dir = bin.parent().context("the link's directory")?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(".kiwi-nest.link.new");
        let _ = std::fs::remove_file(&tmp);
        std::os::unix::fs::symlink(&want, &tmp).with_context(|| format!("making {}", tmp.display()))?;
        std::fs::rename(&tmp, bin).with_context(|| format!("replacing {}", bin.display()))?;
        Ok(())
    }

    /// Removes the oldest releases, keeping the newest `keep` and the current one. Returns what it removed.
    pub fn prune(&self, keep: usize) -> anyhow::Result<Vec<String>> {
        let installed = self.installed()?;
        let current = self.current();
        let n = installed.len().saturating_sub(keep);
        let mut removed = Vec::new();
        for v in installed.into_iter().take(n) {
            if current.as_deref() == Some(v.as_str()) {
                continue;
            }
            let dir = self.release_dir(&v);
            std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
            removed.push(v);
        }
        Ok(removed)
    }

    /// The newest installed release older than `than`.
    pub fn previous(&self, than: &str) -> anyhow::Result<Option<String>> {
        let than = Version::parse(than).with_context(|| format!("{than} isn't a version"))?;
        Ok(self
            .installed()?
            .into_iter()
            .rev()
            .find(|v| Version::parse(v).is_some_and(|v| v < than)))
    }
}

/// Refuses a version that couldn't be a directory's name safely.
pub fn check_version(version: &str) -> anyhow::Result<()> {
    if Version::parse(version).is_none() || version.starts_with('v') || version.contains('/') {
        bail!("{version:?} isn't a release version, such as 0.2.0");
    }
    Ok(())
}

fn sync_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("syncing {}", dir.display()))
}

/// Checks that a binary runs here and is the version it should be: `BINARY version` prints
/// `kiwi-nest VERSION (commit …)`. Catches a file for another architecture, or a damaged one, before it's swapped
/// in.
pub fn check_binary(path: &Path, version: &str) -> anyhow::Result<()> {
    let out = Command::new(path)
        .arg("version")
        .env("KIWI_NEST_LOG", "error")
        .output()
        .with_context(|| format!("running {} version", path.display()))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        bail!(
            "{} version failed ({}): {}",
            path.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let said = text
        .trim()
        .strip_prefix("kiwi-nest ")
        .and_then(|r| r.split_whitespace().next());
    if said != Some(version) {
        bail!(
            "{} says it's {:?}, not kiwi-nest {version}",
            path.display(),
            text.trim()
        );
    }
    Ok(())
}

/// Writes a file atomically: to a temporary file beside it, synced, then renamed over it.
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> anyhow::Result<()> {
    let dir = path.parent().context("a file's directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path.file_name().context("a file name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.new"));
    let mut f = std::fs::File::create(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.set_permissions(std::fs::Permissions::from_mode(mode))?;
    f.write_all(contents)?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_binary(path: &Path, version: &str) {
        std::fs::write(path, format!("#!/bin/sh\necho 'kiwi-nest {version} (commit test)'\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn finds_the_layout_from_a_binary() {
        let l = Layout::of_binary(Path::new("/usr/local/lib/kiwi-nest/releases/0.2.0/kiwi-nest")).unwrap();
        assert_eq!(l, Layout::system());
        for not in [
            "/usr/local/bin/kiwi-nest",
            "/home/me/src/kiwi-nest/target/debug/kiwi-nest",
            "/x/releases/latest/kiwi-nest",
            "/x/releases/0.2.0/other",
        ] {
            assert!(Layout::of_binary(Path::new(not)).is_none(), "{not}");
        }
    }

    #[test]
    fn installs_activates_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let l = Layout::new(dir.path().join("lib"));
        assert_eq!(l.current(), None);
        assert!(l.installed().unwrap().is_empty());
        let src = dir.path().join("src");
        for v in ["0.1.0", "0.10.0", "0.2.0", "0.3.0-rc.1"] {
            fake_binary(&src, v);
            l.install_file(v, &src).unwrap();
            check_binary(&l.binary(v), v).unwrap();
        }
        assert_eq!(l.installed().unwrap(), ["0.1.0", "0.2.0", "0.3.0-rc.1", "0.10.0"]);
        assert!(check_binary(&l.binary("0.2.0"), "0.1.0").is_err());

        l.activate("0.2.0").unwrap();
        assert_eq!(l.current().as_deref(), Some("0.2.0"));
        let bin = dir.path().join("bin/kiwi-nest");
        l.link_bin(&bin).unwrap();
        l.link_bin(&bin).unwrap();
        check_binary(&bin, "0.2.0").unwrap();
        l.activate("0.10.0").unwrap();
        check_binary(&bin, "0.10.0").unwrap();
        assert!(l.activate("9.9.9").is_err());
        assert!(l.activate("../x").is_err());

        assert_eq!(l.previous("0.10.0").unwrap().as_deref(), Some("0.3.0-rc.1"));
        assert_eq!(l.previous("0.1.0").unwrap(), None);
        // Installing a release's own binary onto itself is harmless.
        l.install_file("0.10.0", &l.binary("0.10.0")).unwrap();
        check_binary(&l.binary("0.10.0"), "0.10.0").unwrap();

        l.activate("0.1.0").unwrap();
        assert_eq!(l.prune(2).unwrap(), ["0.2.0"]);
        assert_eq!(l.installed().unwrap(), ["0.1.0", "0.3.0-rc.1", "0.10.0"]);

        // A file in the link's place isn't replaced.
        let file = dir.path().join("bin/other");
        std::fs::write(&file, b"x").unwrap();
        assert!(l.link_bin(&file).is_err());
    }
}

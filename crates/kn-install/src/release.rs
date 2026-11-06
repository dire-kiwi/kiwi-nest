//! Releases: their files' names, and fetching them with their checksums.
//!
//! A release on GitHub (tag `vVERSION`) has one file per target, `kiwi-nest-VERSION-TARGET`, for
//! `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (static, for servers and nodes) and the macOS CLI
//! (`aarch64-apple-darwin`, `x86_64-apple-darwin`), and `SHA256SUMS` listing them all. `make release` writes the
//! same files into `dist/`, so a directory of them can stand in for GitHub (`kiwi-nest upgrade --from DIR`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use kn_proto::version::Version;
use tokio::io::AsyncWriteExt;

use crate::checksums;

/// The file listing every release file's SHA-256.
pub const SUMS: &str = "SHA256SUMS";

/// The Linux targets every release has, for servers and nodes.
pub const LINUX_TARGETS: [&str; 2] = ["x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl"];

/// A release file's name.
pub fn asset(version: &str, target: &str) -> String {
    format!("kiwi-nest-{version}-{target}")
}

/// The target a release file is for, if it's one of `version`'s.
pub fn target_of<'a>(name: &'a str, version: &str) -> Option<&'a str> {
    let target = name
        .strip_prefix("kiwi-nest-")?
        .strip_prefix(version)?
        .strip_prefix('-')?;
    // A pre-release's name starts like its release's: `kiwi-nest-0.2.0-rc.1-…` isn't 0.2.0's.
    ARCHES.iter().any(|a| target.starts_with(a)).then_some(target)
}

/// What a target's name starts with, for the architectures there are releases for.
const ARCHES: [&str; 2] = ["x86_64-", "aarch64-"];

/// The architecture, as nodes report it, of a Linux target: `amd64` or `arm64`.
pub fn linux_arch(target: &str) -> Option<&'static str> {
    if !target.contains("-linux") {
        return None;
    }
    match target.split('-').next()? {
        "x86_64" => Some("amd64"),
        "aarch64" => Some("arm64"),
        _ => None,
    }
}

/// The Linux release target for this machine's architecture.
pub fn this_linux_target() -> anyhow::Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok(LINUX_TARGETS[0]),
        "aarch64" => Ok(LINUX_TARGETS[1]),
        a => bail!("there's no kiwi-nest release for {a}"),
    }
}

/// Where releases come from.
#[derive(Debug, Clone)]
pub enum Source {
    /// A repository's releases on GitHub, such as `dire-kiwi/kiwi-nest`.
    GitHub { repository: String },
    /// A directory holding a release's files and its `SHA256SUMS`, as `make release` writes them.
    Dir(PathBuf),
}

impl Source {
    pub fn github() -> Source {
        Source::GitHub {
            repository: kn_proto::REPOSITORY.into(),
        }
    }

    /// The newest release: GitHub's latest, or the one a directory's `SHA256SUMS` lists.
    pub async fn latest(&self, http: &reqwest::Client) -> anyhow::Result<String> {
        match self {
            Source::GitHub { repository } => {
                let url = format!("https://api.github.com/repos/{repository}/releases/latest");
                let resp = http
                    .get(&url)
                    .header(reqwest::header::ACCEPT, "application/vnd.github+json")
                    .send()
                    .await
                    .with_context(|| format!("asking GitHub for {repository}'s latest release"))?;
                if !resp.status().is_success() {
                    bail!("GitHub answered {} for {repository}'s latest release", resp.status());
                }
                let body: serde_json::Value = resp.json().await?;
                let tag = body["tag_name"]
                    .as_str()
                    .context("GitHub's latest release has no tag")?;
                let v =
                    Version::parse(tag).with_context(|| format!("the latest release's tag {tag} isn't a version"))?;
                Ok(v.to_string())
            }
            Source::Dir(dir) => {
                let text = std::fs::read_to_string(dir.join(SUMS))
                    .with_context(|| format!("reading {}", dir.join(SUMS).display()))?;
                let mut versions: Vec<Version> = checksums::parse(&text)?
                    .keys()
                    .filter_map(|n| {
                        let rest = n.strip_prefix("kiwi-nest-")?;
                        // kiwi-nest-VERSION-TARGET, where VERSION may hold `-` and TARGET starts with an
                        // architecture.
                        rest.match_indices('-')
                            .filter(|(i, _)| ARCHES.iter().any(|a| rest[i + 1..].starts_with(a)))
                            .find_map(|(i, _)| Version::parse(&rest[..i]))
                    })
                    .collect();
                versions.sort();
                versions.dedup();
                match versions.as_slice() {
                    [v] => Ok(v.to_string()),
                    [] => bail!("{} lists no kiwi-nest release files", dir.join(SUMS).display()),
                    _ => bail!(
                        "{} lists several releases; say which with --version",
                        dir.join(SUMS).display()
                    ),
                }
            }
        }
    }

    /// Fetches one of a release's files into `dest`.
    async fn fetch(&self, http: &reqwest::Client, version: &str, name: &str, dest: &Path) -> anyhow::Result<()> {
        match self {
            Source::Dir(dir) => {
                let src = dir.join(name);
                tokio::fs::copy(&src, dest)
                    .await
                    .with_context(|| format!("copying {}", src.display()))?;
                Ok(())
            }
            Source::GitHub { repository } => {
                let url = format!("https://github.com/{repository}/releases/download/v{version}/{name}");
                let resp = http
                    .get(&url)
                    .send()
                    .await
                    .with_context(|| format!("downloading {url}"))?;
                if !resp.status().is_success() {
                    bail!("downloading {url}: {}", resp.status());
                }
                let mut out = tokio::fs::File::create(dest)
                    .await
                    .with_context(|| format!("writing {}", dest.display()))?;
                let mut body = resp.bytes_stream();
                while let Some(chunk) = body.next().await {
                    out.write_all(&chunk.with_context(|| format!("downloading {url}"))?)
                        .await?;
                }
                out.sync_all().await?;
                Ok(())
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Source::GitHub { repository } => format!("GitHub ({repository})"),
            Source::Dir(d) => d.display().to_string(),
        }
    }
}

/// A release file, checked against `SHA256SUMS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct File {
    pub target: String,
    pub path: PathBuf,
    pub sha256: String,
    pub size: u64,
}

/// Fetches a release's files for `targets` into `dir`, with its `SHA256SUMS`, checking each file against it. A file
/// is written beside its place and renamed only once it matches, so `dir` never holds a file that doesn't. A target
/// in `required` must be listed; the others are skipped if they aren't (a build of one's own, for one architecture).
pub async fn fetch(
    source: &Source,
    http: &reqwest::Client,
    version: &str,
    targets: &[&str],
    required: &[&str],
    dir: &Path,
) -> anyhow::Result<Vec<File>> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating {}", dir.display()))?;
    let sums_tmp = dir.join(format!(".{SUMS}.partial"));
    source.fetch(http, version, SUMS, &sums_tmp).await?;
    let text = tokio::fs::read_to_string(&sums_tmp).await?;
    let sums = checksums::parse(&text)?;
    let mut files = Vec::new();
    for target in targets {
        let name = asset(version, target);
        let Some(want) = sums.get(&name) else {
            if required.contains(target) {
                bail!("release {version}'s {SUMS} doesn't list {name}");
            }
            continue;
        };
        let tmp = dir.join(format!(".{name}.partial"));
        source.fetch(http, version, &name, &tmp).await?;
        let size = checksums::verify_file(&tmp, want).with_context(|| format!("checking {name}"))?;
        let path = dir.join(&name);
        tokio::fs::rename(&tmp, &path).await?;
        files.push(File {
            target: target.to_string(),
            path,
            sha256: want.clone(),
            size,
        });
    }
    tokio::fs::rename(&sums_tmp, dir.join(SUMS)).await?;
    Ok(files)
}

/// The Linux release files in a `dist` directory (`kiwi-nest upgrade` fetched them for the server to offer), each
/// checked against the directory's `SHA256SUMS`; those that are missing or don't match are left out, and said why.
pub fn read_dist(dir: &Path, version: &str) -> anyhow::Result<(BTreeMap<String, File>, Vec<String>)> {
    let sums_path = dir.join(SUMS);
    let text = match std::fs::read_to_string(&sums_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((BTreeMap::new(), Vec::new())),
        Err(e) => return Err(e).with_context(|| format!("reading {}", sums_path.display())),
    };
    let sums = checksums::parse(&text).with_context(|| format!("reading {}", sums_path.display()))?;
    let (mut files, mut problems) = (BTreeMap::new(), Vec::new());
    for (name, want) in &sums {
        let Some(target) = target_of(name, version) else {
            continue;
        };
        if linux_arch(target).is_none() {
            continue;
        }
        let path = dir.join(name);
        if !path.exists() {
            continue;
        }
        match checksums::verify_file(&path, want) {
            Ok(size) => {
                files.insert(
                    target.to_string(),
                    File {
                        target: target.to_string(),
                        path,
                        sha256: want.clone(),
                        size,
                    },
                );
            }
            Err(e) => problems.push(format!("{e:#}")),
        }
    }
    Ok((files, problems))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_targets() {
        let n = asset("0.2.0-rc.1", "aarch64-unknown-linux-musl");
        assert_eq!(n, "kiwi-nest-0.2.0-rc.1-aarch64-unknown-linux-musl");
        assert_eq!(target_of(&n, "0.2.0-rc.1"), Some("aarch64-unknown-linux-musl"));
        assert_eq!(target_of(&n, "0.2.0"), None);
        assert_eq!(linux_arch("x86_64-unknown-linux-musl"), Some("amd64"));
        assert_eq!(linux_arch("x86_64-unknown-linux-gnu"), Some("amd64"));
        assert_eq!(linux_arch("aarch64-unknown-linux-musl"), Some("arm64"));
        assert_eq!(linux_arch("aarch64-apple-darwin"), None);
    }

    fn release(dir: &Path, version: &str, files: &[(&str, &[u8])]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut sums = String::new();
        for (target, body) in files {
            let name = asset(version, target);
            std::fs::write(dir.join(&name), body).unwrap();
            let (sum, _) = checksums::sha256_file(&dir.join(&name)).unwrap();
            sums.push_str(&format!("{sum}  {name}\n"));
        }
        std::fs::write(dir.join(SUMS), sums).unwrap();
    }

    #[tokio::test]
    async fn fetches_from_a_directory_and_checks() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("dist");
        release(
            &src,
            "0.2.0-rc.1",
            &[
                ("x86_64-unknown-linux-musl", b"amd"),
                ("aarch64-unknown-linux-musl", b"arm"),
                ("aarch64-apple-darwin", b"mac"),
            ],
        );
        let source = Source::Dir(src.clone());
        let http = crate::http(None).unwrap();
        assert_eq!(source.latest(&http).await.unwrap(), "0.2.0-rc.1");
        let out = tmp.path().join("out");
        let files = fetch(&source, &http, "0.2.0-rc.1", &LINUX_TARGETS, &[], &out)
            .await
            .unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(std::fs::read(&files[1].path).unwrap(), b"arm");
        let (dist, problems) = read_dist(&out, "0.2.0-rc.1").unwrap();
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            dist.keys().collect::<Vec<_>>(),
            ["aarch64-unknown-linux-musl", "x86_64-unknown-linux-musl"]
        );

        // A damaged file is refused, and never put in place.
        std::fs::write(src.join(asset("0.2.0-rc.1", "x86_64-unknown-linux-musl")), b"evil").unwrap();
        let out2 = tmp.path().join("out2");
        let err = fetch(&source, &http, "0.2.0-rc.1", &LINUX_TARGETS, &[], &out2)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("doesn't match its checksum"), "{err:#}");
        assert!(!out2.join(asset("0.2.0-rc.1", "x86_64-unknown-linux-musl")).exists());
        // A required file a release doesn't list is refused; one that isn't required is skipped.
        let riscv = ["riscv64gc-unknown-linux-musl"];
        assert!(
            fetch(&source, &http, "0.2.0-rc.1", &riscv, &riscv, &out2)
                .await
                .is_err()
        );
        let out3 = tmp.path().join("out3");
        let some = fetch(&source, &http, "0.2.0-rc.1", &[riscv[0], LINUX_TARGETS[1]], &[], &out3)
            .await
            .unwrap();
        assert_eq!(some.len(), 1);
        assert_eq!(some[0].target, LINUX_TARGETS[1]);

        // A file changed after it was fetched is left out of what's offered.
        std::fs::write(&files[0].path, b"changed").unwrap();
        let (dist, problems) = read_dist(&out, "0.2.0-rc.1").unwrap();
        assert_eq!(dist.len(), 1);
        assert_eq!(problems.len(), 1);
    }
}

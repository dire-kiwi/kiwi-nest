//! The bundle store: ZIPs named by their digest.
//!
//! ```text
//! STATE/bundles/sha256-HEX.zip   a bundle (or a shared volume's contents), named by its digest
//! STATE/bundles/tmp/             uploads being received and checked, emptied when the server starts
//! ```
//!
//! An upload is streamed to `tmp` under a size limit, extracted there and checked by the same rules a node uses, and
//! only moved into the store by the transaction that saves the change referring to it. So a refused deploy leaves
//! nothing. A file is written once and never changed, and the database keeps a row for each; a bundle that no app
//! or volume refers to any more (an app's previous bundle once it's redeployed, a deleted app's) is removed.
//! Bundles that two apps share are kept once.
//!
//! While the server copies its state for a backup, it holds bundle deletion off ([`Bundles::hold`]): files that
//! would be removed meanwhile are left, and swept afterwards, so every bundle the copied database refers to is
//! still there to be linked into the backup.

use std::path::{Path, PathBuf};

use axum::body::Body;
use futures_util::StreamExt;
use kn_bundle::compose::Mount;
use kn_bundle::{CheckOptions, Limits};
use tokio::io::AsyncWriteExt;

use crate::error::{Error, Result};

/// The default memory counted for a service without a `mem_limit`.
pub const DEFAULT_SERVICE_MEMORY: u64 = 256 * 1024 * 1024;

pub struct Bundles {
    dir: PathBuf,
    tmp: PathBuf,
    /// How many backups are holding deletion off.
    holds: std::sync::Mutex<usize>,
}

/// Holds bundle deletion off until dropped.
pub struct Hold<'a>(&'a Bundles);

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        *self.0.holds.lock().expect("holds") -= 1;
    }
}

/// Whether `d` is a digest kiwi-nest makes: `sha256:` and 64 lowercase hex digits.
pub fn valid_digest(d: &str) -> bool {
    d.strip_prefix("sha256:")
        .is_some_and(|h| h.len() == 64 && h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)))
}

impl Bundles {
    pub fn open(dir: PathBuf) -> anyhow::Result<Bundles> {
        let tmp = dir.join("tmp");
        std::fs::create_dir_all(&dir)?;
        match std::fs::remove_dir_all(&tmp) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        std::fs::create_dir_all(&tmp)?;
        Ok(Bundles {
            dir,
            tmp,
            holds: std::sync::Mutex::new(0),
        })
    }

    /// Holds deletion off, until the guard is dropped: [`Bundles::remove`] leaves files in place meanwhile, for
    /// [`crate::store::apps::sweep`] to remove later.
    pub fn hold(&self) -> Hold<'_> {
        *self.holds.lock().expect("holds") += 1;
        Hold(self)
    }

    /// Where the bundle with this digest is kept.
    pub fn path(&self, digest: &str) -> Option<PathBuf> {
        valid_digest(digest).then(|| self.dir.join(format!("{}.zip", digest.replace(':', "-"))))
    }

    pub fn temp_file(&self) -> Result<tempfile::NamedTempFile> {
        Ok(tempfile::Builder::new().prefix("upload-").tempfile_in(&self.tmp)?)
    }

    pub fn temp_dir(&self) -> Result<tempfile::TempDir> {
        Ok(tempfile::Builder::new().prefix("check-").tempdir_in(&self.tmp)?)
    }

    /// Moves an upload into the store as `digest`, unless that bundle is there already.
    pub fn adopt(&self, upload: &Path, digest: &str) -> Result<()> {
        let dest = self
            .path(digest)
            .ok_or_else(|| Error::Internal(anyhow::anyhow!("bad digest {digest}")))?;
        if !dest.exists() {
            std::fs::rename(upload, &dest)?;
            std::fs::File::open(&self.dir)?.sync_all()?;
        }
        Ok(())
    }

    pub fn remove(&self, digest: &str) {
        // Checked and acted on under the lock, so a hold taken meanwhile waits for the removal to finish.
        let holds = self.holds.lock().expect("holds");
        if *holds > 0 {
            tracing::debug!(digest, "kept a bundle while the server's state is copied for a backup");
            return;
        }
        if let Some(p) = self.path(digest) {
            match std::fs::remove_file(&p) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    tracing::warn!(path = %p.display(), error = %e, "couldn't remove a bundle")
                }
                _ => tracing::debug!(digest, "removed a bundle"),
            }
        }
    }

    /// The digests of the bundle files in the store.
    pub fn digests(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(&self.dir)? {
            let name = e?.file_name().to_string_lossy().into_owned();
            if let Some(d) = name.strip_suffix(".zip").map(|n| n.replacen('-', ":", 1))
                && valid_digest(&d)
            {
                out.push(d);
            }
        }
        Ok(out)
    }
}

/// Streams a request body into `file`, refusing more than `limit` bytes; returns its size.
pub async fn receive(body: Body, file: &tempfile::NamedTempFile, limit: u64) -> Result<u64> {
    let mut out = tokio::fs::File::from_std(file.reopen()?);
    let mut stream = body.into_data_stream();
    let mut n: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| Error::BadRequest(format!("reading the upload: {e}")))?;
        n += chunk.len() as u64;
        if n > limit {
            return Err(Error::TooLarge(format!("the upload is larger than {limit} bytes")));
        }
        out.write_all(&chunk).await?;
    }
    out.flush().await?;
    out.sync_all().await?;
    Ok(n)
}

/// What the server needs from a checked bundle.
#[derive(Debug, Clone)]
pub struct CheckedApp {
    pub digest: String,
    pub domain: String,
    pub arch: Option<String>,
    /// `x-kiwi-nest.nest`.
    pub nest: Option<String>,
    /// The labels its node must have.
    pub node_labels: std::collections::BTreeMap<String, String>,
    /// 0 for an always-on app.
    pub idle_timeout_secs: u64,
    /// The shared volumes it mounts.
    pub shared: Vec<String>,
    /// Those it mounts read-write.
    pub shared_rw: Vec<String>,
    /// The host paths it mounts, which its node must allow.
    pub host_paths: Vec<String>,
    /// What its services may use: each `mem_limit`, or [`DEFAULT_SERVICE_MEMORY`].
    pub memory_bytes: u64,
    /// Whether scheduled backups include it.
    pub backups: bool,
    /// Its basic-auth users, checked by the ingress.
    pub auth: Vec<crate::ingress::BasicAuth>,
    /// Its services' images, as the bundle names them.
    pub images: Vec<String>,
}

fn bundle_error(e: kn_bundle::BundleError) -> Error {
    match e {
        kn_bundle::BundleError::TooLarge(m) => Error::TooLarge(m),
        e => Error::Refused {
            message: "the bundle was refused".into(),
            problems: vec![e.to_string()],
        },
    }
}

/// Extracts a bundle ZIP into `scratch` and checks it as a node would (with any host path allowed, since that
/// depends on the node; see [`CheckedApp::host_paths`]).
pub fn check_app(zip: &Path, scratch: &Path, limits: &Limits) -> Result<CheckedApp> {
    let f = std::fs::File::open(zip)?;
    let index = kn_bundle::zip::extract(std::io::BufReader::new(f), scratch, limits).map_err(bundle_error)?;
    let options = CheckOptions {
        allowed_host_paths: vec![PathBuf::from("/")],
    };
    let bundle = kn_bundle::load(scratch, &index, &options).map_err(|p| Error::Refused {
        message: "the bundle was refused".into(),
        problems: p.0.iter().map(|p| p.to_string()).collect(),
    })?;
    let m = &bundle.manifest;
    let mut host_paths = Vec::new();
    let mut memory_bytes = 0;
    let mut images: Vec<String> = Vec::new();
    for s in &bundle.services {
        if !s.image.is_empty() && !images.contains(&s.image) {
            images.push(s.image.clone());
        }
        for mount in &s.mounts {
            if let Mount::Host { path, .. } = mount {
                host_paths.push(path.clone());
            }
        }
        memory_bytes += s
            .config
            .get("mem_limit")
            .and_then(yaml_bytes)
            .unwrap_or(DEFAULT_SERVICE_MEMORY);
    }
    Ok(CheckedApp {
        digest: index.digest(),
        domain: m.domain.clone(),
        arch: m.arch.map(|a| a.as_str().to_string()),
        nest: m.nest.clone(),
        node_labels: m.node_labels.clone(),
        idle_timeout_secs: m.idle_timeout_secs,
        shared: m.shared.keys().cloned().collect(),
        shared_rw: m
            .shared
            .iter()
            .filter(|(_, s)| s.mode == kn_bundle::manifest::SharedMode::Rw)
            .map(|(n, _)| n.clone())
            .collect(),
        host_paths,
        memory_bytes,
        backups: m.backups,
        auth: m
            .auth
            .iter()
            .map(|a| crate::ingress::BasicAuth {
                user: a.user.clone(),
                hash: a.hash.clone(),
            })
            .collect(),
        images,
    })
}

/// Extracts a shared volume's contents into `scratch`, checking them by the bundle reader's rules; returns their
/// digest.
pub fn check_data(zip: &Path, scratch: &Path, limits: &Limits) -> Result<String> {
    let f = std::fs::File::open(zip)?;
    let index = kn_bundle::zip::extract(std::io::BufReader::new(f), scratch, limits).map_err(|e| match e {
        kn_bundle::BundleError::TooLarge(m) => Error::TooLarge(m),
        e => Error::Refused {
            message: "the volume's contents were refused".into(),
            problems: vec![e.to_string()],
        },
    })?;
    Ok(index.digest())
}

fn yaml_bytes(v: &serde_yaml_ng::Value) -> Option<u64> {
    match v {
        serde_yaml_ng::Value::Number(n) => n.as_u64(),
        serde_yaml_ng::Value::String(s) => parse_bytes(s),
        _ => None,
    }
}

/// Parses a Compose byte size such as `256m`, `1g`, `512mb` or `1048576`.
pub fn parse_bytes(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let digits = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(digits);
    let n: f64 = num.parse().ok()?;
    let mult: u64 = match unit.trim() {
        "" | "b" => 1,
        "k" | "kb" => 1 << 10,
        "m" | "mb" => 1 << 20,
        "g" | "gb" => 1 << 30,
        _ => return None,
    };
    (n >= 0.0).then_some((n * mult as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_checked() {
        let d = format!("sha256:{}", "a1".repeat(32));
        assert!(valid_digest(&d));
        assert!(!valid_digest("sha256:../../etc/passwd"));
        assert!(!valid_digest(&d.to_uppercase()));
        assert!(!valid_digest("md5:abc"));
        let dir = tempfile::tempdir().unwrap();
        let b = Bundles::open(dir.path().join("bundles")).unwrap();
        assert!(b.path("sha256:..").is_none());
        assert!(b.path(&d).unwrap().ends_with(format!("sha256-{}.zip", "a1".repeat(32))));
    }

    #[test]
    fn byte_sizes() {
        assert_eq!(parse_bytes("256m"), Some(256 << 20));
        assert_eq!(parse_bytes("1g"), Some(1 << 30));
        assert_eq!(parse_bytes("512MB"), Some(512 << 20));
        assert_eq!(parse_bytes("1.5g"), Some(3 << 29));
        assert_eq!(parse_bytes("1048576"), Some(1 << 20));
        assert_eq!(parse_bytes("lots"), None);
    }
}

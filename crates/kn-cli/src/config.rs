//! Clusters the CLI knows: `clusters.json` in `~/.config/kiwi-nest` (or `$XDG_CONFIG_HOME/kiwi-nest`, or
//! `$KIWI_NEST_CONFIG_DIR`), with each cluster's server URL and token, and which one is current.
//!
//! The file holds tokens, so it's written with mode 0600, atomically.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default)]
    pub clusters: BTreeMap<String, Cluster>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cluster {
    pub url: String,
    pub token: String,
    /// A CA certificate (PEM) to trust for the server, such as the ingress's internal CA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<String>,
}

/// The directory of the CLI's configuration.
pub fn dir(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(d) = explicit {
        return Ok(d.to_path_buf());
    }
    if let Some(x) = std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()) {
        return Ok(PathBuf::from(x).join("kiwi-nest"));
    }
    let home = std::env::var_os("HOME").context("HOME isn't set, so there's no ~/.config/kiwi-nest")?;
    Ok(PathBuf::from(home).join(".config/kiwi-nest"))
}

impl Config {
    pub fn path(dir: &Path) -> PathBuf {
        dir.join("clusters.json")
    }

    pub fn load(dir: &Path) -> anyhow::Result<Config> {
        let path = Self::path(dir);
        match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, dir: &Path) -> anyhow::Result<()> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        let path = Self::path(dir);
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        tmp.as_file().set_permissions(std::fs::Permissions::from_mode(0o600))?;
        tmp.write_all(&serde_json::to_vec_pretty(self)?)?;
        tmp.as_file().sync_all()?;
        tmp.persist(&path)
            .map_err(|e| e.error)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_privately() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path().join("kn");
        assert!(Config::load(&dir).unwrap().clusters.is_empty());
        let mut c = Config::default();
        c.clusters.insert(
            "home".into(),
            Cluster {
                url: "https://kn.example".into(),
                token: "kna_x".into(),
                ca_cert: None,
            },
        );
        c.current = Some("home".into());
        c.save(&dir).unwrap();
        let mode = std::fs::metadata(Config::path(&dir)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        let back = Config::load(&dir).unwrap();
        assert_eq!(back.current.as_deref(), Some("home"));
        assert_eq!(back.clusters["home"].token, "kna_x");
        assert_eq!(dir_of(Some(&dir)), dir);
    }

    fn dir_of(p: Option<&Path>) -> PathBuf {
        super::dir(p).unwrap()
    }
}

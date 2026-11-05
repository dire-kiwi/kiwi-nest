//! Backups: how targets are configured, and what the backup routes take and answer.
//!
//! Routes, under [`crate::API_PREFIX`], on a standalone node (with `--backup-targets`) and on the server:
//!
//! | Route | Body | Response |
//! | --- | --- | --- |
//! | `POST /backups/apps/{name}?` [`BackupQuery`] | | [`BackupResult`] |
//! | `POST /backups/volumes/{name}?` [`BackupQuery`] | | [`BackupResult`] |
//! | `GET /backups?` [`ListQuery`] | | `Vec<`[`BackupInfo`]`>` |
//! | `POST /backups/server?` [`BackupQuery`] | | [`BackupResult`] (server only) |
//!
//! Restores go through `deploy --restore` and `volume create --restore` ([`crate::api::DeployQuery::restore`]).

use serde::{Deserialize, Serialize};

/// How a target stores backups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// A `btrfs send` stream through zstd (`.send.zst`): incremental against the previous backup, with a full one
    /// now and then. Restoring needs btrfs.
    #[default]
    Send,
    /// A full tar archive through zstd (`.tar.zst`), every time. Restores anywhere, even with plain `tar`.
    Tar,
}

impl Format {
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Send => "send",
            Format::Tar => "tar",
        }
    }

    /// The suffix of its keys.
    pub fn suffix(self) -> &'static str {
        match self {
            Format::Send => ".send.zst",
            Format::Tar => ".tar.zst",
        }
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The file given to `--backup-targets`: YAML (or JSON).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetsFile {
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

/// One backup target: where backups go (exactly one of `s3`, `ssh` and `dir`), in which format, how often, and
/// how many are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    /// What `--target` calls it.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub s3: Option<S3Config>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh: Option<SshConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<DirConfig>,
    /// Put before every key, such as `home/`, so several clusters can share a bucket.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prefix: String,
    #[serde(default)]
    pub format: Format,
    /// How often the server backs up every app, shared volume and itself here, such as `24h`; without it, only
    /// `kiwi-nest backup` does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every: Option<String>,
    /// For `send`: how old a chain's full backup may be before the next backup is a full one again [default: 7d].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_every: Option<String>,
    /// Which backups to keep; with none of it set, every backup is kept.
    #[serde(default)]
    pub keep: Keep,
}

/// An S3-compatible bucket, such as R2 or S3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct S3Config {
    /// Such as `https://ACCOUNT.r2.cloudflarestorage.com` or `https://s3.eu-west-1.amazonaws.com`.
    pub endpoint: String,
    pub bucket: String,
    /// `auto` for R2.
    #[serde(default = "default_region")]
    pub region: String,
    pub access_key: String,
    /// The secret key, or `secret_key_file` naming a file that holds it (read on the server, which hands nodes the
    /// key itself).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key_file: Option<String>,
    /// Address the bucket as `BUCKET.ENDPOINT` rather than `ENDPOINT/BUCKET`.
    #[serde(default)]
    pub virtual_host: bool,
}

fn default_region() -> String {
    "auto".into()
}

/// A directory on another machine, reached with `ssh` (OpenSSH) as the user running kiwi-nest. The key and the
/// known-hosts file are read on the server, which hands their contents to nodes, so they need be only there; without
/// `known_hosts`, each machine checks the host against its own user's known hosts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SshConfig {
    /// `USER@HOST` or `HOST`.
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// The directory on the host (absolute).
    pub path: String,
    /// The private key to log in with, on each machine that backs up here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// A known-hosts file to check the host against, instead of the user's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hosts: Option<String>,
    /// The contents of `identity`, which the server sends nodes in its place (not for a targets file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_data: Option<String>,
    /// The contents of `known_hosts`, likewise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub known_hosts_data: Option<String>,
}

/// A directory on the machine itself, such as a mounted NAS share. Each node writes to its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DirConfig {
    /// Absolute.
    pub path: String,
}

/// Which backups a target keeps, per app, volume and the server: the newest `last`, and the newest of each of the
/// last `daily` days, `weekly` weeks and `monthly` months that have one. A backup that a kept incremental one needs
/// is always kept too, and so is the newest.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keep {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly: Option<u32>,
}

impl Keep {
    /// Whether every backup is kept.
    pub fn everything(&self) -> bool {
        self.last.is_none() && self.daily.is_none() && self.weekly.is_none() && self.monthly.is_none()
    }
}

/// What a backup is of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    App,
    Volume,
    Server,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::App => "app",
            Kind::Volume => "volume",
            Kind::Server => "server",
        }
    }
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The query of `POST /backups/…`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackupQuery {
    /// Only this target; by default, every target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// A full backup, even where an incremental one could be made.
    #[serde(default)]
    pub full: bool,
}

/// The query of `GET /backups`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListQuery {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// One backup in a target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupInfo {
    pub target: String,
    /// What `--restore` takes: the key, without the target's prefix.
    pub key: String,
    pub kind: Kind,
    pub name: String,
    pub format: Format,
    /// When its snapshot was taken (RFC 3339).
    pub taken_at: String,
    /// For an incremental `send` backup, when the backup it follows was taken (RFC 3339).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// In bytes, as stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
}

impl BackupInfo {
    /// `full`, `incremental` or `tar`.
    pub fn describe(&self) -> &'static str {
        match (self.format, &self.parent) {
            (Format::Tar, _) => "tar",
            (Format::Send, None) => "full",
            (Format::Send, Some(_)) => "incremental",
        }
    }
}

/// A target that a backup couldn't be made to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetError {
    pub target: String,
    pub error: String,
}

/// What a backup made.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupResult {
    /// One per target it worked for.
    #[serde(default)]
    pub made: Vec<BackupInfo>,
    #[serde(default)]
    pub failed: Vec<TargetError>,
    /// Such as which volumes it covered, and what retention removed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// A backup to restore from, as the desired state carries it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restore {
    pub target: String,
    pub key: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_parse() {
        let f: TargetsFile = serde_json::from_str(
            r#"{"targets":[{"name":"r2","s3":{"endpoint":"https://x","bucket":"b","access_key":"a",
                 "secret_key":"s"},"every":"24h","keep":{"daily":7}},
               {"name":"nas","ssh":{"host":"u@nas","path":"/srv/kn"},"format":"tar"}]}"#,
        )
        .unwrap();
        assert_eq!(f.targets[0].format, Format::Send);
        assert_eq!(f.targets[0].s3.as_ref().unwrap().region, "auto");
        assert!(!f.targets[0].keep.everything());
        assert_eq!(f.targets[1].format, Format::Tar);
        assert!(f.targets[1].keep.everything());
        // Typos are refused.
        assert!(serde_json::from_str::<TargetsFile>(r#"{"targets":[{"name":"x","dri":{"path":"/x"}}]}"#).is_err());
    }
}

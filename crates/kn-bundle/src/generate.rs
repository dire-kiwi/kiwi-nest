//! The Compose file a node actually runs, written from a checked bundle.
//!
//! - Every named volume becomes a bind mount of a directory of the app's (its subvolume), each shared volume a bind
//!   mount of the shared subvolume or of the app's snapshot of it, and bundle paths read-only bind mounts of the
//!   extracted bundle. None of them creates a missing source on the host.
//! - Only the routed service gets a port, published on `127.0.0.1`.
//! - `restart: always` becomes `unless-stopped`, so Docker doesn't restart an app that kiwi-nest stopped.
//! - Env files are referred to by absolute path, and every path is absolute, so the file doesn't depend on where
//!   it's kept. The node keeps it outside the bundle's directory, where a container can't change it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_yaml_ng::{Mapping, Value};

use crate::compose::{CheckedBundle, Mount, Service};

/// Where things are on the node, for one deploy of an app.
#[derive(Debug, Clone)]
pub struct Params<'a> {
    /// The Compose project name.
    pub project: &'a str,
    /// The extracted bundle (absolute).
    pub bundle_dir: &'a Path,
    /// The host directory for each named volume (absolute).
    pub volume_dirs: &'a BTreeMap<String, PathBuf>,
    /// The host directory for each shared volume in `x-kiwi-nest.shared` (absolute): the volume itself for `ro`
    /// and `rw`, or the app's own snapshot of it.
    pub shared_dirs: &'a BTreeMap<String, PathBuf>,
    /// The loopback port to publish the routed service on.
    pub host_port: u16,
    /// Labels added to every service, such as the app's name.
    pub labels: &'a BTreeMap<String, String>,
}

#[derive(Debug, thiserror::Error)]
pub enum GenerateError {
    #[error("`{0}` isn't an absolute path")]
    NotAbsolute(String),
    #[error("`{0}` can't be used in a Compose file (it isn't UTF-8, or has a `$`)")]
    BadPath(String),
    #[error("no directory was given for volume `{0}`")]
    MissingVolume(String),
    #[error("no directory was given for shared volume `{0}`")]
    MissingShared(String),
    #[error("writing YAML: {0}")]
    Yaml(#[from] serde_yaml_ng::Error),
}

fn s(v: &str) -> Value {
    Value::String(v.to_string())
}

fn path_value(p: &Path) -> Result<Value, GenerateError> {
    let text = p
        .to_str()
        .filter(|t| !t.contains('$'))
        .ok_or_else(|| GenerateError::BadPath(p.display().to_string()))?;
    if !p.is_absolute() {
        return Err(GenerateError::NotAbsolute(text.to_string()));
    }
    Ok(s(text))
}

fn bind(source: Value, target: &str, read_only: bool, selinux: &Option<String>) -> Value {
    let mut b = Mapping::new();
    b.insert(s("create_host_path"), Value::Bool(false));
    if let Some(z) = selinux {
        b.insert(s("selinux"), s(z));
    }
    let mut m = Mapping::new();
    m.insert(s("type"), s("bind"));
    m.insert(s("source"), source);
    m.insert(s("target"), s(target));
    m.insert(s("read_only"), Value::Bool(read_only));
    m.insert(s("bind"), Value::Mapping(b));
    Value::Mapping(m)
}

/// Maps a bundle's restart policy to the one the node runs.
pub fn restart_policy(restart: &str) -> &str {
    if restart == "always" { "unless-stopped" } else { restart }
}

fn service(svc: &Service, bundle: &CheckedBundle, p: &Params) -> Result<Value, GenerateError> {
    let mut out = Mapping::new();
    out.insert(s("image"), s(&svc.image));
    for (k, v) in &svc.config {
        out.insert(k.clone(), v.clone());
    }
    if let Some(r) = &svc.restart {
        out.insert(s("restart"), s(restart_policy(r)));
    }
    if !svc.env_files.is_empty() {
        let files = svc
            .env_files
            .iter()
            .map(|f| path_value(&p.bundle_dir.join(f)))
            .collect::<Result<Vec<_>, _>>()?;
        out.insert(s("env_file"), Value::Sequence(files));
    }
    let shared: Vec<_> = bundle
        .manifest
        .shared
        .iter()
        .filter(|(_, m)| m.mounted_into(&svc.name))
        .collect();
    if !svc.mounts.is_empty() || !shared.is_empty() {
        let mut mounts = Vec::new();
        for m in &svc.mounts {
            mounts.push(match m {
                Mount::Volume {
                    name,
                    target,
                    read_only,
                    selinux,
                } => {
                    let dir = p
                        .volume_dirs
                        .get(name)
                        .ok_or_else(|| GenerateError::MissingVolume(name.clone()))?;
                    bind(path_value(dir)?, target, *read_only, selinux)
                }
                Mount::Bundle { path, target, selinux } => {
                    let src = if path.is_empty() {
                        p.bundle_dir.to_path_buf()
                    } else {
                        p.bundle_dir.join(path)
                    };
                    bind(path_value(&src)?, target, true, selinux)
                }
                Mount::Host {
                    path,
                    target,
                    read_only,
                    selinux,
                } => bind(path_value(Path::new(path))?, target, *read_only, selinux),
                Mount::Tmpfs { target, options } => {
                    let mut t = Mapping::new();
                    t.insert(s("type"), s("tmpfs"));
                    t.insert(s("target"), s(target));
                    if !options.is_empty() {
                        t.insert(s("tmpfs"), Value::Mapping(options.clone()));
                    }
                    Value::Mapping(t)
                }
            });
        }
        for (name, m) in shared {
            let dir = p
                .shared_dirs
                .get(name)
                .ok_or_else(|| GenerateError::MissingShared(name.clone()))?;
            mounts.push(bind(path_value(dir)?, &m.target, m.mode.read_only(), &None));
        }
        out.insert(s("volumes"), Value::Sequence(mounts));
    }
    if svc.name == bundle.manifest.service {
        let mut port = Mapping::new();
        port.insert(s("target"), Value::Number(bundle.manifest.port.into()));
        port.insert(s("published"), s(&p.host_port.to_string()));
        port.insert(s("host_ip"), s("127.0.0.1"));
        port.insert(s("protocol"), s("tcp"));
        out.insert(s("ports"), Value::Sequence(vec![Value::Mapping(port)]));
    }
    if !svc.labels.is_empty() || !p.labels.is_empty() {
        let mut labels = Mapping::new();
        for (k, v) in svc
            .labels
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .chain(p.labels.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        {
            labels.insert(s(k), s(v));
        }
        out.insert(s("labels"), Value::Mapping(labels));
    }
    Ok(Value::Mapping(out))
}

/// Writes the Compose file for `bundle`.
pub fn generate(bundle: &CheckedBundle, p: &Params) -> Result<String, GenerateError> {
    path_value(p.bundle_dir)?;
    let mut services = Mapping::new();
    for svc in &bundle.services {
        services.insert(s(&svc.name), service(svc, bundle, p)?);
    }
    let mut doc = Mapping::new();
    doc.insert(s("name"), s(p.project));
    doc.insert(s("services"), Value::Mapping(services));
    if !bundle.networks.is_empty() {
        let mut networks = Mapping::new();
        for (name, settings) in &bundle.networks {
            networks.insert(s(name), Value::Mapping(settings.clone()));
        }
        doc.insert(s("networks"), Value::Mapping(networks));
    }
    let yaml = serde_yaml_ng::to_string(&Value::Mapping(doc))?;
    Ok(format!(
        "# Written by kiwi-nest from the app's bundle. Don't edit: the next deploy replaces it.\n{yaml}"
    ))
}

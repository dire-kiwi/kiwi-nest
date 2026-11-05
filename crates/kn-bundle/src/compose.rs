//! Checking a bundle's `compose.yaml` against the subset kiwi-nest accepts.
//!
//! The rules, in short (see `docs/design.md`, Bundles):
//!
//! - Every service needs an `image`; `build` is refused, since images are built elsewhere.
//! - No privileged mode, added capabilities, devices, security options or sysctls, and no host namespaces: only
//!   `service:NAME` (and the private settings) for `network_mode`, `pid`, `ipc`, `uts`, `userns_mode` and `cgroup`.
//! - No published `ports` and no `container_name`: kiwi-nest publishes the routed service on loopback and names
//!   the containers itself.
//! - Volumes and networks must be the app's own: `{}` volumes (each becomes a directory of the app's), and networks
//!   with at most `internal`. External, custom-named and driver volumes and networks are refused, as are anonymous
//!   volumes.
//! - A mount's source is a declared named volume, a path in the bundle (`./site`, always read-only), a `tmpfs`, or an
//!   absolute host path that the node allows.
//! - No variable interpolation: kiwi-nest runs Compose with an empty environment, so a `$` must be written `$$`.
//! - No `include`, `extends`, `configs`, `secrets`, `profiles` or `deploy`; YAML tags are refused. Merge keys
//!   (`<<`) and anchors work.
//!
//! Anything not on the list of known keys is refused, so a new Compose feature is refused until it's reviewed.

use std::path::{Component, Path, PathBuf};

use serde_yaml_ng::{Mapping, Value};

use crate::manifest::{self, Manifest, scalar_string, yaml_inline};
use crate::{FileIndex, MAX_COMPOSE_SIZE};

/// What the node allows beyond the default rules.
#[derive(Debug, Clone, Default)]
pub struct CheckOptions {
    /// Host directories that bundles may mount. A mount must be one of these or inside one.
    pub allowed_host_paths: Vec<PathBuf>,
}

/// One reason a bundle was refused, and where in `compose.yaml` it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// A path such as `services.web.ports`; empty for the bundle as a whole.
    pub at: String,
    pub message: String,
}

impl Problem {
    pub fn new(at: impl Into<String>, message: impl Into<String>) -> Self {
        Problem {
            at: at.into(),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.at.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.at, self.message)
        }
    }
}

/// Every problem found in a refused bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problems(pub Vec<Problem>);

impl std::fmt::Display for Problems {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the bundle was refused:")?;
        for p in &self.0 {
            write!(f, "\n  - {p}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Problems {}

/// A bundle that passed the checks.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedBundle {
    pub manifest: Manifest,
    /// In the order of `compose.yaml`.
    pub services: Vec<Service>,
    /// The named volumes, in the order declared.
    pub volumes: Vec<String>,
    /// The networks, with their (checked) settings.
    pub networks: Vec<(String, Mapping)>,
}

impl CheckedBundle {
    pub fn service(&self, name: &str) -> Option<&Service> {
        self.services.iter().find(|s| s.name == name)
    }
}

/// A checked service.
#[derive(Debug, Clone, PartialEq)]
pub struct Service {
    pub name: String,
    pub image: String,
    /// The settings passed through to the generated file as they are, in their original order.
    pub config: Mapping,
    pub mounts: Vec<Mount>,
    /// Env files, as paths in the bundle.
    pub env_files: Vec<String>,
    pub restart: Option<String>,
    pub labels: Vec<(String, String)>,
}

/// A checked mount.
#[derive(Debug, Clone, PartialEq)]
pub enum Mount {
    /// One of the app's named volumes.
    Volume {
        name: String,
        target: String,
        read_only: bool,
        selinux: Option<String>,
    },
    /// A path in the bundle (empty for the whole bundle), always read-only.
    Bundle {
        path: String,
        target: String,
        selinux: Option<String>,
    },
    /// A host path that the node allows.
    Host {
        path: String,
        target: String,
        read_only: bool,
        selinux: Option<String>,
    },
    /// A tmpfs, with its `tmpfs:` settings.
    Tmpfs { target: String, options: Mapping },
}

/// Service settings passed through without changes.
const PASSTHROUGH: &[&str] = &[
    "command",
    "entrypoint",
    "environment",
    "working_dir",
    "user",
    "group_add",
    "hostname",
    "domainname",
    "healthcheck",
    "stop_signal",
    "stop_grace_period",
    "init",
    "read_only",
    "tty",
    "stdin_open",
    "tmpfs",
    "expose",
    "ulimits",
    "shm_size",
    "mem_limit",
    "mem_reservation",
    "memswap_limit",
    "mem_swappiness",
    "cpus",
    "cpu_shares",
    "cpu_quota",
    "cpu_period",
    "cpuset",
    "pids_limit",
    "cap_drop",
    "dns",
    "dns_search",
    "dns_opt",
    "extra_hosts",
    "platform",
];

/// Label prefixes kept for Docker, Compose and kiwi-nest.
const RESERVED_LABELS: &[&str] = &["com.docker.", "kiwi-nest."];

/// Whether `name` can name a service, volume or network: letters, digits, `_`, `.` and `-`, starting with a letter
/// or digit, at most 63 characters.
pub fn is_valid_object_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

/// Checks `compose.yaml` against the subset kiwi-nest accepts, given the bundle's files.
pub fn check(yaml: &str, files: &FileIndex, options: &CheckOptions) -> Result<CheckedBundle, Problems> {
    let one = |m: String| Err(Problems(vec![Problem::new("compose.yaml", m)]));
    if yaml.len() as u64 > MAX_COMPOSE_SIZE {
        return one(format!("is larger than {MAX_COMPOSE_SIZE} bytes"));
    }
    let mut doc: Value = match serde_yaml_ng::from_str(yaml) {
        Ok(v) => v,
        Err(e) => return one(format!("isn't valid YAML: {e}")),
    };
    if let Err(e) = doc.apply_merge() {
        return one(format!("has a bad merge key (`<<`): {e}"));
    }
    let Value::Mapping(top) = doc else {
        return one("must be a mapping with `services` and `x-kiwi-nest`".into());
    };
    let mut c = Checker {
        files,
        options,
        problems: Vec::new(),
    };

    let (mut services, mut volumes, mut networks, mut manifest) = (None, None, None, None);
    for (k, v) in &top {
        let Some(key) = k.as_str() else {
            c.refuse("", format!("top-level key `{}` isn't a string", yaml_inline(k)));
            continue;
        };
        match key {
            "services" => services = Some(v),
            "volumes" => volumes = Some(v),
            "networks" => networks = Some(v),
            "x-kiwi-nest" => manifest = Some(v),
            // kiwi-nest names the project itself; `version` is obsolete.
            "name" | "version" => {}
            "include" => c.refuse(key, "isn't supported; put the whole app in compose.yaml"),
            "configs" | "secrets" => c.refuse(key, "aren't supported; put the files in the bundle and mount them"),
            k if k.starts_with("x-") => {}
            _ => c.refuse(key, "isn't a supported top-level key"),
        }
    }
    for (at, v) in [("services", services), ("volumes", volumes), ("networks", networks)] {
        if let Some(v) = v {
            c.scan(at, v);
        }
    }

    let volumes = c.check_volumes(volumes);
    let networks = c.check_networks(networks);
    let services_v = services;
    let services = c.check_services(services, &volumes, &networks);
    let manifest = match manifest {
        None => {
            c.refuse(
                "x-kiwi-nest",
                "is required: it says how the app is served, such as its domain",
            );
            None
        }
        Some(v) => match manifest::parse(v) {
            Ok(m) => Some(m),
            Err(ps) => {
                c.problems.extend(ps);
                None
            }
        },
    };
    let services_map = services_v.and_then(Value::as_mapping);
    if let Some(m) = &manifest
        && services_map.is_some_and(|m| !m.is_empty())
    {
        let declared = services_map.is_some_and(|map| map.contains_key(m.service.as_str()));
        match services.iter().find(|s| s.name == m.service) {
            None if declared => {}
            None => c.refuse(
                "x-kiwi-nest.service",
                format!("names `{}`, which isn't a service", m.service),
            ),
            Some(s) if s.config.contains_key("network_mode") => c.refuse(
                format!("services.{}.network_mode", s.name),
                "isn't allowed on the routed service, which needs its own network to be published",
            ),
            Some(_) => {}
        }
    }
    match manifest {
        Some(manifest) if c.problems.is_empty() => Ok(CheckedBundle {
            manifest,
            services,
            volumes,
            networks,
        }),
        _ => Err(Problems(c.problems)),
    }
}

struct Checker<'a> {
    files: &'a FileIndex,
    options: &'a CheckOptions,
    problems: Vec<Problem>,
}

/// Whether `s` uses interpolation: any `$` that isn't part of `$$`.
fn interpolates(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            if b.get(i + 1) == Some(&b'$') {
                i += 2;
                continue;
            }
            return true;
        }
        i += 1;
    }
    false
}

/// Normalizes a relative path in the bundle (`./site/`, `site`, `.`), refusing `..`.
fn bundle_path(src: &str) -> Result<String, &'static str> {
    let mut parts = Vec::new();
    for c in Path::new(src).components() {
        match c {
            Component::CurDir => {}
            Component::Normal(p) => parts.push(p.to_str().ok_or("isn't valid UTF-8")?),
            Component::ParentDir => return Err("goes outside the bundle (`..`)"),
            Component::RootDir | Component::Prefix(_) => return Err("must be a path in the bundle"),
        }
    }
    Ok(parts.join("/"))
}

impl Checker<'_> {
    fn refuse(&mut self, at: impl Into<String>, message: impl Into<String>) {
        self.problems.push(Problem::new(at, message));
    }

    /// Refuses interpolation and YAML tags anywhere in `v`.
    fn scan(&mut self, at: &str, v: &Value) {
        match v {
            Value::String(s) if interpolates(s) => self.refuse(
                at,
                "uses variable interpolation (`$`); kiwi-nest doesn't interpolate, so write `$$` for a literal `$`",
            ),
            Value::Sequence(seq) => {
                for (i, item) in seq.iter().enumerate() {
                    self.scan(&format!("{at}[{i}]"), item);
                }
            }
            Value::Mapping(map) => {
                for (k, item) in map {
                    let key = k.as_str().map(str::to_string).unwrap_or_else(|| yaml_inline(k));
                    if interpolates(&key) {
                        self.refuse(format!("{at}.{key}"), "uses variable interpolation (`$`) in a key");
                    }
                    self.scan(&format!("{at}.{key}"), item);
                }
            }
            Value::Tagged(_) => self.refuse(at, "uses a YAML tag (`!…`), which isn't supported"),
            _ => {}
        }
    }

    fn check_volumes(&mut self, v: Option<&Value>) -> Vec<String> {
        let mut out = Vec::new();
        let Some(v) = v else { return out };
        if v.is_null() {
            return out;
        }
        let Some(map) = v.as_mapping() else {
            self.refuse("volumes", "must be a mapping of volume names");
            return out;
        };
        for (k, settings) in map {
            let Some(name) = k.as_str().filter(|n| is_valid_object_name(n)) else {
                self.refuse("volumes", format!("`{}` isn't a valid volume name", yaml_inline(k)));
                continue;
            };
            let ok = match settings {
                Value::Null => true,
                Value::Mapping(m) => m.keys().all(|k| k.as_str().is_some_and(|k| k.starts_with("x-"))),
                _ => false,
            };
            if !ok {
                self.refuse(
                    format!("volumes.{name}"),
                    "must be `{}`: external, named and driver volumes aren't supported, since each volume is the \
                     app's own directory",
                );
            }
            out.push(name.to_string());
        }
        out
    }

    fn check_networks(&mut self, v: Option<&Value>) -> Vec<(String, Mapping)> {
        let mut out = Vec::new();
        let Some(v) = v else { return out };
        if v.is_null() {
            return out;
        }
        let Some(map) = v.as_mapping() else {
            self.refuse("networks", "must be a mapping of network names");
            return out;
        };
        for (k, settings) in map {
            let Some(name) = k.as_str().filter(|n| is_valid_object_name(n)) else {
                self.refuse("networks", format!("`{}` isn't a valid network name", yaml_inline(k)));
                continue;
            };
            let at = format!("networks.{name}");
            let mut kept = Mapping::new();
            match settings {
                Value::Null => {}
                Value::Mapping(m) => {
                    for (sk, sv) in m {
                        match sk.as_str() {
                            Some("internal") if sv.is_bool() => {
                                kept.insert(sk.clone(), sv.clone());
                            }
                            Some(x) if x.starts_with("x-") => {}
                            _ => self.refuse(
                                format!("{at}.{}", yaml_inline(sk)),
                                "isn't supported: networks may only set `internal`; external, custom-named and \
                                 driver networks aren't supported",
                            ),
                        }
                    }
                }
                _ => self.refuse(&at, "must be a mapping"),
            }
            out.push((name.to_string(), kept));
        }
        out
    }

    fn check_services(
        &mut self,
        v: Option<&Value>,
        volumes: &[String],
        networks: &[(String, Mapping)],
    ) -> Vec<Service> {
        let mut out = Vec::new();
        let Some(map) = v.and_then(Value::as_mapping).filter(|m| !m.is_empty()) else {
            self.refuse("services", "must be a mapping with at least one service");
            return out;
        };
        let names: Vec<&str> = map.keys().filter_map(Value::as_str).collect();
        for (k, v) in map {
            let Some(name) = k.as_str().filter(|n| is_valid_object_name(n)) else {
                self.refuse("services", format!("`{}` isn't a valid service name", yaml_inline(k)));
                continue;
            };
            let Some(settings) = v.as_mapping() else {
                self.refuse(format!("services.{name}"), "must be a mapping");
                continue;
            };
            let ctx = Ctx {
                name,
                services: &names,
                volumes,
                networks,
            };
            if let Some(s) = self.check_service(&ctx, settings) {
                out.push(s);
            }
        }
        out
    }

    fn check_service(&mut self, ctx: &Ctx, map: &Mapping) -> Option<Service> {
        let at = format!("services.{}", ctx.name);
        let mut s = Service {
            name: ctx.name.to_string(),
            image: String::new(),
            config: Mapping::new(),
            mounts: Vec::new(),
            env_files: Vec::new(),
            restart: None,
            labels: Vec::new(),
        };
        let before = self.problems.len();
        for (k, v) in map {
            let Some(key) = k.as_str() else {
                self.refuse(&at, format!("key `{}` isn't a string", yaml_inline(k)));
                continue;
            };
            let kat = format!("{at}.{key}");
            let mut keep = false;
            match key {
                "image" => match v.as_str() {
                    Some(i) if !i.trim().is_empty() => s.image = i.to_string(),
                    _ => self.refuse(kat, "must be an image reference"),
                },
                "build" => self.refuse(
                    kat,
                    "isn't supported: build and push the image elsewhere, and use `image`",
                ),
                "privileged" => {
                    if v.as_bool() != Some(false) {
                        self.refuse(kat, "isn't allowed")
                    }
                }
                "ports" => self.refuse(
                    kat,
                    "aren't allowed: kiwi-nest publishes the routed service's `x-kiwi-nest.port` on loopback itself",
                ),
                "container_name" => self.refuse(kat, "isn't allowed: kiwi-nest names the containers after the app"),
                "network_mode" => keep = self.check_namespace(&kat, v, ctx, &["none"]),
                "pid" | "uts" | "userns_mode" => keep = self.check_namespace(&kat, v, ctx, &[]),
                "ipc" => keep = self.check_namespace(&kat, v, ctx, &["private", "shareable", "none"]),
                "cgroup" => keep = self.check_namespace(&kat, v, ctx, &["private"]),
                "networks" => keep = self.check_service_networks(&kat, v, ctx),
                "volumes" => self.check_mounts(&kat, v, ctx, &mut s.mounts),
                "env_file" => self.check_env_files(&kat, v, &mut s.env_files),
                "restart" => match v.as_str() {
                    Some(r @ ("no" | "always" | "unless-stopped" | "on-failure")) => s.restart = Some(r.to_string()),
                    Some(r) if r.strip_prefix("on-failure:").is_some_and(|n| n.parse::<u32>().is_ok()) => {
                        s.restart = Some(r.to_string())
                    }
                    _ => self.refuse(kat, "must be `no`, `always`, `unless-stopped` or `on-failure[:N]`"),
                },
                "depends_on" => keep = self.check_depends_on(&kat, v, ctx),
                "labels" => self.check_labels(&kat, v, &mut s.labels),
                "logging" => keep = self.check_logging(&kat, v),
                "links" => keep = self.check_service_refs(&kat, v, ctx, false),
                "volumes_from" => keep = self.check_service_refs(&kat, v, ctx, true),
                "pull_policy" => match v.as_str() {
                    Some("always" | "never" | "missing" | "if_not_present" | "daily" | "weekly") => keep = true,
                    Some(p) if p.starts_with("every_") => keep = true,
                    _ => self.refuse(
                        kat,
                        "must be `always`, `never`, `missing`, `daily`, `weekly` or `every_…`",
                    ),
                },
                "cap_add" => self.refuse(kat, "isn't allowed: containers can't gain capabilities"),
                "devices" | "device_cgroup_rules" | "gpus" => self.refuse(kat, "isn't allowed: no host devices"),
                "security_opt" => self.refuse(kat, "isn't allowed"),
                "sysctls" => self.refuse(kat, "aren't allowed"),
                "deploy" => self.refuse(kat, "isn't supported: use `mem_limit` and `cpus`"),
                "extends" => self.refuse(kat, "isn't supported: write the whole service in compose.yaml"),
                "secrets" | "configs" => self.refuse(kat, "aren't supported: mount files from the bundle instead"),
                "profiles" => self.refuse(kat, "aren't supported: every service in the bundle runs"),
                "scale" => self.refuse(kat, "isn't supported: each app runs one instance of each service"),
                k if PASSTHROUGH.contains(&k) => keep = true,
                k if k.starts_with("x-") => {}
                _ => self.refuse(kat, "isn't supported"),
            }
            if keep {
                s.config.insert(k.clone(), v.clone());
            }
        }
        if s.image.is_empty() && !map.contains_key("image") {
            self.refuse(&at, "needs an `image`");
        }
        (self.problems.len() == before).then_some(s)
    }

    /// `network_mode`, `pid`, `ipc` and the like: never the host's, or another container's outside the app.
    fn check_namespace(&mut self, at: &str, v: &Value, ctx: &Ctx, plain: &[&str]) -> bool {
        let ok = match v.as_str() {
            Some(s) if plain.contains(&s) => true,
            Some(s) => s
                .strip_prefix("service:")
                .is_some_and(|svc| svc != ctx.name && ctx.services.contains(&svc)),
            None => false,
        };
        if !ok {
            let mut allowed: Vec<String> = plain.iter().map(|p| format!("`{p}`")).collect();
            allowed.push("`service:NAME`".into());
            self.refuse(
                at,
                format!(
                    "may only be {}: host namespaces and containers outside the app aren't allowed",
                    allowed.join(", ")
                ),
            );
        }
        ok
    }

    fn check_service_networks(&mut self, at: &str, v: &Value, ctx: &Ctx) -> bool {
        let before = self.problems.len();
        let check_name = |c: &mut Self, n: &Value| match n.as_str() {
            Some("default") => {}
            Some(n) if ctx.networks.iter().any(|(name, _)| name == n) => {}
            _ => c.refuse(
                at,
                format!(
                    "network `{}` isn't declared under the top-level `networks`",
                    yaml_inline(n)
                ),
            ),
        };
        match v {
            Value::Sequence(seq) => {
                for n in seq {
                    check_name(self, n);
                }
            }
            Value::Mapping(map) => {
                for (n, settings) in map {
                    check_name(self, n);
                    let ok = match settings {
                        Value::Null => true,
                        Value::Mapping(m) => m.iter().all(|(k, v)| match k.as_str() {
                            Some("aliases") => v.as_sequence().is_some(),
                            Some(k) => k.starts_with("x-"),
                            None => false,
                        }),
                        _ => false,
                    };
                    if !ok {
                        self.refuse(
                            format!("{at}.{}", yaml_inline(n)),
                            "may only set `aliases`: fixed addresses aren't supported",
                        );
                    }
                }
            }
            _ => self.refuse(at, "must be a list or mapping of networks"),
        }
        self.problems.len() == before
    }

    fn check_mounts(&mut self, at: &str, v: &Value, ctx: &Ctx, out: &mut Vec<Mount>) {
        let Some(seq) = v.as_sequence() else {
            self.refuse(at, "must be a list");
            return;
        };
        for (i, item) in seq.iter().enumerate() {
            let iat = format!("{at}[{i}]");
            let parsed = match item {
                Value::String(s) => self.parse_short_mount(s),
                Value::Mapping(m) => self.parse_long_mount(m),
                _ => Err("must be a string or a mapping".to_string()),
            };
            match parsed.and_then(|raw| self.classify_mount(raw, ctx)) {
                Ok(m) => out.push(m),
                Err(e) => self.refuse(iat, e),
            }
        }
    }

    fn parse_short_mount(&self, s: &str) -> Result<RawMount, String> {
        let parts: Vec<&str> = s.split(':').collect();
        let (source, target, opts) = match parts.as_slice() {
            [_] => return Err(ANONYMOUS.to_string()),
            [src, tgt] => (*src, *tgt, None),
            [src, tgt, opts] => (*src, *tgt, Some(*opts)),
            _ => return Err("isn't a valid mount: use `SOURCE:TARGET[:OPTIONS]`".to_string()),
        };
        let mut m = RawMount {
            kind: RawKind::Unknown,
            source: Some(source.to_string()),
            target: target.to_string(),
            ..Default::default()
        };
        for o in opts.unwrap_or("").split(',').filter(|o| !o.is_empty()) {
            match o {
                "ro" => m.read_only = Some(true),
                "rw" => m.read_only = Some(false),
                "z" | "Z" => m.selinux = Some(o.to_string()),
                _ => {
                    return Err(format!(
                        "mount option `{o}` isn't supported (use `ro`, `rw`, `z` or `Z`)"
                    ));
                }
            }
        }
        Ok(m)
    }

    fn parse_long_mount(&self, map: &Mapping) -> Result<RawMount, String> {
        let mut m = RawMount::default();
        for (k, v) in map {
            let key = k.as_str().unwrap_or("");
            let bad = || format!("`{key}` has a bad value");
            match key {
                "type" => {
                    m.kind = match v.as_str() {
                        Some("volume") => RawKind::Volume,
                        Some("bind") => RawKind::Bind,
                        Some("tmpfs") => RawKind::Tmpfs,
                        Some(t) => return Err(format!("mount type `{t}` isn't supported")),
                        None => return Err(bad()),
                    }
                }
                "source" => m.source = Some(v.as_str().ok_or_else(bad)?.to_string()),
                "target" => m.target = v.as_str().ok_or_else(bad)?.to_string(),
                "read_only" => m.read_only = Some(v.as_bool().ok_or_else(bad)?),
                "consistency" => {}
                "bind" => {
                    for (bk, bv) in v.as_mapping().ok_or_else(bad)? {
                        match bk.as_str() {
                            Some("create_host_path") => {}
                            Some("selinux") => m.selinux = Some(bv.as_str().ok_or_else(bad)?.to_string()),
                            Some(other) => return Err(format!("`bind.{other}` isn't supported")),
                            None => return Err(bad()),
                        }
                    }
                }
                "volume" => {
                    for (vk, _) in v.as_mapping().ok_or_else(bad)? {
                        match vk.as_str() {
                            Some("nocopy") => {}
                            Some(other) => return Err(format!("`volume.{other}` isn't supported")),
                            None => return Err(bad()),
                        }
                    }
                }
                "tmpfs" => {
                    let opts = v.as_mapping().ok_or_else(bad)?;
                    if let Some(k) = opts.keys().find(|k| !matches!(k.as_str(), Some("size" | "mode"))) {
                        return Err(format!("`tmpfs.{}` isn't supported", yaml_inline(k)));
                    }
                    m.tmpfs = opts.clone();
                }
                k if k.starts_with("x-") => {}
                _ => return Err(format!("`{key}` isn't supported in a mount")),
            }
        }
        match m.kind {
            RawKind::Unknown => Err("needs a `type`: `volume`, `bind` or `tmpfs`".to_string()),
            RawKind::Volume if m.source.is_none() => Err(ANONYMOUS.to_string()),
            RawKind::Volume if m.source.as_deref().is_some_and(|s| s.starts_with(['.', '/', '~'])) => {
                Err("a `volume` mount's source must be a named volume".to_string())
            }
            RawKind::Bind if !m.source.as_deref().is_some_and(|s| s.starts_with(['.', '/', '~'])) => {
                Err("a `bind` mount's source must be a path: `./…` in the bundle, or an absolute host path".to_string())
            }
            _ => Ok(m),
        }
    }

    fn classify_mount(&self, m: RawMount, ctx: &Ctx) -> Result<Mount, String> {
        if !m.target.starts_with('/') {
            return Err("the target must be an absolute path in the container".to_string());
        }
        let target = m.target;
        if m.kind == RawKind::Tmpfs {
            if m.source.is_some() {
                return Err("a `tmpfs` mount has no source".to_string());
            }
            return Ok(Mount::Tmpfs {
                target,
                options: m.tmpfs,
            });
        }
        let source = m.source.unwrap_or_default();
        if source.starts_with('/') {
            let mut norm = PathBuf::new();
            for c in Path::new(&source).components() {
                match c {
                    Component::RootDir => norm.push("/"),
                    Component::Normal(p) => norm.push(p),
                    Component::CurDir => {}
                    Component::ParentDir | Component::Prefix(_) => {
                        return Err(format!("host path `{source}` mustn't contain `..`"));
                    }
                }
            }
            if !self.options.allowed_host_paths.iter().any(|p| norm.starts_with(p)) {
                return Err(format!(
                    "mounts host path `{source}`, which this node doesn't allow; mount a path in the bundle \
                     (`./…`) or a named volume instead"
                ));
            }
            let path = norm.to_str().ok_or("host path isn't valid UTF-8")?.to_string();
            Ok(Mount::Host {
                path,
                target,
                read_only: m.read_only.unwrap_or(false),
                selinux: m.selinux,
            })
        } else if source.starts_with('~') {
            Err("home-directory paths (`~`) aren't allowed".to_string())
        } else if source.starts_with('.') {
            let path = bundle_path(&source).map_err(|e| format!("`{source}` {e}"))?;
            if !self.files.contains(&path) {
                return Err(format!("`{source}` isn't in the bundle"));
            }
            if m.read_only == Some(false) {
                return Err(
                    "bundle files are always mounted read-only; use a named volume for data the app writes".to_string(),
                );
            }
            Ok(Mount::Bundle {
                path,
                target,
                selinux: m.selinux,
            })
        } else {
            if !ctx.volumes.contains(&source) {
                return Err(format!(
                    "volume `{source}` isn't declared under the top-level `volumes`"
                ));
            }
            Ok(Mount::Volume {
                name: source,
                target,
                read_only: m.read_only.unwrap_or(false),
                selinux: m.selinux,
            })
        }
    }

    fn check_env_files(&mut self, at: &str, v: &Value, out: &mut Vec<String>) {
        let items: Vec<&Value> = match v {
            Value::String(_) | Value::Mapping(_) => vec![v],
            Value::Sequence(s) => s.iter().collect(),
            _ => {
                self.refuse(at, "must be a path, or a list of them");
                return;
            }
        };
        for item in items {
            let (path, required) = match item {
                Value::String(p) => (Some(p.as_str()), true),
                Value::Mapping(m) => (
                    m.get("path").and_then(Value::as_str),
                    m.get("required").and_then(Value::as_bool).unwrap_or(true),
                ),
                _ => (None, true),
            };
            let Some(path) = path else {
                self.refuse(at, "each entry must be a path, or `{ path, required }`");
                continue;
            };
            match bundle_path(path) {
                Ok(p) if self.files.is_file(&p) => out.push(p),
                Ok(_) if !required => {}
                Ok(_) => self.refuse(at, format!("`{path}` isn't a file in the bundle")),
                Err(e) => self.refuse(at, format!("`{path}` {e}")),
            }
        }
    }

    fn check_depends_on(&mut self, at: &str, v: &Value, ctx: &Ctx) -> bool {
        let names: Vec<&Value> = match v {
            Value::Sequence(s) => s.iter().collect(),
            Value::Mapping(m) => m.keys().collect(),
            _ => {
                self.refuse(at, "must be a list or mapping of services");
                return false;
            }
        };
        let mut ok = true;
        for n in names {
            if !n.as_str().is_some_and(|n| n != ctx.name && ctx.services.contains(&n)) {
                self.refuse(at, format!("`{}` isn't another service of this app", yaml_inline(n)));
                ok = false;
            }
        }
        ok
    }

    fn check_service_refs(&mut self, at: &str, v: &Value, ctx: &Ctx, volumes_from: bool) -> bool {
        let Some(seq) = v.as_sequence() else {
            self.refuse(at, "must be a list");
            return false;
        };
        let mut ok = true;
        for item in seq {
            let s = item.as_str().unwrap_or("");
            let (svc, rest) = s.split_once(':').unwrap_or((s, ""));
            let rest_ok = if volumes_from {
                matches!(rest, "" | "ro" | "rw")
            } else {
                true
            };
            if !(rest_ok && svc != ctx.name && ctx.services.contains(&svc)) {
                self.refuse(at, format!("`{s}` must name another service of this app"));
                ok = false;
            }
        }
        ok
    }

    fn check_labels(&mut self, at: &str, v: &Value, out: &mut Vec<(String, String)>) {
        let mut pairs = Vec::new();
        match v {
            Value::Mapping(m) => {
                for (k, v) in m {
                    match (
                        k.as_str(),
                        if v.is_null() {
                            Some(String::new())
                        } else {
                            scalar_string(v)
                        },
                    ) {
                        (Some(k), Some(v)) => pairs.push((k.to_string(), v)),
                        _ => self.refuse(at, format!("label `{}` must have a plain value", yaml_inline(k))),
                    }
                }
            }
            Value::Sequence(s) => {
                for item in s {
                    match item.as_str() {
                        Some(kv) => {
                            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                            pairs.push((k.to_string(), v.to_string()));
                        }
                        None => self.refuse(at, "each label must be `KEY=VALUE`"),
                    }
                }
            }
            _ => self.refuse(at, "must be a mapping or a list of `KEY=VALUE`"),
        }
        for (k, v) in pairs {
            if let Some(prefix) = RESERVED_LABELS.iter().find(|p| k.starts_with(**p)) {
                self.refuse(at, format!("label `{k}` is reserved (`{prefix}…`)"));
            } else if k.is_empty() {
                self.refuse(at, "a label needs a name");
            } else {
                out.push((k, v));
            }
        }
    }

    fn check_logging(&mut self, at: &str, v: &Value) -> bool {
        let Some(m) = v.as_mapping() else {
            self.refuse(at, "must be a mapping");
            return false;
        };
        let mut ok = true;
        for (k, v) in m {
            match (k.as_str(), v) {
                (Some("driver"), Value::String(d)) if d == "json-file" || d == "local" => {}
                (Some("driver"), _) => {
                    self.refuse(
                        at,
                        "may only use the `json-file` or `local` driver, so `kiwi-nest logs` works",
                    );
                    ok = false;
                }
                (Some("options"), Value::Mapping(_)) => {}
                _ => {
                    self.refuse(at, format!("`{}` isn't supported", yaml_inline(k)));
                    ok = false;
                }
            }
        }
        ok
    }
}

const ANONYMOUS: &str = "anonymous volumes aren't supported: name the volume, and declare it under `volumes`";

struct Ctx<'a> {
    name: &'a str,
    services: &'a [&'a str],
    volumes: &'a [String],
    networks: &'a [(String, Mapping)],
}

#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
enum RawKind {
    /// Short syntax: worked out from the source.
    #[default]
    Unknown,
    Volume,
    Bind,
    Tmpfs,
}

#[derive(Debug, Default)]
struct RawMount {
    kind: RawKind,
    source: Option<String>,
    target: String,
    read_only: Option<bool>,
    selinux: Option<String>,
    tmpfs: Mapping,
}

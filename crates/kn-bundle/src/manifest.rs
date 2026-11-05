//! The `x-kiwi-nest` block: how an app is served and placed.
//!
//! ```yaml
//! x-kiwi-nest:
//!   domain: blog-pr12.example.com
//!   service: web            # the service that gets HTTP traffic
//!   port: 8080              # its port inside the container
//!   idle_timeout: 10m       # optional; 0 (the default) = always on
//!   auth:                   # optional basic auth, checked at the ingress
//!     - { user: preview, hash: "$2y$..." }
//!   shared:                 # optional shared volumes; mode is snapshot, ro (the default) or rw
//!     corpus: { target: /corpus, mode: snapshot }   # into every service, or only `services: [web]`
//!   nest: blog              # optional
//!   arch: arm64             # optional: amd64 or arm64
//!   node_labels: { disk: ssd }
//!   backups: false          # optional: skip scheduled backups (previews); `kiwi-nest backup` still works
//! ```

use std::collections::BTreeMap;

use serde_yaml_ng::Value;

use crate::compose::{Problem, is_valid_object_name};

/// A parsed `x-kiwi-nest` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub domain: String,
    pub service: String,
    pub port: u16,
    /// Seconds without requests before the app is stopped; 0 means always on.
    pub idle_timeout_secs: u64,
    pub auth: Vec<BasicAuth>,
    pub shared: BTreeMap<String, SharedMount>,
    pub nest: Option<String>,
    pub arch: Option<Arch>,
    pub node_labels: BTreeMap<String, String>,
    /// Whether the server's scheduled backups include the app (`backup app` always works).
    pub backups: bool,
}

/// One basic-auth user, with a bcrypt hash of their password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicAuth {
    pub user: String,
    pub hash: String,
}

/// A shared volume mounted into the app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedMount {
    pub target: String,
    pub mode: SharedMode,
    /// The services it's mounted into; every service if `None`.
    pub services: Option<Vec<String>>,
}

impl SharedMount {
    /// Whether it's mounted into `service`.
    pub fn mounted_into(&self, service: &str) -> bool {
        self.services.as_ref().is_none_or(|s| s.iter().any(|s| s == service))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SharedMode {
    /// The app's own writable copy, made on its first deploy.
    Snapshot,
    /// Read-only.
    Ro,
    /// Read-write; every app mounting it sees the writes.
    Rw,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    Amd64,
    Arm64,
}

impl SharedMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SharedMode::Snapshot => "snapshot",
            SharedMode::Ro => "ro",
            SharedMode::Rw => "rw",
        }
    }

    /// Whether the app's containers mount it read-only.
    pub fn read_only(self) -> bool {
        self == SharedMode::Ro
    }
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }

    /// The architecture this binary runs on, if it's one kiwi-nest knows.
    pub fn current() -> Option<Arch> {
        match std::env::consts::ARCH {
            "x86_64" => Some(Arch::Amd64),
            "aarch64" => Some(Arch::Arm64),
            _ => None,
        }
    }
}

/// Whether `name` can name an app or a nest: lowercase letters, digits and `-`, at most 50 characters, starting
/// and ending with a letter or digit.
pub fn is_valid_app_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 50
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && b[0] != b'-'
        && b[b.len() - 1] != b'-'
}

/// Whether `domain` is a hostname kiwi-nest accepts: lowercase labels of letters, digits and `-`.
pub fn is_valid_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.split('.').all(|label| {
            let b = label.as_bytes();
            !b.is_empty()
                && b.len() <= 63
                && b.iter()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
                && b[0] != b'-'
                && b[b.len() - 1] != b'-'
        })
}

/// Parses a duration such as `90s`, `10m`, `1h30m` or `2d` (or `0`) into seconds.
pub fn parse_duration(s: &str) -> Option<u64> {
    if s == "0" {
        return Some(0);
    }
    let mut total: u64 = 0;
    let mut number = String::new();
    let mut any = false;
    for c in s.chars() {
        if c.is_ascii_digit() {
            number.push(c);
            continue;
        }
        let unit = match c {
            's' => 1,
            'm' => 60,
            'h' => 3600,
            'd' => 86400,
            _ => return None,
        };
        let n: u64 = number.parse().ok()?;
        total = total.checked_add(n.checked_mul(unit)?)?;
        number.clear();
        any = true;
    }
    (any && number.is_empty()).then_some(total)
}

fn is_bcrypt(hash: &str) -> bool {
    hash.len() == 60 && ["$2a$", "$2b$", "$2y$"].iter().any(|p| hash.starts_with(p))
}

/// Parses `x-kiwi-nest`, returning every problem found.
pub fn parse(value: &Value) -> Result<Manifest, Vec<Problem>> {
    const AT: &str = "x-kiwi-nest";
    let mut problems = Vec::new();
    let mut refuse = |key: &str, message: String| {
        let at = if key.is_empty() {
            AT.to_string()
        } else {
            format!("{AT}.{key}")
        };
        problems.push(Problem::new(at, message));
    };
    let Value::Mapping(map) = value else {
        refuse("", "must be a mapping".into());
        return Err(problems);
    };
    let mut m = Manifest {
        domain: String::new(),
        service: String::new(),
        port: 0,
        idle_timeout_secs: 0,
        auth: Vec::new(),
        shared: BTreeMap::new(),
        nest: None,
        arch: None,
        node_labels: BTreeMap::new(),
        backups: true,
    };
    let (mut domain, mut service, mut port) = (false, false, false);
    for (k, v) in map {
        let Some(key) = k.as_str() else {
            refuse("", "keys must be strings".into());
            continue;
        };
        match key {
            "domain" => match v.as_str() {
                Some(d) if is_valid_domain(d) => {
                    m.domain = d.to_string();
                    domain = true;
                }
                _ => refuse(key, "must be a lowercase hostname, such as `blog.example.com`".into()),
            },
            "service" => match v.as_str() {
                Some(s) if !s.is_empty() => {
                    m.service = s.to_string();
                    service = true;
                }
                _ => refuse(key, "must name the service that gets HTTP traffic".into()),
            },
            "port" => match v.as_u64() {
                Some(p @ 1..=65535) => {
                    m.port = p as u16;
                    port = true;
                }
                _ => refuse(key, "must be a port number from 1 to 65535".into()),
            },
            "idle_timeout" => {
                let secs = match v {
                    Value::Number(n) => n.as_u64(),
                    Value::String(s) => parse_duration(s),
                    _ => None,
                };
                match secs {
                    Some(s) => m.idle_timeout_secs = s,
                    None => refuse(
                        key,
                        "must be a duration such as `10m`, `1h30m` or `90s`, or `0` for always on".into(),
                    ),
                }
            }
            "auth" => {
                let Some(list) = v.as_sequence() else {
                    refuse(key, "must be a list of `{ user, hash }`".into());
                    continue;
                };
                for (i, item) in list.iter().enumerate() {
                    let user = item.get("user").and_then(Value::as_str);
                    let hash = item.get("hash").and_then(Value::as_str);
                    let extra = item
                        .as_mapping()
                        .is_some_and(|m| m.keys().any(|k| !matches!(k.as_str(), Some("user" | "hash"))));
                    match (user, hash) {
                        (Some(u), Some(h))
                            if !extra
                                && !u.is_empty()
                                && !u.contains(':')
                                && !u.chars().any(|c| c.is_control() || c.is_whitespace())
                                && is_bcrypt(h) =>
                        {
                            m.auth.push(BasicAuth {
                                user: u.to_string(),
                                hash: h.to_string(),
                            })
                        }
                        _ => refuse(
                            &format!("auth[{i}]"),
                            "must be `{ user, hash }`, with a user name without `:` or spaces and a bcrypt hash".into(),
                        ),
                    }
                }
            }
            "shared" => {
                let Some(shared) = v.as_mapping() else {
                    refuse(key, "must map shared volume names to `{ target, mode }`".into());
                    continue;
                };
                for (name, item) in shared {
                    let Some(name) = name.as_str().filter(|n| is_valid_object_name(n)) else {
                        refuse(key, format!("`{}` isn't a valid volume name", yaml_inline(name)));
                        continue;
                    };
                    let at = format!("shared.{name}");
                    let target = item.get("target").and_then(Value::as_str);
                    let mode = match item.get("mode").map(|m| m.as_str()) {
                        None => Some(SharedMode::Ro),
                        Some(Some("ro")) => Some(SharedMode::Ro),
                        Some(Some("rw")) => Some(SharedMode::Rw),
                        Some(Some("snapshot")) => Some(SharedMode::Snapshot),
                        Some(_) => None,
                    };
                    let services = match item.get("services") {
                        None => Some(None),
                        Some(Value::Sequence(list)) if !list.is_empty() => list
                            .iter()
                            .map(|s| s.as_str().filter(|s| !s.is_empty()).map(str::to_string))
                            .collect::<Option<Vec<_>>>()
                            .map(Some),
                        Some(_) => None,
                    };
                    let extra = item.as_mapping().is_some_and(|m| {
                        m.keys()
                            .any(|k| !matches!(k.as_str(), Some("target" | "mode" | "services")))
                    });
                    match (target, mode, services) {
                        (Some(t), Some(mode), Some(services)) if t.starts_with('/') && !extra => {
                            m.shared.insert(
                                name.to_string(),
                                SharedMount {
                                    target: t.to_string(),
                                    mode,
                                    services,
                                },
                            );
                        }
                        _ => refuse(
                            &at,
                            "must be `{ target, mode, services }`, with an absolute target, a mode of `snapshot`, \
                             `ro` or `rw`, and optionally a list of services"
                                .into(),
                        ),
                    }
                }
            }
            "nest" => match v.as_str() {
                Some(n) if is_valid_app_name(n) => m.nest = Some(n.to_string()),
                _ => refuse(key, "must be a nest name: lowercase letters, digits and `-`".into()),
            },
            "arch" => match v.as_str() {
                Some("amd64") => m.arch = Some(Arch::Amd64),
                Some("arm64") => m.arch = Some(Arch::Arm64),
                _ => refuse(key, "must be `amd64` or `arm64`".into()),
            },
            "node_labels" => {
                let Some(labels) = v.as_mapping() else {
                    refuse(key, "must map label names to values".into());
                    continue;
                };
                for (lk, lv) in labels {
                    match (lk.as_str(), scalar_string(lv)) {
                        (Some(lk), Some(lv)) if !lk.is_empty() => {
                            m.node_labels.insert(lk.to_string(), lv);
                        }
                        _ => refuse(key, "must map label names to plain values".into()),
                    }
                }
            }
            "backups" => match v.as_bool() {
                Some(b) => m.backups = b,
                None => refuse(
                    key,
                    "must be `true` or `false` (whether scheduled backups include the app)".into(),
                ),
            },
            other => refuse(other, "isn't a known `x-kiwi-nest` setting".into()),
        }
    }
    if !domain && !map.contains_key("domain") {
        refuse("", "needs a `domain`".into());
    }
    if !service && !map.contains_key("service") {
        refuse("", "needs a `service`: the one that gets HTTP traffic".into());
    }
    if !port && !map.contains_key("port") {
        refuse(
            "",
            "needs a `port`: the routed service's port inside its container".into(),
        );
    }
    if problems.is_empty() { Ok(m) } else { Err(problems) }
}

/// A scalar as a string, as Compose reads label values.
pub(crate) fn scalar_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A short rendering of a YAML value, for messages.
pub(crate) fn yaml_inline(v: &Value) -> String {
    scalar_string(v).unwrap_or_else(|| {
        serde_yaml_ng::to_string(v)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_str(yaml: &str) -> Result<Manifest, Vec<Problem>> {
        parse(&serde_yaml_ng::from_str::<Value>(yaml).unwrap())
    }

    fn problems(yaml: &str) -> String {
        parse_str(yaml)
            .expect_err("should be refused")
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    const HASH: &str = "$2y$05$abcdefghijklmnopqrstuuG3dNw1n3eO1n2Cx4M3jW7sXkEw0LZ6u";

    #[test]
    fn parses_a_full_manifest() {
        let m = parse_str(&format!(
            r#"
domain: blog-pr12.example.com
service: web
port: 8080
idle_timeout: 1h30m
auth: [{{ user: preview, hash: "{HASH}" }}]
shared:
  corpus: {{ target: /corpus, mode: snapshot }}
  maps: {{ target: /maps, services: [web] }}
nest: blog
arch: arm64
node_labels: {{ disk: ssd, gen: 3 }}
backups: false
"#
        ))
        .unwrap();
        assert_eq!(m.domain, "blog-pr12.example.com");
        assert_eq!(m.service, "web");
        assert_eq!(m.port, 8080);
        assert_eq!(m.idle_timeout_secs, 5400);
        assert_eq!(
            m.auth,
            vec![BasicAuth {
                user: "preview".into(),
                hash: HASH.into()
            }]
        );
        assert_eq!(m.shared["corpus"].mode, SharedMode::Snapshot);
        assert_eq!(m.shared["maps"].mode, SharedMode::Ro);
        assert_eq!(m.shared["corpus"].services, None);
        assert!(m.shared["corpus"].mounted_into("db"));
        assert_eq!(m.shared["maps"].services, Some(vec!["web".to_string()]));
        assert!(!m.shared["maps"].mounted_into("db"));
        assert_eq!(m.nest.as_deref(), Some("blog"));
        assert_eq!(m.arch, Some(Arch::Arm64));
        assert_eq!(m.node_labels["gen"], "3");
        assert!(!m.backups);
    }

    #[test]
    fn defaults_to_always_on() {
        let m = parse_str("{ domain: a.example.com, service: web, port: 80 }").unwrap();
        assert_eq!(m.idle_timeout_secs, 0);
        assert!(m.auth.is_empty() && m.shared.is_empty() && m.nest.is_none() && m.arch.is_none());
        assert!(m.backups, "scheduled backups are on unless turned off");
    }

    #[test]
    fn requires_domain_service_and_port() {
        let p = problems("{ idle_timeout: 0 }");
        assert!(p.contains("needs a `domain`"), "{p}");
        assert!(p.contains("needs a `service`"), "{p}");
        assert!(p.contains("needs a `port`"), "{p}");
        assert!(problems("[]").contains("must be a mapping"));
    }

    #[test]
    fn refuses_bad_values() {
        let base = "domain: a.example.com\nservice: web\nport: 80\n";
        for (extra, expect) in [
            ("domain: Blog.Example.com", "x-kiwi-nest.domain"),
            ("domain: -a.example.com", "x-kiwi-nest.domain"),
            ("domain: a..b", "x-kiwi-nest.domain"),
            ("port: 0", "x-kiwi-nest.port"),
            ("port: 70000", "x-kiwi-nest.port"),
            ("port: http", "x-kiwi-nest.port"),
            ("service: ''", "x-kiwi-nest.service"),
            ("idle_timeout: 10x", "x-kiwi-nest.idle_timeout"),
            ("idle_timeout: '600'", "x-kiwi-nest.idle_timeout"),
            ("idle_timeout: -1", "x-kiwi-nest.idle_timeout"),
            ("auth: [{ user: a, hash: plain }]", "x-kiwi-nest.auth[0]"),
            ("auth: [{ user: 'a:b', hash: plain }]", "x-kiwi-nest.auth[0]"),
            ("auth: { user: a }", "x-kiwi-nest.auth"),
            ("shared: { corpus: { target: corpus } }", "x-kiwi-nest.shared.corpus"),
            (
                "shared: { corpus: { target: /c, mode: write } }",
                "x-kiwi-nest.shared.corpus",
            ),
            ("shared: { '../x': { target: /c } }", "x-kiwi-nest.shared"),
            (
                "shared: { corpus: { target: /c, services: web } }",
                "x-kiwi-nest.shared.corpus",
            ),
            (
                "shared: { corpus: { target: /c, services: [] } }",
                "x-kiwi-nest.shared.corpus",
            ),
            (
                "shared: { corpus: { target: /c, size: 1 } }",
                "x-kiwi-nest.shared.corpus",
            ),
            ("nest: Blog", "x-kiwi-nest.nest"),
            ("arch: x86_64", "x-kiwi-nest.arch"),
            ("node_labels: [ssd]", "x-kiwi-nest.node_labels"),
            ("replicas: 2", "x-kiwi-nest.replicas: isn't a known"),
            ("backups: 'no'", "x-kiwi-nest.backups"),
            ("backups: daily", "x-kiwi-nest.backups"),
        ] {
            let key = extra.split(':').next().unwrap();
            let yaml = base
                .lines()
                .filter(|l| !l.starts_with(&format!("{key}:")))
                .chain([extra])
                .collect::<Vec<_>>()
                .join("\n");
            let p = problems(&yaml);
            assert!(p.contains(expect), "{extra}: {p}");
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("0"), Some(0));
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("10m"), Some(600));
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration("2d"), Some(172800));
        assert_eq!(parse_duration("10"), None);
        assert_eq!(parse_duration("m"), None);
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("1 h"), None);
    }

    #[test]
    fn names() {
        assert!(is_valid_app_name("blog-pr12"));
        assert!(!is_valid_app_name("Blog"));
        assert!(!is_valid_app_name("-blog"));
        assert!(!is_valid_app_name("blog_pr"));
        assert!(!is_valid_app_name(&"a".repeat(51)));
        assert!(is_valid_domain("localhost"));
        assert!(!is_valid_domain("a.example.com."));
    }
}

//! The systemd units, and the drop-ins that hold each host's own flags.
//!
//! Each unit runs `/usr/local/bin/kiwi-nest ROLE $KIWI_NEST_ARGS`, and is the same on every host: upgrades write it
//! again from the new release. What's particular to a host (its domains, its address, its volumes' mount) goes in
//! a drop-in, `UNIT.d/host.conf`, which `server init` and `node join` write and upgrades never touch. Further
//! drop-ins beside it (`UNIT.d/*.conf`) are yours, and are left alone too.
//!
//! ```text
//! /etc/systemd/system/kiwi-nest-server.service          the server, as the unprivileged `kiwi-nest` user
//! /etc/systemd/system/kiwi-nest-server.service.d/host.conf
//! /etc/systemd/system/kiwi-nest-caddy.service           the ingress's Caddy, as `kiwi-nest-caddy`
//! /etc/systemd/system/kiwi-nest-node.service            a node, as root, after Docker and its volumes' mount
//! /etc/systemd/system/kiwi-nest-node.service.d/host.conf
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, bail};

use crate::layout::{BIN, write_atomic};

pub const SERVER: &str = "kiwi-nest-server.service";
pub const NODE: &str = "kiwi-nest-node.service";
pub const CADDY: &str = "kiwi-nest-caddy.service";

/// The server's user, and its group.
pub const SERVER_USER: &str = "kiwi-nest";
/// Caddy's user, and its group.
pub const CADDY_USER: &str = "kiwi-nest-caddy";

/// The drop-in that holds a host's own flags.
pub const HOST_DROPIN: &str = "host.conf";

/// The variable a unit's drop-in sets to the host's flags.
pub const ARGS_VAR: &str = "KIWI_NEST_ARGS";

/// Caddy's configuration at boot: only its admin API on loopback. The server loads the rest.
pub const CADDY_CONFIG: &str = "/etc/kiwi-nest/caddy.json";
/// Caddy's own storage: its certificates, its ACME account, its internal CA.
pub const CADDY_DATA: &str = "/var/lib/kiwi-nest-caddy";
/// The Caddy binary the unit runs.
pub const CADDY_BIN: &str = "/usr/local/lib/kiwi-nest/caddy/caddy";

const DOCS: &str = "https://github.com/dire-kiwi/kiwi-nest/blob/main/docs/install.md";

/// A unit's text, as this release writes it.
pub fn unit(name: &str) -> Option<String> {
    Some(match name {
        SERVER => format!(
            "# Written by kiwi-nest; upgrades replace it. Put this host's own settings in {SERVER}.d/*.conf.
[Unit]
Description=kiwi-nest server
Documentation={DOCS}
After=network-online.target docker.service {CADDY}
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
User={SERVER_USER}
Group={SERVER_USER}
ExecStart={BIN} server ${ARGS_VAR}
Restart=always
RestartSec=2
TimeoutStopSec=30
LimitNOFILE=65536
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=full
ProtectHome=yes

[Install]
WantedBy=multi-user.target
"
        ),
        NODE => format!(
            "# Written by kiwi-nest; upgrades replace it. Put this host's own settings in {NODE}.d/*.conf.
# The node runs as root: it controls Docker and btrfs. Apps keep running when it stops.
[Unit]
Description=kiwi-nest node
Documentation={DOCS}
After=network-online.target docker.service
Wants=network-online.target docker.service
StartLimitIntervalSec=0

[Service]
ExecStart={BIN} node ${ARGS_VAR}
Restart=always
RestartSec=2
TimeoutStopSec=30
LimitNOFILE=1048576

[Install]
WantedBy=multi-user.target
"
        ),
        CADDY => format!(
            "# Written by kiwi-nest; upgrades replace it. The kiwi-nest server configures this Caddy through its
# admin API on loopback, and keeps the configuration only in Caddy's memory.
[Unit]
Description=kiwi-nest ingress (Caddy)
Documentation={DOCS}
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
User={CADDY_USER}
Group={CADDY_USER}
Environment=XDG_DATA_HOME={CADDY_DATA} XDG_CONFIG_HOME={CADDY_DATA}
ExecStart={CADDY_BIN} run --config {CADDY_CONFIG}
Restart=always
RestartSec=2
TimeoutStopSec=10
LimitNOFILE=1048576
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=full
ProtectHome=yes

[Install]
WantedBy=multi-user.target
"
        ),
        _ => return None,
    })
}

/// Caddy's configuration at boot: its admin API on loopback, nothing else.
pub fn caddy_boot_config(admin: &str) -> String {
    serde_json::json!({ "admin": { "listen": admin } }).to_string() + "\n"
}

/// A host's drop-in: its flags, and any other settings for the unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DropIn {
    /// Settings for `[Unit]`, such as `RequiresMountsFor`.
    pub unit: Vec<(String, String)>,
    /// Settings for `[Service]`, such as `SupplementaryGroups`.
    pub service: Vec<(String, String)>,
    /// The flags, which go in [`ARGS_VAR`].
    pub args: Vec<String>,
}

impl DropIn {
    pub fn render(&self, by: &str) -> anyhow::Result<String> {
        let mut out = format!(
            "# Written by `{by}`. This host's own settings: upgrades leave this file alone. The flags in {ARGS_VAR}\n\
             # are those of `kiwi-nest --help`; after a change, run `systemctl daemon-reload` and restart the unit.\n"
        );
        for (k, v) in self.unit.iter().chain(&self.service) {
            check_value(k, v)?;
        }
        if !self.unit.is_empty() {
            out.push_str("[Unit]\n");
            for (k, v) in &self.unit {
                out.push_str(&format!("{k}={}\n", escape(v)));
            }
        }
        out.push_str("[Service]\n");
        for (k, v) in &self.service {
            out.push_str(&format!("{k}={}\n", escape(v)));
        }
        for a in &self.args {
            check_arg(a)?;
        }
        let args: Vec<String> = self.args.iter().map(|a| escape(a)).collect();
        out.push_str(&format!("Environment=\"{ARGS_VAR}={}\"\n", args.join(" ")));
        Ok(out)
    }

    /// The flags in a drop-in's text, as [`DropIn::render`] wrote them.
    pub fn parse_args(text: &str) -> Vec<String> {
        let prefix = format!("Environment=\"{ARGS_VAR}=");
        text.lines()
            .filter_map(|l| l.trim().strip_prefix(&prefix)?.strip_suffix('"').map(str::to_string))
            .next_back()
            .map(|v| v.split_whitespace().map(|a| a.replace("%%", "%")).collect())
            .unwrap_or_default()
    }
}

/// The value of a flag in a list of them, given as `--flag VALUE` or `--flag=VALUE`.
pub fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let eq = format!("{name}=");
    let mut it = args.iter();
    let mut found = None;
    while let Some(a) = it.next() {
        if a == name {
            found = it.next().map(String::as_str);
        } else if let Some(v) = a.strip_prefix(&eq) {
            found = Some(v);
        }
    }
    found
}

/// `%` is systemd's specifier character.
fn escape(v: &str) -> String {
    v.replace('%', "%%")
}

fn check_arg(a: &str) -> anyhow::Result<()> {
    if a.is_empty()
        || a.chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '\\'))
    {
        bail!("{a:?} can't go in a systemd drop-in's flags: no spaces, quotes or backslashes");
    }
    Ok(())
}

fn check_value(k: &str, v: &str) -> anyhow::Result<()> {
    if v.contains('\n') || k.contains('=') || k.contains('\n') {
        bail!("bad drop-in setting {k}={v:?}");
    }
    Ok(())
}

/// systemd's unit directory, and `systemctl`.
#[derive(Debug, Clone)]
pub struct Systemd {
    pub dir: PathBuf,
}

impl Default for Systemd {
    fn default() -> Self {
        Systemd {
            dir: "/etc/systemd/system".into(),
        }
    }
}

impl Systemd {
    pub fn unit_path(&self, unit: &str) -> PathBuf {
        self.dir.join(unit)
    }

    pub fn dropin_path(&self, unit: &str, name: &str) -> PathBuf {
        self.dir.join(format!("{unit}.d")).join(name)
    }

    /// Whether a unit is installed here.
    pub fn has(&self, unit: &str) -> bool {
        self.unit_path(unit).is_file()
    }

    /// Writes a unit as this release has it. Returns whether it changed.
    pub fn write_unit(&self, unit: &str) -> anyhow::Result<bool> {
        let text = self::unit(unit).with_context(|| format!("no unit {unit}"))?;
        let path = self.unit_path(unit);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(text.as_str()) {
            return Ok(false);
        }
        write_atomic(&path, text.as_bytes(), 0o644)?;
        Ok(true)
    }

    /// Writes a unit's drop-in.
    pub fn write_dropin(&self, unit: &str, name: &str, text: &str) -> anyhow::Result<()> {
        write_atomic(&self.dropin_path(unit, name), text.as_bytes(), 0o644)
    }

    /// The flags in a unit's host drop-in.
    pub fn host_args(&self, unit: &str) -> Vec<String> {
        std::fs::read_to_string(self.dropin_path(unit, HOST_DROPIN))
            .map(|t| DropIn::parse_args(&t))
            .unwrap_or_default()
    }
}

/// Runs `systemctl ARGS`.
pub fn systemctl(args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("systemctl")
        .args(args)
        .output()
        .context("running systemctl")?;
    if !out.status.success() {
        bail!(
            "systemctl {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Whether a unit is active.
pub fn is_active(unit: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", unit])
        .status()
        .is_ok_and(|s| s.success())
}

/// The last lines of a unit's journal, to show why it failed.
pub fn journal_tail(unit: &str, lines: u32) -> String {
    Command::new("journalctl")
        .args(["-u", unit, "-n", &lines.to_string(), "--no-pager", "-o", "cat"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// Whether systemd runs this machine.
pub fn present() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_run_the_installed_binary_with_the_hosts_flags() {
        for u in [SERVER, NODE] {
            let text = unit(u).unwrap();
            assert!(text.contains(&format!("ExecStart={BIN} ")), "{text}");
            assert!(text.contains("$KIWI_NEST_ARGS"), "{text}");
        }
        assert!(unit(SERVER).unwrap().contains("User=kiwi-nest\n"));
        assert!(!unit(NODE).unwrap().contains("User="));
        assert!(unit(CADDY).unwrap().contains("CAP_NET_BIND_SERVICE"));
        assert!(unit("other.service").is_none());
    }

    #[test]
    fn drop_ins_round_trip_their_flags() {
        let d = DropIn {
            unit: vec![("RequiresMountsFor".into(), "/srv/kn".into())],
            service: vec![("SupplementaryGroups".into(), "docker".into())],
            args: vec![
                "--server".into(),
                "https://kn.example.com".into(),
                "--label=disk=50%".into(),
                "--ingress-listen".into(),
                "192.168.1.5:7382".into(),
            ],
        };
        let text = d.render("kiwi-nest node join").unwrap();
        assert!(
            text.contains("[Unit]\nRequiresMountsFor=/srv/kn\n[Service]\nSupplementaryGroups=docker\n"),
            "{text}"
        );
        assert!(text.contains("disk=50%%"), "{text}");
        assert_eq!(DropIn::parse_args(&text), d.args);
        assert_eq!(flag(&d.args, "--server"), Some("https://kn.example.com"));
        assert_eq!(flag(&d.args, "--label"), Some("disk=50%"));
        assert_eq!(flag(&d.args, "--listen"), None);
        for bad in ["two words", "quo\"te", "back\\slash", ""] {
            let d = DropIn {
                args: vec![bad.into()],
                ..Default::default()
            };
            assert!(d.render("x").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn writes_units_and_reads_the_hosts_flags() {
        let dir = tempfile::tempdir().unwrap();
        let s = Systemd { dir: dir.path().into() };
        assert!(!s.has(NODE));
        assert!(s.write_unit(NODE).unwrap());
        assert!(!s.write_unit(NODE).unwrap());
        assert!(s.has(NODE));
        let d = DropIn {
            args: vec!["--listen".into(), "127.0.0.1:7400".into()],
            ..Default::default()
        };
        s.write_dropin(SERVER, HOST_DROPIN, &d.render("x").unwrap()).unwrap();
        assert_eq!(flag(&s.host_args(SERVER), "--listen"), Some("127.0.0.1:7400"));
        assert!(s.host_args(NODE).is_empty());
    }
}

//! Running `docker compose` for an app's project.
//!
//! Every call names the project and its generated file explicitly, and runs with an empty environment apart from
//! what Docker itself needs, so nothing on the node (variables, `.env` files) leaks into an app.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::Deserialize;
use tokio::process::{Child, Command};

/// Environment variables passed through to `docker`.
const PASSED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "DOCKER_HOST",
    "DOCKER_CONFIG",
    "DOCKER_CONTEXT",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "XDG_RUNTIME_DIR",
];

/// The most of a failed command's stderr kept in its error.
const STDERR_LIMIT: usize = 4000;

/// One Compose project: an app's deploy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The Compose project name.
    pub name: String,
    /// The generated Compose file.
    pub file: PathBuf,
    /// The project directory, which holds no `.env` and nothing of the bundle's.
    pub dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("couldn't run `{command}`: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("`{command}` failed ({status}): {stderr}")]
    Failed {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("couldn't read the output of `{command}`: {message}")]
    Output { command: String, message: String },
}

/// A container, as `docker compose ps` reports it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct ContainerState {
    #[serde(rename = "ID")]
    pub id: String,
    pub name: String,
    pub service: String,
    pub state: String,
    pub health: String,
    pub status: String,
    pub exit_code: i64,
}

/// A project, as `docker compose ls` reports it.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "PascalCase", default)]
pub struct ProjectState {
    pub name: String,
    /// Such as `running(2)` or `exited(1), running(1)`.
    pub status: String,
}

/// What to show of an app's logs.
#[derive(Debug, Clone, Default)]
pub struct LogsOptions {
    pub tail: Option<u32>,
    pub follow: bool,
    pub timestamps: bool,
    pub service: Option<String>,
}

/// What removing an image came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageRemoval {
    Removed,
    /// There was no such image.
    Missing,
    /// A container uses it, so it stays.
    InUse,
}

/// An IP network, such as `172.18.0.0/16`: one of a project's networks, whose containers' addresses are in it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subnet {
    addr: IpAddr,
    prefix: u8,
}

impl Subnet {
    /// Parses `ADDR/PREFIX`.
    pub fn parse(s: &str) -> Option<Subnet> {
        let (addr, prefix) = s.trim().split_once('/')?;
        let addr: IpAddr = addr.parse().ok()?;
        let prefix: u8 = prefix.parse().ok()?;
        let max = if addr.is_ipv4() { 32 } else { 128 };
        (prefix <= max).then_some(Subnet { addr, prefix })
    }

    /// Whether `ip` is in it. An IPv4 address mapped into IPv6 (as a dual-stack socket reports it) counts as the
    /// IPv4 address.
    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        let (a, b, bits) = match (self.addr, ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) => (u32::from(a) as u128, u32::from(b) as u128, 32),
            (IpAddr::V6(a), IpAddr::V6(b)) => (u128::from(a), u128::from(b), 128),
            _ => return false,
        };
        let shift = bits - u32::from(self.prefix);
        shift >= bits || (a >> shift) == (b >> shift)
    }
}

impl std::fmt::Display for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// What the node needs from a container runtime: an app's Compose project, created, started, stopped and listed.
///
/// [`Docker`] is the real one. Tests use a fake, so the gateway's lifecycle can be checked without Docker.
pub trait Runtime: Send + Sync + 'static {
    /// Creates and starts the project, waiting until its containers are running and healthy.
    fn up<'a>(&'a self, p: &'a Project, wait_timeout: Duration) -> BoxFuture<'a, Result<(), RuntimeError>>;
    /// Starts the project's existing containers.
    fn start<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>>;
    /// Stops the project's containers, keeping them.
    fn stop<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>>;
    /// Removes the project's containers and networks. The app's data is in bind mounts, so it stays.
    fn down<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>>;
    fn pause<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>>;
    fn unpause<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>>;
    /// The project's containers, stopped ones included.
    fn ps<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<Vec<ContainerState>, RuntimeError>>;
    /// `docker inspect` of some containers.
    fn inspect<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, Result<serde_json::Value, RuntimeError>>;
    /// Every Compose project on the engine whose name starts with `prefix`, stopped ones included.
    fn projects<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<ProjectState>, RuntimeError>>;
    /// Starts `docker compose logs`, with its output piped. The caller reads it, and the process is killed when
    /// the [`Child`] is dropped.
    fn logs(&self, p: &Project, opts: &LogsOptions) -> Result<Child, RuntimeError>;
    /// Removes an image by reference, unless a container (any container on the engine) uses it.
    fn remove_image<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, Result<ImageRemoval, RuntimeError>>;
    /// The subnets of the project's own networks, which its containers' addresses are in.
    fn networks<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<Vec<Subnet>, RuntimeError>>;
}

/// Runs `docker compose`.
#[derive(Debug, Clone)]
pub struct Docker {
    docker: PathBuf,
    /// The Docker configuration directory to use (`DOCKER_CONFIG`), rather than the environment's.
    config: Option<PathBuf>,
}

impl Docker {
    /// `docker` is the Docker CLI to run, such as `docker` (found on `PATH`) or `/usr/bin/docker`.
    pub fn new(docker: impl Into<PathBuf>) -> Self {
        Docker {
            docker: docker.into(),
            config: None,
        }
    }

    /// Docker as the node runs it: with the node's own configuration directory, if it has one.
    pub fn for_node(cfg: &crate::NodeConfig) -> Self {
        Docker {
            docker: cfg.docker.clone(),
            config: cfg.docker_config.clone(),
        }
    }

    fn base(&self) -> Command {
        let mut c = Command::new(&self.docker);
        c.env_clear();
        for k in PASSED_ENV {
            if let Some(v) = std::env::var_os(k) {
                c.env(k, v);
            }
        }
        if std::env::var_os("PATH").is_none() {
            c.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        }
        if let Some(dir) = &self.config {
            c.env("DOCKER_CONFIG", dir);
        }
        c.env("COMPOSE_DISABLE_ENV_FILE", "1");
        c.stdin(Stdio::null()).kill_on_drop(true);
        c
    }

    /// `docker compose` for a project, with its file if it still exists.
    fn compose(&self, p: &Project, args: &[&str]) -> Command {
        let mut c = self.base();
        c.arg("compose").arg("--project-name").arg(&p.name);
        if p.file.exists() {
            c.arg("--project-directory").arg(&p.dir).arg("--file").arg(&p.file);
        }
        c.args(args);
        c
    }

    /// The address of Docker's default bridge network on this host (`docker0`), which is also where a container's
    /// `host-gateway` points by default, if it has one.
    pub async fn bridge_gateway(&self) -> Result<Option<IpAddr>, RuntimeError> {
        let mut c = self.base();
        c.args([
            "network",
            "inspect",
            "bridge",
            "--format",
            "{{range .IPAM.Config}}{{.Gateway}} {{end}}",
        ]);
        let out = self.run(c).await?;
        // The IPv4 gateway, if there's one, as an IPv6 one may come first.
        let mut ips: Vec<IpAddr> = out.split_whitespace().filter_map(|a| a.parse().ok()).collect();
        ips.sort_by_key(|ip| !ip.is_ipv4());
        Ok(ips.first().copied())
    }

    async fn run(&self, mut c: Command) -> Result<String, RuntimeError> {
        let command = describe(&c);
        tracing::debug!(%command, "running");
        let out = c
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await
            .map_err(|source| RuntimeError::Spawn {
                command: command.clone(),
                source,
            })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stderr = stderr.trim();
            let start = stderr.len().saturating_sub(STDERR_LIMIT);
            let start = (start..=stderr.len())
                .find(|i| stderr.is_char_boundary(*i))
                .unwrap_or(0);
            return Err(RuntimeError::Failed {
                command,
                status: out.status.to_string(),
                stderr: stderr[start..].to_string(),
            });
        }
        String::from_utf8(out.stdout).map_err(|e| RuntimeError::Output {
            command,
            message: e.to_string(),
        })
    }
}

impl Runtime for Docker {
    fn up<'a>(&'a self, p: &'a Project, wait_timeout: Duration) -> BoxFuture<'a, Result<(), RuntimeError>> {
        Box::pin(async move {
            let timeout = wait_timeout.as_secs().max(1).to_string();
            let args = [
                "up",
                "--detach",
                "--wait",
                "--wait-timeout",
                &timeout,
                "--remove-orphans",
                "--quiet-pull",
            ];
            self.run(self.compose(p, &args)).await.map(drop)
        })
    }

    fn start<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>> {
        Box::pin(async move { self.run(self.compose(p, &["start"])).await.map(drop) })
    }

    fn stop<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>> {
        Box::pin(async move { self.run(self.compose(p, &["stop"])).await.map(drop) })
    }

    fn down<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>> {
        Box::pin(async move { self.run(self.compose(p, &["down", "--remove-orphans"])).await.map(drop) })
    }

    fn pause<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>> {
        Box::pin(async move { self.run(self.compose(p, &["pause"])).await.map(drop) })
    }

    fn unpause<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>> {
        Box::pin(async move { self.run(self.compose(p, &["unpause"])).await.map(drop) })
    }

    fn ps<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<Vec<ContainerState>, RuntimeError>> {
        Box::pin(async move {
            let c = self.compose(p, &["ps", "--all", "--format", "json"]);
            let command = describe(&c);
            let out = self.run(c).await?;
            parse_json_lines(&out).map_err(|message| RuntimeError::Output { command, message })
        })
    }

    fn inspect<'a>(&'a self, ids: &'a [String]) -> BoxFuture<'a, Result<serde_json::Value, RuntimeError>> {
        Box::pin(async move {
            let mut c = self.base();
            c.arg("inspect").args(ids);
            let command = describe(&c);
            let out = self.run(c).await?;
            serde_json::from_str(&out).map_err(|e| RuntimeError::Output {
                command,
                message: e.to_string(),
            })
        })
    }

    fn projects<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<ProjectState>, RuntimeError>> {
        Box::pin(async move {
            let mut c = self.base();
            c.args(["compose", "ls", "--all", "--format", "json"]);
            let command = describe(&c);
            let out = self.run(c).await?;
            let all: Vec<ProjectState> =
                parse_json_lines(&out).map_err(|message| RuntimeError::Output { command, message })?;
            Ok(all.into_iter().filter(|p| p.name.starts_with(prefix)).collect())
        })
    }

    fn logs(&self, p: &Project, opts: &LogsOptions) -> Result<Child, RuntimeError> {
        let tail = opts.tail.map_or_else(|| "all".to_string(), |n| n.to_string());
        let mut args = vec!["logs", "--no-color", "--tail", &tail];
        if opts.follow {
            args.push("--follow");
        }
        if opts.timestamps {
            args.push("--timestamps");
        }
        if let Some(s) = &opts.service {
            args.push(s);
        }
        let mut c = self.compose(p, &args);
        let command = describe(&c);
        c.stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| RuntimeError::Spawn { command, source })
    }

    fn remove_image<'a>(&'a self, reference: &'a str) -> BoxFuture<'a, Result<ImageRemoval, RuntimeError>> {
        Box::pin(async move {
            let mut c = self.base();
            c.args(["image", "rm", reference]);
            match self.run(c).await {
                Ok(_) => Ok(ImageRemoval::Removed),
                Err(RuntimeError::Failed { stderr, .. }) if stderr.contains("No such image") => {
                    Ok(ImageRemoval::Missing)
                }
                Err(RuntimeError::Failed { stderr, .. })
                    if stderr.contains("conflict") || stderr.contains("is using") || stderr.contains("being used") =>
                {
                    Ok(ImageRemoval::InUse)
                }
                Err(e) => Err(e),
            }
        })
    }

    fn networks<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<Vec<Subnet>, RuntimeError>> {
        Box::pin(async move {
            let mut c = self.base();
            c.args(["network", "ls", "--quiet", "--no-trunc", "--filter"])
                .arg(format!("label=com.docker.compose.project={}", p.name));
            let ids = self.run(c).await?;
            let ids: Vec<&str> = ids.split_whitespace().collect();
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let mut c = self.base();
            c.args([
                "network",
                "inspect",
                "--format",
                "{{range .IPAM.Config}}{{.Subnet}} {{end}}",
            ])
            .args(&ids);
            let command = describe(&c);
            let out = self.run(c).await?;
            out.split_whitespace()
                .map(|s| {
                    Subnet::parse(s).ok_or_else(|| RuntimeError::Output {
                        command: command.clone(),
                        message: format!("`{s}` isn't a subnet"),
                    })
                })
                .collect()
        })
    }
}

/// Parses Compose's JSON output: one object per line in current versions, an array in older ones.
fn parse_json_lines<T: for<'de> Deserialize<'de>>(out: &str) -> Result<Vec<T>, String> {
    let out = out.trim();
    if out.is_empty() {
        return Ok(Vec::new());
    }
    if out.starts_with('[') {
        return serde_json::from_str(out).map_err(|e| e.to_string());
    }
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| e.to_string()))
        .collect()
}

fn describe(c: &Command) -> String {
    let std = c.as_std();
    let mut parts = vec![Path::new(std.get_program()).display().to_string()];
    parts.extend(std.get_args().map(|a| a.to_string_lossy().into_owned()));
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ps_output_in_both_formats() {
        let line = r#"{"ID":"abc","Name":"kn-blog-web-1","Service":"web","State":"running","Health":"healthy","Status":"Up 2 minutes (healthy)","ExitCode":0,"Publishers":[]}"#;
        let one: Vec<ContainerState> = parse_json_lines(&format!("{line}\n{line}\n")).unwrap();
        assert_eq!(one.len(), 2);
        assert_eq!(one[0].service, "web");
        assert_eq!(one[0].health, "healthy");
        let arr: Vec<ContainerState> = parse_json_lines(&format!("[{line}]")).unwrap();
        assert_eq!(arr, one[..1]);
        assert!(parse_json_lines::<ContainerState>("").unwrap().is_empty());
    }

    #[test]
    fn subnets() {
        let s = Subnet::parse("172.18.0.0/16").unwrap();
        assert_eq!(s.to_string(), "172.18.0.0/16");
        assert!(s.contains("172.18.0.5".parse().unwrap()));
        assert!(s.contains("172.18.255.255".parse().unwrap()));
        assert!(!s.contains("172.19.0.5".parse().unwrap()));
        assert!(
            s.contains("::ffff:172.18.0.5".parse().unwrap()),
            "an IPv4-mapped address"
        );
        assert!(!s.contains("fd00::5".parse().unwrap()));
        let one = Subnet::parse("10.0.0.7/32").unwrap();
        assert!(one.contains("10.0.0.7".parse().unwrap()) && !one.contains("10.0.0.8".parse().unwrap()));
        assert!(Subnet::parse("0.0.0.0/0").unwrap().contains("8.8.8.8".parse().unwrap()));
        let v6 = Subnet::parse("fd00:1::/64").unwrap();
        assert!(v6.contains("fd00:1::9".parse().unwrap()) && !v6.contains("fd00:2::9".parse().unwrap()));
        for bad in ["172.18.0.0", "172.18.0.0/33", "fd00::/129", "x/8", "10.0.0.0/-1"] {
            assert!(Subnet::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn names_the_project_and_file_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("compose-1.yaml");
        std::fs::write(&file, "services: {}").unwrap();
        let p = Project {
            name: "kn-blog".into(),
            file: file.clone(),
            dir: dir.path().into(),
        };
        let rt = Docker::new("docker");
        let c = rt.compose(&p, &["up"]);
        let d = describe(&c);
        assert!(
            d.starts_with("docker compose --project-name kn-blog --project-directory"),
            "{d}"
        );
        assert!(d.contains(&format!("--file {}", file.display())), "{d}");
        let envs: Vec<_> = c
            .as_std()
            .get_envs()
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(
            envs.iter()
                .all(|k| PASSED_ENV.contains(&k.as_str()) || k == "COMPOSE_DISABLE_ENV_FILE"),
            "{envs:?}"
        );
        // Without its file (say, after a crash), the project is still reached by name.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            describe(&rt.compose(&p, &["down"])),
            "docker compose --project-name kn-blog down"
        );
    }
}

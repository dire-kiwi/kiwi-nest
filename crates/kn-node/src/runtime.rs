//! Running `docker compose` for an app's project.
//!
//! Every call names the project and its generated file explicitly, and runs with an empty environment apart from
//! what Docker itself needs, so nothing on the node (variables, `.env` files) leaks into an app.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

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

/// Runs `docker compose`.
#[derive(Debug, Clone)]
pub struct Runtime {
    docker: PathBuf,
}

impl Runtime {
    /// `docker` is the Docker CLI to run, such as `docker` (found on `PATH`) or `/usr/bin/docker`.
    pub fn new(docker: impl Into<PathBuf>) -> Self {
        Runtime { docker: docker.into() }
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

    /// Creates and starts the project, waiting until its containers are running and healthy.
    pub async fn up(&self, p: &Project, wait_timeout: Duration) -> Result<(), RuntimeError> {
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
    }

    /// Starts the project's existing containers.
    pub async fn start(&self, p: &Project) -> Result<(), RuntimeError> {
        self.run(self.compose(p, &["start"])).await.map(drop)
    }

    /// Stops the project's containers, keeping them.
    pub async fn stop(&self, p: &Project) -> Result<(), RuntimeError> {
        self.run(self.compose(p, &["stop"])).await.map(drop)
    }

    /// Removes the project's containers and networks. The app's data is in bind mounts, so it stays.
    pub async fn down(&self, p: &Project) -> Result<(), RuntimeError> {
        self.run(self.compose(p, &["down", "--remove-orphans"])).await.map(drop)
    }

    pub async fn pause(&self, p: &Project) -> Result<(), RuntimeError> {
        self.run(self.compose(p, &["pause"])).await.map(drop)
    }

    pub async fn unpause(&self, p: &Project) -> Result<(), RuntimeError> {
        self.run(self.compose(p, &["unpause"])).await.map(drop)
    }

    /// The project's containers, stopped ones included.
    pub async fn ps(&self, p: &Project) -> Result<Vec<ContainerState>, RuntimeError> {
        let c = self.compose(p, &["ps", "--all", "--format", "json"]);
        let command = describe(&c);
        let out = self.run(c).await?;
        parse_json_lines(&out).map_err(|message| RuntimeError::Output { command, message })
    }

    /// `docker inspect` of some containers.
    pub async fn inspect(&self, ids: &[String]) -> Result<serde_json::Value, RuntimeError> {
        let mut c = self.base();
        c.arg("inspect").args(ids);
        let command = describe(&c);
        let out = self.run(c).await?;
        serde_json::from_str(&out).map_err(|e| RuntimeError::Output {
            command,
            message: e.to_string(),
        })
    }

    /// Every Compose project on the engine whose name starts with `prefix`, stopped ones included.
    pub async fn projects(&self, prefix: &str) -> Result<Vec<ProjectState>, RuntimeError> {
        let mut c = self.base();
        c.args(["compose", "ls", "--all", "--format", "json"]);
        let command = describe(&c);
        let out = self.run(c).await?;
        let all: Vec<ProjectState> =
            parse_json_lines(&out).map_err(|message| RuntimeError::Output { command, message })?;
        Ok(all.into_iter().filter(|p| p.name.starts_with(prefix)).collect())
    }

    /// Starts `docker compose logs`, with its output piped. The caller reads it, and the process is killed when
    /// the [`Child`] is dropped.
    pub fn logs(&self, p: &Project, opts: &LogsOptions) -> Result<Child, RuntimeError> {
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
    fn names_the_project_and_file_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("compose-1.yaml");
        std::fs::write(&file, "services: {}").unwrap();
        let p = Project {
            name: "kn-blog".into(),
            file: file.clone(),
            dir: dir.path().into(),
        };
        let rt = Runtime::new("docker");
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

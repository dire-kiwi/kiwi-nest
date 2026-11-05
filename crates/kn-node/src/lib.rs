//! The kiwi-nest node.
//!
//! - [`runtime`] runs `docker compose` for each app's project.
//! - [`apps`] handles each app's lifecycle, one task per app, and the node's state file.
//! - [`api`] is the standalone node's loopback HTTP API.
//!
//! Later steps add btrfs volumes, the gateway and the link to the server.

pub mod api;
pub mod apps;
pub mod runtime;

use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use kn_bundle::Limits;

/// How a node is set up.
#[derive(Debug, Clone)]
pub struct NodeConfig {
    /// Where the node keeps its state, bundles and generated files (absolute).
    pub state_dir: PathBuf,
    /// Where apps' volumes live (absolute).
    pub volumes_dir: PathBuf,
    /// Put before every app's name to make its Compose project name, so several nodes can share a Docker Engine.
    pub project_prefix: String,
    /// The loopback ports that apps' routed services are published on.
    pub ports: RangeInclusive<u16>,
    /// Host directories that bundles may mount.
    pub allowed_host_paths: Vec<PathBuf>,
    pub limits: Limits,
    /// The Docker CLI.
    pub docker: PathBuf,
    /// How long `docker compose up` may wait for an app's containers to be running and healthy.
    pub up_timeout: Duration,
}

impl NodeConfig {
    /// A configuration with the defaults, keeping everything under `state_dir`.
    pub fn new(state_dir: PathBuf) -> Self {
        NodeConfig {
            volumes_dir: state_dir.join("volumes"),
            state_dir,
            project_prefix: "kn-".into(),
            ports: 30000..=39999,
            allowed_host_paths: Vec::new(),
            limits: Limits::default(),
            docker: "docker".into(),
            up_timeout: Duration::from_secs(300),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        for (what, dir) in [
            ("state directory", &self.state_dir),
            ("volumes directory", &self.volumes_dir),
        ] {
            let Some(text) = dir.to_str() else {
                bail!("the {what} must be a UTF-8 path")
            };
            if !dir.is_absolute() {
                bail!("the {what} must be an absolute path, not {text}");
            }
            if text.contains('$') || text.contains(':') {
                bail!("the {what} can't contain `$` or `:`: {text}");
            }
        }
        let p = self.project_prefix.as_bytes();
        if p.is_empty()
            || p.len() > 20
            || !p[0].is_ascii_lowercase()
            || !p
                .iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_'))
        {
            bail!(
                "the project prefix must be up to 20 lowercase letters, digits, `-` and `_`, starting with a letter, \
                 not {:?}",
                self.project_prefix
            );
        }
        if self.ports.is_empty() || *self.ports.start() == 0 {
            bail!("the port range is empty");
        }
        for p in &self.allowed_host_paths {
            if !p.is_absolute() {
                bail!("allowed host paths must be absolute, not {}", p.display());
            }
        }
        Ok(())
    }
}

/// Refuses an address that isn't loopback: the standalone API has no authentication.
pub fn check_loopback(addr: &SocketAddr) -> anyhow::Result<()> {
    if !addr.ip().is_loopback() {
        bail!("the standalone node only listens on loopback (such as 127.0.0.1), not {addr}");
    }
    Ok(())
}

/// Runs a standalone node: its apps and its loopback API, until interrupted.
///
/// Prints `listening on http://ADDR` to stdout once the API is up. Apps keep running after the node exits.
pub async fn run_standalone(cfg: NodeConfig, listen: SocketAddr) -> anyhow::Result<()> {
    check_loopback(&listen)?;
    let apps = apps::Apps::open(cfg).await?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    let addr = listener.local_addr()?;
    println!("listening on http://{addr}");
    tracing::info!(%addr, "standalone node API listening");
    api::serve(listener, apps, shutdown_signal()).await?;
    tracing::info!("stopped; apps keep running");
    Ok(())
}

async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = async { match term.as_mut() { Some(t) => { t.recv().await; } None => std::future::pending().await } } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_the_config() {
        let ok = NodeConfig::new("/var/lib/kiwi-nest".into());
        ok.validate().unwrap();
        assert!(NodeConfig::new("relative".into()).validate().is_err());
        assert!(NodeConfig::new("/a$b".into()).validate().is_err());
        assert!(
            NodeConfig {
                project_prefix: "KN".into(),
                ..ok.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            NodeConfig {
                project_prefix: "".into(),
                ..ok.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            NodeConfig {
                project_prefix: "kn2_t-".into(),
                ..ok.clone()
            }
            .validate()
            .is_ok()
        );
        assert!(check_loopback(&"127.0.0.1:1".parse().unwrap()).is_ok());
        assert!(check_loopback(&"[::1]:1".parse().unwrap()).is_ok());
        assert!(check_loopback(&"0.0.0.0:1".parse().unwrap()).is_err());
    }
}

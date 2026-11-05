//! The kiwi-nest node.
//!
//! - [`volumes`] is the layout of the node's btrfs subvolumes, and the checks before every start.
//! - [`runtime`] runs `docker compose` for each app's project.
//! - [`apps`] handles each app's lifecycle, one task per app, its volumes, shared volumes, and the node's state file.
//! - [`gateway`] proxies requests to apps by Host, starts stopped apps for them, and lets idle ones be stopped.
//! - [`agent`] is the link to the server: joining, polling, applying the desired state, and commands.
//! - [`api`] is the standalone node's loopback HTTP API.

pub mod agent;
pub mod api;
pub mod apps;
pub mod gateway;
pub mod runtime;
#[cfg(test)]
mod testing;
pub mod volumes;

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
    /// Where apps' volumes live (absolute): a directory on btrfs.
    pub volumes_dir: PathBuf,
    /// Put before every app's name to make its Compose project name, so several nodes can share a Docker Engine.
    pub project_prefix: String,
    /// The loopback ports that apps' routed services are published on.
    pub ports: RangeInclusive<u16>,
    /// Host directories that bundles may mount.
    pub allowed_host_paths: Vec<PathBuf>,
    pub limits: Limits,
    /// The limits on what `volume load` reads.
    pub load_limits: Limits,
    /// The Docker CLI.
    pub docker: PathBuf,
    /// The Docker configuration directory the node runs Docker with (`DOCKER_CONFIG`), if not the environment's: a
    /// node joined to a server has its own, `STATE/docker`, holding its registry credential.
    pub docker_config: Option<PathBuf>,
    /// How long an image an app stopped using is kept, so deploying it again doesn't pull it.
    pub image_grace: Duration,
    /// How often unused images are removed; zero for never.
    pub prune_interval: Duration,
    /// How long `docker compose up` may wait for an app's containers to be running and healthy.
    pub up_timeout: Duration,
    /// How long starting a stopped app may take, until it's healthy and its port answers.
    pub start_timeout: Duration,
    /// After a failed start, how long requests are refused before one may start the app again.
    pub start_cooldown: Duration,
    /// How long a browser's page load waits for a starting app before it gets the starting page instead.
    pub starting_page_after: Duration,
    /// How often idle apps are looked for.
    pub idle_check_interval: Duration,
    /// A standalone node's backup targets (a node joined to a server gets the server's).
    pub backup_targets: Option<PathBuf>,
    /// How often the volumes' space is measured; zero never.
    pub usage_interval: Duration,
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
            load_limits: Limits::for_volumes(),
            docker: "docker".into(),
            docker_config: None,
            image_grace: Duration::from_secs(24 * 3600),
            prune_interval: Duration::from_secs(3600),
            up_timeout: Duration::from_secs(300),
            start_timeout: Duration::from_secs(120),
            start_cooldown: Duration::from_secs(15),
            starting_page_after: Duration::from_secs(1),
            idle_check_interval: Duration::from_secs(1),
            backup_targets: None,
            usage_interval: Duration::from_secs(300),
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

/// Refuses an address that isn't loopback: the standalone API and gateway have no authentication.
pub fn check_loopback(addr: &SocketAddr) -> anyhow::Result<()> {
    if !addr.ip().is_loopback() {
        bail!("the standalone node only listens on loopback (such as 127.0.0.1), not {addr}");
    }
    Ok(())
}

/// Runs a standalone node: its apps, its loopback API and its gateway, until interrupted.
///
/// Prints `listening on http://ADDR` (the API), then `gateway on http://ADDR`, to stdout once both are up. Apps
/// keep running after the node exits, and the next node agent adopts them.
pub async fn run_standalone(cfg: NodeConfig, listen: SocketAddr, gateway_listen: SocketAddr) -> anyhow::Result<()> {
    check_loopback(&listen)?;
    check_loopback(&gateway_listen)?;
    let usage_interval = cfg.usage_interval;
    let apps = apps::Apps::open(cfg).await?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    let gw_listener = tokio::net::TcpListener::bind(gateway_listen)
        .await
        .with_context(|| format!("listening on {gateway_listen}"))?;
    let (addr, gw_addr) = (listener.local_addr()?, gw_listener.local_addr()?);
    println!("listening on http://{addr}");
    println!("gateway on http://{gw_addr}");
    tracing::info!(%addr, gateway = %gw_addr, "standalone node listening");
    let stop = tokio_util::sync::CancellationToken::new();
    tokio::spawn({
        let stop = stop.clone();
        async move {
            shutdown_signal().await;
            stop.cancel();
        }
    });
    apps.spawn_usage(usage_interval, stop.clone());
    tokio::try_join!(
        api::serve(listener, apps.clone(), stop.clone().cancelled_owned()),
        gateway::serve(gw_listener, apps, gateway::Access::Local, stop.cancelled_owned()),
    )?;
    tracing::info!("stopped; apps keep running");
    Ok(())
}

/// Refuses an address the ingress can't be told to reach: `0.0.0.0` or `::`.
pub fn check_ingress_listen(addr: &SocketAddr) -> anyhow::Result<()> {
    if addr.ip().is_unspecified() {
        bail!(
            "the gateway's ingress address must be the node's own address on the private network (such as \
             10.0.0.5:7382), since the ingress is told to reach it there; not {addr}"
        );
    }
    Ok(())
}

/// Runs a node joined to a server: its apps, the agent and its gateway, until interrupted.
///
/// Prints `gateway on http://ADDR`, then `ingress gateway on http://ADDR` if it listens for the ingress
/// (`ingress_listen`, on the node's private address), then `node NAME polling URL` once it has joined (on its
/// first start, with `agent.join_token`). Apps keep running after the node exits, and the next node agent adopts
/// them, unless the server forgot the node: then it takes them down, sets its data aside and fails.
pub async fn run_agent(
    cfg: NodeConfig,
    mut agent: agent::AgentConfig,
    gateway_listen: SocketAddr,
    ingress_listen: Option<SocketAddr>,
) -> anyhow::Result<()> {
    check_loopback(&gateway_listen)?;
    if let Some(a) = &ingress_listen {
        check_ingress_listen(a)?;
    }
    // Docker runs with the node's own configuration, which holds its registry credential.
    let mut cfg = cfg;
    cfg.docker_config = Some(cfg.state_dir.join("docker"));
    // A node the server forgot finishes taking its apps down before it opens (and adopts) them.
    cfg.validate()?;
    agent::prepare(&cfg, &runtime::Docker::for_node(&cfg), agent.join_token.is_some()).await?;
    let usage_interval = cfg.usage_interval;
    let apps = apps::Apps::open(cfg).await?;
    let gw_listener = tokio::net::TcpListener::bind(gateway_listen)
        .await
        .with_context(|| format!("listening on {gateway_listen}"))?;
    let gw_addr = gw_listener.local_addr()?;
    println!("gateway on http://{gw_addr}");
    agent.gateway = gw_addr.to_string();
    let ingress_listener = match ingress_listen {
        Some(a) => {
            let l = tokio::net::TcpListener::bind(a)
                .await
                .with_context(|| format!("listening on {a}"))?;
            let addr = l.local_addr()?;
            println!("ingress gateway on http://{addr}");
            agent.ingress = addr.to_string();
            Some(l)
        }
        None => None,
    };
    let server = agent.server.clone();
    let agent = agent::Agent::start(apps.clone(), agent).await?;
    println!("node {} polling {server}", agent.name());
    tracing::info!(node = agent.name(), server, gateway = %gw_addr, "node agent running");
    let ingress = match ingress_listener {
        Some(l) => {
            let token = agent.ingress_token().context(
                "the server gave this node no ingress token, so its gateway can't take requests from the ingress",
            )?;
            Some((l, gateway::Access::Ingress(token.into())))
        }
        None => {
            tracing::warn!(
                "the gateway isn't listening for the ingress (--ingress-listen), so the ingress can't reach this \
                 node's apps"
            );
            None
        }
    };
    let stop = tokio_util::sync::CancellationToken::new();
    tokio::spawn({
        let stop = stop.clone();
        async move {
            shutdown_signal().await;
            stop.cancel();
        }
    });
    apps.spawn_usage(usage_interval, stop.clone());
    let polling = async {
        let r = agent.run(stop.clone()).await;
        // Forgotten: the gateways stop too, and the node exits with why.
        stop.cancel();
        r
    };
    let gateway = gateway::serve(
        gw_listener,
        apps.clone(),
        gateway::Access::Local,
        stop.clone().cancelled_owned(),
    );
    let for_ingress = async {
        match ingress {
            Some((l, access)) => gateway::serve(l, apps, access, stop.clone().cancelled_owned()).await,
            None => Ok(()),
        }
    };
    let (polled, served, served_ingress) = tokio::join!(polling, gateway, for_ingress);
    polled?;
    served?;
    served_ingress?;
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

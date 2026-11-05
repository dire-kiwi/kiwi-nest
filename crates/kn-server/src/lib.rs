//! The kiwi-nest server.
//!
//! - [`store`] is the SQLite database, on a thread of its own: tokens, nodes, apps, shared volumes, the desired
//!   state of each node, and commands.
//! - [`bundles`] keeps bundles as files named by digest, and checks uploads.
//! - [`auth`] makes and checks tokens.
//! - [`scheduler`] places new nests on nodes.
//! - [`hub`] is what's only in memory: which nodes are online, waking polls, and callers waiting for results.
//! - [`api`] is the API for the CLI, and [`agent`] the link for nodes.
//! - [`backups`] is the backup targets, the schedule, and backups (and restores) of the server's own state.
//! - [`ingress`] configures the cluster's Caddy.
//! - [`registry`] runs the cluster's image registry, checks its credentials and collects its garbage.
//!
//! On disk:
//!
//! ```text
//! STATE/server.sqlite     the database (WAL mode)
//! STATE/bundles/          bundles by digest; see [`bundles`]
//! STATE/ingress-ca.crt    the ingress's internal CA's root certificate, with `--ingress internal`
//! STATE/bundles/tmp/      uploads being checked, and the server's state being copied for a backup
//! STATE/registry/         the image registry's configuration and storage; see [`registry::container`]
//! ```

pub mod agent;
pub mod api;
pub mod auth;
pub mod backups;
pub mod bundles;
pub mod error;
pub mod hub;
pub mod ingress;
pub mod registry;
pub mod scheduler;
pub mod store;

use std::future::Future;
use std::net::SocketAddr;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use kn_bundle::Limits;
use kn_proto::agent::Action;
use kn_proto::api::TokenKind;
use tokio_util::sync::CancellationToken;

use crate::bundles::Bundles;
use crate::error::{Error, Result};
use crate::hub::{Hub, Outcome};
use crate::store::Store;

/// The largest JSON body accepted, such as a node's report.
pub const MAX_JSON: usize = 8 * 1024 * 1024;

/// How a server is set up.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Where the server keeps its database and bundles.
    pub state_dir: PathBuf,
    /// The longest a node's poll is held open.
    pub poll_timeout: Duration,
    /// A node that hasn't polled for this long is offline.
    pub offline_after: Duration,
    /// How long a command (logs, suspend, resume, …) may take before its caller gives up.
    pub command_timeout: Duration,
    /// The limits on bundles.
    pub limits: Limits,
    /// The limits on what `volume load` uploads.
    pub load_limits: Limits,
    /// The ingress Caddy to configure, if any.
    pub ingress: Option<ingress::IngressConfig>,
    /// How placement counts memory.
    pub scheduler: scheduler::Settings,
    /// The image registry to run, if any. It needs the ingress.
    pub registry: Option<registry::RegistryConfig>,
    /// Where backups go: handed to nodes, and used for the server's own.
    pub backup_targets: Vec<kn_proto::backup::TargetConfig>,
    /// How often the schedule looks for backups that are due.
    pub backup_check_interval: Duration,
    /// How long a backup on a node may take before its caller gives up (the node carries on).
    pub backup_timeout: Duration,
}

impl ServerConfig {
    pub fn new(state_dir: PathBuf) -> Self {
        ServerConfig {
            state_dir,
            poll_timeout: Duration::from_secs(25),
            offline_after: Duration::from_secs(15),
            command_timeout: Duration::from_secs(600),
            limits: Limits::default(),
            load_limits: Limits::for_volumes(),
            ingress: None,
            scheduler: scheduler::Settings::default(),
            registry: None,
            backup_targets: Vec::new(),
            backup_check_interval: Duration::from_secs(60),
            backup_timeout: Duration::from_secs(6 * 3600),
        }
    }
}

/// The server's state, shared by every request.
#[derive(Clone)]
pub struct Server(Arc<Inner>);

pub struct Inner {
    pub cfg: ServerConfig,
    pub store: Store,
    pub bundles: Arc<Bundles>,
    pub hub: Arc<Hub>,
    pub backups: Arc<backups::Backups>,
    /// Cancelled when the server shuts down, so open polls answer at once.
    pub stop: CancellationToken,
    pub ingress: Option<Arc<ingress::Ingress>>,
    pub registry: Option<Arc<registry::Registry>>,
}

impl Deref for Server {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl Server {
    /// Opens the server's state, creating it if needed. On the first start (no admin token yet) it makes an admin
    /// token and returns it: the only time it's shown.
    pub async fn open(cfg: ServerConfig) -> anyhow::Result<(Server, Option<String>)> {
        Server::open_with(cfg, None).await
    }

    /// Opens the server, with the registry's container run by `container` if given (a fake, in tests).
    pub async fn open_with(
        mut cfg: ServerConfig,
        container: Option<Arc<dyn registry::container::Container>>,
    ) -> anyhow::Result<(Server, Option<String>)> {
        std::fs::create_dir_all(&cfg.state_dir).with_context(|| format!("creating {}", cfg.state_dir.display()))?;
        let backups = Arc::new(backups::Backups::new(cfg.backup_targets.clone()).context("the backup targets")?);
        let registry = match (&cfg.registry, &mut cfg.ingress) {
            (None, _) => None,
            (Some(_), None) => anyhow::bail!("the registry is served by the ingress, so it needs one"),
            (Some(r), Some(i)) => {
                if i.api_domain.as_deref() == Some(r.domain.as_str()) {
                    anyhow::bail!("the registry and the server's API need different domains");
                }
                i.registry = Some(ingress::RegistryRoute {
                    domain: r.domain.clone(),
                    upstream: r.listen,
                });
                Some(Arc::new(match container {
                    Some(c) => registry::Registry::with_container(r.clone(), i, c)?,
                    None => registry::Registry::new(r.clone(), i, &cfg.state_dir)?,
                }))
            }
        };
        let ingress = cfg
            .ingress
            .clone()
            .map(|i| ingress::Ingress::new(i, &cfg.state_dir).map(Arc::new))
            .transpose()?;
        let store = Store::open(&cfg.state_dir.join("server.sqlite"))?;
        let bundles = Arc::new(Bundles::open(cfg.state_dir.join("bundles"))?);
        let hub = Arc::new(Hub::new(cfg.offline_after));
        let (b, limits) = (bundles.clone(), cfg.limits.clone());
        let first = store
            .call(move |c| {
                let now = store::now();
                let lost = store::commands::lose_open(c, now)?;
                if lost > 0 {
                    tracing::info!(lost, "commands open when the server stopped won't be answered");
                }
                store::apps::sweep(c, &b)?;
                store::apps::backfill(c, &b, &limits)?;
                if store::tokens::admin_count(c)? > 0 {
                    return Ok(None);
                }
                let token = auth::generate(TokenKind::Admin);
                store::tokens::create(c, TokenKind::Admin, &auth::hash(&token), "first", &[], &[], None, now)?;
                Ok(Some(token))
            })
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        let server = Server(Arc::new(Inner {
            cfg,
            store,
            bundles,
            hub,
            backups,
            stop: CancellationToken::new(),
            ingress,
            registry,
        }));
        Ok((server, first))
    }

    /// Every route: the CLI's API and the agent link.
    pub fn router(&self) -> axum::Router {
        api::router()
            .merge(agent::router())
            .merge(registry::router())
            // JSON bodies (reports, tokens, joins) are small; uploads are streamed under their own limits.
            .layer(axum::extract::DefaultBodyLimit::max(MAX_JSON))
            .with_state(self.clone())
    }

    /// Has the ingress's configuration checked, after something it's made from may have changed.
    pub fn ingress_changed(&self) {
        if let Some(i) = &self.ingress {
            i.changed();
        }
    }

    /// What a domain is kept for, if apps can't use it: the server's own API, or the registry.
    pub fn reserved_domain(&self, domain: &str) -> Option<&'static str> {
        let i = self.ingress.as_ref()?;
        if i.config().api_domain.as_deref() == Some(domain) {
            return Some("the server's API domain");
        }
        let r = i.config().registry.as_ref()?;
        (r.domain == domain).then_some("the image registry's domain")
    }

    /// Whether a node is online, for the store's transactions.
    pub fn online(&self) -> impl Fn(u64) -> bool + Send + 'static {
        let hub = self.hub.clone();
        move |id| hub.is_online(id)
    }

    /// Runs a command on a node and waits for its outcome. Refused at once if the node is offline.
    pub async fn command(&self, node_id: u64, node: &str, action: Action) -> Result<Outcome> {
        self.command_with(node_id, node, action, self.cfg.command_timeout).await
    }

    /// [`Server::command`], waiting up to `timeout`.
    pub async fn command_with(&self, node_id: u64, node: &str, action: Action, timeout: Duration) -> Result<Outcome> {
        if !self.hub.is_online(node_id) {
            return Err(Error::Offline(format!("node `{node}` is offline")));
        }
        let hub = self.hub.clone();
        let (id, rx) = self
            .store
            .call(move |c| {
                let id = store::commands::insert(c, node_id, &action, store::now())?;
                Ok((id, hub.wait_for(id)))
            })
            .await?;
        let guard = CommandGuard {
            server: self.clone(),
            id,
        };
        self.hub.wake(node_id);
        // Give up as soon as the node goes offline, rather than waiting out the timeout for an answer that won't
        // come. (One it's running may still finish; its result is then recorded and dropped.)
        let offline = async {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if !self.hub.is_online(node_id) {
                    return;
                }
            }
        };
        let outcome = tokio::select! {
            r = tokio::time::timeout(timeout, rx) => r,
            _ = offline => {
                return Err(Error::Offline(format!("node `{node}` went offline before answering")));
            }
        };
        drop(guard);
        match outcome {
            Ok(Ok(Outcome::Lost)) => Err(Error::Offline(format!(
                "node `{node}` didn't get the command, or restarted while running it; try again"
            ))),
            Ok(Ok(o)) => Ok(o),
            Ok(Err(_)) => Err(Error::Internal(anyhow::anyhow!("the command was dropped"))),
            Err(_) => Err(Error::Offline(format!("node `{node}` didn't answer in time"))),
        }
    }
}

/// Cancels a command whose caller stopped waiting, if it hasn't been handed out yet.
struct CommandGuard {
    server: Server,
    id: u64,
}

impl Drop for CommandGuard {
    fn drop(&mut self) {
        self.server.hub.forget_waiter(self.id);
        let (store, id) = (self.server.store.clone(), self.id);
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = store.call(move |c| store::commands::cancel(c, id, store::now())).await;
            });
        }
    }
}

/// Serves the server's routes until `shutdown` completes; open polls answer at once, and anything still streaming
/// (such as `logs --follow`) gets a few seconds.
pub async fn serve(
    listener: tokio::net::TcpListener,
    server: Server,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let stop = server.stop.clone();
    tokio::spawn({
        let stop = stop.clone();
        async move {
            shutdown.await;
            stop.cancel();
        }
    });
    tokio::spawn(backups::schedule(server.clone(), stop.clone()));
    if let Some(ingress) = server.ingress.clone() {
        let api = ingress::reachable(listener.local_addr()?);
        let (server, stop) = (server.clone(), stop.clone());
        tokio::spawn(async move { ingress.run(server, api, stop).await });
    }
    if let Some(registry) = server.registry.clone() {
        let (server, stop) = (server.clone(), stop.clone());
        tokio::spawn(async move { registry.run(server, stop).await });
    }
    let router = server.router();
    let graceful = axum::serve(listener, router).with_graceful_shutdown(stop.clone().cancelled_owned());
    tokio::select! {
        r = graceful => r,
        _ = async { stop.cancelled().await; tokio::time::sleep(Duration::from_secs(5)).await } => Ok(()),
    }
}

/// Runs a server until interrupted. Prints `admin token: TOKEN` on the first start, then
/// `listening on http://ADDR` once it's listening.
pub async fn run(cfg: ServerConfig, listen: SocketAddr) -> anyhow::Result<()> {
    let (server, first) = Server::open(cfg).await?;
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("listening on {listen}"))?;
    let addr = listener.local_addr()?;
    if let Some(token) = first {
        println!("admin token: {token}");
        eprintln!("kiwi-nest: this is the first admin token, shown only now; keep it safe");
    }
    println!("listening on http://{addr}");
    tracing::info!(%addr, "server listening");
    serve(listener, server, shutdown_signal()).await?;
    tracing::info!("stopped");
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
mod tests;

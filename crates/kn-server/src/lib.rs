//! The kiwi-nest server.
//!
//! - [`store`] is the SQLite database, on a thread of its own: tokens, nodes, apps, shared volumes, the desired
//!   state of each node, and commands.
//! - [`bundles`] keeps bundles as files named by digest, and checks uploads.
//! - [`auth`] makes and checks tokens.
//! - [`scheduler`] picks a node for a new app or volume.
//! - [`hub`] is what's only in memory: which nodes are online, waking polls, and callers waiting for results.
//! - [`api`] is the API for the CLI, and [`agent`] the link for nodes.
//!
//! On disk:
//!
//! ```text
//! STATE/server.sqlite   the database (WAL mode)
//! STATE/bundles/        bundles by digest; see [`bundles`]
//! ```

pub mod agent;
pub mod api;
pub mod auth;
pub mod bundles;
pub mod error;
pub mod hub;
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
    /// Cancelled when the server shuts down, so open polls answer at once.
    pub stop: CancellationToken,
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
        std::fs::create_dir_all(&cfg.state_dir)
            .with_context(|| format!("creating {}", cfg.state_dir.display()))?;
        let store = Store::open(&cfg.state_dir.join("server.sqlite"))?;
        let bundles = Arc::new(Bundles::open(cfg.state_dir.join("bundles"))?);
        let hub = Arc::new(Hub::new(cfg.offline_after));
        let b = bundles.clone();
        let first = store
            .call(move |c| {
                let now = store::now();
                let lost = store::commands::lose_open(c, now)?;
                if lost > 0 {
                    tracing::info!(lost, "commands open when the server stopped won't be answered");
                }
                store::apps::sweep(c, &b)?;
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
            stop: CancellationToken::new(),
        }));
        Ok((server, first))
    }

    /// Every route: the CLI's API and the agent link.
    pub fn router(&self) -> axum::Router {
        api::router()
            .merge(agent::router())
            .layer(axum::extract::DefaultBodyLimit::disable())
            .with_state(self.clone())
    }

    /// Whether a node is online, for the store's transactions.
    pub fn online(&self) -> impl Fn(u64) -> bool + Send + 'static {
        let hub = self.hub.clone();
        move |id| hub.is_online(id)
    }

    /// Runs a command on a node and waits for its outcome. Refused at once if the node is offline.
    pub async fn command(&self, node_id: u64, node: &str, action: Action) -> Result<Outcome> {
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
        let outcome = tokio::time::timeout(self.cfg.command_timeout, rx).await;
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

//! The kiwi-nest CLI's commands.
//!
//! Commands go to a cluster's server, or to a standalone node's loopback API (`--node-api`). The target is, in
//! order: `--node-api`; `--server` with `--token` (or `KIWI_NEST_SERVER` and `KIWI_NEST_TOKEN`, for CI); `--cluster`;
//! the current cluster in `~/.config/kiwi-nest` ([`config`]); and otherwise the standalone node on this machine.
//!
//! Through a server, changes (deploy, delete, `volume create|load|delete`) are saved first and then waited for:
//! the CLI follows the change until the node reports it done, and fails if it failed. A change for an offline
//! node is refused, saving nothing, unless `--queue` is given.
//!
//! `volume …` manages shared volumes; `deploy --from` and `delete --keep-data` decide what happens to an app's.

pub mod config;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use clap::{Args, Subcommand, ValueEnum};
use futures_util::StreamExt;
use kn_proto::api::{
    Accepted, AppInfo, AppSummary, ChangeStatus, CreateToken, CreatedToken, DeleteQuery, DeployDirectory, DeployQuery,
    ErrorBody, LogsQuery, NodeInfo, TokenInfo, TokenKind, VolumeInfo, VolumeQuery, WaitQuery, WhoAmI,
};

/// The standalone node API's default address.
pub const DEFAULT_NODE_API: &str = "http://127.0.0.1:7380";

/// Options for every client command.
#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// A standalone node's API to talk to, rather than a cluster.
    #[arg(long, global = true, env = "KIWI_NEST_NODE_API", value_name = "URL")]
    pub node_api: Option<String>,
    /// The cluster to use, by the name given to `cluster add` [default: the current cluster].
    #[arg(long, global = true, env = "KIWI_NEST_CLUSTER", value_name = "NAME")]
    pub cluster: Option<String>,
    /// A server's URL, used with --token instead of a configured cluster.
    #[arg(long, global = true, env = "KIWI_NEST_SERVER", value_name = "URL")]
    pub server: Option<String>,
    /// The token for --server, or for `cluster add`.
    #[arg(
        long,
        global = true,
        env = "KIWI_NEST_TOKEN",
        hide_env_values = true,
        value_name = "TOKEN"
    )]
    pub token: Option<String>,
    /// Where the CLI keeps its clusters [default: ~/.config/kiwi-nest].
    #[arg(long, global = true, env = "KIWI_NEST_CONFIG_DIR", value_name = "DIR")]
    pub config_dir: Option<PathBuf>,
    /// A CA certificate (PEM) to trust for the server, as well as the system's: the ingress's internal CA (the
    /// server's STATE/ingress-ca.crt), say. `cluster add` keeps it with the cluster; a node uses it to reach its
    /// --server.
    #[arg(long, global = true, env = "KIWI_NEST_CA_CERT", value_name = "FILE")]
    pub ca_cert: Option<PathBuf>,
}

impl GlobalArgs {
    /// The contents of --ca-cert, checked to hold at least one certificate.
    pub fn read_ca_cert(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let Some(path) = &self.ca_cert else { return Ok(None) };
        let pem = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        check_ca_cert(&pem).with_context(|| format!("reading {}", path.display()))?;
        Ok(Some(pem))
    }
}

/// Checks that PEM holds at least one certificate.
fn check_ca_cert(pem: &[u8]) -> anyhow::Result<()> {
    let certs = reqwest::Certificate::from_pem_bundle(pem)?;
    if certs.is_empty() {
        bail!("there's no PEM certificate in it");
    }
    Ok(())
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Deploy a bundle (a directory or a ZIP) as NAME, replacing the app if it exists.
    ///
    /// Through a server, it waits until the node reports the new generation. It's refused, saving nothing, if the
    /// app's node is offline, unless --queue is given.
    Deploy {
        name: String,
        /// The bundle: a directory with compose.yaml at its root, or a ZIP of one.
        path: PathBuf,
        /// Refuse if the app already exists.
        #[arg(long)]
        create_only: bool,
        /// On the app's first deploy, start each named volume it shares with APP as an instant snapshot of APP's.
        #[arg(long, value_name = "APP")]
        from: Option<String>,
        /// The node for a new app; by default the server picks the one with the most free memory.
        #[arg(long, value_name = "NODE")]
        node: Option<String>,
        /// Save the deploy even if its node is offline; it's applied when the node is back. Doesn't wait.
        #[arg(long)]
        queue: bool,
        /// How long to wait for the node, such as 90s or 15m.
        #[arg(long, default_value = "15m")]
        timeout: String,
        /// Have the node read the directory itself, rather than uploading it (a standalone node on this machine).
        #[arg(long)]
        node_reads_path: bool,
    },
    /// List the apps.
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show an app's details and containers.
    Inspect {
        name: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show an app's logs.
    Logs {
        name: String,
        /// Lines to show from the end of each container's log.
        #[arg(long, short = 'n')]
        tail: Option<u32>,
        /// Keep streaming new lines.
        #[arg(long, short)]
        follow: bool,
        /// Prefix each line with its timestamp.
        #[arg(long, short)]
        timestamps: bool,
        /// Only this service.
        #[arg(long, short)]
        service: Option<String>,
    },
    /// Stop an app until it's resumed; requests don't start it meanwhile. Refused while it has open requests.
    Suspend { name: String },
    /// Start an app (suspended, idle or failed) and wait until it's ready.
    Resume { name: String },
    /// Delete an app: its containers, its bundle and its volumes.
    Delete {
        name: String,
        /// Keep the app's volumes (and its snapshots of shared volumes), so a later deploy of NAME gets them back.
        #[arg(long)]
        keep_data: bool,
        /// Save the delete even if the app's node is offline. Doesn't wait.
        #[arg(long)]
        queue: bool,
    },
    /// Shared volumes, which apps mount through `x-kiwi-nest.shared`.
    #[command(subcommand)]
    Volume(VolumeCommand),
    /// Tokens for the CLI, CI and joining nodes (admin only).
    #[command(subcommand)]
    Token(TokenCommand),
    /// The clusters this CLI knows, in ~/.config/kiwi-nest.
    #[command(subcommand)]
    Cluster(ClusterCommand),
}

#[derive(Debug, Clone, Subcommand)]
pub enum VolumeCommand {
    /// Create an empty shared volume.
    Create {
        name: String,
        /// Through a server: the node for it; by default the server picks one.
        #[arg(long, value_name = "NODE")]
        node: Option<String>,
        /// Save it even if its node is offline. Doesn't wait.
        #[arg(long)]
        queue: bool,
    },
    /// List the shared volumes.
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show a shared volume: the apps that mount it, and its snapshots.
    Inspect {
        name: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Replace a shared volume's contents with a directory (or a ZIP of one) of plain files.
    ///
    /// Refused while an app mounts it `ro` or `rw`. Apps' snapshots keep what they had.
    Load {
        name: String,
        path: PathBuf,
        /// Have the node read the directory itself, rather than uploading it (a standalone node on this machine).
        #[arg(long)]
        node_reads_path: bool,
        /// Save it even if the volume's node is offline. Doesn't wait.
        #[arg(long)]
        queue: bool,
    },
    /// Delete a shared volume. Refused while an app mounts it or any snapshot of it exists.
    Delete {
        name: String,
        /// Save it even if the volume's node is offline. Doesn't wait.
        #[arg(long)]
        queue: bool,
    },
    /// Delete APP's snapshot of a shared volume. Refused while APP is deployed and mounts it.
    DeleteSnapshot { name: String, app: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum NewTokenKind {
    /// May do anything.
    Admin,
    /// May deploy and manage the apps whose names match its --pattern.
    Deploy,
    /// Lets one node join, once.
    Join,
}

#[derive(Debug, Clone, Subcommand)]
pub enum TokenCommand {
    /// Make a token, and print it (the only time it's shown).
    Create {
        kind: NewTokenKind,
        /// What it's for.
        #[arg(long, default_value = "")]
        name: String,
        /// For a deploy token: the app names it may manage, where `*` matches anything, such as `blog-pr*`
        /// (repeatable).
        #[arg(long = "pattern", value_name = "PATTERN")]
        patterns: Vec<String>,
        /// For a deploy token: apps it may name as `--from` besides its own, such as `blog` (repeatable).
        #[arg(long = "from", value_name = "PATTERN")]
        from: Vec<String>,
    },
    /// List the tokens (not the tokens themselves, which the server doesn't keep).
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Revoke a token by its ID.
    Revoke { id: u64 },
}

#[derive(Debug, Clone, Subcommand)]
pub enum ClusterCommand {
    /// Add a cluster: its server's URL, and a token (--token, or KIWI_NEST_TOKEN). The first one added is current.
    Add {
        name: String,
        url: String,
        /// Make it the current cluster.
        #[arg(long = "use")]
        make_current: bool,
    },
    /// Make a cluster the current one.
    Use { name: String },
    /// List the clusters.
    List,
    /// Forget a cluster.
    Remove { name: String },
}

/// Where commands go.
#[derive(Debug, Clone)]
pub enum Target {
    Node(String),
    Server {
        url: String,
        token: String,
        /// A CA certificate (PEM) to trust as well as the system's.
        ca: Option<Vec<u8>>,
    },
}

impl Target {
    /// Works out the target from the flags, the environment and the configured clusters.
    pub fn resolve(global: &GlobalArgs) -> anyhow::Result<Target> {
        if let Some(url) = &global.node_api {
            return Ok(Target::Node(url.clone()));
        }
        if let Some(url) = &global.server {
            let token = global
                .token
                .clone()
                .context("--server needs --token (or KIWI_NEST_TOKEN)")?;
            return Ok(Target::Server {
                url: url.clone(),
                token,
                ca: global.read_ca_cert()?,
            });
        }
        let dir = config::dir(global.config_dir.as_deref())?;
        let cfg = config::Config::load(&dir)?;
        let name = match (&global.cluster, &cfg.current) {
            (Some(n), _) => n.clone(),
            (None, Some(n)) => n.clone(),
            (None, None) if cfg.clusters.is_empty() => return Ok(Target::Node(DEFAULT_NODE_API.into())),
            (None, None) => bail!("no cluster is current: pick one with `kiwi-nest cluster use NAME`"),
        };
        let c = cfg.clusters.get(&name).with_context(|| {
            format!(
                "there's no cluster `{name}` in {}",
                config::Config::path(&dir).display()
            )
        })?;
        let ca = match global.read_ca_cert()? {
            Some(pem) => Some(pem),
            None => c.ca_cert.clone().map(String::into_bytes),
        };
        Ok(Target::Server {
            url: c.url.clone(),
            token: global.token.clone().unwrap_or_else(|| c.token.clone()),
            ca,
        })
    }
}

/// A client for a node's API or a server's.
pub struct Client {
    http: reqwest::Client,
    base: String,
    token: Option<String>,
}

/// Installs ring as the process's TLS provider, once.
pub fn install_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

impl Client {
    pub fn new(target: &Target) -> anyhow::Result<Self> {
        let (base, token, ca) = match target {
            Target::Node(url) => (url.as_str(), None, None),
            Target::Server { url, token, ca } => (url.as_str(), Some(token.clone()), ca.as_deref()),
        };
        let base = base.trim_end_matches('/').to_string();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            bail!("the API must be an http:// or https:// URL, not {base}");
        }
        install_tls();
        let mut http = reqwest::Client::builder()
            .user_agent(format!("kiwi-nest/{}", kn_proto::VERSION))
            .connect_timeout(Duration::from_secs(10));
        if let Some(pem) = ca {
            http = http.tls_certs_merge(reqwest::Certificate::from_pem_bundle(pem)?);
        }
        let http = http.build()?;
        Ok(Client { http, base, token })
    }

    /// Whether this talks to a server (rather than a standalone node).
    pub fn is_server(&self) -> bool {
        self.token.is_some()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}{path}", self.base, kn_proto::API_PREFIX)
    }

    fn req(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let r = self.http.request(method, self.url(path));
        match &self.token {
            Some(t) => r.bearer_auth(t),
            None => r,
        }
    }

    fn get(&self, path: &str) -> reqwest::RequestBuilder {
        self.req(reqwest::Method::GET, path)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let what = if self.is_server() { "the server" } else { "the node API" };
        let resp = req
            .send()
            .await
            .with_context(|| format!("couldn't reach {what} at {}", self.base))?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let message = match serde_json::from_str::<ErrorBody>(&text) {
            Ok(body) => message(&body.error, &body.problems),
            Err(_) => format!("{what} answered {status}: {}", text.trim()),
        };
        Err(StatusError {
            status: status.as_u16(),
            message,
        }
        .into())
    }

    async fn json<T: serde::de::DeserializeOwned>(&self, req: reqwest::RequestBuilder) -> anyhow::Result<T> {
        Ok(self.send(req).await?.json().await?)
    }

    pub async fn deploy_zip(&self, name: &str, zip: Vec<u8>, q: &DeployQuery) -> anyhow::Result<reqwest::Response> {
        let req = self
            .req(reqwest::Method::PUT, &format!("/apps/{name}"))
            .query(q)
            .header(reqwest::header::CONTENT_TYPE, "application/zip")
            .body(zip);
        self.send(req).await
    }

    pub async fn deploy_directory(&self, name: &str, dir: &Path, q: &DeployQuery) -> anyhow::Result<AppInfo> {
        let req = self
            .req(reqwest::Method::PUT, &format!("/apps/{name}"))
            .query(q)
            .json(&directory(dir)?);
        self.json(req).await
    }

    /// Whether a change on the server is done, waiting up to `wait` seconds.
    pub async fn change(&self, kind: &str, name: &str, generation: u64, wait: u64) -> anyhow::Result<ChangeStatus> {
        self.json(
            self.get(&format!("/{kind}/{name}/changes/{generation}"))
                .query(&WaitQuery { wait })
                .timeout(Duration::from_secs(wait + 30)),
        )
        .await
    }

    pub async fn volumes(&self) -> anyhow::Result<Vec<VolumeInfo>> {
        self.json(self.get("/volumes")).await
    }

    pub async fn volume(&self, name: &str) -> anyhow::Result<VolumeInfo> {
        self.json(self.get(&format!("/volumes/{name}"))).await
    }

    pub async fn volume_create(&self, name: &str, q: &VolumeQuery) -> anyhow::Result<reqwest::Response> {
        self.send(self.req(reqwest::Method::PUT, &format!("/volumes/{name}")).query(q))
            .await
    }

    pub async fn volume_load_zip(
        &self,
        name: &str,
        zip: Vec<u8>,
        q: &VolumeQuery,
    ) -> anyhow::Result<reqwest::Response> {
        let req = self
            .req(reqwest::Method::PUT, &format!("/volumes/{name}/data"))
            .query(q)
            .header(reqwest::header::CONTENT_TYPE, "application/zip")
            .body(zip);
        self.send(req).await
    }

    pub async fn volume_load_directory(&self, name: &str, dir: &Path) -> anyhow::Result<VolumeInfo> {
        let req = self
            .req(reqwest::Method::PUT, &format!("/volumes/{name}/data"))
            .json(&directory(dir)?);
        self.json(req).await
    }

    pub async fn volume_delete(&self, name: &str, q: &VolumeQuery) -> anyhow::Result<reqwest::Response> {
        self.send(self.req(reqwest::Method::DELETE, &format!("/volumes/{name}")).query(q))
            .await
    }

    pub async fn volume_delete_snapshot(&self, name: &str, app: &str) -> anyhow::Result<()> {
        self.send(self.req(reqwest::Method::DELETE, &format!("/volumes/{name}/snapshots/{app}")))
            .await?;
        Ok(())
    }

    pub async fn list(&self) -> anyhow::Result<Vec<AppSummary>> {
        self.json(self.get("/apps")).await
    }

    pub async fn inspect(&self, name: &str) -> anyhow::Result<AppInfo> {
        self.json(self.get(&format!("/apps/{name}"))).await
    }

    pub async fn logs(&self, name: &str, q: &LogsQuery) -> anyhow::Result<reqwest::Response> {
        self.send(self.get(&format!("/apps/{name}/logs")).query(q)).await
    }

    pub async fn suspend(&self, name: &str) -> anyhow::Result<AppInfo> {
        self.json(self.req(reqwest::Method::POST, &format!("/apps/{name}/suspend")))
            .await
    }

    pub async fn resume(&self, name: &str) -> anyhow::Result<AppInfo> {
        self.json(self.req(reqwest::Method::POST, &format!("/apps/{name}/resume")))
            .await
    }

    pub async fn delete(&self, name: &str, q: &DeleteQuery) -> anyhow::Result<reqwest::Response> {
        self.send(self.req(reqwest::Method::DELETE, &format!("/apps/{name}")).query(q))
            .await
    }

    pub async fn nodes(&self) -> anyhow::Result<Vec<NodeInfo>> {
        self.json(self.get("/nodes")).await
    }

    pub async fn whoami(&self) -> anyhow::Result<WhoAmI> {
        self.json(self.get("/whoami")).await
    }
}

/// An error answer from the API.
#[derive(Debug)]
pub struct StatusError {
    pub status: u16,
    pub message: String,
}

impl std::fmt::Display for StatusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StatusError {}

/// Whether an error looks like the server (or a proxy in front of it) being briefly away, such as while it
/// restarts: no connection, a broken or timed-out one, or a gateway error.
fn transient(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<reqwest::Error>().is_some()
            || c.downcast_ref::<StatusError>()
                .is_some_and(|s| matches!(s.status, 502..=504))
    })
}

fn message(error: &str, problems: &[String]) -> String {
    let mut msg = error.to_string();
    for p in problems {
        msg.push_str("\n  - ");
        msg.push_str(p);
    }
    msg
}

fn directory(dir: &Path) -> anyhow::Result<DeployDirectory> {
    Ok(DeployDirectory {
        directory: dir.to_str().context("the path isn't UTF-8")?.to_string(),
    })
}

fn duration(flag: &str, value: &str) -> anyhow::Result<Duration> {
    let secs = kn_bundle::manifest::parse_duration(value).with_context(|| format!("bad {flag} {value}"))?;
    Ok(Duration::from_secs(secs))
}

fn server_only(client: &Client, what: &str) -> anyhow::Result<()> {
    if !client.is_server() {
        bail!("{what} needs a cluster: add one with `kiwi-nest cluster add`, or pass --server and --token");
    }
    Ok(())
}

/// Follows a saved change until the node reports it done; fails if it failed.
async fn wait_for(client: &Client, kind: &str, acc: &Accepted, timeout: Duration) -> anyhow::Result<ChangeStatus> {
    let deadline = Instant::now() + timeout;
    let mut said_offline = false;
    let mut unreachable_since: Option<Instant> = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            bail!(
                "gave up waiting for node `{}` to apply generation {} of `{}`; it's saved, and `kiwi-nest inspect \
                 {}` shows when it's done",
                acc.node,
                acc.generation,
                acc.name,
                acc.name
            );
        }
        let st = match client
            .change(kind, &acc.name, acc.generation, left.as_secs().clamp(1, 25))
            .await
        {
            Ok(st) => {
                unreachable_since = None;
                st
            }
            // The server may be restarting: the change is saved, so keep following it for a while.
            Err(e) if transient(&e) => {
                let since = *unreachable_since.get_or_insert_with(Instant::now);
                if since.elapsed() > Duration::from_secs(120) {
                    return Err(e);
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Err(e) => return Err(e),
        };
        if st.done {
            // Another deploy came after this one (a later push, say): the app is that one's now, which isn't a
            // failure of this one.
            if let Some(g) = st.superseded_by {
                eprintln!(
                    "generation {} of `{}` was followed by generation {g} before this could see how it went; the app \
                     is at generation {g} now",
                    acc.generation, acc.name
                );
                return Ok(st);
            }
            if !st.ok {
                bail!(message(
                    st.error.as_deref().unwrap_or("the change failed on the node"),
                    &st.problems
                ));
            }
            return Ok(st);
        }
        if !st.node_online && !said_offline {
            eprintln!("node `{}` is offline; waiting for it to come back", st.node);
            said_offline = true;
        }
    }
}

/// Runs a client command.
pub async fn run(global: &GlobalArgs, cmd: Command) -> anyhow::Result<()> {
    if let Command::Cluster(c) = cmd {
        return cluster(global, c).await;
    }
    let client = Client::new(&Target::resolve(global)?)?;
    match cmd {
        Command::Deploy {
            name,
            path,
            create_only,
            from,
            node,
            queue,
            timeout,
            node_reads_path,
        } => {
            let timeout = duration("--timeout", &timeout)?;
            let q = DeployQuery {
                create_only,
                from,
                node,
                queue,
            };
            if !client.is_server() {
                let info = if node_reads_path {
                    let dir = std::path::absolute(&path)?;
                    client.deploy_directory(&name, &dir, &q).await?
                } else {
                    let zip = read_bundle(path, kn_bundle::Limits::default()).await?;
                    client.deploy_zip(&name, zip, &q).await?.json().await?
                };
                print_deployed(&info, None);
                return Ok(());
            }
            if node_reads_path {
                bail!("--node-reads-path only works with a standalone node; a server takes the bundle as a ZIP");
            }
            let zip = read_bundle(path, kn_bundle::Limits::default()).await?;
            let acc: Accepted = client.deploy_zip(&name, zip, &q).await?.json().await?;
            if !acc.node_online {
                println!(
                    "queued {} (generation {}) for node {}, which is offline; it's deployed when the node is back",
                    acc.name, acc.generation, acc.node
                );
                return Ok(());
            }
            eprintln!(
                "deploying {} (generation {}) on node {}",
                acc.name, acc.generation, acc.node
            );
            let st = wait_for(&client, "apps", &acc, timeout).await?;
            if st.superseded_by.is_some() {
                return Ok(());
            }
            match client.inspect(&name).await {
                Ok(info) => print_deployed(&info, Some(&st.notes)),
                Err(_) => println!(
                    "deployed {} (generation {}) on node {}",
                    acc.name, acc.generation, acc.node
                ),
            }
        }
        Command::List { json } => {
            let apps = client.list().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&apps)?);
            } else if client.is_server() {
                print_table(
                    &["NAME", "STATUS", "CONTAINERS", "GEN", "NODE", "DOMAIN", "UPDATED"],
                    apps.iter()
                        .map(|a| {
                            vec![
                                a.name.clone(),
                                status(a),
                                a.containers.clone(),
                                a.generation.to_string(),
                                a.node.clone().unwrap_or_default(),
                                a.domain.clone(),
                                a.updated_at.clone(),
                            ]
                        })
                        .collect(),
                );
            } else {
                print_table(
                    &["NAME", "STATUS", "CONTAINERS", "GEN", "PORT", "DOMAIN", "UPDATED"],
                    apps.iter()
                        .map(|a| {
                            vec![
                                a.name.clone(),
                                a.status.to_string(),
                                a.containers.clone(),
                                a.generation.to_string(),
                                a.port.to_string(),
                                a.domain.clone(),
                                a.updated_at.clone(),
                            ]
                        })
                        .collect(),
                );
            }
        }
        Command::Inspect { name, json } => {
            let info = client.inspect(&name).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                print_info(&info);
            }
        }
        Command::Logs {
            name,
            tail,
            follow,
            timestamps,
            service,
        } => {
            let resp = client
                .logs(
                    &name,
                    &LogsQuery {
                        tail,
                        follow,
                        timestamps,
                        service,
                    },
                )
                .await?;
            let mut stream = resp.bytes_stream();
            let mut out = std::io::stdout().lock();
            while let Some(chunk) = stream.next().await {
                out.write_all(&chunk?)?;
                out.flush()?;
            }
        }
        Command::Suspend { name } => {
            let info = client.suspend(&name).await?;
            println!("suspended {}", info.name);
        }
        Command::Resume { name } => {
            let info = client.resume(&name).await?;
            println!("resumed {}: {}", info.name, info.status);
        }
        Command::Delete { name, keep_data, queue } => {
            let resp = client.delete(&name, &DeleteQuery { keep_data, queue }).await?;
            if client.is_server() {
                let acc: Accepted = resp.json().await?;
                if !acc.node_online {
                    println!("queued the delete of {name} for node {}, which is offline", acc.node);
                    return Ok(());
                }
                wait_for(&client, "apps", &acc, Duration::from_secs(900)).await?;
            }
            if keep_data {
                println!("deleted {name}, keeping its volumes");
            } else {
                println!("deleted {name}");
            }
        }
        Command::Volume(cmd) => volume(&client, cmd).await?,
        Command::Token(cmd) => token(&client, cmd).await?,
        Command::Cluster(_) => unreachable!(),
    }
    Ok(())
}

/// A server change's status column: the node's, plus any generation it hasn't applied yet.
fn status(a: &AppSummary) -> String {
    match a.pending {
        Some(g) if a.generation == 0 => format!("pending (gen {g})"),
        Some(g) => format!("{} (gen {g} pending)", a.status),
        None => a.status.to_string(),
    }
}

fn print_deployed(info: &AppInfo, notes: Option<&Vec<String>>) {
    match &info.node {
        Some(node) => println!(
            "deployed {} (generation {}) on node {node}: {} ({}:{})",
            info.name, info.generation, info.domain, info.service, info.container_port
        ),
        None => println!(
            "deployed {} (generation {}): {} on http://127.0.0.1:{} ({}:{})",
            info.name, info.generation, info.domain, info.port, info.service, info.container_port
        ),
    }
    for note in notes.unwrap_or(&info.notes) {
        println!("  {note}");
    }
}

/// Follows a server's volume change, unless it was queued for an offline node.
async fn follow_volume(client: &Client, resp: reqwest::Response, what: &str) -> anyhow::Result<()> {
    let acc: Accepted = resp.json().await?;
    if !acc.node_online {
        println!("queued: {what} on node {}, which is offline", acc.node);
        return Ok(());
    }
    wait_for(client, "volumes", &acc, Duration::from_secs(3600)).await?;
    Ok(())
}

async fn volume(client: &Client, cmd: VolumeCommand) -> anyhow::Result<()> {
    match cmd {
        VolumeCommand::Create { name, node, queue } => {
            let resp = client.volume_create(&name, &VolumeQuery { node, queue }).await?;
            if client.is_server() {
                follow_volume(client, resp, &format!("creating shared volume {name}")).await?;
            }
            println!("created shared volume {name}");
        }
        VolumeCommand::List { json } => {
            let volumes = client.volumes().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&volumes)?);
            } else {
                let mut headers = vec!["NAME", "MOUNTED BY", "SNAPSHOTS", "LOADED", "PROBLEM"];
                if client.is_server() {
                    headers.insert(1, "NODE");
                }
                print_table(
                    &headers,
                    volumes
                        .iter()
                        .map(|v| {
                            let mut row = vec![
                                v.name.clone(),
                                mounts(v),
                                v.snapshots.join(","),
                                v.loaded_at.clone().unwrap_or_default(),
                                v.problem.clone().unwrap_or_default(),
                            ];
                            if client.is_server() {
                                row.insert(1, v.node.clone().unwrap_or_default());
                            }
                            row
                        })
                        .collect(),
                );
            }
        }
        VolumeCommand::Inspect { name, json } => {
            let v = client.volume(&name).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("name:        {}", v.name);
                if let Some(n) = &v.node {
                    println!("node:        {n}");
                }
                if let Some(g) = v.pending {
                    println!("pending:     generation {g}, not applied by the node yet");
                }
                println!("created:     {}", v.created_at);
                println!("loaded:      {}", v.loaded_at.as_deref().unwrap_or("never"));
                println!("mounted by:  {}", or_none(mounts(&v)));
                println!("snapshots:   {}", or_none(v.snapshots.join(", ")));
                if let Some(p) = &v.problem {
                    println!("problem:     {p}");
                }
            }
        }
        VolumeCommand::Load {
            name,
            path,
            node_reads_path,
            queue,
        } => {
            if node_reads_path {
                if client.is_server() {
                    bail!("--node-reads-path only works with a standalone node; a server takes a ZIP");
                }
                let dir = std::path::absolute(&path)?;
                client.volume_load_directory(&name, &dir).await?;
            } else {
                let zip = read_bundle(path, kn_bundle::Limits::for_volumes()).await?;
                let resp = client
                    .volume_load_zip(&name, zip, &VolumeQuery { node: None, queue })
                    .await?;
                if client.is_server() {
                    follow_volume(client, resp, &format!("loading shared volume {name}")).await?;
                }
            }
            println!("loaded shared volume {name}");
        }
        VolumeCommand::Delete { name, queue } => {
            let resp = client.volume_delete(&name, &VolumeQuery { node: None, queue }).await?;
            if client.is_server() {
                follow_volume(client, resp, &format!("deleting shared volume {name}")).await?;
            }
            println!("deleted shared volume {name}");
        }
        VolumeCommand::DeleteSnapshot { name, app } => {
            client.volume_delete_snapshot(&name, &app).await?;
            println!("deleted {app}'s snapshot of {name}");
        }
    }
    Ok(())
}

async fn token(client: &Client, cmd: TokenCommand) -> anyhow::Result<()> {
    server_only(client, "`token`")?;
    match cmd {
        TokenCommand::Create {
            kind,
            name,
            patterns,
            from,
        } => {
            let kind = match kind {
                NewTokenKind::Admin => TokenKind::Admin,
                NewTokenKind::Deploy => TokenKind::Deploy,
                NewTokenKind::Join => TokenKind::Join,
            };
            let created: CreatedToken = client
                .json(client.req(reqwest::Method::POST, "/tokens").json(&CreateToken {
                    kind,
                    name,
                    patterns,
                    from,
                }))
                .await?;
            eprintln!("made {} token {}; it's shown only now", created.kind, created.id);
            println!("{}", created.token);
        }
        TokenCommand::List { json } => {
            let tokens: Vec<TokenInfo> = client.json(client.get("/tokens")).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&tokens)?);
            } else {
                print_table(
                    &["ID", "KIND", "NAME", "PATTERNS", "FROM", "NODE", "CREATED"],
                    tokens
                        .iter()
                        .map(|t| {
                            vec![
                                t.id.to_string(),
                                t.kind.to_string(),
                                t.name.clone(),
                                t.patterns.join(","),
                                t.from.join(","),
                                t.node.clone().unwrap_or_default(),
                                t.created_at.clone(),
                            ]
                        })
                        .collect(),
                );
            }
        }
        TokenCommand::Revoke { id } => {
            client
                .send(client.req(reqwest::Method::DELETE, &format!("/tokens/{id}")))
                .await?;
            println!("revoked token {id}");
        }
    }
    Ok(())
}

async fn cluster(global: &GlobalArgs, cmd: ClusterCommand) -> anyhow::Result<()> {
    let dir = config::dir(global.config_dir.as_deref())?;
    let mut cfg = config::Config::load(&dir)?;
    match cmd {
        ClusterCommand::Add {
            name,
            url,
            make_current,
        } => {
            let token = global
                .token
                .clone()
                .context("pass the cluster's token with --token (or KIWI_NEST_TOKEN)")?;
            let url = url.trim_end_matches('/').to_string();
            let ca = global.read_ca_cert()?;
            let target = Target::Server {
                url: url.clone(),
                token: token.clone(),
                ca: ca.clone(),
            };
            let who = Client::new(&target)?
                .whoami()
                .await
                .with_context(|| format!("checking the token with {url}"))?;
            let ca_cert = ca.map(|pem| String::from_utf8_lossy(&pem).into_owned());
            cfg.clusters
                .insert(name.clone(), config::Cluster { url, token, ca_cert });
            if make_current || cfg.current.is_none() {
                cfg.current = Some(name.clone());
            }
            cfg.save(&dir)?;
            println!(
                "added cluster {name} ({} token{}){}",
                who.kind,
                if who.patterns.is_empty() {
                    String::new()
                } else {
                    format!(" for {}", who.patterns.join(", "))
                },
                if cfg.current.as_deref() == Some(&name) {
                    "; it's current"
                } else {
                    ""
                }
            );
        }
        ClusterCommand::Use { name } => {
            if !cfg.clusters.contains_key(&name) {
                bail!("there's no cluster `{name}`; add it with `kiwi-nest cluster add`");
            }
            cfg.current = Some(name.clone());
            cfg.save(&dir)?;
            println!("using cluster {name}");
        }
        ClusterCommand::List => {
            print_table(
                &["", "NAME", "URL"],
                cfg.clusters
                    .iter()
                    .map(|(n, c)| {
                        let mark = if cfg.current.as_deref() == Some(n) { "*" } else { "" };
                        vec![mark.to_string(), n.clone(), c.url.clone()]
                    })
                    .collect(),
            );
        }
        ClusterCommand::Remove { name } => {
            if cfg.clusters.remove(&name).is_none() {
                bail!("there's no cluster `{name}`");
            }
            if cfg.current.as_deref() == Some(&name) {
                cfg.current = None;
            }
            cfg.save(&dir)?;
            println!("removed cluster {name}");
        }
    }
    Ok(())
}

/// `kiwi-nest node list`.
pub async fn node_list(global: &GlobalArgs, json: bool) -> anyhow::Result<()> {
    let client = Client::new(&Target::resolve(global)?)?;
    server_only(&client, "`node list`")?;
    let nodes = client.nodes().await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&nodes)?);
        return Ok(());
    }
    let gib = |b: u64| format!("{:.1}G", b as f64 / f64::from(1u32 << 30));
    print_table(
        &[
            "NAME",
            "ONLINE",
            "ARCH",
            "MEMORY",
            "APPS",
            "APP MEMORY",
            "BTRFS FREE",
            "UNALLOCATED",
            "VERSION",
            "LAST SEEN",
        ],
        nodes
            .iter()
            .map(|n| {
                let f = n.facts.clone().unwrap_or_default();
                vec![
                    n.name.clone(),
                    if n.online { "yes" } else { "no" }.to_string(),
                    f.arch.clone(),
                    gib(f.memory_bytes),
                    n.apps.to_string(),
                    gib(n.app_memory_bytes),
                    gib(f.volumes.free_bytes),
                    f.volumes.unallocated_bytes.map(gib).unwrap_or_default(),
                    f.version.clone(),
                    n.last_seen.clone().unwrap_or_default(),
                ]
            })
            .collect(),
    );
    Ok(())
}

fn mounts(v: &VolumeInfo) -> String {
    v.mounts
        .iter()
        .map(|m| format!("{} ({})", m.app, m.mode))
        .collect::<Vec<_>>()
        .join(", ")
}

fn or_none(s: String) -> String {
    if s.is_empty() { "none".into() } else { s }
}

/// A bundle as a ZIP: a ZIP file as it is, or a directory packed (refusing what a node would refuse).
async fn read_bundle(path: PathBuf, limits: kn_bundle::Limits) -> anyhow::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let meta = std::fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        if meta.is_dir() {
            kn_bundle::dir::pack(&path, &limits).with_context(|| format!("packing {}", path.display()))
        } else {
            std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
        }
    })
    .await?
}

fn print_table(headers: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let s: Vec<String> = cells.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect();
        println!("{}", s.join("  ").trim_end());
    };
    line(headers.to_vec());
    for row in &rows {
        line(row.iter().map(String::as_str).collect());
    }
}

fn print_info(i: &AppInfo) {
    let idle = if i.idle_timeout_secs == 0 {
        "always on".to_string()
    } else {
        format!("{}s", i.idle_timeout_secs)
    };
    println!("name:        {}", i.name);
    if let Some(n) = &i.node {
        println!("node:        {n}");
    }
    println!("status:      {} (generation {})", i.status, i.generation);
    if let Some(g) = i.pending {
        println!("pending:     generation {g}, not applied by the node yet");
    }
    if i.stale {
        println!("note:        the node is offline, so this is what it last reported");
    }
    if let Some(e) = &i.error {
        println!("error:       {e}");
    }
    println!("domain:      {}", i.domain);
    if !i.stale {
        println!(
            "routed to:   {}:{} on http://127.0.0.1:{}",
            i.service, i.container_port, i.port
        );
        println!("idle:        {idle}");
        println!("open:        {} request(s)", i.open_requests);
        println!("project:     {}", i.project);
    }
    println!("bundle:      {}", i.bundle_digest);
    if !i.stale {
        println!("volumes:     {}", or_none(i.volumes.join(", ")));
        println!(
            "shared:      {}",
            or_none(
                i.shared
                    .iter()
                    .map(|m| format!("{} at {} ({})", m.name, m.target, m.mode))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        );
    }
    if let Some(from) = &i.from {
        println!("from:        {from}");
    }
    println!("created:     {}", i.created_at);
    println!("updated:     {}", i.updated_at);
    if i.stale {
        return;
    }
    println!();
    print_table(
        &["SERVICE", "CONTAINER", "STATE", "HEALTH", "STATUS"],
        i.containers
            .iter()
            .map(|c| {
                vec![
                    c.service.clone(),
                    c.name.clone(),
                    c.state.clone(),
                    c.health.clone(),
                    c.status.clone(),
                ]
            })
            .collect(),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn global(dir: &Path) -> GlobalArgs {
        GlobalArgs {
            node_api: None,
            cluster: None,
            server: None,
            token: None,
            config_dir: Some(dir.to_path_buf()),
            ca_cert: None,
        }
    }

    #[test]
    fn a_clusters_ca_certificate_is_used_unless_another_is_given() {
        let d = tempfile::tempdir().unwrap();
        let mut cfg = config::Config::default();
        cfg.clusters.insert(
            "home".into(),
            config::Cluster {
                url: "https://kn.home.example".into(),
                token: "kna_home".into(),
                ca_cert: Some("-----BEGIN CERTIFICATE-----".into()),
            },
        );
        cfg.current = Some("home".into());
        cfg.save(d.path()).unwrap();
        let Target::Server { ca, .. } = Target::resolve(&global(d.path())).unwrap() else {
            panic!("a server")
        };
        assert_eq!(ca.as_deref(), Some(&b"-----BEGIN CERTIFICATE-----"[..]));
        // A --ca-cert that isn't a certificate is refused, rather than ignored.
        let bad = d.path().join("bad.pem");
        std::fs::write(&bad, "not a certificate").unwrap();
        let g = GlobalArgs {
            ca_cert: Some(bad.clone()),
            ..global(d.path())
        };
        let err = format!("{:#}", Target::resolve(&g).unwrap_err());
        assert!(err.contains("no PEM certificate"), "{err}");
        let g = GlobalArgs {
            ca_cert: Some(d.path().join("missing.pem")),
            ..global(d.path())
        };
        assert!(Target::resolve(&g).is_err());
    }

    #[test]
    fn only_a_server_briefly_away_is_retried() {
        let gateway: anyhow::Error = StatusError {
            status: 502,
            message: "bad gateway".into(),
        }
        .into();
        assert!(transient(&gateway.context("following the deploy")));
        let refused: anyhow::Error = StatusError {
            status: 403,
            message: "no".into(),
        }
        .into();
        assert!(!transient(&refused));
        assert!(!transient(&anyhow::anyhow!("couldn't reach the server")));
    }

    #[test]
    fn the_target_comes_from_flags_then_the_config() {
        let d = tempfile::tempdir().unwrap();
        // Nothing configured: the standalone node on this machine.
        assert!(matches!(Target::resolve(&global(d.path())).unwrap(), Target::Node(u) if u == DEFAULT_NODE_API));
        let mut cfg = config::Config::default();
        for (name, url) in [("home", "https://home.example"), ("work", "https://work.example")] {
            cfg.clusters.insert(
                name.into(),
                config::Cluster {
                    url: url.into(),
                    token: format!("kna_{name}"),
                    ca_cert: None,
                },
            );
        }
        cfg.save(d.path()).unwrap();
        let err = Target::resolve(&global(d.path())).unwrap_err().to_string();
        assert!(err.contains("cluster use"), "{err}");
        cfg.current = Some("home".into());
        cfg.save(d.path()).unwrap();
        let Target::Server { url, token, .. } = Target::resolve(&global(d.path())).unwrap() else {
            panic!("a server")
        };
        assert_eq!((url.as_str(), token.as_str()), ("https://home.example", "kna_home"));
        let g = GlobalArgs {
            cluster: Some("work".into()),
            ..global(d.path())
        };
        assert!(matches!(Target::resolve(&g).unwrap(), Target::Server { url, .. } if url == "https://work.example"));
        let g = GlobalArgs {
            server: Some("http://ci.example".into()),
            token: Some("knd_ci".into()),
            ..global(d.path())
        };
        assert!(matches!(Target::resolve(&g).unwrap(), Target::Server { token, .. } if token == "knd_ci"));
        let g = GlobalArgs {
            node_api: Some("http://127.0.0.1:1".into()),
            ..g
        };
        assert!(matches!(Target::resolve(&g).unwrap(), Target::Node(_)));
        let g = GlobalArgs {
            server: Some("http://ci.example".into()),
            ..global(d.path())
        };
        assert!(Target::resolve(&g).is_err(), "--server needs a token");
    }
}

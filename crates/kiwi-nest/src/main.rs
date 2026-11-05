//! `kiwi-nest`: the one binary, for the server, the node and the CLI.

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "kiwi-nest", version = kn_proto::LONG_VERSION, about = "Hosting for single-instance apps on a small cluster")]
struct Cli {
    #[command(flatten)]
    global: kn_cli::GlobalArgs,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the version and the git commit it was built from.
    Version,
    /// Run the server: the cluster's inventory, its API, and the link its nodes poll.
    Server(ServerArgs),
    /// Run a node, joined to a server (--server) or standalone; or manage the cluster's nodes (`node list`, …).
    Node(Box<NodeArgs>),
    #[command(flatten)]
    Client(kn_cli::Command),
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum IngressMode {
    /// The server doesn't configure an ingress.
    None,
    /// HTTPS, with certificates from an ACME CA (Let's Encrypt and ZeroSSL by default).
    Acme,
    /// HTTPS, with certificates from Caddy's internal CA, for clusters reached only over a VPN.
    Internal,
    /// HTTPS, with the certificates in --certs-dir, obtained elsewhere (such as a DNS-01 wildcard).
    Certs,
    /// Plain HTTP, behind another proxy or a tunnel that handles TLS.
    Http,
}

#[derive(clap::Args)]
struct ServerArgs {
    /// The address to listen on, for the CLI and the nodes (directly, or through the ingress at --api-domain).
    #[arg(long, default_value = "127.0.0.1:7400")]
    listen: SocketAddr,
    /// Where the server keeps its database and bundles.
    #[arg(
        long,
        env = "KIWI_NEST_SERVER_STATE_DIR",
        default_value = "/var/lib/kiwi-nest-server"
    )]
    state_dir: PathBuf,
    /// The longest a node's poll is held open, such as 25s.
    #[arg(long, default_value = "25s")]
    poll_timeout: String,
    /// Memory kept back on every node when placing nests, for the system and Docker, such as 512m or 1g.
    #[arg(long, default_value = "512m")]
    memory_reserve: String,
    /// The share of an idling app's `mem_limit` that placement counts against its node, from 0 to 1.
    #[arg(long, default_value = "0.5")]
    idle_memory_factor: f64,
    /// A node that hasn't polled for this long is offline.
    #[arg(long, default_value = "15s")]
    offline_after: String,
    /// The cluster's ingress: the Caddy the server configures, and how it gets certificates.
    #[arg(long, value_enum, default_value = "none")]
    ingress: IngressMode,
    /// Caddy's admin API, on loopback.
    #[arg(long, default_value = "http://127.0.0.1:2019", value_name = "URL")]
    caddy_admin: String,
    /// The domain the ingress publishes the server's API at, such as kn.example.com, for nodes and the CLI.
    #[arg(long, value_name = "DOMAIN")]
    api_domain: Option<String>,
    /// The ingress's port for HTTP (in the HTTPS modes, it redirects to HTTPS).
    #[arg(long, default_value_t = 80)]
    http_port: u16,
    /// The ingress's port for HTTPS.
    #[arg(long, default_value_t = 443)]
    https_port: u16,
    /// With --ingress acme: the email address for the ACME account.
    #[arg(long, value_name = "EMAIL")]
    acme_email: Option<String>,
    /// With --ingress acme: the ACME CA's directory URL [default: Let's Encrypt, then ZeroSSL].
    #[arg(long, value_name = "URL")]
    acme_ca: Option<String>,
    /// With --ingress certs: the folder of certificates, as .pem files each holding a certificate chain and its key.
    #[arg(long, value_name = "DIR")]
    certs_dir: Option<PathBuf>,
    /// A proxy in front of the ingress whose X-Forwarded-For is believed, as an address or CIDR range (repeatable).
    #[arg(long = "trusted-proxy", value_name = "CIDR")]
    trusted_proxies: Vec<String>,
    /// How often the ingress's configuration is checked, so a restarted Caddy gets it back.
    #[arg(long, default_value = "10s")]
    ingress_sync: String,
}

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
struct NodeArgs {
    #[command(subcommand)]
    cmd: Option<kn_cli::NodeCommand>,
    /// Run on its own, with a local API on loopback (rather than joined to a server).
    #[arg(long)]
    standalone: bool,
    /// The one-time token that lets this node join the server (--server), the first time it starts.
    #[arg(long, env = "KIWI_NEST_JOIN_TOKEN", hide_env_values = true)]
    join_token: Option<String>,
    /// The node's name in the cluster [default: the host name].
    #[arg(long)]
    name: Option<String>,
    /// A label for placement, as KEY=VALUE (repeatable).
    #[arg(long = "label", value_name = "KEY=VALUE")]
    labels: Vec<String>,
    /// The address of the standalone API; it must be loopback.
    #[arg(long, default_value = "127.0.0.1:7380")]
    listen: SocketAddr,
    /// The address of the gateway, which proxies requests to apps by Host, on loopback.
    #[arg(long, default_value = "127.0.0.1:7381")]
    gateway_listen: SocketAddr,
    /// Where the gateway listens for the cluster's ingress: the node's address on the private network, such as
    /// 10.0.0.5:7382. Requests there need the node's ingress token, which the ingress adds.
    #[arg(long, value_name = "ADDR")]
    ingress_listen: Option<SocketAddr>,
    /// Where the node keeps its state, bundles and generated files.
    #[arg(long, env = "KIWI_NEST_STATE_DIR", default_value = "/var/lib/kiwi-nest")]
    state_dir: PathBuf,
    /// Where apps' volumes live, as btrfs subvolumes: a directory on btrfs, reached without symlinks
    /// [default: STATE_DIR/volumes].
    #[arg(long, env = "KIWI_NEST_VOLUMES_DIR")]
    volumes_dir: Option<PathBuf>,
    /// Put before each app's name to make its Compose project name.
    #[arg(long, default_value = "kn-")]
    project_prefix: String,
    /// The loopback ports for apps, as FIRST-LAST.
    #[arg(long, default_value = "30000-39999")]
    ports: String,
    /// A host directory that bundles may mount (repeatable).
    #[arg(long = "allow-host-path", value_name = "DIR")]
    allow_host_paths: Vec<PathBuf>,
    /// The Docker CLI.
    #[arg(long, default_value = "docker")]
    docker: PathBuf,
    /// How long a deploy waits for the app to be running and healthy, such as 90s or 5m.
    #[arg(long, default_value = "5m")]
    up_timeout: String,
    /// How long starting a stopped app for a request may take, until it's healthy and its port answers.
    #[arg(long, default_value = "2m")]
    start_timeout: String,
    /// After a start fails, how long requests are refused before one may try again (`resume` doesn't wait).
    #[arg(long, default_value = "15s")]
    start_cooldown: String,
}

fn duration(flag: &str, value: &str) -> anyhow::Result<Duration> {
    let secs = kn_bundle::manifest::parse_duration(value).with_context(|| format!("bad {flag} {value}"))?;
    Ok(Duration::from_secs(secs))
}

fn parse_ports(s: &str) -> anyhow::Result<std::ops::RangeInclusive<u16>> {
    let (a, b) = s
        .split_once('-')
        .context("ports must be FIRST-LAST, such as 30000-39999")?;
    let (a, b): (u16, u16) = (a.trim().parse()?, b.trim().parse()?);
    if a == 0 || b < a {
        bail!("bad port range {s}");
    }
    Ok(a..=b)
}

/// Makes a directory absolute without needing it to exist.
fn absolute(p: PathBuf) -> anyhow::Result<PathBuf> {
    Ok(std::path::absolute(&p)?)
}

/// The machine's name, as a node name: lowercase letters, digits and `-`.
fn host_name() -> String {
    let raw = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let name: String = raw
        .trim()
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() { "node".into() } else { name }
}

async fn server(args: ServerArgs) -> anyhow::Result<()> {
    let mut cfg = kn_server::ServerConfig::new(absolute(args.state_dir)?);
    cfg.poll_timeout = duration("--poll-timeout", &args.poll_timeout)?;
    cfg.offline_after = duration("--offline-after", &args.offline_after)?;
    cfg.scheduler.memory_reserve = kn_server::bundles::parse_bytes(&args.memory_reserve)
        .with_context(|| format!("bad --memory-reserve {}", args.memory_reserve))?;
    if !(0.0..=1.0).contains(&args.idle_memory_factor) {
        bail!("--idle-memory-factor must be between 0 and 1");
    }
    cfg.scheduler.idle_factor = args.idle_memory_factor;
    if cfg.poll_timeout.is_zero() || cfg.poll_timeout.as_secs() > kn_proto::agent::MAX_POLL_SECS {
        bail!(
            "--poll-timeout must be between 1s and {}s",
            kn_proto::agent::MAX_POLL_SECS
        );
    }
    let tls = match args.ingress {
        IngressMode::None => None,
        IngressMode::Acme => Some(kn_server::ingress::Tls::Acme {
            email: args.acme_email.clone(),
            ca: args.acme_ca.clone(),
        }),
        IngressMode::Internal => Some(kn_server::ingress::Tls::Internal),
        IngressMode::Certs => Some(kn_server::ingress::Tls::Folder(absolute(
            args.certs_dir.clone().context("--ingress certs needs --certs-dir")?,
        )?)),
        IngressMode::Http => Some(kn_server::ingress::Tls::Http),
    };
    if args.ingress != IngressMode::Acme && (args.acme_email.is_some() || args.acme_ca.is_some()) {
        bail!("--acme-email and --acme-ca go with --ingress acme");
    }
    if args.ingress != IngressMode::Certs && args.certs_dir.is_some() {
        bail!("--certs-dir goes with --ingress certs");
    }
    if let Some(tls) = tls {
        let mut i = kn_server::ingress::IngressConfig::new(args.caddy_admin, tls);
        i.api_domain = args.api_domain;
        i.http_port = args.http_port;
        i.https_port = args.https_port;
        i.trusted_proxies = args.trusted_proxies;
        i.sync_interval = duration("--ingress-sync", &args.ingress_sync)?;
        if i.sync_interval.is_zero() {
            bail!("--ingress-sync must be at least 1s");
        }
        i.validate()?;
        cfg.ingress = Some(i);
    } else if args.api_domain.is_some() {
        bail!("--api-domain needs an --ingress");
    }
    kn_server::run(cfg, args.listen).await
}

async fn node(global: &kn_cli::GlobalArgs, args: NodeArgs) -> anyhow::Result<()> {
    if let Some(cmd) = args.cmd {
        return kn_cli::node(global, cmd).await;
    }
    let server = match (&global.server, args.standalone) {
        (Some(_), true) => bail!("a node is either --standalone or joined to a --server, not both"),
        (None, false) => bail!(
            "run `kiwi-nest node --server URL --join-token TOKEN` to join a server, or `kiwi-nest node \
             --standalone`"
        ),
        (s, _) => s.clone(),
    };
    let state_dir = absolute(args.state_dir)?;
    let mut cfg = kn_node::NodeConfig::new(state_dir);
    if let Some(v) = args.volumes_dir {
        cfg.volumes_dir = absolute(v)?;
    }
    cfg.project_prefix = args.project_prefix;
    cfg.ports = parse_ports(&args.ports)?;
    cfg.allowed_host_paths = args
        .allow_host_paths
        .into_iter()
        .map(absolute)
        .collect::<Result<_, _>>()?;
    cfg.docker = args.docker;
    cfg.up_timeout = duration("--up-timeout", &args.up_timeout)?;
    cfg.start_timeout = duration("--start-timeout", &args.start_timeout)?;
    cfg.start_cooldown = duration("--start-cooldown", &args.start_cooldown)?;
    let Some(server) = server else {
        if args.ingress_listen.is_some() {
            bail!("a standalone node has no ingress, so no --ingress-listen");
        }
        return kn_node::run_standalone(cfg, args.listen, args.gateway_listen).await;
    };
    let mut agent = kn_node::agent::AgentConfig::new(server, args.name.unwrap_or_else(host_name));
    agent.join_token = args.join_token;
    agent.ca_certs = global.read_ca_cert()?;
    for l in &args.labels {
        let (k, v) = l
            .split_once('=')
            .with_context(|| format!("a label is KEY=VALUE, not {l}"))?;
        agent.labels.insert(k.trim().to_string(), v.trim().to_string());
    }
    kn_node::run_agent(cfg, agent, args.gateway_listen, args.ingress_listen).await
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let default_level =
        if matches!(&cli.command, Cmd::Node(n) if n.cmd.is_none()) || matches!(cli.command, Cmd::Server(_)) {
            "info"
        } else {
            "warn"
        };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_env("KIWI_NEST_LOG").unwrap_or_else(|_| EnvFilter::new(default_level)))
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .init();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("kiwi-nest: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let result = rt.block_on(async {
        match cli.command {
            Cmd::Version => {
                println!("kiwi-nest {}", kn_proto::LONG_VERSION);
                Ok(())
            }
            Cmd::Server(args) => server(args).await,
            Cmd::Node(args) => node(&cli.global, *args).await,
            Cmd::Client(cmd) => kn_cli::run(&cli.global, cmd).await,
        }
    });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("kiwi-nest: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

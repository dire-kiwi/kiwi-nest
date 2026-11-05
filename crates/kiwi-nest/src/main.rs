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
    /// Run a node, joined to a server (--server) or standalone; or `node list` the cluster's nodes.
    Node(Box<NodeArgs>),
    #[command(flatten)]
    Client(kn_cli::Command),
}

#[derive(clap::Args)]
struct ServerArgs {
    /// The address to listen on, for the CLI and the nodes.
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
    /// A node that hasn't polled for this long is offline.
    #[arg(long, default_value = "15s")]
    offline_after: String,
}

#[derive(clap::Subcommand)]
enum NodeCmd {
    /// List the cluster's nodes: online or not, what they reported, and what's placed on them.
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
struct NodeArgs {
    #[command(subcommand)]
    cmd: Option<NodeCmd>,
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
    /// The address of the gateway, which proxies requests to apps by Host; it must be loopback for now.
    #[arg(long, default_value = "127.0.0.1:7381")]
    gateway_listen: SocketAddr,
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
    if cfg.poll_timeout.is_zero() || cfg.poll_timeout.as_secs() > kn_proto::agent::MAX_POLL_SECS {
        bail!(
            "--poll-timeout must be between 1s and {}s",
            kn_proto::agent::MAX_POLL_SECS
        );
    }
    kn_server::run(cfg, args.listen).await
}

async fn node(global: &kn_cli::GlobalArgs, args: NodeArgs) -> anyhow::Result<()> {
    if let Some(NodeCmd::List { json }) = args.cmd {
        return kn_cli::node_list(global, json).await;
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
        return kn_node::run_standalone(cfg, args.listen, args.gateway_listen).await;
    };
    let mut agent = kn_node::agent::AgentConfig::new(server, args.name.unwrap_or_else(host_name));
    agent.join_token = args.join_token;
    for l in &args.labels {
        let (k, v) = l
            .split_once('=')
            .with_context(|| format!("a label is KEY=VALUE, not {l}"))?;
        agent.labels.insert(k.trim().to_string(), v.trim().to_string());
    }
    kn_node::run_agent(cfg, agent, args.gateway_listen).await
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

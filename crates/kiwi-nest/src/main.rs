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
    /// Run a node.
    Node(NodeArgs),
    #[command(flatten)]
    Client(kn_cli::Command),
}

#[derive(clap::Args)]
struct NodeArgs {
    /// Run on its own, with a local API on loopback (rather than joined to a server).
    #[arg(long)]
    standalone: bool,
    /// The address of the standalone API; it must be loopback.
    #[arg(long, default_value = "127.0.0.1:7380")]
    listen: SocketAddr,
    /// Where the node keeps its state, bundles and generated files.
    #[arg(long, env = "KIWI_NEST_STATE_DIR", default_value = "/var/lib/kiwi-nest")]
    state_dir: PathBuf,
    /// Where apps' volumes live [default: STATE_DIR/volumes].
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

async fn node(args: NodeArgs) -> anyhow::Result<()> {
    if !args.standalone {
        bail!("joining a server isn't supported yet: run `kiwi-nest node --standalone`");
    }
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
    cfg.up_timeout = Duration::from_secs(
        kn_bundle::manifest::parse_duration(&args.up_timeout)
            .with_context(|| format!("bad --up-timeout {}", args.up_timeout))?,
    );
    kn_node::run_standalone(cfg, args.listen).await
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let default_level = if matches!(cli.command, Cmd::Node(_)) {
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
            Cmd::Node(args) => node(args).await,
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

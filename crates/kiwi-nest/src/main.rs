//! `kiwi-nest`: the one binary, for the server, the node and the CLI.
//!
//! The `cluster` feature (on by default) holds the server, the node and installing them, which need Linux; without
//! it the binary is only the CLI, as it's built for macOS.

#[cfg(feature = "cluster")]
mod cluster;
#[cfg(feature = "cluster")]
mod install;

use std::io::IsTerminal;
use std::path::PathBuf;

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

// Parsed once, so the size of its largest variant doesn't matter.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Print the version and the git commit it was built from.
    Version,
    /// Run the server: the cluster's inventory, its API, and the link its nodes poll; or set it up (`server init`).
    #[cfg(feature = "cluster")]
    Server(Box<cluster::ServerArgs>),
    /// Run a node, joined to a server (--server) or standalone; set one up (`node join`); or manage the cluster's
    /// nodes (`node list`, …).
    #[cfg(feature = "cluster")]
    Node(Box<cluster::NodeArgs>),
    /// Upgrade kiwi-nest on this machine, as root: fetch a release and its checksums, install it beside the others,
    /// make it current and restart; on the server's host, the server then upgrades the nodes, one at a time. Or
    /// follow that rollout (`upgrade status`, `upgrade nodes`).
    Upgrade(Box<UpgradeArgs>),
    /// Go back to an earlier release installed on this machine, as root, and restart into it.
    #[cfg(feature = "cluster")]
    Rollback(install::RollbackArgs),
    /// Write this release's systemd units for what's installed here (`upgrade` and `rollback` run this with the
    /// release they switch to).
    #[cfg(feature = "cluster")]
    #[command(hide = true)]
    Units,
    #[command(flatten)]
    Client(kn_cli::Command),
}

#[derive(clap::Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct UpgradeArgs {
    #[command(subcommand)]
    cmd: Option<kn_cli::UpgradeCommand>,
    /// The release to install [default: the latest on GitHub, or the one in --from].
    #[arg(long, value_name = "VERSION")]
    pub version: Option<String>,
    /// A directory of release files with their SHA256SUMS (as `make release` writes them in dist/), instead of
    /// GitHub's releases.
    #[arg(long, value_name = "DIR")]
    pub from: Option<PathBuf>,
    /// How many releases to keep installed (the current one is always kept), for rollbacks.
    #[arg(long, default_value_t = 3)]
    pub keep: usize,
    /// On the server's host: don't follow the rollout to the nodes (it carries on anyway).
    #[arg(long)]
    pub no_wait: bool,
}

/// How the program ends.
pub enum Exit {
    Done,
    /// Run this binary in this process's place, with the same arguments: a node that upgraded itself.
    #[cfg_attr(not(feature = "cluster"), allow(dead_code))]
    Exec(PathBuf),
}

async fn upgrade(global: &kn_cli::GlobalArgs, args: UpgradeArgs) -> anyhow::Result<()> {
    if let Some(cmd) = args.cmd {
        return kn_cli::upgrade(global, cmd).await;
    }
    #[cfg(feature = "cluster")]
    return install::upgrade(global, args).await;
    #[cfg(not(feature = "cluster"))]
    anyhow::bail!(
        "this build of kiwi-nest is only the CLI: download a newer one from https://github.com/{}/releases (see \
         docs/install.md)",
        kn_proto::REPOSITORY
    )
}

/// Replaces this process with `binary`, given the same arguments. Only returns if that fails.
#[cfg(feature = "cluster")]
fn exec(binary: &std::path::Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    eprintln!("kiwi-nest: restarting into {}", binary.display());
    std::process::Command::new(binary)
        .args(std::env::args_os().skip(1))
        .exec()
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    #[cfg(feature = "cluster")]
    let runs = matches!(&cli.command, Cmd::Node(n) if n.cmd.is_none())
        || matches!(&cli.command, Cmd::Server(s) if s.cmd.is_none());
    #[cfg(not(feature = "cluster"))]
    let runs = false;
    let default_level = if runs { "info" } else { "warn" };
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
    let done = |r: anyhow::Result<()>| r.map(|()| Exit::Done);
    let result = rt.block_on(async {
        match cli.command {
            Cmd::Version => {
                println!("kiwi-nest {}", kn_proto::LONG_VERSION);
                Ok(Exit::Done)
            }
            #[cfg(feature = "cluster")]
            Cmd::Server(args) => done(cluster::server(&cli.global, *args).await),
            #[cfg(feature = "cluster")]
            Cmd::Node(args) => cluster::node(&cli.global, *args).await,
            Cmd::Upgrade(args) => done(upgrade(&cli.global, *args).await),
            #[cfg(feature = "cluster")]
            Cmd::Rollback(args) => done(install::rollback(args).await),
            #[cfg(feature = "cluster")]
            Cmd::Units => done(install::units()),
            Cmd::Client(cmd) => done(kn_cli::run(&cli.global, cmd).await),
        }
    });
    // Nothing of the runtime is left running into an exec.
    rt.shutdown_timeout(std::time::Duration::from_secs(2));
    match result {
        Ok(Exit::Done) => std::process::ExitCode::SUCCESS,
        #[cfg(feature = "cluster")]
        Ok(Exit::Exec(binary)) => {
            let e = exec(&binary);
            eprintln!("kiwi-nest: couldn't run {}: {e}", binary.display());
            std::process::ExitCode::FAILURE
        }
        #[cfg(not(feature = "cluster"))]
        Ok(Exit::Exec(_)) => std::process::ExitCode::FAILURE,
        Err(e) => {
            eprintln!("kiwi-nest: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

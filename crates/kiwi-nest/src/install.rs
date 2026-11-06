//! Setting machines up, and upgrading them: `server init`, `node join`, `upgrade`, `rollback` (and the hidden
//! `units`). The pieces are in `kn-install`; this puts them together with the server and the node.
//!
//! Every command here runs as root on the machine it sets up. Each installs the running binary as a release
//! (`/usr/local/lib/kiwi-nest/releases/VERSION/kiwi-nest`, see [`kn_install::layout`]), writes the systemd units, and
//! puts the host's own flags in a drop-in (`UNIT.d/host.conf`) that upgrades leave alone.

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::Parser;
use kn_install::layout::BIN;
use kn_install::systemd::{self, CADDY, DropIn, HOST_DROPIN, NODE, SERVER, Systemd};
use kn_install::{Layout, checks, release};
use kn_proto::version::Version;

use crate::cluster::{IngressMode, absolute, host_name};

/// Where `server init` and `node join` keep files they're given, such as a CA certificate.
const ETC: &str = "/etc/kiwi-nest";

#[derive(clap::Args)]
pub struct InitArgs {
    /// The domain the ingress publishes the server's API at, such as kn.example.com: nodes and the CLI reach the
    /// server there. Point it (and the apps' domains, usually a wildcard) at this machine.
    #[arg(long, value_name = "DOMAIN")]
    api_domain: String,
    /// How the ingress gets certificates: acme (Let's Encrypt or ZeroSSL), internal (Caddy's own CA, for a cluster
    /// reached only over a VPN or LAN), certs (files in --certs-dir) or http (behind another proxy).
    #[arg(long, value_enum)]
    ingress: IngressMode,
    /// With --ingress acme: the email address for the ACME account.
    #[arg(long, value_name = "EMAIL")]
    acme_email: Option<String>,
    /// With --ingress acme: the ACME CA's directory URL [default: Let's Encrypt, then ZeroSSL].
    #[arg(long, value_name = "URL")]
    acme_ca: Option<String>,
    /// With --ingress certs: the folder of .pem files (each a certificate chain and its key), readable by the
    /// kiwi-nest and kiwi-nest-caddy users.
    #[arg(long, value_name = "DIR")]
    certs_dir: Option<PathBuf>,
    /// Run the cluster's image registry at this domain, such as registry.example.com. Needs Docker on this machine.
    #[arg(long, value_name = "DOMAIN")]
    registry_domain: Option<String>,
    /// The backup targets file (readable by the kiwi-nest user, as are the files it names).
    #[arg(long, value_name = "FILE")]
    backup_targets: Option<PathBuf>,
    /// A proxy in front of the ingress whose X-Forwarded-For is believed (repeatable).
    #[arg(long = "trusted-proxy", value_name = "CIDR")]
    trusted_proxies: Vec<String>,
    /// The ingress's port for HTTP.
    #[arg(long, default_value_t = 80)]
    http_port: u16,
    /// The ingress's port for HTTPS.
    #[arg(long, default_value_t = 443)]
    https_port: u16,
    /// Where the server keeps its database and bundles.
    #[arg(long, default_value = "/var/lib/kiwi-nest-server")]
    state_dir: PathBuf,
    /// The server's own listener, on loopback; the ingress publishes it at --api-domain.
    #[arg(long, default_value = "127.0.0.1:7400")]
    listen: SocketAddr,
    /// A Caddy binary to install, instead of downloading the Caddy release kiwi-nest pins.
    #[arg(long, value_name = "FILE")]
    caddy: Option<PathBuf>,
    /// Install and enable everything, but don't start it.
    #[arg(long)]
    no_start: bool,
}

#[derive(clap::Args)]
pub struct JoinArgs {
    /// The server's URL: its API domain through the ingress, such as https://kn.example.com.
    url: String,
    /// A join token, from `kiwi-nest token create join`; used once.
    #[arg(long, env = "KIWI_NEST_JOIN_TOKEN", hide_env_values = true)]
    token: String,
    /// The node's name in the cluster [default: the host name].
    #[arg(long)]
    name: Option<String>,
    /// This machine's address on the cluster's private network, where the ingress reaches its gateway [default: the
    /// address it reaches the server from].
    #[arg(long, value_name = "IP")]
    address: Option<IpAddr>,
    /// The gateway's port for the ingress, on --address. Firewall it to the server's host.
    #[arg(long, default_value_t = 7382)]
    port: u16,
    /// Allow an --address that isn't a private one (on a network that's private anyway).
    #[arg(long)]
    allow_public_address: bool,
    /// Where apps' volumes live: a directory on btrfs (a disk, or a loop-mounted file), mounted at boot.
    #[arg(long, default_value = "/var/lib/kiwi-nest/volumes", value_name = "DIR")]
    volumes_dir: PathBuf,
    /// Where the node keeps its state.
    #[arg(long, default_value = "/var/lib/kiwi-nest", value_name = "DIR")]
    state_dir: PathBuf,
    /// A label for placement, as KEY=VALUE (repeatable).
    #[arg(long = "label", value_name = "KEY=VALUE")]
    labels: Vec<String>,
    /// A host directory that bundles may mount (repeatable).
    #[arg(long = "allow-host-path", value_name = "DIR")]
    allow_host_paths: Vec<PathBuf>,
    /// Install and enable the node, but don't start it.
    #[arg(long)]
    no_start: bool,
}

#[derive(clap::Args)]
pub struct RollbackArgs {
    /// The release to go back to [default: the newest one installed before the current one].
    version: Option<String>,
}

fn s(x: impl ToString) -> String {
    x.to_string()
}

/// Installs the running binary as its release, makes it current, and links it onto the PATH.
fn install_self(layout: &Layout) -> anyhow::Result<()> {
    let exe = std::env::current_exe().context("finding this binary")?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    layout.install_file(kn_proto::VERSION, &exe)?;
    layout.activate(kn_proto::VERSION)?;
    layout.link_bin(Path::new(BIN))?;
    Ok(())
}

/// Makes a system user (and its group) with `home`, unless it exists.
fn ensure_user(name: &str, home: &Path) -> anyhow::Result<()> {
    let exists = std::process::Command::new("getent")
        .args(["passwd", name])
        .output()
        .is_ok_and(|o| o.status.success());
    if exists {
        return Ok(());
    }
    let nologin = ["/usr/sbin/nologin", "/sbin/nologin", "/bin/false"]
        .into_iter()
        .find(|p| Path::new(p).exists())
        .unwrap_or("/bin/false");
    let st = std::process::Command::new("useradd")
        .args([
            "--system",
            "--user-group",
            "--no-create-home",
            "--shell",
            nologin,
            "--home-dir",
        ])
        .arg(home)
        .arg(name)
        .status()
        .context("running useradd")?;
    if !st.success() {
        bail!("useradd {name} failed");
    }
    println!("made the system user {name}");
    Ok(())
}

/// Hands a directory (and everything in it) to a user.
fn chown_r(path: &Path, user: &str) -> anyhow::Result<()> {
    let st = std::process::Command::new("chown")
        .arg("-R")
        .arg(format!("{user}:{user}"))
        .arg(path)
        .status()
        .context("running chown")?;
    if !st.success() {
        bail!("chown -R {user} {} failed", path.display());
    }
    Ok(())
}

fn mkdir(path: &Path, mode: u32) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Waits for `url`'s `/v1/version` to answer `version`.
async fn wait_for_version(http: &reqwest::Client, url: &str, version: &str, timeout: Duration) -> anyhow::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last;
    loop {
        match http.get(format!("{url}/v1/version")).send().await {
            Ok(r) if r.status().is_success() => {
                let v: kn_proto::api::VersionInfo = r.json().await?;
                if v.version == version {
                    return Ok(());
                }
                last = format!("it answers version {}", v.version);
            }
            Ok(r) => last = format!("it answers {}", r.status()),
            Err(e) => last = e.to_string(),
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("the server at {url} didn't come up running {version}: {last}");
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Starts (or restarts) a unit, and checks it's still running a moment later; shows its journal if not.
async fn restart(unit: &str) -> anyhow::Result<()> {
    systemd::systemctl(&["restart", unit])?;
    tokio::time::sleep(Duration::from_secs(3)).await;
    if !systemd::is_active(unit) {
        bail!(
            "{unit} didn't stay up. Its journal (`journalctl -u {unit}`) ends:\n{}",
            systemd::journal_tail(unit, 20)
        );
    }
    Ok(())
}

/// `kiwi-nest server init`.
pub async fn server_init(global: &kn_cli::GlobalArgs, a: InitArgs) -> anyhow::Result<()> {
    if a.ingress == IngressMode::None {
        bail!("`server init` sets up the ingress: choose --ingress acme, internal, certs or http");
    }
    checks::root("kiwi-nest server init")?;
    checks::systemd()?;
    let state_dir = absolute(a.state_dir.clone())?;

    // The server's flags, checked as the server will check them.
    let mut args = vec![
        s("--ingress"),
        s(a.ingress.as_str()),
        s("--api-domain"),
        a.api_domain.clone(),
    ];
    if a.listen.to_string() != "127.0.0.1:7400" {
        args.extend([s("--listen"), a.listen.to_string()]);
    }
    if state_dir != Path::new("/var/lib/kiwi-nest-server") {
        args.extend([s("--state-dir"), state_dir.display().to_string()]);
    }
    for (flag, v) in [
        ("--acme-email", a.acme_email.clone()),
        ("--acme-ca", a.acme_ca.clone()),
        ("--registry-domain", a.registry_domain.clone()),
    ] {
        if let Some(v) = v {
            args.extend([s(flag), v]);
        }
    }
    for (flag, p) in [("--certs-dir", &a.certs_dir), ("--backup-targets", &a.backup_targets)] {
        if let Some(p) = p {
            args.extend([s(flag), absolute(p.clone())?.display().to_string()]);
        }
    }
    for p in &a.trusted_proxies {
        args.extend([s("--trusted-proxy"), p.clone()]);
    }
    if a.http_port != 80 {
        args.extend([s("--http-port"), a.http_port.to_string()]);
    }
    if a.https_port != 443 {
        args.extend([s("--https-port"), a.https_port.to_string()]);
    }
    let parsed = crate::Cli::try_parse_from(
        ["kiwi-nest", "server"]
            .into_iter()
            .map(String::from)
            .chain(args.clone()),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let crate::Cmd::Server(server_args) = parsed.command else {
        unreachable!("parsed as `server`")
    };
    crate::cluster::config(*server_args).context("the server's settings")?;
    let dropin = DropIn {
        service: if a.registry_domain.is_some() {
            vec![(s("SupplementaryGroups"), s("docker"))]
        } else {
            Vec::new()
        },
        args,
        ..Default::default()
    }
    .render("kiwi-nest server init")?;

    // What the machine needs.
    let mut problems = Vec::new();
    if a.registry_domain.is_some()
        && let Err(e) = checks::docker(Path::new("docker"))
    {
        problems.push(format!("the registry runs in Docker: {e}"));
    }
    if !systemd::is_active(CADDY) {
        for port in [a.http_port, a.https_port] {
            if let Err(e) = checks::port_free(port) {
                problems.push(e.to_string());
            }
        }
    }
    if !problems.is_empty() {
        bail!("this machine isn't ready:\n  - {}", problems.join("\n  - "));
    }

    let layout = Layout::system();
    install_self(&layout)?;
    println!(
        "installed kiwi-nest {} in {}",
        kn_proto::VERSION,
        layout.root().display()
    );
    ensure_user(systemd::SERVER_USER, &state_dir)?;
    ensure_user(systemd::CADDY_USER, Path::new(systemd::CADDY_DATA))?;
    mkdir(Path::new(ETC), 0o755)?;
    mkdir(&state_dir, 0o750)?;
    mkdir(Path::new(systemd::CADDY_DATA), 0o700)?;
    chown_r(Path::new(systemd::CADDY_DATA), systemd::CADDY_USER)?;

    let caddy_bin = Path::new(systemd::CADDY_BIN);
    match &a.caddy {
        Some(p) => kn_install::caddy::install_file(p, caddy_bin)?,
        None if kn_install::caddy::version(caddy_bin)
            .is_ok_and(|v| v.trim_start_matches('v') == kn_install::caddy::VERSION) => {}
        None => {
            println!("downloading Caddy {}", kn_install::caddy::VERSION);
            kn_install::caddy::download(&kn_install::http(None)?, caddy_bin).await?;
        }
    }
    println!(
        "Caddy {} is at {}",
        kn_install::caddy::version(caddy_bin)?,
        caddy_bin.display()
    );
    kn_install::layout::write_atomic(
        Path::new(systemd::CADDY_CONFIG),
        systemd::caddy_boot_config("127.0.0.1:2019").as_bytes(),
        0o644,
    )?;

    let sd = Systemd::default();
    for u in [SERVER, CADDY] {
        sd.write_unit(u)?;
    }
    sd.write_dropin(SERVER, HOST_DROPIN, &dropin)?;
    println!(
        "wrote {} and {}",
        sd.unit_path(SERVER).display(),
        sd.dropin_path(SERVER, HOST_DROPIN).display()
    );

    let token = kn_server::first_admin_token(&state_dir).await?;
    chown_r(&state_dir, systemd::SERVER_USER)?;

    systemd::systemctl(&["daemon-reload"])?;
    systemd::systemctl(&["enable", CADDY, SERVER])?;
    let url = format!("https://{}", a.api_domain);
    let url = if a.ingress == IngressMode::Http {
        format!("http://{}", a.api_domain)
    } else if a.https_port != 443 {
        format!("{url}:{}", a.https_port)
    } else {
        url
    };
    if a.no_start {
        println!("enabled {CADDY} and {SERVER}; start them with `systemctl start {CADDY} {SERVER}`");
    } else {
        restart(CADDY).await?;
        restart(SERVER).await?;
        wait_for_version(
            &kn_install::http(None)?,
            &format!("http://{}", a.listen),
            kn_proto::VERSION,
            Duration::from_secs(30),
        )
        .await?;
        println!("the server is up, and the ingress publishes it at {url}");
    }
    let ca = state_dir.join("ingress-ca.crt");
    if a.ingress == IngressMode::Internal && !a.no_start {
        for _ in 0..60 {
            if ca.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    let ca_flag = if a.ingress == IngressMode::Internal {
        format!(" --ca-cert {}", ca.display())
    } else {
        String::new()
    };
    match token {
        Some(t) => {
            println!("\nThe first admin token, shown only now: keep it safe.\n\n  {t}\n");
            println!(
                "On your workstation (copy {} there first, if it's internal):",
                ca.display()
            );
            println!("  kiwi-nest cluster add NAME {url} --token {t}{ca_flag}");
        }
        None => println!("\nThe server has admin tokens already, so no new one was made."),
    }
    println!("\nThen, for each node: `kiwi-nest token create join`, and on the node, as root:");
    println!("  kiwi-nest node join {url} --token JOIN_TOKEN{ca_flag}");
    let _ = global;
    Ok(())
}

impl IngressMode {
    fn as_str(self) -> &'static str {
        match self {
            IngressMode::None => "none",
            IngressMode::Acme => "acme",
            IngressMode::Internal => "internal",
            IngressMode::Certs => "certs",
            IngressMode::Http => "http",
        }
    }
}

/// `kiwi-nest node join`.
pub async fn node_join(global: &kn_cli::GlobalArgs, a: JoinArgs) -> anyhow::Result<()> {
    checks::root("kiwi-nest node join")?;
    checks::systemd()?;
    if systemd::is_active(NODE) {
        bail!(
            "this machine is a node already ({NODE} is running). To join another cluster, stop it and move its \
             state ({}) aside first",
            a.state_dir.display()
        );
    }
    let url = a.url.trim_end_matches('/').to_string();
    let parsed = reqwest::Url::parse(&url).with_context(|| format!("{url} isn't a URL"))?;
    let host = parsed.host_str().context("the server's URL has no host")?.to_string();
    let port = parsed.port_or_known_default().unwrap_or(443);
    let state_dir = absolute(a.state_dir.clone())?;
    let volumes_dir = absolute(a.volumes_dir.clone())?;
    let name = a.name.clone().unwrap_or_else(host_name);
    let ca = global.read_ca_cert()?;

    // Everything that's wrong, at once.
    let mut problems = Vec::new();
    let mut notes = Vec::new();
    let docker = PathBuf::from("docker");
    match checks::docker(&docker) {
        Ok(v) => notes.push(format!("Docker {v}")),
        Err(e) => problems.push(e.to_string()),
    }
    match checks::compose(&docker) {
        Ok(v) => notes.push(format!("Compose {v}")),
        Err(e) => problems.push(e.to_string()),
    }
    match checks::btrfs(&volumes_dir) {
        Ok(()) => {
            let mount = checks::mount_point(&volumes_dir);
            notes.push(format!(
                "volumes in {} (btrfs, mounted at {})",
                volumes_dir.display(),
                mount.as_deref().map_or("?".into(), |m| m.display().to_string())
            ));
            if let Some(m) = mount
                && m != Path::new("/")
                && !checks::in_fstab(&m)
            {
                eprintln!(
                    "warning: {} isn't in /etc/fstab: add it, or the volumes won't be there after a reboot (the node \
                     then refuses to start, rather than start empty)",
                    m.display()
                );
            }
        }
        Err(e) => problems.push(e.to_string()),
    }
    let address = match a.address {
        Some(ip) => Some(ip),
        None => match checks::address_towards(&host, port) {
            Ok(ip) => Some(ip),
            Err(e) => {
                problems.push(format!(
                    "couldn't work out this machine's address towards {host} ({e}): give --address"
                ));
                None
            }
        },
    };
    let ingress_listen = address.map(|ip| SocketAddr::new(ip, a.port));
    if let Some(addr) = ingress_listen {
        match checks::ingress_address(addr, a.allow_public_address) {
            Ok(()) => notes.push(format!("the gateway listens for the ingress on {addr}")),
            Err(e) => problems.push(e.to_string()),
        }
    }
    let http = kn_install::http(ca.as_deref())?;
    match http.get(format!("{url}/v1/version")).send().await {
        Ok(r) if r.status().is_success() => {
            let v: kn_proto::api::VersionInfo = r.json().await?;
            notes.push(format!("the server at {url} runs kiwi-nest {}", v.version));
            match (Version::parse(&v.version), Version::current()) {
                (Some(server), me) if server < me => problems.push(format!(
                    "this kiwi-nest ({me}) is newer than the server's ({server}): upgrade the server first, or use \
                     its release here"
                )),
                (Some(server), me) if server > me => eprintln!(
                    "warning: this kiwi-nest ({me}) is older than the server's ({server}); the server upgrades the \
                     node once it has joined"
                ),
                _ => {}
            }
        }
        Ok(r) => problems.push(format!("the server at {url} answered {} for its version", r.status())),
        Err(e) => problems.push(format!(
            "couldn't reach the server at {url} ({e:#}){}",
            if ca.is_none() {
                "; with --ingress internal, pass its CA with --ca-cert"
            } else {
                ""
            }
        )),
    }
    for n in &notes {
        println!("ok: {n}");
    }
    if !problems.is_empty() {
        bail!(
            "this machine isn't ready to be a node:\n  - {}",
            problems.join("\n  - ")
        );
    }
    let ingress_listen = ingress_listen.expect("checked");

    let layout = Layout::system();
    mkdir(&state_dir, 0o700)?;

    // The flags, as the unit will run the node with them.
    let mut args = vec![
        s("--server"),
        url.clone(),
        s("--name"),
        name.clone(),
        s("--ingress-listen"),
        ingress_listen.to_string(),
        s("--volumes-dir"),
        volumes_dir.display().to_string(),
    ];
    if state_dir != Path::new("/var/lib/kiwi-nest") {
        args.extend([s("--state-dir"), state_dir.display().to_string()]);
    }
    for l in &a.labels {
        args.extend([s("--label"), l.clone()]);
    }
    for p in &a.allow_host_paths {
        args.extend([s("--allow-host-path"), absolute(p.clone())?.display().to_string()]);
    }
    let ca_path = Path::new(ETC).join("server-ca.crt");
    if let Some(pem) = &ca {
        mkdir(Path::new(ETC), 0o755)?;
        kn_install::layout::write_atomic(&ca_path, pem, 0o644)?;
        args.extend([s("--ca-cert"), ca_path.display().to_string()]);
    }
    let parsed = crate::Cli::try_parse_from(["kiwi-nest", "node"].into_iter().map(String::from).chain(args.clone()))
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let dropin = DropIn {
        unit: vec![(s("RequiresMountsFor"), volumes_dir.display().to_string())],
        args,
        ..Default::default()
    }
    .render("kiwi-nest node join")?;
    drop(parsed);

    // Join now, so a bad token fails here rather than in the service's log.
    let mut node_cfg = kn_node::NodeConfig::new(state_dir.clone());
    node_cfg.volumes_dir = volumes_dir.clone();
    let mut agent = kn_node::agent::AgentConfig::new(url.clone(), name.clone());
    agent.join_token = Some(a.token.clone());
    agent.ca_certs = ca.clone();
    agent.installed = Some(layout.clone());
    agent.ingress = ingress_listen.to_string();
    for l in &a.labels {
        let (k, v) = l
            .split_once('=')
            .with_context(|| format!("a label is KEY=VALUE, not {l}"))?;
        agent.labels.insert(k.trim().into(), v.trim().into());
    }
    let joined = kn_node::agent::join(&node_cfg, &agent).await?;
    println!("joined {url} as node {} (node {})", joined.name, joined.node_id);
    install_self(&layout)?;
    println!(
        "installed kiwi-nest {} in {}",
        kn_proto::VERSION,
        layout.root().display()
    );
    if let (Some(pem), Some(registry)) = (&ca, &joined.registry)
        && !registry.starts_with("localhost")
    {
        // Docker checks the registry's certificate itself.
        let p = Path::new("/etc/docker/certs.d").join(registry).join("ca.crt");
        kn_install::layout::write_atomic(&p, pem, 0o644)?;
        println!("Docker trusts the CA for the registry at {registry} ({})", p.display());
    }

    let sd = Systemd::default();
    sd.write_unit(NODE)?;
    sd.write_dropin(NODE, HOST_DROPIN, &dropin)?;
    println!(
        "wrote {} and {}",
        sd.unit_path(NODE).display(),
        sd.dropin_path(NODE, HOST_DROPIN).display()
    );
    systemd::systemctl(&["daemon-reload"])?;
    systemd::systemctl(&["enable", NODE])?;
    if a.no_start {
        println!("enabled {NODE}; start it with `systemctl start {NODE}`");
    } else {
        restart(NODE).await?;
        println!("{NODE} is running; `journalctl -u {NODE} -f` follows it");
    }
    let server_ip = checks::address_towards(&host, port)
        .ok()
        .and_then(|_| {
            use std::net::ToSocketAddrs;
            (host.as_str(), port).to_socket_addrs().ok()?.next().map(|a| a.ip())
        })
        .filter(|ip| checks::is_private(*ip));
    println!(
        "\nThe gateway listens for the ingress on {ingress_listen}. Only the server's host should reach it: firewall \
         that port to {}, for example with\n  ufw allow from {} to any port {} proto tcp && ufw deny {}/tcp",
        server_ip.map_or("the server's address".into(), |ip| ip.to_string()),
        server_ip.map_or("SERVER_IP".into(), |ip| ip.to_string()),
        a.port,
        a.port
    );
    Ok(())
}

/// The roles installed on this machine.
fn roles(sd: &Systemd) -> (bool, bool) {
    (sd.has(SERVER), sd.has(NODE))
}

/// `kiwi-nest units`: this release's units, for what's installed.
pub fn units() -> anyhow::Result<()> {
    checks::root("kiwi-nest units")?;
    let sd = Systemd::default();
    let (server, node) = roles(&sd);
    let mut changed = Vec::new();
    for (has, units) in [(server, &[SERVER, CADDY][..]), (node, &[NODE][..])] {
        if has {
            for u in units {
                if sd.write_unit(u)? {
                    changed.push(*u);
                }
            }
        }
    }
    if !changed.is_empty() {
        println!("updated {}", changed.join(", "));
    }
    Ok(())
}

/// Has a release's binary write its units, then has systemd read them.
fn switch_units(binary: &Path) -> anyhow::Result<()> {
    let st = std::process::Command::new(binary)
        .arg("units")
        .status()
        .with_context(|| format!("running {} units", binary.display()))?;
    if !st.success() {
        eprintln!(
            "warning: {} units failed; the units stay as they were",
            binary.display()
        );
    }
    systemd::systemctl(&["daemon-reload"])
}

/// The server's state directory and listener, from its drop-in's flags.
fn server_settings(sd: &Systemd) -> (PathBuf, String) {
    let args = sd.host_args(SERVER);
    let state = systemd::flag(&args, "--state-dir").unwrap_or("/var/lib/kiwi-nest-server");
    let listen = systemd::flag(&args, "--listen").unwrap_or("127.0.0.1:7400");
    (PathBuf::from(state), format!("http://{listen}"))
}

/// `kiwi-nest upgrade`, on this machine.
pub async fn upgrade(global: &kn_cli::GlobalArgs, a: crate::UpgradeArgs) -> anyhow::Result<()> {
    checks::root("kiwi-nest upgrade")?;
    checks::systemd()?;
    let layout = Layout::system();
    let Some(current) = layout.current() else {
        bail!(
            "kiwi-nest isn't installed on this machine (`kiwi-nest server init` or `node join` installs it in {})",
            layout.root().display()
        );
    };
    let sd = Systemd::default();
    let (server, node) = roles(&sd);
    let source = match &a.from {
        Some(d) => release::Source::Dir(absolute(d.clone())?),
        None => release::Source::github(),
    };
    let http = kn_install::http(None)?;
    let version = match &a.version {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => source.latest(&http).await?,
    };
    kn_install::layout::check_version(&version)?;
    let (want, have) = (
        Version::parse(&version).expect("checked"),
        Version::parse(&current).context("the current release's version")?,
    );
    if want < have {
        bail!("{version} is older than the current release, {current}: `kiwi-nest rollback {version}` goes back");
    }
    // This machine's target, and on the server's host every Linux target, to offer the nodes.
    let own = release::this_linux_target()?;
    let targets: Vec<&str> = if server {
        release::LINUX_TARGETS.to_vec()
    } else {
        vec![own]
    };
    let dist = layout.dist(&version);
    println!(
        "fetching kiwi-nest {version} ({}) from {}",
        targets.join(", "),
        source.describe()
    );
    let files = release::fetch(&source, &http, &version, &targets, &[own], &dist).await?;
    for t in &targets {
        if !files.iter().any(|f| f.target == *t) {
            eprintln!(
                "warning: release {version} has no {t} file, so the server can't offer it to {} nodes: they keep \
                 their release",
                release::linux_arch(t).unwrap_or(t)
            );
        }
    }
    let mine = files
        .iter()
        .find(|f| f.target == own)
        .context("this machine's release file")?;
    let bin = layout.install_file(&version, &mine.path)?;
    kn_install::layout::check_binary(&bin, &version)?;
    if want == have {
        println!("kiwi-nest {version} is the current release already");
    }

    if server && want > have {
        // A copy of the database, in case the new release's migrations need undoing.
        let (state_dir, _) = server_settings(&sd);
        let db = state_dir.join("server.sqlite");
        if db.exists() {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let copy = state_dir
                .join("upgrades")
                .join(format!("server-{current}-{stamp}.sqlite"));
            mkdir(copy.parent().expect("a directory"), 0o750)?;
            let (from, to) = (db.clone(), copy.clone());
            tokio::task::spawn_blocking(move || kn_server::backups::copy_database(&from, &to)).await??;
            chown_r(copy.parent().expect("a directory"), systemd::SERVER_USER)?;
            println!("copied the server's database to {}", copy.display());
        }
    }
    layout.activate(&version)?;
    layout.link_bin(Path::new(BIN))?;
    switch_units(&bin)?;
    println!("kiwi-nest {version} is current (was {current})");

    if server {
        restart(SERVER).await?;
        let (_, url) = server_settings(&sd);
        wait_for_version(&http, &url, &version, Duration::from_secs(60)).await?;
        println!("the server runs {version}; it now upgrades the nodes, one at a time");
    } else if node && want > have {
        restart(NODE).await?;
        println!("the node runs {version}; its apps kept running");
    }
    let removed = layout.prune(a.keep.max(1))?;
    if !removed.is_empty() {
        println!("removed old releases: {}", removed.join(", "));
    }
    if server && !a.no_wait {
        match kn_cli::Target::resolve(global) {
            // The new release follows it: the server it now runs refuses this older CLI.
            Ok(kn_cli::Target::Server { .. }) => follow_with(&bin, global)?,
            _ => println!(
                "`kiwi-nest upgrade status`, with an admin token (a cluster added, or --server and --token), shows \
                 how it goes"
            ),
        }
    }
    Ok(())
}

/// Follows the rollout with `binary upgrade status --wait`, given this command's cluster settings.
fn follow_with(binary: &Path, global: &kn_cli::GlobalArgs) -> anyhow::Result<()> {
    let mut cmd = std::process::Command::new(binary);
    cmd.args(["upgrade", "status", "--wait"]);
    let path = |p: &Option<PathBuf>| p.as_ref().map(|p| absolute(p.clone())).transpose();
    for (var, value) in [
        ("KIWI_NEST_CLUSTER", global.cluster.clone()),
        ("KIWI_NEST_SERVER", global.server.clone()),
        ("KIWI_NEST_TOKEN", global.token.clone()),
        (
            "KIWI_NEST_CONFIG_DIR",
            path(&global.config_dir)?.map(|p| p.display().to_string()),
        ),
        (
            "KIWI_NEST_CA_CERT",
            path(&global.ca_cert)?.map(|p| p.display().to_string()),
        ),
    ] {
        if let Some(v) = value {
            cmd.env(var, v);
        }
    }
    let st = cmd
        .status()
        .with_context(|| format!("running {} upgrade status --wait", binary.display()))?;
    if !st.success() {
        bail!("the rollout to the nodes didn't finish: `kiwi-nest upgrade status` says why");
    }
    Ok(())
}

/// `kiwi-nest rollback`.
pub async fn rollback(a: RollbackArgs) -> anyhow::Result<()> {
    checks::root("kiwi-nest rollback")?;
    checks::systemd()?;
    let layout = Layout::system();
    let current = layout.current().context("kiwi-nest isn't installed on this machine")?;
    let target = match a.version {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => layout
            .previous(&current)?
            .with_context(|| format!("there's no release installed before {current}"))?,
    };
    if target == current {
        bail!("{target} is the current release already");
    }
    if !layout.installed()?.contains(&target) {
        bail!(
            "release {target} isn't installed (these are: {}); `kiwi-nest upgrade --version {target}` installs it",
            layout.installed()?.join(", ")
        );
    }
    let bin = layout.binary(&target);
    kn_install::layout::check_binary(&bin, &target)?;
    layout.activate(&target)?;
    switch_units(&bin)?;
    println!("kiwi-nest {target} is current (was {current})");
    let sd = Systemd::default();
    let (server, node) = roles(&sd);
    if server {
        if let Err(e) = restart(SERVER).await {
            let (state_dir, _) = server_settings(&sd);
            bail!(
                "{e:#}\n\nIf the server refuses its database as being from a newer kiwi-nest, the newer release changed \
                 it: stop the server, put back the copy `upgrade` made in {} as server.sqlite (removing \
                 server.sqlite-wal and -shm), and start it. Changes made since are lost.",
                state_dir.join("upgrades").display()
            );
        }
        println!(
            "the server runs {target}. Nodes newer than it keep their release: roll them back too if it can't talk to them"
        );
    }
    if node {
        restart(NODE).await?;
        println!("the node runs {target}; its apps kept running");
    }
    Ok(())
}

//! What a machine needs before kiwi-nest is installed on it. Each check says what's wrong and how to fix it, and
//! the installers report every problem at once.

use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};

/// Refuses unless running as root.
pub fn root(what: &str) -> anyhow::Result<()> {
    // SAFETY: no arguments, no memory.
    if unsafe { libc::geteuid() } != 0 {
        bail!("{what} needs root: run it with sudo");
    }
    Ok(())
}

/// Refuses unless systemd runs the machine.
pub fn systemd() -> anyhow::Result<()> {
    if !crate::systemd::present() {
        bail!("kiwi-nest installs systemd units, and this machine doesn't run systemd");
    }
    Ok(())
}

fn run(cmd: &mut Command) -> anyhow::Result<String> {
    let out = cmd.output()?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!("{}", err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Docker Engine is installed and running: its server's version.
pub fn docker(docker: &Path) -> anyhow::Result<String> {
    run(Command::new(docker).args(["version", "--format", "{{.Server.Version}}"])).map_err(|e| {
        anyhow::anyhow!(
            "Docker isn't working ({e}): install Docker Engine (https://docs.docker.com/engine/install/) and start \
             it"
        )
    })
}

/// The Compose plugin is installed: its version.
pub fn compose(docker: &Path) -> anyhow::Result<String> {
    run(Command::new(docker).args(["compose", "version", "--short"])).map_err(|e| {
        anyhow::anyhow!("the Docker Compose plugin isn't working ({e}): install it (the docker-compose-plugin package)")
    })
}

/// The volumes directory is on btrfs, reached without symlinks. Says where it's mounted.
pub fn btrfs(dir: &Path) -> anyhow::Result<()> {
    if !dir.is_absolute() {
        bail!("the volumes directory must be an absolute path, not {}", dir.display());
    }
    if !dir.is_dir() {
        bail!(
            "{} doesn't exist: mount a btrfs filesystem there (or make it a directory on one), and add it to \
             /etc/fstab so it's there after a reboot",
            dir.display()
        );
    }
    kn_btrfs::filesystem(dir).map_err(|e| match e {
        kn_btrfs::Error::NotBtrfs(_) => anyhow::anyhow!(
            "{} isn't on btrfs: apps' volumes are btrfs subvolumes. Mount a btrfs disk there, or a loop-mounted \
             file (see docs/install.md)",
            dir.display()
        ),
        e => anyhow::anyhow!("{e}"),
    })?;
    Ok(())
}

/// The mount point a path is on, from `/proc/self/mountinfo`: the longest mount point that's a prefix of it.
pub fn mount_point(path: &Path) -> Option<std::path::PathBuf> {
    let text = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    text.lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|m| std::path::PathBuf::from(m.replace("\\040", " ")))
        .filter(|m| path.starts_with(m))
        .max_by_key(|m| m.as_os_str().len())
}

/// Whether `/etc/fstab` mounts something at `mount`, so it's there after a reboot. (A `.mount` unit would do too;
/// this only warns.)
pub fn in_fstab(mount: &Path) -> bool {
    std::fs::read_to_string("/etc/fstab").is_ok_and(|t| {
        t.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .filter_map(|l| l.split_whitespace().nth(1))
            .any(|m| Path::new(m) == mount)
    })
}

/// Whether an address is on a private network: RFC 1918, shared address space (CGNAT, as Tailscale uses), or IPv6
/// unique local. Loopback isn't: other machines can't reach it.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_private() || (o[0] == 100 && (64..128).contains(&o[1]))
        }
        IpAddr::V6(v6) => (v6.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// This machine's address on the way to `host`: the source address the kernel picks for it. (No packet is sent.)
pub fn address_towards(host: &str, port: u16) -> anyhow::Result<IpAddr> {
    use std::net::ToSocketAddrs;
    let target = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("looking up {host}"))?
        .next()
        .with_context(|| format!("{host} has no address"))?;
    let bind: SocketAddr = if target.is_ipv4() {
        "0.0.0.0:0".parse()?
    } else {
        "[::]:0".parse()?
    };
    let s = UdpSocket::bind(bind)?;
    s.connect(target)
        .with_context(|| format!("finding the route to {host}"))?;
    Ok(s.local_addr()?.ip())
}

/// The gateway's address for the ingress is usable: this machine's own, private (unless allowed otherwise), and
/// free to listen on.
pub fn ingress_address(addr: SocketAddr, allow_public: bool) -> anyhow::Result<()> {
    let ip = addr.ip();
    if ip.is_unspecified() || ip.is_loopback() {
        bail!(
            "the gateway's address for the ingress must be this machine's address on the private network, which \
             the server's host can reach, not {ip}"
        );
    }
    if !is_private(ip) && !allow_public {
        bail!(
            "{ip} isn't a private address: the ingress reaches the gateway over the cluster's private network (a \
             LAN, a VPC, WireGuard or Tailscale). Give --address, or --allow-public-address if this network is \
             private anyway"
        );
    }
    TcpListener::bind(addr).map_err(|e| match e.kind() {
        std::io::ErrorKind::AddrNotAvailable => anyhow::anyhow!("{ip} isn't an address of this machine"),
        std::io::ErrorKind::AddrInUse => anyhow::anyhow!("something is already listening on {addr}"),
        _ => anyhow::anyhow!("can't listen on {addr}: {e}"),
    })?;
    Ok(())
}

/// A port on every address is free (for Caddy's 80 and 443).
pub fn port_free(port: u16) -> anyhow::Result<()> {
    TcpListener::bind(("0.0.0.0", port))
        .map(drop)
        .map_err(|e| anyhow::anyhow!("port {port} isn't free ({e}): the ingress's Caddy listens on it"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_addresses() {
        for p in [
            "10.0.0.5",
            "192.168.1.20",
            "172.16.0.1",
            "100.64.1.1",
            "100.127.255.1",
            "fd12::1",
        ] {
            assert!(is_private(p.parse().unwrap()), "{p}");
        }
        for p in [
            "8.8.8.8",
            "100.128.0.1",
            "127.0.0.1",
            "::1",
            "2001:db8::1",
            "172.32.0.1",
        ] {
            assert!(!is_private(p.parse().unwrap()), "{p}");
        }
    }

    #[test]
    fn the_ingress_address_must_be_ours_and_private() {
        assert!(ingress_address("0.0.0.0:7382".parse().unwrap(), true).is_err());
        assert!(ingress_address("127.0.0.1:7382".parse().unwrap(), true).is_err());
        let err = ingress_address("8.8.8.8:7382".parse().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("isn't a private address"), "{err}");
        let err = ingress_address("10.255.255.254:7382".parse().unwrap(), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("isn't an address of this machine"), "{err}");
    }

    #[test]
    fn finds_mount_points() {
        assert_eq!(
            mount_point(Path::new("/proc/self")).as_deref(),
            Some(Path::new("/proc"))
        );
        assert!(mount_point(Path::new("/")).is_some());
    }

    #[test]
    fn btrfs_needs_a_directory_on_btrfs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(btrfs(&dir.path().join("missing")).is_err());
        assert!(btrfs(Path::new("relative")).is_err());
        if let Ok(d) = std::env::var("KIWI_NEST_TEST_BTRFS_DIR") {
            btrfs(Path::new(&d)).unwrap();
        }
    }
}

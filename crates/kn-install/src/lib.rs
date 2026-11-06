//! Installing kiwi-nest on a machine, and upgrading it.
//!
//! - [`layout`]: where releases are installed, side by side, and which one is current.
//! - [`checksums`]: `SHA256SUMS` files, and checking files against them.
//! - [`release`]: release files' names, and fetching a release from GitHub (or a directory) with its checksums.
//! - [`systemd`]: the units, the drop-ins that hold each host's own flags, and `systemctl`.
//! - [`checks`]: what a machine needs: root, systemd, Docker and Compose, btrfs, a private address.
//! - [`caddy`]: the ingress's Caddy, a pinned release downloaded and checked.
//!
//! The commands that use them (`server init`, `node join`, `upgrade`, `rollback`) are in the `kiwi-nest` binary,
//! which also has the server and the node to call on.

pub mod caddy;
pub mod checks;
pub mod checksums;
pub mod layout;
pub mod release;
pub mod systemd;

pub use layout::Layout;

/// An HTTP client for downloads, with ring as the TLS provider and extra roots if given.
pub fn http(ca: Option<&[u8]>) -> anyhow::Result<reqwest::Client> {
    let _ = rustls_provider();
    let mut b = reqwest::Client::builder()
        .user_agent(format!("{}{}", kn_proto::version::CLI_PRODUCT, kn_proto::VERSION))
        .connect_timeout(std::time::Duration::from_secs(15));
    if let Some(pem) = ca {
        b = b.tls_certs_merge(reqwest::Certificate::from_pem_bundle(pem)?);
    }
    Ok(b.build()?)
}

fn rustls_provider() -> bool {
    // reqwest is built without a provider of its own; whoever runs first installs ring. Doing it here too keeps
    // this crate usable on its own (its tests).
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    true
}

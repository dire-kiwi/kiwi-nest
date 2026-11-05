//! The ingress: the cluster's Caddy, which the server configures through Caddy's admin API on loopback.
//!
//! - **The whole configuration** is generated here ([`caddy_config`]) from the apps and nodes in the store, and
//!   loaded with `POST /load`: when the server starts, after every change, and every `sync_interval`. Each time the
//!   server first reads Caddy's configuration back (`GET /config/`) and only loads its own if they differ, so a
//!   Caddy that restarted with nothing gets its routes back within one interval, and an unchanged one isn't
//!   reloaded. Caddy is told not to save the configuration to disk (`persist: false`), since it holds the nodes'
//!   ingress tokens; the server always has it.
//! - **Routing.** Each app's domain goes to its node's gateway, at the ingress address the node reported (on the
//!   private network), with the node's ingress token in [`INGRESS_HEADER`]. The gateway refuses requests without
//!   it, so a machine on the private network can't skip basic auth. Streams are flushed as they come, and
//!   WebSockets outlive a reload by [`STREAM_CLOSE_DELAY`], since every change in the cluster reloads Caddy. An
//!   unknown host gets 404 from Caddy itself.
//! - **Basic auth** is checked by Caddy, with bcrypt and its cache of verified credentials, so a request without
//!   them never reaches a node, nor wakes an app. An app whose users aren't known (deployed before the server kept
//!   them, and its bundle unreadable) isn't routed at all, rather than routed without its auth.
//! - **The server's own API** is published at `api_domain`, proxied to the server's listener, so nodes and the CLI
//!   reach it over HTTPS.
//! - **TLS**, per cluster ([`Tls`]): automatic ACME; Caddy's internal CA (whose root the server writes to
//!   `STATE/ingress-ca.crt`, for nodes and the CLI to trust); a folder of certificates obtained elsewhere (reloaded
//!   when the folder changes); or plain HTTP behind another proxy.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use kn_proto::agent::INGRESS_HEADER;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::Server;
use crate::error::Result;

/// How long an upgraded connection (a WebSocket) is kept open after Caddy loads a new configuration, before the
/// old configuration's handler closes it. Every change in the cluster reloads Caddy, so this is long.
pub const STREAM_CLOSE_DELAY: &str = "24h";

/// How long Caddy keeps an idle connection to a gateway: less than the gateway's 30 seconds for a request's headers,
/// so Caddy never sends a request on a connection the gateway is closing.
pub const UPSTREAM_IDLE_TIMEOUT: &str = "20s";

/// Forwarding headers Caddy passes on as the client sent them (it sets only `X-Forwarded-For`, `-Proto` and
/// `-Host`): removed, so an app can't be fooled by them.
const UNTRUSTED_FORWARDING: [&str; 3] = ["Forwarded", "X-Forwarded-Port", "X-Real-IP"];

/// The name of the HTTP server in Caddy's configuration.
const SERVER_NAME: &str = "kiwi-nest";

/// The body Caddy answers an unknown host with.
const UNKNOWN_HOST: &str = "nothing is served at this address\n";

/// How certificates are got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tls {
    /// Caddy gets each domain's certificate from an ACME CA (Let's Encrypt and ZeroSSL by default, or `ca`).
    Acme { email: Option<String>, ca: Option<String> },
    /// Caddy's own CA issues them, for clusters reached only over a VPN; clients trust its root.
    Internal,
    /// Certificates obtained elsewhere (such as a DNS-01 wildcard), as `.pem` files each holding a certificate
    /// chain and its key, in this folder.
    Folder(PathBuf),
    /// Plain HTTP, behind another proxy or a tunnel that handles TLS.
    Http,
}

impl Tls {
    pub fn name(&self) -> &'static str {
        match self {
            Tls::Acme { .. } => "acme",
            Tls::Internal => "internal",
            Tls::Folder(_) => "certs",
            Tls::Http => "http",
        }
    }
}

/// How the server configures the ingress.
#[derive(Debug, Clone)]
pub struct IngressConfig {
    /// Caddy's admin API, such as `http://127.0.0.1:2019`.
    pub admin: String,
    pub tls: Tls,
    /// The port for plain HTTP (in TLS modes, it redirects to HTTPS).
    pub http_port: u16,
    pub https_port: u16,
    /// Where the server's own API is published, such as `kn.example.com`.
    pub api_domain: Option<String>,
    /// Proxies in front of the ingress (CIDR ranges) whose `X-Forwarded-For` is believed.
    pub trusted_proxies: Vec<String>,
    /// How often the configuration is checked, so a restarted Caddy gets it back.
    pub sync_interval: Duration,
}

impl IngressConfig {
    pub fn new(admin: impl Into<String>, tls: Tls) -> Self {
        IngressConfig {
            admin: admin.into(),
            tls,
            http_port: 80,
            https_port: 443,
            api_domain: None,
            trusted_proxies: Vec::new(),
            sync_interval: Duration::from_secs(10),
        }
    }

    /// Checks the configuration, and returns the admin API's address as Caddy's `admin.listen` (`HOST:PORT`).
    pub fn validate(&self) -> anyhow::Result<String> {
        let listen = admin_listen(&self.admin)?;
        if let Some(d) = &self.api_domain
            && !kn_bundle::manifest::is_valid_domain(d)
        {
            anyhow::bail!("the API domain must be a hostname in lowercase, not {d:?}");
        }
        if self.http_port == 0 || self.https_port == 0 || self.http_port == self.https_port {
            anyhow::bail!("the ingress needs two different ports for HTTP and HTTPS");
        }
        for r in &self.trusted_proxies {
            if !valid_cidr(r) {
                anyhow::bail!("a trusted proxy is an IP address or a CIDR range, such as 10.0.0.0/8, not {r:?}");
            }
        }
        match &self.tls {
            Tls::Folder(dir) if !dir.is_absolute() => {
                anyhow::bail!(
                    "the certificates folder must be an absolute path, not {}",
                    dir.display()
                )
            }
            Tls::Acme { email: Some(e), .. } if e.is_empty() || e.contains(['{', '}']) || !e.contains('@') => {
                anyhow::bail!("the ACME email must be an email address, not {e:?}")
            }
            Tls::Acme { ca: Some(ca), .. } if !ca.starts_with("https://") && !ca.starts_with("http://") => {
                anyhow::bail!("the ACME CA must be a directory URL, not {ca:?}")
            }
            _ => {}
        }
        Ok(listen)
    }
}

/// `HOST:PORT` from an `http://HOST:PORT` URL.
fn admin_listen(url: &str) -> anyhow::Result<String> {
    let rest = url
        .trim_end_matches('/')
        .strip_prefix("http://")
        .ok_or_else(|| anyhow::anyhow!("Caddy's admin API must be an http:// URL, not {url:?}"))?;
    let ok = !rest.is_empty()
        && rest.rsplit_once(':').is_some_and(|(host, port)| {
            !host.is_empty() && port.parse::<u16>().is_ok_and(|p| p > 0) && !host.contains(['/', '@', '{', '}'])
        });
    if !ok {
        anyhow::bail!("Caddy's admin API must be http://HOST:PORT, such as http://127.0.0.1:2019, not {url:?}");
    }
    Ok(rest.to_string())
}

fn valid_cidr(r: &str) -> bool {
    let (ip, bits) = match r.split_once('/') {
        Some((ip, bits)) => (ip, Some(bits)),
        None => (r, None),
    };
    let Ok(ip) = ip.parse::<IpAddr>() else { return false };
    let max = if ip.is_ipv4() { 32 } else { 128 };
    bits.is_none_or(|b| b.parse::<u8>().is_ok_and(|b| b <= max))
}

/// One basic-auth user of an app: a name and a bcrypt hash (checked by the bundle checker).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BasicAuth {
    pub user: String,
    pub hash: String,
}

/// An app's route: its domain, to its node's gateway with the node's ingress token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub domain: String,
    pub upstream: SocketAddr,
    pub token: String,
    pub auth: Vec<BasicAuth>,
}

/// The address to reach a listener at: an unspecified address (`0.0.0.0`, `::`) becomes loopback.
pub fn reachable(addr: SocketAddr) -> SocketAddr {
    match addr.ip() {
        IpAddr::V4(ip) if ip.is_unspecified() => SocketAddr::new(Ipv4Addr::LOCALHOST.into(), addr.port()),
        IpAddr::V6(ip) if ip.is_unspecified() => SocketAddr::new(Ipv6Addr::LOCALHOST.into(), addr.port()),
        _ => addr,
    }
}

/// The routes for every app the ingress can reach: active apps, on nodes that reported an ingress address. Apps
/// that can't be routed are left out, so Caddy answers 404 for them, and returned as warnings.
pub fn routes(c: &Connection) -> Result<(Vec<Route>, Vec<String>)> {
    let mut st = c.prepare(
        "SELECT a.name, a.domain, a.auth, n.name, n.ingress_token, json_extract(n.facts, '$.ingress')
         FROM apps a JOIN nodes n ON n.id = a.node_id
         WHERE a.state = 'active' AND a.domain IS NOT NULL
         ORDER BY a.domain",
    )?;
    type Row = (String, String, Option<String>, String, Option<String>, Option<String>);
    let rows = st.query_map([], |r| -> rusqlite::Result<Row> {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
    })?;
    let (mut routes, mut warnings) = (Vec::new(), Vec::new());
    for row in rows {
        let (app, domain, auth, node, token, ingress) = row?;
        let auth: Option<Vec<BasicAuth>> = match auth.map(|a| serde_json::from_str(&a)) {
            Some(Ok(a)) => Some(a),
            Some(Err(_)) | None => None,
        };
        let Some(auth) = auth else {
            warnings.push(format!(
                "app `{app}` isn't routed: its basic-auth users aren't known; deploy it again"
            ));
            continue;
        };
        let Some(token) = token.filter(|t| !t.is_empty()) else {
            warnings.push(format!("app `{app}` isn't routed: node `{node}` has no ingress token"));
            continue;
        };
        let upstream = match ingress
            .as_deref()
            .filter(|a| !a.is_empty())
            .map(str::parse::<SocketAddr>)
        {
            Some(Ok(a)) if !a.ip().is_unspecified() && a.port() != 0 => a,
            Some(_) => {
                warnings.push(format!(
                    "app `{app}` isn't routed: node `{node}` reported a bad ingress address"
                ));
                continue;
            }
            None => {
                warnings.push(format!(
                    "app `{app}` isn't routed: node `{node}` has no ingress address (start it with --ingress-listen)"
                ));
                continue;
            }
        };
        routes.push(Route {
            domain,
            upstream,
            token,
            auth,
        });
    }
    Ok((routes, warnings))
}

/// Why the ingress mustn't proxy to `upstream`, if it mustn't: it's Caddy's own admin API or the server's API, which
/// a node could otherwise point an app's domain at by reporting that address.
pub fn forbidden_upstream(upstream: SocketAddr, admin_listen: &str, api: SocketAddr) -> Option<&'static str> {
    let admin = admin_listen.rsplit_once(':').and_then(|(host, port)| {
        let port: u16 = port.parse().ok()?;
        let host = host.trim_start_matches('[').trim_end_matches(']');
        Some((host.parse::<IpAddr>().ok(), host == "localhost", port))
    });
    let is_admin = admin.is_some_and(|(ip, localhost, port)| {
        upstream.port() == port && (ip == Some(upstream.ip()) || (localhost && upstream.ip().is_loopback()))
    });
    if is_admin {
        Some("Caddy's admin API")
    } else if upstream == api
        || (upstream.port() == api.port() && upstream.ip().is_loopback() && api.ip().is_loopback())
    {
        Some("the server's API")
    } else {
        None
    }
}

/// Caddy's whole configuration: the admin API at `admin_listen`, the server's API at `api` (if it has a domain),
/// and `routes` (bar any whose upstream is [`forbidden_upstream`]).
pub fn caddy_config(cfg: &IngressConfig, admin_listen: &str, api: SocketAddr, routes: &[Route]) -> Value {
    let mut handlers = Vec::new();
    if let Some(domain) = &cfg.api_domain {
        handlers.push(json!({
            "match": [{ "host": [domain] }],
            "handle": [{
                "handler": "reverse_proxy",
                "upstreams": [{ "dial": api.to_string() }],
                "headers": { "request": { "delete": [INGRESS_HEADER] } },
                // Logs stream; polls are long.
                "flush_interval": -1,
            }],
            "terminal": true,
        }));
    }
    for r in routes {
        if cfg.api_domain.as_deref() == Some(r.domain.as_str())
            || forbidden_upstream(r.upstream, admin_listen, api).is_some()
        {
            continue;
        }
        let mut handle = Vec::new();
        if !r.auth.is_empty() {
            let accounts: Vec<Value> = r
                .auth
                .iter()
                .map(|a| json!({ "username": a.user, "password": a.hash }))
                .collect();
            handle.push(json!({
                "handler": "authentication",
                "providers": {
                    "http_basic": {
                        "accounts": accounts,
                        "hash": { "algorithm": "bcrypt" },
                        "hash_cache": {},
                        "realm": r.domain,
                    },
                },
            }));
        }
        handle.push(json!({
            "handler": "reverse_proxy",
            "upstreams": [{ "dial": r.upstream.to_string() }],
            "headers": { "request": {
                "set": { INGRESS_HEADER: [r.token] },
                "delete": UNTRUSTED_FORWARDING,
            } },
            "transport": { "protocol": "http", "keep_alive": { "idle_timeout": UPSTREAM_IDLE_TIMEOUT } },
            "flush_interval": -1,
            "stream_close_delay": STREAM_CLOSE_DELAY,
        }));
        handlers.push(json!({
            "match": [{ "host": [r.domain] }],
            "handle": handle,
            "terminal": true,
        }));
    }
    handlers.push(json!({
        "handle": [{
            "handler": "static_response",
            "status_code": 404,
            "headers": {
                "Content-Type": ["text/plain; charset=utf-8"],
                "Cache-Control": ["no-store"],
            },
            "body": UNKNOWN_HOST,
        }],
    }));

    let tls = cfg.tls != Tls::Http;
    let mut server = json!({
        "listen": [format!(":{}", if tls { cfg.https_port } else { cfg.http_port })],
        "routes": handlers,
    });
    if !cfg.trusted_proxies.is_empty() {
        server["trusted_proxies"] = json!({ "source": "static", "ranges": cfg.trusted_proxies });
    }
    let mut apps = json!({});
    match &cfg.tls {
        Tls::Acme { email, ca } => {
            server["tls_connection_policies"] = json!([{}]);
            if email.is_some() || ca.is_some() {
                let mut issuer = json!({ "module": "acme" });
                if let Some(e) = email {
                    issuer["email"] = json!(e);
                }
                if let Some(ca) = ca {
                    issuer["ca"] = json!(ca);
                }
                apps["tls"] = json!({ "automation": { "policies": [{ "issuers": [issuer] }] } });
            }
        }
        Tls::Internal => {
            server["tls_connection_policies"] = json!([{}]);
            apps["tls"] = json!({ "automation": { "policies": [{ "issuers": [{ "module": "internal" }] }] } });
            // Caddy would otherwise try to add its root to this machine's trust store.
            apps["pki"] = json!({ "certificate_authorities": { "local": { "install_trust": false } } });
        }
        Tls::Folder(dir) => {
            server["tls_connection_policies"] = json!([{}]);
            // The folder's certificates only: never ask a CA. HTTP still redirects to HTTPS.
            server["automatic_https"] = json!({ "disable_certificates": true });
            apps["tls"] = json!({ "certificates": { "load_folders": [dir.to_string_lossy()] } });
        }
        Tls::Http => {
            server["automatic_https"] = json!({ "disable": true });
        }
    }
    let mut http = json!({ "http_port": cfg.http_port, "servers": { SERVER_NAME: server } });
    if tls {
        http["https_port"] = json!(cfg.https_port);
    }
    apps["http"] = http;
    json!({
        "admin": {
            "listen": admin_listen,
            // The configuration holds the nodes' ingress tokens; the server loads it again whenever Caddy starts.
            "config": { "persist": false },
        },
        "apps": apps,
    })
}

/// Something that changes when a file in the folder does: each file's path, size and modification time.
fn folder_fingerprint(dir: &Path) -> Vec<(PathBuf, u64, Option<SystemTime>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                out.push((e.path(), m.len(), m.modified().ok()));
            }
        }
    }
    out.sort();
    out
}

/// The ingress's state in the server: what it's configured with, and a signal for changes.
pub struct Ingress {
    cfg: IngressConfig,
    admin_listen: String,
    http: reqwest::Client,
    changed: Notify,
    /// Where the internal CA's root is written.
    ca_path: PathBuf,
}

/// What syncing came to.
#[derive(Debug, PartialEq, Eq)]
enum Synced {
    /// Caddy already had the configuration.
    Unchanged,
    Loaded,
}

impl Ingress {
    pub fn new(cfg: IngressConfig, state_dir: &Path) -> anyhow::Result<Ingress> {
        let admin_listen = cfg.validate()?;
        let _ = rustls::crypto::ring::default_provider().install_default();
        let http = reqwest::Client::builder()
            .user_agent(format!("kiwi-nest-server/{}", kn_proto::VERSION))
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Ingress {
            cfg,
            admin_listen,
            http,
            changed: Notify::new(),
            ca_path: state_dir.join("ingress-ca.crt"),
        })
    }

    pub fn config(&self) -> &IngressConfig {
        &self.cfg
    }

    /// Has the configuration checked soon, since something it's made from may have changed.
    pub fn changed(&self) {
        self.changed.notify_one();
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.cfg.admin.trim_end_matches('/'))
    }

    /// Loads `config` into Caddy, unless Caddy has it already. `force` reloads it even then (the certificates in
    /// the folder changed).
    async fn sync(&self, config: &Value, force: bool) -> anyhow::Result<Synced> {
        if !force {
            let resp = self.http.get(self.url("/config/")).send().await?;
            if resp.status().is_success() {
                let current: Value = resp.json().await.unwrap_or(Value::Null);
                if &current == config {
                    return Ok(Synced::Unchanged);
                }
            }
        }
        let mut req = self.http.post(self.url("/load")).json(config);
        if force {
            req = req.header(reqwest::header::CACHE_CONTROL, "must-revalidate");
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v["error"].as_str().map(str::to_string))
                .unwrap_or(text);
            anyhow::bail!("Caddy refused the configuration ({status}): {}", message.trim());
        }
        Ok(Synced::Loaded)
    }

    /// Writes the internal CA's root certificate to `STATE/ingress-ca.crt`, if it's changed.
    async fn save_ca(&self) -> anyhow::Result<()> {
        let resp = self.http.get(self.url("/pki/ca/local")).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("Caddy answered {} for its CA", resp.status());
        }
        let ca: Value = resp.json().await?;
        let Some(root) = ca["root_certificate"]
            .as_str()
            .filter(|r| r.contains("BEGIN CERTIFICATE"))
        else {
            anyhow::bail!("Caddy's CA has no root certificate");
        };
        if std::fs::read_to_string(&self.ca_path).ok().as_deref() == Some(root) {
            return Ok(());
        }
        let dir = self.ca_path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut tmp, root.as_bytes())?;
        tmp.as_file().sync_all()?;
        tmp.persist(&self.ca_path)?;
        tracing::info!(path = %self.ca_path.display(), "wrote the ingress CA's root certificate");
        Ok(())
    }

    /// Keeps Caddy configured until `stop` is cancelled: now, after each change, and every `sync_interval`.
    pub async fn run(&self, server: Server, api: SocketAddr, stop: CancellationToken) {
        let mut warned: Vec<String> = Vec::new();
        let mut failing: Option<String> = None;
        let mut certs = None;
        let mut loaded_certs = None;
        let mut last_routes = None;
        loop {
            match server.store.call(|c| routes(c)).await {
                Ok((routes, mut warnings)) => {
                    for r in &routes {
                        if let Some(what) = forbidden_upstream(r.upstream, &self.admin_listen, api) {
                            warnings.push(format!(
                                "`{}` isn't routed: its node reported {what}'s address as its ingress address",
                                r.domain
                            ));
                        }
                    }
                    for w in warnings.iter().filter(|w| !warned.contains(w)) {
                        tracing::warn!("{w}");
                    }
                    warned = warnings;
                    if let Tls::Folder(dir) = &self.cfg.tls {
                        let d = dir.clone();
                        certs = tokio::task::spawn_blocking(move || folder_fingerprint(&d)).await.ok();
                    }
                    let config = caddy_config(&self.cfg, &self.admin_listen, api, &routes);
                    let force = loaded_certs.is_some() && certs != loaded_certs;
                    match self.sync(&config, force).await {
                        Ok(synced) => {
                            if failing.take().is_some() {
                                tracing::info!(admin = self.cfg.admin, "reached Caddy again");
                            }
                            if synced == Synced::Loaded || last_routes != Some(routes.len()) {
                                tracing::info!(
                                    routes = routes.len(),
                                    tls = self.cfg.tls.name(),
                                    "configured the ingress"
                                );
                            }
                            last_routes = Some(routes.len());
                            loaded_certs = certs.clone();
                            if self.cfg.tls == Tls::Internal
                                && let Err(e) = self.save_ca().await
                            {
                                tracing::warn!(error = %e, "couldn't save the ingress CA's root certificate");
                            }
                        }
                        Err(e) => {
                            let msg = format!("{e:#}");
                            if failing.as_deref() != Some(&msg) {
                                tracing::warn!(admin = self.cfg.admin, error = msg, "couldn't configure the ingress");
                            }
                            failing = Some(msg);
                        }
                    }
                }
                Err(e) => tracing::warn!(error = %e, "couldn't read the routes for the ingress"),
            }
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = self.changed.notified() => {
                    // Let a burst of changes settle into one load.
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                _ = tokio::time::sleep(self.cfg.sync_interval) => {}
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;

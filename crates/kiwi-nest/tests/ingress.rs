//! End-to-end tests of the ingress with real Caddy: a server configuring it with its internal CA, nodes and the CLI
//! reaching the server through it over HTTPS, and requests through it to apps on the nodes.
//!
//! They need `KIWI_NEST_INTEGRATION=1` and `KIWI_NEST_TEST_BTRFS_DIR` (see `common`), and Caddy: the binary named
//! by `KIWI_NEST_TEST_CADDY`, or `caddy` on the PATH. Caddy runs on ports of its own, with its admin API on
//! loopback and its data (the internal CA) in a temporary directory.

mod common;

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// The Caddy binary, or `None` (and a note) if there isn't one.
fn caddy_bin() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("KIWI_NEST_TEST_CADDY") {
        return Some(PathBuf::from(p));
    }
    let found = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("caddy"))
            .find(|p| p.is_file())
    });
    if found.is_none() {
        eprintln!("skipped: set KIWI_NEST_TEST_CADDY, or put caddy on the PATH, to run the tests with real Caddy");
    }
    found
}

/// A free loopback port (free when asked, at least).
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Caddy, started with nothing but its admin API, as it would be by its own service; the server configures the
/// rest. Killed when dropped.
struct Caddy {
    bin: PathBuf,
    child: Option<Child>,
    admin: u16,
    dir: tempfile::TempDir,
}

impl Caddy {
    fn start(bin: PathBuf) -> Caddy {
        let mut c = Caddy {
            bin,
            child: None,
            admin: free_port(),
            dir: tempfile::tempdir().unwrap(),
        };
        c.spawn();
        c
    }

    fn spawn(&mut self) {
        let boot = self.dir.path().join("boot.json");
        std::fs::write(&boot, format!(r#"{{"admin":{{"listen":"127.0.0.1:{}"}}}}"#, self.admin)).unwrap();
        let log = std::fs::File::create(self.dir.path().join("caddy.log")).unwrap();
        let child = Command::new(&self.bin)
            .args(["run", "--config"])
            .arg(&boot)
            .env("HOME", self.dir.path())
            .env("XDG_DATA_HOME", self.dir.path().join("data"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap();
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        while TcpStream::connect(("127.0.0.1", self.admin)).is_err() {
            assert!(Instant::now() < deadline, "Caddy should start: {}", self.log());
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Kills Caddy and starts it again, empty.
    fn restart(&mut self) {
        self.kill();
        self.spawn();
    }

    fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("caddy.log")).unwrap_or_default()
    }

    /// A bcrypt hash of `password`, from `caddy hash-password`.
    fn hash(&self, password: &str) -> String {
        let out = Command::new(&self.bin)
            .args(["hash-password", "--plaintext", password])
            .output()
            .unwrap();
        assert!(out.status.success());
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }
}

impl Drop for Caddy {
    fn drop(&mut self) {
        self.kill();
    }
}

fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// An HTTPS client for the ingress at `https`, trusting its CA, with the test's domains resolved to it.
fn client(ca: &Path, https: u16) -> reqwest::Client {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let pem = std::fs::read(ca).unwrap();
    let addr: SocketAddr = ([127, 0, 0, 1], https).into();
    reqwest::Client::builder()
        .tls_certs_merge(reqwest::Certificate::from_pem_bundle(&pem).unwrap())
        .resolve("wake.localhost", addr)
        .resolve("kn.localhost", addr)
        .timeout(Duration::from_secs(120))
        .build()
        .unwrap()
}

fn status(server: &Server, app: &str) -> String {
    server.inspect(app)["status"].as_str().unwrap().to_string()
}

#[test]
fn requests_through_the_ingress_wake_apps_and_need_their_credentials_and_the_nodes_token() {
    if !docker_and_btrfs() {
        return;
    }
    let Some(bin) = caddy_bin() else { return };
    let mut caddy = Caddy::start(bin);
    let (http, https) = (free_port(), free_port());
    let mut server = Server::start_with(&[
        "--ingress",
        "internal",
        "--caddy-admin",
        &format!("http://127.0.0.1:{}", caddy.admin),
        "--api-domain",
        "kn.localhost",
        "--http-port",
        &http.to_string(),
        "--https-port",
        &https.to_string(),
        "--ingress-sync",
        "1s",
    ]);
    // The server writes the internal CA's root once Caddy has its configuration.
    let ca = server.dir.path().join("server/ingress-ca.crt");
    wait_for("the ingress CA is saved", Duration::from_secs(30), || ca.exists());

    // From here on, the CLI and the nodes reach the server through the ingress, over HTTPS.
    server.url = format!("https://kn.localhost:{https}");
    server.ca_cert = Some(ca.clone());
    server.ok(&["node", "list"]);
    let prefix = random_prefix();
    let ingress_listen = ["--ingress-listen", "127.0.0.1:0"];
    let _a = ClusterNode::join_with(&server, "a", &format!("{prefix}a-"), &ingress_listen);
    let b = ClusterNode::join_with(&server, "b", &format!("{prefix}b-"), &ingress_listen);
    server.wait_online("a", true);
    server.wait_online("b", true);

    let bundle = tempfile::tempdir().unwrap();
    let hash = caddy.hash("open sesame");
    write_python_bundle(
        bundle.path(),
        "wake.localhost",
        "3s",
        &format!(", auth: [{{ user: preview, hash: '{hash}' }}]"),
    );
    let out = server.ok(&["deploy", "wake", bundle.path().to_str().unwrap(), "--node", "b"]);
    assert!(out.contains("on node b"), "{out}");
    // The API never shows the hash.
    let info = server.ok(&["inspect", "wake", "--json"]);
    assert!(!info.contains("$2"), "{info}");
    wait_for("wake is idle", Duration::from_secs(60), || {
        status(&server, "wake") == "idle"
    });

    let rt = tokio::runtime::Runtime::new().unwrap();
    // reqwest makes its timers when a request is sent, so the futures are made inside the runtime too.
    let _in_runtime = rt.enter();
    let http_client = client(&ca, https);
    let url = format!("https://wake.localhost:{https}/");
    let get = |user: Option<(&str, &str)>| {
        let mut req = http_client.get(&url);
        if let Some((u, p)) = user {
            req = req.basic_auth(u, Some(p));
        }
        let resp = rt.block_on(req.send()).unwrap();
        let code = resp.status().as_u16();
        (code, rt.block_on(resp.text()).unwrap_or_default())
    };

    // Without credentials, or with wrong ones, Caddy answers, and the app isn't woken: the request never reached
    // the node.
    assert_eq!(get(None).0, 401);
    assert_eq!(get(Some(("preview", "wrong"))).0, 401);
    assert_eq!(get(Some(("someone", "open sesame"))).0, 401);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(status(&server, "wake"), "idle");

    // Straight to node b's gateway on its ingress address, without the token: 403, and nothing wakes.
    let direct = rt
        .block_on(
            reqwest::Client::new()
                .get(format!("http://127.0.0.1:{}/", b.ingress.unwrap()))
                .header("host", "wake.localhost")
                .basic_auth("preview", Some("open sesame"))
                .send(),
        )
        .unwrap();
    assert_eq!(direct.status().as_u16(), 403);
    assert_eq!(status(&server, "wake"), "idle");

    // With credentials, through the ingress: the app on node b wakes and answers.
    assert_eq!(get(Some(("preview", "open sesame"))), (200, "hello".to_string()));
    assert_eq!(status(&server, "wake"), "running");
    wait_for("wake is idle again", Duration::from_secs(60), || {
        status(&server, "wake") == "idle"
    });

    // A WebSocket through the ingress keeps the app awake past its idle timeout.
    let mut ws = rt.block_on(async {
        let resp = http_client
            .get(format!("https://wake.localhost:{https}/ws"))
            .basic_auth("preview", Some("open sesame"))
            .header("connection", "Upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-version", "13")
            .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 101);
        resp.upgrade().await.unwrap()
    });
    let mut echo = |msg: &[u8]| {
        rt.block_on(async {
            ws.write_all(msg).await.unwrap();
            let mut buf = vec![0u8; msg.len()];
            tokio::time::timeout(Duration::from_secs(10), ws.read_exact(&mut buf))
                .await
                .expect("an echo")
                .unwrap();
            assert_eq!(buf, msg);
        })
    };
    echo(b"first");
    std::thread::sleep(Duration::from_secs(8));
    assert_eq!(status(&server, "wake"), "running", "the open WebSocket holds the app");
    echo(b"still there");
    drop(echo);
    drop(ws);
    wait_for(
        "wake is idle once the WebSocket closes",
        Duration::from_secs(60),
        || status(&server, "wake") == "idle",
    );

    // A restarted Caddy has no routes, until the server gives them back.
    caddy.restart();
    wait_for("the route is back", Duration::from_secs(30), || {
        let resp = rt.block_on(
            http_client
                .get(&url)
                .basic_auth("preview", Some("open sesame"))
                .timeout(Duration::from_secs(60))
                .send(),
        );
        resp.is_ok_and(|r| r.status().as_u16() == 200)
    });
    // And the CLI reaches the server through it again.
    server.ok(&["inspect", "wake"]);
}

/// The web UI through the ingress, as a browser on another machine reaches it: the page over HTTPS at the API domain,
/// a login whose cookie then works from the page's origin (as the ingress passes it on), and not from an app's.
#[test]
fn the_web_ui_works_through_the_ingress_from_its_own_origin_only() {
    if !integration() {
        return;
    }
    let Some(bin) = caddy_bin() else { return };
    let caddy = Caddy::start(bin);
    let (http, https) = (free_port(), free_port());
    let server = Server::start_with(&[
        "--ingress",
        "internal",
        "--caddy-admin",
        &format!("http://127.0.0.1:{}", caddy.admin),
        "--api-domain",
        "kn.localhost",
        "--http-port",
        &http.to_string(),
        "--https-port",
        &https.to_string(),
        "--ingress-sync",
        "1s",
    ]);
    let ca = server.dir.path().join("server/ingress-ca.crt");
    wait_for("the ingress CA is saved", Duration::from_secs(30), || ca.exists());

    let rt = tokio::runtime::Runtime::new().unwrap();
    let _in_runtime = rt.enter();
    let c = client(&ca, https);
    let base = format!("https://kn.localhost:{https}");
    let page = rt.block_on(c.get(format!("{base}/")).send()).unwrap();
    assert_eq!(page.status(), 200);
    assert!(
        page.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("script-src 'self'")
    );

    let api = format!("{base}{}", kn_proto::API_PREFIX);
    let r = rt
        .block_on(
            c.post(format!("{api}/users"))
                .bearer_auth(&server.admin)
                .json(&serde_json::json!({"name": "ivan", "password": "long enough pw"}))
                .send(),
        )
        .unwrap();
    assert_eq!(r.status(), 201);
    let r = rt
        .block_on(
            c.post(format!("{api}/session"))
                .header("origin", &base)
                .json(&serde_json::json!({"user": "ivan", "password": "long enough pw"}))
                .send(),
        )
        .unwrap();
    assert_eq!(r.status(), 200);
    let set = r.headers()["set-cookie"].to_str().unwrap().to_string();
    let cookie = set.split(';').next().unwrap().to_string();
    assert!(cookie.starts_with("__Host-kn-session=kns_"), "{set}");

    // From the page's origin, as the ingress passes the request on, the session works, changes included.
    let from = |origin: &str, site: &str| {
        c.post(format!("{api}/users"))
            .header("cookie", &cookie)
            .header("origin", origin)
            .header("sec-fetch-site", site)
            .json(&serde_json::json!({"name": format!("u{}", origin.len() + site.len()), "password": "long enough pw"}))
    };
    let r = rt.block_on(from(&base, "same-origin").send()).unwrap();
    assert_eq!(r.status(), 201, "{}", rt.block_on(r.text()).unwrap());
    // From an app's page on a sibling domain, it doesn't.
    let r = rt
        .block_on(from(&format!("https://wake.localhost:{https}"), "same-site").send())
        .unwrap();
    assert_eq!(r.status(), 403);
    drop(caddy);
}

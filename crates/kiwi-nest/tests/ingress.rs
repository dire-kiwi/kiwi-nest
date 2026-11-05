//! End-to-end tests of the ingress with real Caddy: a server configuring it with its internal CA, nodes and the CLI
//! reaching the server through it over HTTPS, and requests through it to apps on the nodes.
//!
//! They need `KIWI_NEST_INTEGRATION=1` and `KIWI_NEST_TEST_BTRFS_DIR` (see `common`), and Caddy: the binary named
//! by `KIWI_NEST_TEST_CADDY`, or `caddy` on the PATH. Caddy runs on ports of its own, with its admin API on
//! loopback and its data (the internal CA) in a temporary directory.

mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use common::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

//! The gateway against the fake runtime: routing, waking, the starting page, leases, idling and adoption.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::HeaderMap;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use kn_proto::state::AppStatus;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::gate::Phase;
use super::normalize_host;
use crate::NodeConfig;
use crate::apps::{Apps, AppsError, DeployOptions};
use crate::testing::{Behaviour, Fake};

struct Env {
    dir: tempfile::TempDir,
    fake: Arc<Fake>,
    apps: Apps,
    gw: SocketAddr,
    /// The listener for the ingress, which needs [`INGRESS_TOKEN`].
    ingress: SocketAddr,
}

const INGRESS_TOKEN: &str = "kni_0123456789abcdef";

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

fn config(dir: &std::path::Path) -> NodeConfig {
    let mut cfg = NodeConfig::new(dir.join("state"));
    cfg.start_timeout = Duration::from_secs(5);
    cfg.start_cooldown = Duration::from_secs(1);
    cfg.starting_page_after = Duration::from_millis(300);
    cfg.idle_check_interval = Duration::from_millis(50);
    cfg
}

impl Env {
    async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        Env::open(dir, Fake::new()).await
    }

    async fn open(dir: tempfile::TempDir, fake: Arc<Fake>) -> Env {
        let volumes = Arc::new(crate::volumes::fake::Fake::default());
        let apps = Apps::open_with_runtime(config(dir.path()), volumes, fake.clone())
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gw = listener.local_addr().unwrap();
        tokio::spawn(super::serve(
            listener,
            apps.clone(),
            super::Access::Local,
            std::future::pending(),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ingress = listener.local_addr().unwrap();
        tokio::spawn(super::serve(
            listener,
            apps.clone(),
            super::Access::Ingress(INGRESS_TOKEN.into()),
            std::future::pending(),
        ));
        Env {
            dir,
            fake,
            apps,
            gw,
            ingress,
        }
    }

    /// Deploys `name` at `NAME.test`, with one service, `web`.
    async fn deploy(&self, name: &str, idle_timeout: &str) {
        let b = self.dir.path().join(format!("bundle-{name}"));
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(
            b.join("compose.yaml"),
            format!(
                "x-kiwi-nest: {{ domain: {name}.test, service: web, port: 80, idle_timeout: {idle_timeout} }}\n\
                 services:\n  web:\n    image: x\n"
            ),
        )
        .unwrap();
        let staged = self.apps.stage_dir(b).await.unwrap();
        self.apps.deploy(name, staged, DeployOptions::default()).await.unwrap();
    }

    fn project(name: &str) -> String {
        format!("kn-{name}")
    }

    fn starts(&self, name: &str) -> usize {
        self.fake.calls("start", &Env::project(name))
    }

    fn stops(&self, name: &str) -> usize {
        self.fake.calls("stop", &Env::project(name))
    }

    fn phase(&self, name: &str) -> Phase {
        self.apps.route(&format!("{name}.test")).unwrap().phase()
    }

    async fn status(&self, name: &str) -> AppStatus {
        self.apps.inspect(name).await.unwrap().status
    }

    /// Waits until `f` holds, failing after a few seconds.
    async fn until(&self, what: &str, mut f: impl FnMut(&Env) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !f(self) {
            assert!(Instant::now() < deadline, "timed out waiting until {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn until_stopped(&self, name: &str) {
        self.until("the app is stopped", |e| e.phase(name) == Phase::Stopped)
            .await;
    }

    async fn send(&self, req: Request<Full<Bytes>>) -> Reply {
        self.send_to(self.gw, req).await
    }

    async fn send_to(&self, addr: SocketAddr, req: Request<Full<Bytes>>) -> Reply {
        let stream = TcpStream::connect(addr).await.unwrap();
        let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .unwrap();
        tokio::spawn(conn);
        let resp = sender.send_request(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }

    async fn get(&self, host: &str, path: &str) -> Reply {
        self.send(Request::get(path).header("host", host).body(Full::default()).unwrap())
            .await
    }

    /// A browser loading a page.
    async fn navigate(&self, host: &str) -> Reply {
        self.send(
            Request::get("/")
                .header("host", host)
                .header("sec-fetch-mode", "navigate")
                .header("accept", "text/html")
                .body(Full::default())
                .unwrap(),
        )
        .await
    }

    /// The starting page's poll.
    async fn poll(&self, host: &str) -> String {
        let r = self
            .send(
                Request::get("/")
                    .header("host", host)
                    .header("kiwi-nest-status", "1")
                    .body(Full::default())
                    .unwrap(),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK);
        assert_eq!(r.headers["cache-control"], "no-store");
        r.body
    }

    /// Opens a WebSocket (an upgraded connection to the fake's echo) through the gateway.
    async fn websocket(&self, host: &str) -> TcpStream {
        let mut s = TcpStream::connect(self.gw).await.unwrap();
        s.write_all(
            format!(
                "GET /ws HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
                 Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut b = [0u8; 1];
            assert_eq!(
                s.read(&mut b).await.unwrap(),
                1,
                "the connection closed during the handshake"
            );
            head.push(b[0]);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.starts_with("HTTP/1.1 101"), "{head}");
        assert!(head.to_ascii_lowercase().contains("upgrade: websocket"), "{head}");
        s
    }
}

async fn echo(ws: &mut TcpStream, msg: &[u8]) {
    ws.write_all(msg).await.unwrap();
    let mut buf = vec![0u8; msg.len()];
    tokio::time::timeout(Duration::from_secs(2), ws.read_exact(&mut buf))
        .await
        .expect("an echo")
        .unwrap();
    assert_eq!(buf, msg);
}

#[test]
fn hosts_are_compared_without_case_or_port() {
    assert_eq!(
        normalize_host("Blog.Example.COM:8080").as_deref(),
        Some("blog.example.com")
    );
    assert_eq!(normalize_host("blog.example.com.").as_deref(), Some("blog.example.com"));
    assert_eq!(normalize_host("[::1]:80").as_deref(), Some("[::1]"));
    assert_eq!(normalize_host("blog:x"), None);
    assert_eq!(normalize_host(""), None);
}

#[tokio::test]
async fn routes_by_host_and_passes_requests_through() {
    let env = Env::new().await;
    env.deploy("blog", "0").await;
    let r = env
        .send(
            Request::post("/echo?a=1&b=%20x")
                .header("host", "BLOG.test:7381")
                .header("x-test", "kept")
                .header("connection", "x-test, keep-alive")
                .body(Full::new(Bytes::from_static(b"the body")))
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    // The Host is passed on as it came; a header the client named in Connection is hop-by-hop, so it isn't.
    assert_eq!(r.body, "POST /echo?a=1&b=%20x host=BLOG.test:7381 x-test=\nthe body");
    let r = env
        .send(
            Request::get("/echo")
                .header("host", "blog.test")
                .header("x-test", "kept")
                .body(Full::default())
                .unwrap(),
        )
        .await;
    assert!(r.body.contains("x-test=kept"), "{}", r.body);

    let r = env.get("blog.test", "/stream?n=3").await;
    assert_eq!(r.body, "tick 0\ntick 1\ntick 2\n");

    let r = env.get("other.test", "/").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    assert_eq!(env.starts("blog"), 0, "a deployed app is running already");
}

#[tokio::test]
async fn a_request_wakes_a_stopped_app_and_idle_apps_stop() {
    let env = Env::new().await;
    env.deploy("blog", "1s").await;
    assert_eq!(env.phase("blog"), Phase::Ready);
    env.until_stopped("blog").await;
    assert_eq!(env.stops("blog"), 1);
    assert_eq!(env.status("blog").await, AppStatus::Idle);

    let r = env.get("blog.test", "/").await;
    assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, "hello from kn-blog"));
    assert_eq!(env.starts("blog"), 1);
    assert_eq!(env.status("blog").await, AppStatus::Running);

    // And it idles out again.
    env.until_stopped("blog").await;
    assert_eq!(env.stops("blog"), 2);
}

#[tokio::test]
async fn an_always_on_app_is_never_stopped() {
    let env = Env::new().await;
    env.deploy("blog", "0").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(env.phase("blog"), Phase::Ready);
    assert_eq!(env.stops("blog"), 0);
}

#[tokio::test]
async fn ten_concurrent_requests_share_one_start() {
    let env = Arc::new(Env::new().await);
    env.deploy("blog", "1s").await;
    env.until_stopped("blog").await;
    env.fake.set_behaviour(Behaviour {
        start_delay: Duration::from_millis(500),
        ..Default::default()
    });
    let mut tasks = Vec::new();
    for _ in 0..10 {
        let env = env.clone();
        tasks.push(tokio::spawn(async move { env.get("blog.test", "/").await }));
    }
    for t in tasks {
        let r = t.await.unwrap();
        assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, "hello from kn-blog"));
    }
    assert_eq!(env.starts("blog"), 1);
}

#[tokio::test]
async fn a_client_that_goes_away_cancels_only_its_own_wait() {
    let env = Env::new().await;
    env.deploy("blog", "1s").await;
    env.until_stopped("blog").await;
    env.fake.set_behaviour(Behaviour {
        start_delay: Duration::from_millis(600),
        ..Default::default()
    });
    let mut s = TcpStream::connect(env.gw).await.unwrap();
    s.write_all(b"GET / HTTP/1.1\r\nHost: blog.test\r\n\r\n").await.unwrap();
    env.until("the start began", |e| e.phase("blog") == Phase::Starting)
        .await;
    drop(s);
    env.until("the start finished", |e| e.phase("blog") == Phase::Ready)
        .await;
    let r = env.get("blog.test", "/").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(env.starts("blog"), 1);
}

#[tokio::test]
async fn an_open_websocket_keeps_the_app_awake() {
    let env = Env::new().await;
    env.deploy("blog", "1s").await;
    let mut ws = env.websocket("blog.test").await;
    echo(&mut ws, b"kia ora").await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(env.stops("blog"), 0, "the WebSocket is open");
    assert_eq!(env.apps.inspect("blog").await.unwrap().open_requests, 1);
    echo(&mut ws, b"still here").await;
    drop(ws);
    env.until_stopped("blog").await;
    assert_eq!(env.apps.inspect("blog").await.unwrap().open_requests, 0);
}

#[tokio::test]
async fn a_streaming_response_keeps_the_app_awake() {
    let env = Env::new().await;
    env.deploy("blog", "1s").await;
    let r = env.get("blog.test", "/stream?n=25").await;
    assert_eq!(r.body.lines().count(), 25);
    assert_eq!(env.stops("blog"), 0, "the response took 2.5s");
    env.until_stopped("blog").await;
}

#[tokio::test]
async fn a_browser_gets_the_starting_page_while_others_wait() {
    let env = Arc::new(Env::new().await);
    env.deploy("blog", "1s").await;
    env.until_stopped("blog").await;
    env.fake.set_behaviour(Behaviour {
        start_delay: Duration::from_millis(1500),
        ..Default::default()
    });
    let waiting = {
        let env = env.clone();
        tokio::spawn(async move {
            let t = Instant::now();
            (env.get("blog.test", "/").await, t.elapsed())
        })
    };
    let t = Instant::now();
    let page = env.navigate("blog.test").await;
    assert!(t.elapsed() < Duration::from_millis(1200), "{:?}", t.elapsed());
    assert_eq!(page.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(page.headers["retry-after"], "2");
    assert_eq!(page.headers["cache-control"], "no-store");
    assert!(page.headers["content-type"].to_str().unwrap().starts_with("text/html"));
    assert!(page.body.contains("Starting up"), "{}", page.body);
    assert_eq!(env.poll("blog.test").await, r#"{"state":"starting"}"#);

    let (r, waited) = waiting.await.unwrap();
    assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, "hello from kn-blog"));
    assert!(waited >= Duration::from_millis(1000), "{waited:?}");
    assert_eq!(env.poll("blog.test").await, r#"{"state":"ready"}"#);
    let r = env.navigate("blog.test").await;
    assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, "hello from kn-blog"));
    assert_eq!(env.starts("blog"), 1);
}

#[tokio::test]
async fn the_starting_page_poll_wakes_a_stopped_app() {
    let env = Env::new().await;
    env.deploy("blog", "1s").await;
    env.until_stopped("blog").await;
    assert_eq!(env.poll("blog.test").await, r#"{"state":"starting"}"#);
    env.until("it started", |e| e.phase("blog") == Phase::Ready).await;
    assert_eq!(env.poll("blog.test").await, r#"{"state":"ready"}"#);
}

#[tokio::test]
async fn a_crashing_app_doesnt_restart_in_a_loop() {
    let env = Env::new().await;
    env.deploy("blog", "1s").await;
    env.until_stopped("blog").await;
    env.fake.set_behaviour(Behaviour {
        crash_on_start: true,
        ..Default::default()
    });
    let r = env.get("blog.test", "/").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.body.contains("couldn't start"), "{}", r.body);
    assert_eq!(env.starts("blog"), 1);
    // It was stopped after the failed start, so Docker doesn't keep restarting it.
    assert_eq!(env.stops("blog"), 2);
    assert_eq!(env.fake.state("kn-blog", "web").as_deref(), Some("exited"));
    let info = env.apps.inspect("blog").await.unwrap();
    assert_eq!(info.status, AppStatus::Idle);
    assert!(info.error.as_deref().unwrap().contains("keeps restarting"), "{info:?}");

    // During the cooldown, requests are refused without starting it; a browser gets a page that doesn't reload.
    let page = env.navigate("blog.test").await;
    assert_eq!(page.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(page.headers.contains_key("retry-after"));
    assert!(page.body.contains("couldn't start"), "{}", page.body);
    assert!(!page.body.contains("<script"), "{}", page.body);
    assert_eq!(env.poll("blog.test").await, r#"{"state":"failed"}"#);
    assert_eq!(env.starts("blog"), 1);

    // After the cooldown, polls still don't start it again; a request does.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(env.poll("blog.test").await, r#"{"state":"failed"}"#);
    assert_eq!(env.starts("blog"), 1);
    let r = env.get("blog.test", "/").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(env.starts("blog"), 2);

    // A resume skips the cooldown.
    env.fake.set_behaviour(Behaviour::default());
    let info = env.apps.resume("blog").await.unwrap();
    assert_eq!((info.status, info.error), (AppStatus::Running, None));
    assert_eq!(env.starts("blog"), 3);
    assert_eq!(env.get("blog.test", "/").await.status, StatusCode::OK);
}

#[tokio::test]
async fn a_failed_connection_gets_503_and_the_next_request_recovers_the_app() {
    let env = Env::new().await;
    env.deploy("blog", "0").await;
    env.fake.kill_server("kn-blog");
    // Let the aborted server let go of its port.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let r = env.get("blog.test", "/").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        env.starts("blog"),
        0,
        "the request isn't retried, and nothing is started yet"
    );
    let r = env.get("blog.test", "/").await;
    assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, "hello from kn-blog"));
    assert_eq!(env.starts("blog"), 1);
}

#[tokio::test]
async fn suspend_waits_for_open_requests_and_only_resume_starts_it_again() {
    let env = Env::new().await;
    env.deploy("blog", "0").await;
    let ws = env.websocket("blog.test").await;
    let e = env.apps.suspend("blog").await.unwrap_err();
    assert!(matches!(&e, AppsError::Busy(m) if m.contains("1 open request")), "{e}");
    drop(ws);
    env.until("the WebSocket closed", |e| {
        e.apps.route("blog.test").unwrap().open_leases() == 0
    })
    .await;
    let info = env.apps.suspend("blog").await.unwrap();
    assert_eq!(info.status, AppStatus::Suspended);
    assert_eq!(env.stops("blog"), 1);

    let r = env.get("blog.test", "/").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(r.body.contains("suspended"), "{}", r.body);
    assert!(env.navigate("blog.test").await.body.contains("suspended"));
    assert_eq!(env.poll("blog.test").await, r#"{"state":"suspended"}"#);
    assert_eq!(env.starts("blog"), 0);

    env.apps.resume("blog").await.unwrap();
    assert_eq!(env.get("blog.test", "/").await.status, StatusCode::OK);
    assert_eq!(env.status("blog").await, AppStatus::Running);
}

#[tokio::test]
async fn delete_breaks_open_leases() {
    let env = Env::new().await;
    env.deploy("blog", "0").await;
    let mut ws = env.websocket("blog.test").await;
    echo(&mut ws, b"hello").await;
    env.apps.delete("blog", false).await.unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(2), ws.read(&mut buf))
        .await
        .expect("the WebSocket is cut off")
        .unwrap_or(0);
    assert_eq!(n, 0);
    assert_eq!(env.get("blog.test", "/").await.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn requests_wait_for_a_deploy() {
    let env = Arc::new(Env::new().await);
    env.deploy("blog", "0").await;
    // Redeploying stops the old project (the fake's `down`) and starts the new one; requests meanwhile wait.
    let gate = env.apps.route("blog.test").unwrap();
    gate.set_phase(Phase::Busy);
    let waiting = {
        let env = env.clone();
        tokio::spawn(async move { env.get("blog.test", "/").await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!waiting.is_finished());
    env.deploy("blog", "0").await;
    assert_eq!(waiting.await.unwrap().status, StatusCode::OK);
}

/// Writes a state file as an earlier node agent would have left it.
fn write_state(dir: &std::path::Path, apps: &[(&str, &str, u16, u64, &[&str])]) {
    let mut map = serde_json::Map::new();
    for (name, status, port, idle, services) in apps {
        map.insert(
            name.to_string(),
            serde_json::json!({
                "name": name, "status": status, "generation": 1, "project": format!("kn-{name}"),
                "port": port, "domain": format!("{name}.test"), "service": services[0], "container_port": 80,
                "idle_timeout_secs": idle, "bundle_digest": "sha256:0", "services": services, "volumes": [],
                "created_at": 0, "updated_at": 0,
            }),
        );
    }
    std::fs::create_dir_all(dir.join("state")).unwrap();
    std::fs::write(
        dir.join("state/state.json"),
        serde_json::to_vec(&serde_json::json!({"version": 1, "apps": map})).unwrap(),
    )
    .unwrap();
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn a_restarted_agent_adopts_running_apps_without_restarting_them() {
    let dir = tempfile::tempdir().unwrap();
    let fake = Fake::new();
    let ports: Vec<u16> = (0..5).map(|_| free_port()).collect();
    write_state(
        dir.path(),
        &[
            ("up", "running", ports[0], 60, &["web", "db"]),
            ("asleep", "idle", ports[1], 60, &["web"]),
            ("half", "running", ports[2], 60, &["web", "db"]),
            ("always", "running", ports[3], 0, &["web"]),
            ("off", "suspended", ports[4], 0, &["web"]),
        ],
    );
    fake.add_project("kn-up", ports[0], &[("web", "running", 0), ("db", "running", 0)]);
    fake.add_project("kn-asleep", ports[1], &[("web", "exited", 0)]);
    fake.add_project("kn-half", ports[2], &[("web", "running", 0), ("db", "exited", 137)]);
    fake.add_project("kn-always", ports[3], &[("web", "exited", 137)]);
    fake.add_project("kn-off", ports[4], &[("web", "exited", 0)]);
    let env = Env::open(dir, fake).await;
    env.until("every app is adopted", |e| {
        ["up", "asleep", "half", "always", "off"]
            .iter()
            .all(|a| !e.phase(a).is_transient())
    })
    .await;

    // Still running: left alone.
    assert_eq!(env.phase("up"), Phase::Ready);
    assert_eq!(env.get("up.test", "/").await.body, "hello from kn-up");
    assert_eq!((env.starts("up"), env.stops("up")), (0, 0));
    // Stopped for being idle: stays stopped until a request.
    assert_eq!(env.phase("asleep"), Phase::Stopped);
    assert_eq!(env.starts("asleep"), 0);
    assert_eq!(env.get("asleep.test", "/").await.status, StatusCode::OK);
    assert_eq!(env.starts("asleep"), 1);
    // Partly running: stopped and suspended.
    assert_eq!(env.phase("half"), Phase::Suspended);
    assert_eq!(env.stops("half"), 1);
    let info = env.apps.inspect("half").await.unwrap();
    assert_eq!(info.status, AppStatus::Suspended);
    assert!(info.error.unwrap().contains("partly running"));
    assert_eq!(env.get("half.test", "/").await.status, StatusCode::SERVICE_UNAVAILABLE);
    // Always on, but stopped: started.
    assert_eq!(env.phase("always"), Phase::Ready);
    assert_eq!(env.starts("always"), 1);
    // Suspended: stays so.
    assert_eq!(env.phase("off"), Phase::Suspended);
    assert_eq!(env.starts("off"), 0);
}

/// A request to `/headers` on `host`, with `headers`.
fn headers_request(host: &str, headers: &[(&str, &str)]) -> Request<Full<Bytes>> {
    let mut r = Request::get("/headers").header("host", host);
    for (k, v) in headers {
        r = r.header(*k, *v);
    }
    r.body(Full::default()).unwrap()
}

#[tokio::test]
async fn the_ingress_listener_takes_only_requests_with_the_token() {
    let env = Env::new().await;
    env.deploy("blog", "1s").await;
    env.until_stopped("blog").await;
    let token = ("kiwi-nest-ingress", INGRESS_TOKEN);
    // Without the token, with a wrong one, or with two, the answer is 403, whatever the host, and nothing wakes.
    for headers in [
        vec![],
        vec![("kiwi-nest-ingress", "kni_wrong")],
        vec![("kiwi-nest-ingress", &INGRESS_TOKEN[..INGRESS_TOKEN.len() - 1])],
        vec![token, ("kiwi-nest-ingress", "kni_wrong")],
        vec![token, token],
    ] {
        for host in ["blog.test", "other.test"] {
            let r = env.send_to(env.ingress, headers_request(host, &headers)).await;
            assert_eq!(r.status, StatusCode::FORBIDDEN, "{host} {headers:?}");
        }
    }
    let r = env
        .send_to(
            env.ingress,
            Request::get("/")
                .header("host", "blog.test")
                .header("kiwi-nest-status", "1")
                .body(Full::default())
                .unwrap(),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "not even the starting page's poll");
    assert_eq!(env.starts("blog"), 0);

    // With it, the request goes through (waking the app), without the token, and with the ingress's forwarding
    // headers as they were.
    let r = env
        .send_to(
            env.ingress,
            headers_request(
                "blog.test",
                &[
                    token,
                    ("x-forwarded-for", "203.0.113.7"),
                    ("x-forwarded-proto", "https"),
                    ("x-forwarded-host", "blog.test"),
                ],
            ),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(!r.body.contains("kiwi-nest-ingress"), "{}", r.body);
    assert!(!r.body.contains(INGRESS_TOKEN), "{}", r.body);
    assert!(r.body.contains("x-forwarded-for: 203.0.113.7\n"), "{}", r.body);
    assert!(r.body.contains("x-forwarded-proto: https\n"), "{}", r.body);
    assert_eq!(env.starts("blog"), 1);
    // An unknown host still gets 404.
    let r = env.send_to(env.ingress, headers_request("other.test", &[token])).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn forwarding_headers_without_the_token_are_replaced() {
    let env = Env::new().await;
    env.deploy("blog", "0").await;
    let r = env
        .send(headers_request(
            "blog.test",
            &[
                ("x-forwarded-for", "203.0.113.7"),
                ("x-forwarded-proto", "https"),
                ("x-forwarded-host", "bank.example"),
                ("forwarded", "for=203.0.113.7"),
                ("x-real-ip", "203.0.113.7"),
                ("kiwi-nest-ingress", "kni_anything"),
            ],
        ))
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let b = &r.body;
    assert!(b.contains("x-forwarded-for: 127.0.0.1\n"), "{b}");
    assert!(b.contains("x-forwarded-proto: http\n"), "{b}");
    assert!(b.contains("x-forwarded-host: blog.test\n"), "{b}");
    assert!(!b.contains("203.0.113.7") && !b.contains("bank.example"), "{b}");
    assert!(!b.contains("kiwi-nest-ingress"), "{b}");
}

#[tokio::test]
async fn a_websocket_through_the_ingress_listener_needs_the_token_and_works_with_it() {
    let env = Env::new().await;
    env.deploy("blog", "0").await;
    for token in ["", "kiwi-nest-ingress: kni_wrong\r\n"] {
        let mut s = TcpStream::connect(env.ingress).await.unwrap();
        s.write_all(
            format!("GET /ws HTTP/1.1\r\nHost: blog.test\r\n{token}Connection: Upgrade\r\nUpgrade: websocket\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
        let mut buf = [0u8; 12];
        s.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"HTTP/1.1 403");
    }
    let mut s = TcpStream::connect(env.ingress).await.unwrap();
    s.write_all(
        format!(
            "GET /ws HTTP/1.1\r\nHost: blog.test\r\nkiwi-nest-ingress: {INGRESS_TOKEN}\r\nConnection: Upgrade\r\n\
             Upgrade: websocket\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        assert_eq!(s.read(&mut b).await.unwrap(), 1);
        head.push(b[0]);
    }
    assert!(String::from_utf8_lossy(&head).starts_with("HTTP/1.1 101"));
    echo(&mut s, b"through the ingress").await;
}

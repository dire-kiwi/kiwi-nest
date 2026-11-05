//! End-to-end tests of the gateway with real Docker: waking, idling, WebSockets, the starting page, a crashing
//! app, and restarting the node agent. They only run with `KIWI_NEST_INTEGRATION=1` and `KIWI_NEST_TEST_BTRFS_DIR`
//! (see `common`).

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use common::*;

/// A response from the gateway.
struct Reply {
    status: u16,
    head: String,
    body: String,
}

/// Sends a request through the gateway on `port`, with `Connection: close`, and reads the whole response.
fn request(port: u16, host: &str, path: &str, headers: &[(&str, &str)]) -> Reply {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(120))).unwrap();
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).unwrap();
    let mut resp = Vec::new();
    s.read_to_end(&mut resp).unwrap();
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((&resp, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("not an HTTP response: {resp:?}"));
    Reply {
        status,
        head: head.to_ascii_lowercase(),
        body: body.to_string(),
    }
}

/// Opens a WebSocket through the gateway; the app echoes what's sent after the handshake.
fn websocket(port: u16, host: &str) -> TcpStream {
    let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();
    s.write_all(
        format!(
            "GET /ws HTTP/1.1\r\nHost: {host}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n"
        )
        .as_bytes(),
    )
    .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        assert_eq!(s.read(&mut b).unwrap(), 1, "the connection closed during the handshake");
        head.push(b[0]);
    }
    let head = String::from_utf8_lossy(&head).to_ascii_lowercase();
    assert!(head.starts_with("http/1.1 101"), "{head}");
    assert!(
        head.contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo="),
        "{head}"
    );
    s
}

fn echo(ws: &mut TcpStream, msg: &[u8]) {
    ws.write_all(msg).unwrap();
    let mut buf = vec![0u8; msg.len()];
    ws.read_exact(&mut buf).unwrap();
    assert_eq!(buf, msg);
}

/// The state and start time of each of a project's containers, as Docker reports them.
fn containers(project: &str) -> Vec<(String, String, String)> {
    let out = Command::new("docker")
        .args(["ps", "--all", "-q", "--filter"])
        .arg(format!("label=com.docker.compose.project={project}"))
        .output()
        .unwrap();
    let ids: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .split_whitespace()
        .map(String::from)
        .collect();
    if ids.is_empty() {
        return Vec::new();
    }
    let out = Command::new("docker")
        .args(["inspect", "--format", "{{.Id}} {{.State.Status}} {{.State.StartedAt}}"])
        .args(&ids)
        .output()
        .unwrap();
    let mut all: Vec<(String, String, String)> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let mut p = l.split(' ');
            let mut next = || p.next().unwrap_or_default().to_string();
            (next(), next(), next())
        })
        .collect();
    all.sort();
    all
}

fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// A Python web app that takes two seconds to start listening, answers `/` with `hello`, and echoes bytes on a
/// WebSocket at `/ws`.
fn write_python_bundle(dir: &Path, idle_timeout: &str) {
    std::fs::create_dir_all(dir.join("app")).unwrap();
    std::fs::write(
        dir.join("app/server.py"),
        r#"import base64, hashlib, http.server, time

time.sleep(2)

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path == "/ws":
            key = self.headers["Sec-WebSocket-Key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", base64.b64encode(hashlib.sha1(key.encode()).digest()).decode())
            self.end_headers()
            self.wfile.flush()
            self.close_connection = True
            while True:
                data = self.connection.recv(4096)
                if not data:
                    return
                self.connection.sendall(data)
        body = b"hello"
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

http.server.ThreadingHTTPServer(("0.0.0.0", 8080), Handler).serve_forever()
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        format!(
            r#"x-kiwi-nest: {{ domain: wake.localhost, service: web, port: 8080, idle_timeout: {idle_timeout} }}
services:
  web:
    image: python:3.13-alpine
    command: ["python", "-u", "/app/server.py"]
    user: "{}"
    init: true
    volumes: ["./app:/app"]
    restart: always
    healthcheck:
      test: ["CMD", "python", "-c", "import urllib.request; urllib.request.urlopen('http://127.0.0.1:8080/')"]
      interval: 1s
      retries: 60
"#,
            uid_gid()
        ),
    )
    .unwrap();
}

#[test]
fn waking_idling_websockets_and_the_starting_page() {
    if !docker_and_btrfs() {
        return;
    }
    let prefix = random_prefix();
    let node = Node::start(&prefix);
    let bundle = tempfile::tempdir().unwrap();
    write_python_bundle(bundle.path(), "4s");
    node.ok(&["deploy", "wake", bundle.path().to_str().unwrap()]);
    let project = format!("{prefix}wake");
    let gw = node.gateway;

    // Routed by Host, whatever its case and port.
    let r = request(gw, "WAKE.localhost:80", "/", &[]);
    assert_eq!((r.status, r.body.as_str()), (200, "hello"), "{}", r.head);
    assert_eq!(request(gw, "nothing.localhost", "/", &[]).status, 404);

    // An open WebSocket keeps the app awake well past its idle timeout.
    let mut ws = websocket(gw, "wake.localhost");
    echo(&mut ws, b"kia ora");
    std::thread::sleep(Duration::from_secs(8));
    assert_eq!(node.inspect("wake")["status"], "running");
    assert_eq!(node.inspect("wake")["open_requests"], 1);
    echo(&mut ws, b"still here");
    drop(ws);

    // Once it's closed, the app is stopped after its timeout, and its containers are kept.
    wait_for("the app is idle", Duration::from_secs(30), || {
        node.inspect("wake")["status"] == "idle"
    });
    let cs = containers(&project);
    assert_eq!(cs.len(), 1);
    assert_eq!(cs[0].1, "exited");

    // Ten requests and a browser at once: the browser gets the starting page (the app takes over two seconds to
    // listen), the others wait and get the app.
    let waiting: Vec<_> = (0..10)
        .map(|_| std::thread::spawn(move || request(gw, "wake.localhost", "/", &[])))
        .collect();
    let t = Instant::now();
    let page = request(gw, "wake.localhost", "/", &[("Sec-Fetch-Mode", "navigate")]);
    assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
    assert_eq!(page.status, 503);
    assert!(page.head.contains("retry-after:"), "{}", page.head);
    assert!(page.head.contains("cache-control: no-store"), "{}", page.head);
    assert!(page.body.contains("Starting up"), "{}", page.body);
    let poll = request(gw, "wake.localhost", "/", &[("Kiwi-Nest-Status", "1")]);
    assert_eq!(poll.body, r#"{"state":"starting"}"#);
    for w in waiting {
        let r = w.join().unwrap();
        assert_eq!((r.status, r.body.as_str()), (200, "hello"), "{}", r.head);
    }
    let poll = request(gw, "wake.localhost", "/", &[("Kiwi-Nest-Status", "1")]);
    assert_eq!(poll.body, r#"{"state":"ready"}"#);
    let r = request(gw, "wake.localhost", "/", &[("Sec-Fetch-Mode", "navigate")]);
    assert_eq!((r.status, r.body.as_str()), (200, "hello"));
    // The same container, started again rather than recreated.
    assert_eq!(containers(&project)[0].0, cs[0].0);
    assert_eq!(node.inspect("wake")["status"], "running");

    // Suspended: requests don't start it, and a resume does.
    node.ok(&["suspend", "wake"]);
    assert_eq!(containers(&project)[0].1, "exited");
    assert_eq!(request(gw, "wake.localhost", "/", &[]).status, 503);
    assert_eq!(containers(&project)[0].1, "exited");
    node.ok(&["resume", "wake"]);
    assert_eq!(request(gw, "wake.localhost", "/", &[]).body, "hello");
}

/// A web app that serves its first run and then exits with an error every time it's started again.
fn write_crashing_bundle(dir: &Path) {
    std::fs::create_dir_all(dir.join("site")).unwrap();
    std::fs::write(dir.join("site/index.html"), "first run").unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        format!(
            r#"x-kiwi-nest: {{ domain: crash.localhost, service: web, port: 8080, idle_timeout: 2s }}
services:
  web:
    image: busybox:1.37
    command: ["sh", "-c", "if [ -e /data/ran ]; then echo crashing; exit 1; fi; touch /data/ran; exec httpd -f -p 8080 -h /www"]
    user: "{}"
    init: true
    restart: always
    volumes: ["./site:/www", "data:/data"]
volumes:
  data: {{}}
"#,
            uid_gid()
        ),
    )
    .unwrap();
}

#[test]
fn a_crashing_app_doesnt_restart_in_a_loop() {
    if !docker_and_btrfs() {
        return;
    }
    let prefix = random_prefix();
    let node = Node::start_with(&prefix, &["--start-cooldown", "1m", "--start-timeout", "30s"]);
    let bundle = tempfile::tempdir().unwrap();
    write_crashing_bundle(bundle.path());
    node.ok(&["deploy", "crash", bundle.path().to_str().unwrap()]);
    let project = format!("{prefix}crash");
    let gw = node.gateway;
    assert_eq!(request(gw, "crash.localhost", "/", &[]).body, "first run");
    wait_for("the app is idle", Duration::from_secs(30), || {
        node.inspect("crash")["status"] == "idle"
    });

    // Waking it fails: the request gets a 503, and the app is stopped rather than left restarting.
    let r = request(gw, "crash.localhost", "/", &[]);
    assert_eq!(r.status, 503, "{}", r.body);
    assert!(r.body.contains("couldn't start"), "{}", r.body);
    let info = node.inspect("crash");
    assert!(info["error"].as_str().unwrap().contains("starting failed"), "{info}");
    let after_failure = containers(&project);
    assert_eq!(after_failure[0].1, "exited", "{after_failure:?}");

    // Nothing restarts it: not Docker, and not more requests or page polls during the cooldown.
    std::thread::sleep(Duration::from_secs(3));
    let r = request(gw, "crash.localhost", "/", &[("Sec-Fetch-Mode", "navigate")]);
    assert_eq!(r.status, 503);
    assert!(!r.body.contains("<script"), "the error page doesn't reload itself");
    let poll = request(gw, "crash.localhost", "/", &[("Kiwi-Nest-Status", "1")]);
    assert_eq!(poll.body, r#"{"state":"failed"}"#);
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(
        containers(&project),
        after_failure,
        "the container wasn't started again"
    );

    // Resume skips the cooldown, and fails the same way.
    let err = node.fails(&["resume", "crash"]);
    assert!(err.contains("starting `crash` failed"), "{err}");
    assert_eq!(containers(&project)[0].1, "exited");
}

#[test]
fn restarting_the_agent_leaves_apps_running() {
    if !docker_and_btrfs() {
        return;
    }
    let prefix = random_prefix();
    let mut node = Node::start(&prefix);
    let bundle = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(bundle.path().join("site")).unwrap();
    std::fs::write(bundle.path().join("site/index.html"), "always on").unwrap();
    std::fs::write(
        bundle.path().join("compose.yaml"),
        format!(
            r#"x-kiwi-nest: {{ domain: steady.localhost, service: web, port: 8080 }}
services:
  web:
    image: busybox:1.37
    command: ["httpd", "-f", "-p", "8080", "-h", "/www"]
    user: "{}"
    init: true
    restart: always
    volumes: ["./site:/www"]
"#,
            uid_gid()
        ),
    )
    .unwrap();
    node.ok(&["deploy", "steady", bundle.path().to_str().unwrap()]);
    let project = format!("{prefix}steady");
    assert_eq!(request(node.gateway, "steady.localhost", "/", &[]).body, "always on");
    let before = containers(&project);
    assert_eq!(before[0].1, "running");

    node.restart();
    let r = request(node.gateway, "steady.localhost", "/", &[]);
    assert_eq!((r.status, r.body.as_str()), (200, "always on"));
    assert_eq!(containers(&project), before, "the same container, never restarted");
    assert_eq!(node.inspect("steady")["status"], "running");
}

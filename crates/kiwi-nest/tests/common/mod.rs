//! What the end-to-end tests share: a standalone node run from the real binary, the CLI, and cleaning up.
//!
//! A node keeps its volumes on btrfs, so the tests that start one need `KIWI_NEST_TEST_BTRFS_DIR`, a directory on
//! btrfs (such as a loop-mounted file: see `scripts/test-btrfs.sh`). The tests that need Docker also need
//! `KIWI_NEST_INTEGRATION=1`. Each uses a random Compose project prefix, so concurrent runs don't collide, and
//! removes only the projects with its own prefix and the directories it made.

#![allow(dead_code)]

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

pub const BIN: &str = env!("CARGO_BIN_EXE_kiwi-nest");

pub fn integration() -> bool {
    if std::env::var("KIWI_NEST_INTEGRATION").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("skipped: set KIWI_NEST_INTEGRATION=1 to run the tests that need Docker");
        false
    }
}

/// The directory on btrfs for nodes' volumes, or `None` (and a note) if it isn't set.
pub fn btrfs_dir() -> Option<PathBuf> {
    match std::env::var_os("KIWI_NEST_TEST_BTRFS_DIR") {
        Some(d) => Some(std::fs::canonicalize(d).expect("KIWI_NEST_TEST_BTRFS_DIR should exist")),
        None => {
            eprintln!(
                "skipped: set KIWI_NEST_TEST_BTRFS_DIR to a directory on btrfs to run the tests that start a node"
            );
            None
        }
    }
}

/// Whether the tests that need Docker and btrfs can run.
pub fn docker_and_btrfs() -> bool {
    integration() && btrfs_dir().is_some()
}

pub fn random_prefix() -> String {
    let n = RandomState::new().hash_one((std::process::id(), SystemTime::now()));
    format!("knit{:08x}-", n as u32)
}

/// Removes the Compose projects whose names start with a prefix.
pub fn remove_projects(prefix: &str) {
    let Ok(out) = Command::new("docker")
        .args(["compose", "ls", "--all", "--format", "json"])
        .output()
    else {
        return;
    };
    let Ok(projects) = serde_json::from_slice::<Vec<serde_json::Value>>(&out.stdout) else {
        return;
    };
    for p in projects {
        if let Some(name) = p["Name"].as_str().filter(|n| n.starts_with(prefix)) {
            eprintln!("cleaning up project {name}");
            let _ = Command::new("docker")
                .args(["compose", "-p", name, "down", "--remove-orphans"])
                .status();
        }
    }
}

/// A standalone node running in the background. When dropped, it's killed, its projects are removed, and then its
/// directories.
pub struct Node {
    child: Child,
    /// The node API.
    pub url: String,
    /// The gateway's port.
    pub gateway: u16,
    pub prefix: String,
    pub dir: tempfile::TempDir,
    pub volumes: tempfile::TempDir,
    extra: Vec<String>,
}

impl Node {
    pub fn start(prefix: &str) -> Node {
        Node::start_with(prefix, &[])
    }

    /// A node with extra `kiwi-nest node` flags.
    pub fn start_with(prefix: &str, extra: &[&str]) -> Node {
        let dir = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir_in(btrfs_dir().expect("KIWI_NEST_TEST_BTRFS_DIR")).unwrap();
        let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        let (child, url, gateway) = spawn_node(dir.path(), volumes.path(), prefix, &extra);
        Node {
            child,
            url,
            gateway,
            prefix: prefix.to_string(),
            dir,
            volumes,
            extra,
        }
    }

    /// Kills the node agent and starts a new one on the same state. Its apps keep running meanwhile.
    pub fn restart(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let (child, url, gateway) = spawn_node(self.dir.path(), self.volumes.path(), &self.prefix, &self.extra);
        self.child = child;
        self.url = url;
        self.gateway = gateway;
    }

    /// A path in the node's volumes directory.
    pub fn volume(&self, path: &str) -> PathBuf {
        self.volumes.path().join(path)
    }

    pub fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--node-api")
            .arg(&self.url)
            .args(args)
            .output()
            .unwrap()
    }

    /// Runs the CLI and returns its stdout, failing the test if it fails.
    pub fn ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "kiwi-nest {args:?} failed\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    }

    /// Runs the CLI, expecting it to fail, and returns its stderr.
    pub fn fails(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(!out.status.success(), "kiwi-nest {args:?} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    pub fn inspect(&self, app: &str) -> serde_json::Value {
        serde_json::from_str(&self.ok(&["inspect", app, "--json"])).unwrap()
    }
}

/// Starts `kiwi-nest node --standalone` on loopback ports of its own; returns it, its API's URL and its gateway's
/// port.
pub fn spawn_node(dir: &Path, volumes: &Path, prefix: &str, extra: &[String]) -> (Child, String, u16) {
    let mut child = Command::new(BIN)
        .args([
            "node",
            "--standalone",
            "--listen",
            "127.0.0.1:0",
            "--gateway-listen",
            "127.0.0.1:0",
        ])
        .args(["--up-timeout", "2m"])
        .arg("--state-dir")
        .arg(dir.join("state"))
        .arg("--volumes-dir")
        .arg(volumes)
        .arg("--project-prefix")
        .arg(prefix)
        .args(extra)
        .env(
            "KIWI_NEST_LOG",
            std::env::var("KIWI_NEST_LOG").unwrap_or_else(|_| "warn".into()),
        )
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        for line in lines.by_ref().take(2).map_while(Result::ok) {
            let _ = tx.send(line);
        }
        for _ in lines {}
    });
    let line = rx.recv_timeout(Duration::from_secs(20)).expect("the node should start");
    let url = line
        .strip_prefix("listening on ")
        .expect("the node should print its address")
        .to_string();
    let line = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the gateway should start");
    let gateway = line
        .strip_prefix("gateway on http://")
        .and_then(|a| a.rsplit(':').next())
        .and_then(|p| p.parse().ok())
        .expect("the node should print its gateway's address");
    (child, url, gateway)
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::env::var("KIWI_NEST_INTEGRATION").as_deref() == Ok("1") {
            remove_projects(&self.prefix);
        }
    }
}

/// GETs `/` on a loopback port, retrying for a while; returns the body.
pub fn get(port: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let attempt = (|| -> std::io::Result<String> {
            let mut s = TcpStream::connect(("127.0.0.1", port as u16))?;
            s.set_read_timeout(Some(Duration::from_secs(5)))?;
            s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
            let mut resp = String::new();
            s.read_to_string(&mut resp)?;
            match resp.split_once("\r\n\r\n") {
                Some((head, body)) if head.starts_with("HTTP/1.0 200") || head.starts_with("HTTP/1.1 200") => {
                    Ok(body.to_string())
                }
                _ => Err(std::io::Error::other(format!("unexpected response: {resp}"))),
            }
        })();
        match attempt {
            Ok(body) => return body,
            Err(e) if Instant::now() > deadline => panic!("GET on port {port} failed: {e}"),
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

pub fn unreachable(port: u64) -> bool {
    TcpStream::connect(("127.0.0.1", port as u16)).is_err()
}

pub fn uid_gid() -> String {
    let out = Command::new("id").arg("-u").output().unwrap();
    let uid = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let out = Command::new("id").arg("-g").output().unwrap();
    let gid = String::from_utf8(out.stdout).unwrap().trim().to_string();
    format!("{uid}:{gid}")
}

/// A server run from the real binary, on a loopback port of its own, with short poll timeouts. Killed when dropped.
pub struct Server {
    child: Child,
    /// Where the CLI and nodes reach the server: its own address, or (once set) the ingress's.
    pub url: String,
    /// A CA certificate for `url`, such as the ingress's internal CA.
    pub ca_cert: Option<PathBuf>,
    addr: String,
    extra: Vec<String>,
    pub admin: String,
    pub dir: tempfile::TempDir,
    /// The CLI's configuration directory, empty, so the tests never use the user's clusters.
    pub config: tempfile::TempDir,
    /// More environment for the CLI, such as its own `DOCKER_CONFIG`.
    pub env: Vec<(String, PathBuf)>,
}

/// Reads a child's first `n` lines of stdout, then keeps draining it.
fn first_lines(child: &mut Child, n: usize, what: &str) -> Vec<String> {
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        for line in lines.by_ref().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut out = Vec::new();
    while out.len() < n {
        out.push(
            rx.recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|_| panic!("{what} should start")),
        );
    }
    // Keep draining in the background thread; later lines are dropped.
    std::thread::spawn(move || for _ in rx {});
    out
}

fn spawn_server(dir: &Path, listen: &str, extra: &[String]) -> (Child, String, Option<String>) {
    let mut child = Command::new(BIN)
        .args([
            "server",
            "--listen",
            listen,
            "--poll-timeout",
            "2s",
            "--offline-after",
            "3s",
        ])
        .arg("--state-dir")
        .arg(dir)
        .args(extra)
        .env(
            "KIWI_NEST_LOG",
            std::env::var("KIWI_NEST_LOG").unwrap_or_else(|_| "warn".into()),
        )
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let first = !dir.join("server.sqlite").exists();
    let lines = first_lines(&mut child, if first { 2 } else { 1 }, "the server");
    let admin = first.then(|| {
        lines[0]
            .strip_prefix("admin token: ")
            .expect("the first start prints an admin token")
            .to_string()
    });
    let url = lines
        .last()
        .unwrap()
        .strip_prefix("listening on ")
        .expect("the server prints its address")
        .to_string();
    (child, url, admin)
}

impl Server {
    pub fn start() -> Server {
        Server::start_with(&[])
    }

    /// Starts a server with more flags.
    pub fn start_with(extra: &[&str]) -> Server {
        let dir = tempfile::tempdir().unwrap();
        let extra: Vec<String> = extra.iter().map(|s| s.to_string()).collect();
        let (child, url, admin) = spawn_server(&dir.path().join("server"), "127.0.0.1:0", &extra);
        Server {
            child,
            addr: url.trim_start_matches("http://").to_string(),
            url,
            ca_cert: None,
            extra,
            admin: admin.unwrap(),
            dir,
            config: tempfile::tempdir().unwrap(),
            env: Vec::new(),
        }
    }

    /// The address the server itself listens on.
    pub fn direct_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Kills the server and starts it again on the same state and address.
    pub fn restart(&mut self) {
        self.down_for(Duration::ZERO);
    }

    /// Kills the server, waits a while (an outage, as the nodes see it), and starts it again as it was.
    pub fn down_for(&mut self, outage: Duration) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        std::thread::sleep(outage);
        let (child, url, admin) = spawn_server(&self.dir.path().join("server"), &self.addr, &self.extra);
        assert!(admin.is_none(), "a restart makes no new admin token");
        assert_eq!(url, self.direct_url());
        self.child = child;
    }

    /// The bundle files the server keeps.
    pub fn bundles(&self) -> usize {
        std::fs::read_dir(self.dir.path().join("server/bundles"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".zip"))
            .count()
    }

    /// Runs the CLI against this server with a token.
    pub fn cli_as(&self, token: &str, args: &[&str]) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .env("KIWI_NEST_SERVER", &self.url)
            .env("KIWI_NEST_TOKEN", token)
            .env("KIWI_NEST_CONFIG_DIR", self.config.path())
            .env_remove("KIWI_NEST_NODE_API")
            .env_remove("KIWI_NEST_CLUSTER")
            .env_remove("KIWI_NEST_CA_CERT");
        if let Some(ca) = &self.ca_cert {
            cmd.env("KIWI_NEST_CA_CERT", ca);
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd.output().unwrap()
    }

    pub fn ok_as(&self, token: &str, args: &[&str]) -> String {
        let out = self.cli_as(token, args);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "kiwi-nest {args:?} failed\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout
    }

    pub fn fails_as(&self, token: &str, args: &[&str]) -> String {
        let out = self.cli_as(token, args);
        assert!(
            !out.status.success(),
            "kiwi-nest {args:?} should fail\nstdout: {}",
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    /// Runs the CLI as the admin.
    pub fn ok(&self, args: &[&str]) -> String {
        self.ok_as(&self.admin, args)
    }

    pub fn fails(&self, args: &[&str]) -> String {
        self.fails_as(&self.admin, args)
    }

    pub fn join_token(&self) -> String {
        self.ok(&["token", "create", "join"]).trim().to_string()
    }

    pub fn nodes(&self) -> Vec<serde_json::Value> {
        serde_json::from_str(&self.ok(&["node", "list", "--json"])).unwrap()
    }

    pub fn inspect(&self, app: &str) -> serde_json::Value {
        serde_json::from_str(&self.ok(&["inspect", app, "--json"])).unwrap()
    }

    /// Waits until node `name` is online, or offline.
    pub fn wait_online(&self, name: &str, online: bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if self.nodes().iter().any(|n| n["name"] == name && n["online"] == online) {
                return;
            }
            assert!(Instant::now() < deadline, "node {name} should be online: {online}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A node joined to a [`Server`], run from the real binary. When dropped, it's killed, its projects are removed,
/// and then its directories.
pub struct ClusterNode {
    child: Option<Child>,
    pub name: String,
    server: String,
    ca_cert: Option<PathBuf>,
    extra: Vec<String>,
    pub gateway: u16,
    /// The port its gateway listens on for the ingress (with `--ingress-listen`).
    pub ingress: Option<u16>,
    pub prefix: String,
    pub dir: tempfile::TempDir,
    pub volumes: tempfile::TempDir,
}

impl ClusterNode {
    pub fn join(server: &Server, name: &str, prefix: &str) -> ClusterNode {
        ClusterNode::join_with(server, name, prefix, &[])
    }

    /// Joins with more flags.
    pub fn join_with(server: &Server, name: &str, prefix: &str, extra: &[&str]) -> ClusterNode {
        let mut n = ClusterNode {
            child: None,
            name: name.into(),
            server: server.url.clone(),
            ca_cert: server.ca_cert.clone(),
            extra: extra.iter().map(|s| s.to_string()).collect(),
            gateway: 0,
            ingress: None,
            prefix: prefix.into(),
            dir: tempfile::tempdir().unwrap(),
            volumes: tempfile::tempdir_in(btrfs_dir().expect("KIWI_NEST_TEST_BTRFS_DIR")).unwrap(),
        };
        n.spawn(Some(server.join_token()));
        n
    }

    /// The command that runs this node.
    fn command(&self, join_token: Option<String>) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.args(["node", "--server", &self.server, "--name", &self.name])
            .args(["--gateway-listen", "127.0.0.1:0", "--up-timeout", "2m"])
            .arg("--state-dir")
            .arg(self.dir.path().join("state"))
            .arg("--volumes-dir")
            .arg(self.volumes.path())
            .args(["--project-prefix", &self.prefix])
            .args(&self.extra)
            .env_remove("KIWI_NEST_CA_CERT")
            .env(
                "KIWI_NEST_LOG",
                std::env::var("KIWI_NEST_LOG").unwrap_or_else(|_| "warn".into()),
            )
            .env_remove("KIWI_NEST_TOKEN")
            .env_remove("KIWI_NEST_JOIN_TOKEN");
        if let Some(t) = join_token {
            cmd.args(["--join-token", &t]);
        }
        if let Some(ca) = &self.ca_cert {
            cmd.arg("--ca-cert").arg(ca);
        }
        cmd
    }

    fn spawn(&mut self, join_token: Option<String>) {
        let mut child = self.command(join_token).stdout(Stdio::piped()).spawn().unwrap();
        let ingress = self.extra.iter().any(|a| a == "--ingress-listen");
        let lines = first_lines(&mut child, if ingress { 3 } else { 2 }, "the node");
        let port = |line: &str, prefix: &str| -> u16 {
            line.strip_prefix(prefix)
                .and_then(|a| a.rsplit(':').next())
                .and_then(|p| p.parse().ok())
                .unwrap_or_else(|| panic!("the node should print `{prefix}ADDR`: {lines:?}"))
        };
        self.gateway = port(&lines[0], "gateway on http://");
        if ingress {
            self.ingress = Some(port(&lines[1], "ingress gateway on http://"));
        }
        assert!(
            lines
                .last()
                .unwrap()
                .starts_with(&format!("node {} polling", self.name)),
            "{lines:?}"
        );
        self.child = Some(child);
    }

    /// Runs the node until it exits by itself (it refuses to start, say), and returns what it printed.
    pub fn run_once(&self, join_token: Option<String>) -> Output {
        assert!(self.child.is_none());
        self.command(join_token).output().unwrap()
    }

    /// Waits for the running node to exit by itself.
    pub fn exits(&mut self) -> std::process::ExitStatus {
        let mut c = self.child.take().expect("a running node");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = c.try_wait().unwrap() {
                return status;
            }
            if Instant::now() > deadline {
                let _ = c.kill();
                panic!("node {} should have exited", self.name);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Starts the node with a fresh join token.
    pub fn join_again(&mut self, server: &Server) {
        assert!(self.child.is_none());
        self.spawn(Some(server.join_token()));
    }

    pub fn volumes_dir(&self) -> &Path {
        self.volumes.path()
    }

    /// Kills the node agent (SIGKILL), wherever it was. Its apps keep running.
    pub fn kill(&mut self) {
        let mut c = self.child.take().expect("a running node");
        let _ = c.kill();
        let _ = c.wait();
    }

    /// Starts the node again on its state.
    pub fn start(&mut self) {
        assert!(self.child.is_none());
        self.spawn(None);
    }

    pub fn state(&self, file: &str) -> String {
        std::fs::read_to_string(self.dir.path().join("state").join(file)).unwrap_or_default()
    }
}

impl Drop for ClusterNode {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        if std::env::var("KIWI_NEST_INTEGRATION").as_deref() == Ok("1") {
            remove_projects(&self.prefix);
        }
    }
}

/// A Python web app that takes two seconds to start listening, answers `/` with `hello`, and echoes bytes on a
/// WebSocket at `/ws`, at `domain`, with more of `x-kiwi-nest` in `extra` (such as `, auth: [...]`).
pub fn write_python_bundle(dir: &Path, domain: &str, idle_timeout: &str, extra: &str) {
    write_python_bundle_with(dir, domain, idle_timeout, extra, "python:3.13-alpine")
}

/// [`write_python_bundle`], running `image` (which must have Python).
pub fn write_python_bundle_with(dir: &Path, domain: &str, idle_timeout: &str, extra: &str, image: &str) {
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
            r#"x-kiwi-nest: {{ domain: {domain}, service: web, port: 8080, idle_timeout: {idle_timeout}{extra} }}
services:
  web:
    image: {image}
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
            uid_gid(),
        ),
    )
    .unwrap();
}

/// The Caddy binary, or `None` (and a note) if there isn't one.
pub fn caddy_bin() -> Option<PathBuf> {
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
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// Caddy, started with nothing but its admin API, as it would be by its own service; the server configures the
/// rest. Killed when dropped.
pub struct Caddy {
    bin: PathBuf,
    child: Option<Child>,
    pub admin: u16,
    pub dir: tempfile::TempDir,
}

impl Caddy {
    pub fn start(bin: PathBuf) -> Caddy {
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
    pub fn restart(&mut self) {
        self.kill();
        self.spawn();
    }

    pub fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    pub fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("caddy.log")).unwrap_or_default()
    }

    /// A bcrypt hash of `password`, from `caddy hash-password`.
    pub fn hash(&self, password: &str) -> String {
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

pub fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

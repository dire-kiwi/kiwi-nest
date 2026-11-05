//! End-to-end tests of a standalone node and the CLI, through the real binary.
//!
//! A node keeps its volumes on btrfs, so the tests that start one need `KIWI_NEST_TEST_BTRFS_DIR`, a directory on
//! btrfs (such as a loop-mounted file: see `scripts/test-btrfs.sh`). The tests that need Docker also need
//! `KIWI_NEST_INTEGRATION=1`. Each uses a random Compose project prefix, so concurrent runs don't collide, and
//! removes only the projects with its own prefix and the directories it made.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime};

const BIN: &str = env!("CARGO_BIN_EXE_kiwi-nest");

fn integration() -> bool {
    if std::env::var("KIWI_NEST_INTEGRATION").as_deref() == Ok("1") {
        true
    } else {
        eprintln!("skipped: set KIWI_NEST_INTEGRATION=1 to run the tests that need Docker");
        false
    }
}

/// The directory on btrfs for nodes' volumes, or `None` (and a note) if it isn't set.
fn btrfs_dir() -> Option<PathBuf> {
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
fn docker_and_btrfs() -> bool {
    integration() && btrfs_dir().is_some()
}

fn random_prefix() -> String {
    let n = RandomState::new().hash_one((std::process::id(), SystemTime::now()));
    format!("knit{:08x}-", n as u32)
}

/// Removes the Compose projects whose names start with a prefix.
fn remove_projects(prefix: &str) {
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
struct Node {
    child: Child,
    url: String,
    prefix: String,
    dir: tempfile::TempDir,
    volumes: tempfile::TempDir,
}

impl Node {
    fn start(prefix: &str) -> Node {
        let dir = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir_in(btrfs_dir().expect("KIWI_NEST_TEST_BTRFS_DIR")).unwrap();
        let mut child = Command::new(BIN)
            .args(["node", "--standalone", "--listen", "127.0.0.1:0", "--up-timeout", "2m"])
            .arg("--state-dir")
            .arg(dir.path().join("state"))
            .arg("--volumes-dir")
            .arg(volumes.path())
            .arg("--project-prefix")
            .arg(prefix)
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
            if let Some(Ok(line)) = lines.next() {
                let _ = tx.send(line);
            }
            for _ in lines {}
        });
        let line = rx.recv_timeout(Duration::from_secs(20)).expect("the node should start");
        let url = line
            .strip_prefix("listening on ")
            .expect("the node should print its address")
            .to_string();
        Node {
            child,
            url,
            prefix: prefix.to_string(),
            dir,
            volumes,
        }
    }

    /// A path in the node's volumes directory.
    fn volume(&self, path: &str) -> PathBuf {
        self.volumes.path().join(path)
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .arg("--node-api")
            .arg(&self.url)
            .args(args)
            .output()
            .unwrap()
    }

    /// Runs the CLI and returns its stdout, failing the test if it fails.
    fn ok(&self, args: &[&str]) -> String {
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
    fn fails(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(!out.status.success(), "kiwi-nest {args:?} should fail");
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn inspect(&self, app: &str) -> serde_json::Value {
        serde_json::from_str(&self.ok(&["inspect", app, "--json"])).unwrap()
    }
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
fn get(port: u64) -> String {
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

fn unreachable(port: u64) -> bool {
    TcpStream::connect(("127.0.0.1", port as u16)).is_err()
}

fn uid_gid() -> String {
    let out = Command::new("id").arg("-u").output().unwrap();
    let uid = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let out = Command::new("id").arg("-g").output().unwrap();
    let gid = String::from_utf8(out.stdout).unwrap().trim().to_string();
    format!("{uid}:{gid}")
}

/// A web service (busybox httpd serving `./site` from the bundle) and a database (Redis with a named volume).
///
/// The containers run as the current user, so the test can remove what they write.
fn write_two_service_bundle(dir: &Path, page: &str) {
    let user = uid_gid();
    std::fs::create_dir_all(dir.join("site")).unwrap();
    std::fs::write(dir.join("site/index.html"), page).unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        format!(
            r#"x-kiwi-nest:
  domain: shop.localhost
  service: web
  port: 8080
services:
  web:
    image: busybox:1.37
    command: ["httpd", "-f", "-p", "8080", "-h", "/www"]
    user: "{user}"
    init: true
    volumes: ["./site:/www"]
    healthcheck:
      test: ["CMD", "wget", "-q", "-O", "/dev/null", "http://127.0.0.1:8080/"]
      interval: 1s
      retries: 60
    restart: always
    depends_on:
      db: {{ condition: service_healthy }}
  db:
    image: redis:7-alpine
    user: "{user}"
    volumes: ["data:/data"]
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 1s
      retries: 60
    restart: always
volumes:
  data: {{}}
"#
        ),
    )
    .unwrap();
}

fn redis(project: &str, args: &[&str]) -> String {
    let out = Command::new("docker")
        .args(["ps", "-q", "--filter"])
        .arg(format!("label=com.docker.compose.project={project}"))
        .args(["--filter", "label=com.docker.compose.service=db"])
        .output()
        .unwrap();
    let id = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(!id.is_empty(), "the db container of {project} should be running");
    let out = Command::new("docker")
        .args(["exec", &id, "redis-cli"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "redis-cli {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

fn project_exists(project: &str) -> bool {
    let out = Command::new("docker")
        .args(["compose", "ls", "--all", "--format", "json"])
        .output()
        .unwrap();
    let projects: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    projects.iter().any(|p| p["Name"] == project)
}

#[test]
fn deploy_redeploy_and_delete_a_two_service_app() {
    if !docker_and_btrfs() {
        return;
    }
    let prefix = random_prefix();
    let node = Node::start(&prefix);
    let bundle = tempfile::tempdir().unwrap();
    write_two_service_bundle(bundle.path(), "version one");
    let b = bundle.path().to_str().unwrap();

    // Deploy, and reach the web service on its loopback port.
    let out = node.ok(&["deploy", "shop", b, "--create-only"]);
    assert!(out.contains("deployed shop (generation 1)"), "{out}");
    let info = node.inspect("shop");
    let port = info["port"].as_u64().unwrap();
    let project = info["project"].as_str().unwrap().to_string();
    assert_eq!(project, format!("{prefix}shop"));
    assert_eq!(info["status"], "running");
    assert_eq!(info["containers"].as_array().unwrap().len(), 2);
    assert_eq!(get(port), "version one");

    // Only the routed service is published, and only on loopback.
    let generated = std::fs::read_to_string(node.dir.path().join("state/apps/shop/compose-1.yaml")).unwrap();
    assert!(generated.contains("host_ip: 127.0.0.1"), "{generated}");
    assert_eq!(generated.matches("ports:").count(), 1, "{generated}");

    // Another app can't take the same domain.
    let err = node.fails(&["deploy", "shop-copy", b]);
    assert!(
        err.contains("domain `shop.localhost` is already used by app `shop`"),
        "{err}"
    );

    // Write some data, then redeploy a new version: the data stays, the new code is served.
    assert_eq!(redis(&project, &["SET", "greeting", "kia ora"]), "OK");
    assert_eq!(redis(&project, &["SAVE"]), "OK");
    let err = node.fails(&["deploy", "shop", b, "--create-only"]);
    assert!(err.contains("already exists"), "{err}");
    write_two_service_bundle(bundle.path(), "version two");
    let out = node.ok(&["deploy", "shop", b]);
    assert!(out.contains("generation 2"), "{out}");
    let info = node.inspect("shop");
    assert_eq!(info["port"].as_u64().unwrap(), port, "a redeploy keeps the app's port");
    assert_eq!(get(port), "version two");
    assert_eq!(redis(&project, &["GET", "greeting"]), "kia ora");
    assert!(
        !node.dir.path().join("state/apps/shop/bundle-1").exists(),
        "the old bundle is removed"
    );

    // List and logs.
    let list = node.ok(&["list"]);
    assert!(
        list.lines().any(|l| l.starts_with("shop ") && l.contains("running")),
        "{list}"
    );
    let logs = node.ok(&["logs", "shop", "--service", "db", "--tail", "50"]);
    assert!(logs.contains("Ready to accept connections"), "{logs}");
    let err = node.fails(&["logs", "shop", "--service", "cache"]);
    assert!(err.contains("no service `cache`"), "{err}");

    // Delete: the containers, the bundle and the volume go.
    node.ok(&["delete", "shop"]);
    assert!(!project_exists(&project), "the project should be gone");
    assert!(unreachable(port), "nothing should listen on {port} any more");
    assert!(!node.volume("apps/shop").exists());
    assert!(!node.dir.path().join("state/apps/shop").exists());
    let err = node.fails(&["inspect", "shop"]);
    assert!(err.contains("doesn't exist"), "{err}");
    assert!(node.ok(&["list"]).lines().count() == 1, "only the header is left");
}

fn write_web_bundle(dir: &Path, page: &str) {
    std::fs::create_dir_all(dir.join("site")).unwrap();
    std::fs::write(dir.join("site/index.html"), page).unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        format!(
            r#"x-kiwi-nest: {{ domain: same.localhost, service: web, port: 8080 }}
services:
  web:
    image: busybox:1.37
    command: ["httpd", "-f", "-p", "8080", "-h", "/www"]
    user: "{}"
    init: true
    volumes: ["./site:/www"]
"#,
            uid_gid()
        ),
    )
    .unwrap();
}

#[test]
fn two_nodes_share_one_engine() {
    if !docker_and_btrfs() {
        return;
    }
    let (pa, pb) = (random_prefix(), random_prefix());
    let (a, b) = (Node::start(&pa), Node::start(&pb));
    let (da, db) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write_web_bundle(da.path(), "on a");
    write_web_bundle(db.path(), "on b");

    // The same app name on both nodes is two separate projects.
    a.ok(&["deploy", "same", da.path().to_str().unwrap()]);
    b.ok(&["deploy", "same", db.path().to_str().unwrap(), "--node-reads-path"]);
    let (ia, ib) = (a.inspect("same"), b.inspect("same"));
    assert_ne!(ia["project"], ib["project"]);
    let (port_a, port_b) = (ia["port"].as_u64().unwrap(), ib["port"].as_u64().unwrap());
    assert_ne!(port_a, port_b);
    assert_eq!(get(port_a), "on a");
    assert_eq!(get(port_b), "on b");

    // Deleting one leaves the other alone.
    a.ok(&["delete", "same"]);
    assert!(!project_exists(&format!("{}same", a.prefix)));
    assert_eq!(get(port_b), "on b");
    b.ok(&["delete", "same"]);
    assert!(!project_exists(&format!("{}same", b.prefix)));
}

/// Doesn't need Docker or btrfs: the version, and the node's refusals at startup.
#[test]
fn version_and_refusals_at_startup() {
    let out = Command::new(BIN).arg("version").output().unwrap();
    let version = String::from_utf8(out.stdout).unwrap();
    assert!(
        version.starts_with(&format!("kiwi-nest {} (commit ", env!("CARGO_PKG_VERSION"))),
        "{version}"
    );

    // A non-loopback address is refused outright.
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(["node", "--standalone", "--listen", "0.0.0.0:0", "--state-dir"])
        .arg(dir.path().join("state"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("only listens on loopback"));

    // So is a volumes directory that isn't on btrfs (unless the temporary directory happens to be).
    let real = std::fs::canonicalize(dir.path()).unwrap();
    if matches!(kn_btrfs::filesystem(&real), Err(kn_btrfs::Error::NotBtrfs(_))) {
        let out = Command::new(BIN)
            .args(["node", "--standalone", "--listen", "127.0.0.1:0", "--state-dir"])
            .arg(real.join("state"))
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success());
        assert!(stderr.contains("isn't on btrfs"), "{stderr}");
    }
}

/// Doesn't need Docker: the refusals that happen before anything runs.
#[test]
fn refuses_bad_requests_without_docker() {
    if btrfs_dir().is_none() {
        return;
    }
    let node = Node::start(&random_prefix());
    let bundle = tempfile::tempdir().unwrap();
    let b = bundle.path().to_str().unwrap();
    std::fs::write(
        bundle.path().join("compose.yaml"),
        "services:\n  web:\n    image: x\n    ports: [\"80:80\"]\n    privileged: true\n",
    )
    .unwrap();
    let err = node.fails(&["deploy", "web", b]);
    assert!(err.contains("the bundle was refused"), "{err}");
    assert!(err.contains("services.web.ports"), "{err}");
    assert!(err.contains("services.web.privileged"), "{err}");
    assert!(err.contains("x-kiwi-nest: is required"), "{err}");

    std::os::unix::fs::symlink("/etc/passwd", bundle.path().join("passwd")).unwrap();
    let err = node.fails(&["deploy", "web", b, "--node-reads-path"]);
    assert!(err.contains("is a symlink"), "{err}");
    let err = node.fails(&["deploy", "web", b]);
    assert!(err.contains("is a symlink"), "{err}");
    std::fs::remove_file(bundle.path().join("passwd")).unwrap();

    let err = node.fails(&["deploy", "Web", b]);
    assert!(err.contains("isn't a valid app name"), "{err}");
    let err = node.fails(&["inspect", "nothing"]);
    assert!(err.contains("app `nothing` doesn't exist"), "{err}");
    let err = node.fails(&["delete", "nothing"]);
    assert!(err.contains("app `nothing` doesn't exist"), "{err}");

    // Requests a web page could make (DNS rebinding) are refused: another Host, or any Origin.
    let port: u16 = node.url.rsplit(':').next().unwrap().parse().unwrap();
    let raw = |request: &str| {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        resp
    };
    let resp = raw("DELETE /v1/apps/web HTTP/1.1\r\nHost: evil.example:7380\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 421"), "{resp}");
    let resp = raw("GET /v1/version HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 421"), "{resp}");
    let resp =
        raw("GET /v1/version HTTP/1.1\r\nHost: localhost\r\nOrigin: http://evil.example\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 403"), "{resp}");
    let resp = raw("GET /v1/version HTTP/1.1\r\nHost: localhost:1\r\nConnection: close\r\n\r\n");
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");

    // Shared volume refusals that need no app.
    let err = node.fails(&["volume", "delete", "nothing"]);
    assert!(err.contains("shared volume `nothing` doesn't exist"), "{err}");
    let err = node.fails(&["volume", "create", "_x"]);
    assert!(err.contains("isn't a valid volume name"), "{err}");
    node.ok(&["volume", "create", "corpus"]);
    let err = node.fails(&["volume", "create", "corpus"]);
    assert!(err.contains("already exists"), "{err}");
    let list = node.ok(&["volume", "list"]);
    assert!(list.lines().any(|l| l.starts_with("corpus")), "{list}");
    node.ok(&["volume", "delete", "corpus"]);
    assert!(!node.volume("shared/corpus").exists());
}

/// A web service and a Redis database that writes every change to its append-only file, so a snapshot taken at any
/// instant holds every write it acknowledged, as after a power cut.
fn write_database_bundle(dir: &Path, domain: &str) {
    let user = uid_gid();
    std::fs::create_dir_all(dir.join("site")).unwrap();
    std::fs::write(dir.join("site/index.html"), domain).unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        format!(
            r#"x-kiwi-nest: {{ domain: {domain}, service: web, port: 8080 }}
services:
  web:
    image: busybox:1.37
    command: ["httpd", "-f", "-p", "8080", "-h", "/www"]
    user: "{user}"
    init: true
    volumes: ["./site:/www"]
  db:
    image: redis:7-alpine
    command: ["redis-server", "--appendonly", "yes"]
    user: "{user}"
    volumes: ["data:/data"]
    healthcheck:
      test: ["CMD", "redis-cli", "ping"]
      interval: 1s
      retries: 60
volumes:
  data: {{}}
"#
        ),
    )
    .unwrap();
}

/// Runs `redis-cli ARGS` in an app's database, with `stdin` as its input (for `-x`).
fn redis_with_input(project: &str, args: &[&str], stdin: &[u8]) -> String {
    let out = Command::new("docker")
        .args(["ps", "-q", "--filter"])
        .arg(format!("label=com.docker.compose.project={project}"))
        .args(["--filter", "label=com.docker.compose.service=db"])
        .output()
        .unwrap();
    let id = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert!(!id.is_empty(), "the db container of {project} should be running");
    let mut child = Command::new("docker")
        .args(["exec", "-i", &id, "redis-cli"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "redis-cli {args:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// A megabyte of hex that doesn't compress to nothing.
fn blob() -> Vec<u8> {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    (0..1024 * 1024)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            b"0123456789abcdef"[(x & 15) as usize]
        })
        .collect()
}

/// `btrfs filesystem du -s --raw DIR`'s total and shared bytes, if `btrfs` is installed.
fn btrfs_du(dir: &Path) -> Option<(u64, u64)> {
    let out = Command::new("btrfs")
        .args(["filesystem", "du", "-s", "--raw"])
        .arg(dir)
        .output()
        .ok()?;
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let cols: Vec<u64> = text
        .lines()
        .nth(1)?
        .split_whitespace()
        .take(3)
        .map(|c| c.parse().unwrap())
        .collect();
    Some((cols[0], cols[2]))
}

#[test]
fn a_preview_starts_from_an_instant_snapshot_of_production() {
    if !docker_and_btrfs() {
        return;
    }
    let node = Node::start(&random_prefix());
    let (prod_b, preview_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    write_database_bundle(prod_b.path(), "shop.localhost");
    write_database_bundle(preview_b.path(), "shop-pr1.localhost");
    let (prod_b, preview_b) = (prod_b.path().to_str().unwrap(), preview_b.path().to_str().unwrap());

    // Production writes data, and doesn't save it explicitly: only its append-only file has it.
    node.ok(&["deploy", "shop", prod_b]);
    let prod = node.inspect("shop")["project"].as_str().unwrap().to_string();
    let blob = blob();
    assert_eq!(redis(&prod, &["SET", "greeting", "kia ora"]), "OK");
    assert_eq!(redis_with_input(&prod, &["-x", "SET", "blob"], &blob), "OK");

    // A preview from it sees the same data, at once.
    let out = node.ok(&["deploy", "shop-pr1", preview_b, "--from", "shop"]);
    assert!(out.contains("snapshotted volume `data` from `shop`"), "{out}");
    let info = node.inspect("shop-pr1");
    assert_eq!(info["from"], "shop");
    let preview = info["project"].as_str().unwrap().to_string();
    assert_eq!(redis(&preview, &["GET", "greeting"]), "kia ora");
    assert_eq!(redis(&preview, &["STRLEN", "blob"]), blob.len().to_string());
    // Production was unpaused.
    assert_eq!(redis(&prod, &["PING"]), "PONG");

    // The two share their extents on disk, then diverge.
    if let Some((total, shared)) = btrfs_du(&node.volume("apps")) {
        assert!(shared > 300_000, "shared {shared} of {total}");
    } else {
        eprintln!("note: `btrfs` isn't installed, so the shared extents weren't checked");
    }
    assert_eq!(redis(&preview, &["SET", "greeting", "preview"]), "OK");
    assert_eq!(redis(&prod, &["SET", "greeting", "production"]), "OK");
    assert_eq!(redis(&preview, &["GET", "greeting"]), "preview");
    assert_eq!(redis(&prod, &["GET", "greeting"]), "production");

    // Redeploying the preview keeps its data: only the first deploy snapshots.
    let out = node.ok(&["deploy", "shop-pr1", preview_b, "--from", "shop"]);
    assert!(out.contains("--from only applies to an app's first deploy"), "{out}");
    assert_eq!(redis(&preview, &["GET", "greeting"]), "preview");

    // A volume swapped for a symlink is refused at start, and the running generation stays.
    let data = node.volume("apps/shop-pr1/data");
    let aside = node.volume("apps/shop-pr1/data-aside");
    std::fs::rename(&data, &aside).unwrap();
    std::os::unix::fs::symlink(&aside, &data).unwrap();
    let err = node.fails(&["deploy", "shop-pr1", preview_b]);
    assert!(
        err.contains("refusing to start") && err.contains("is a symlink"),
        "{err}"
    );
    assert_eq!(redis(&preview, &["GET", "greeting"]), "preview");
    std::fs::remove_file(&data).unwrap();
    std::fs::rename(&aside, &data).unwrap();
    node.ok(&["deploy", "shop-pr1", preview_b]);

    // Deleting with --keep-data keeps the preview's data for its next deploy.
    node.ok(&["delete", "shop-pr1", "--keep-data"]);
    assert!(data.exists());
    let out = node.ok(&["deploy", "shop-pr1", preview_b, "--from", "shop"]);
    assert!(out.contains("kept volume `data` from before"), "{out}");
    assert_eq!(redis(&preview, &["GET", "greeting"]), "preview");
    node.ok(&["delete", "shop-pr1"]);
    assert!(!node.volume("apps/shop-pr1").exists());
    assert_eq!(redis(&prod, &["GET", "greeting"]), "production");
}

/// A web service that serves a shared volume, mounted in `mode`.
fn write_shared_bundle(dir: &Path, domain: &str, mode: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        format!(
            r#"x-kiwi-nest:
  domain: {domain}
  service: web
  port: 8080
  shared: {{ corpus: {{ target: /corpus, mode: {mode} }} }}
services:
  web:
    image: busybox:1.37
    command: ["httpd", "-f", "-p", "8080", "-h", "/corpus"]
    user: "{}"
    init: true
"#,
            uid_gid()
        ),
    )
    .unwrap();
}

#[test]
fn shared_volumes_and_the_delete_refusals() {
    if !docker_and_btrfs() {
        return;
    }
    let node = Node::start(&random_prefix());
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(data.join("index.html"), "corpus one").unwrap();
    let data = data.to_str().unwrap();
    node.ok(&["volume", "create", "corpus"]);
    node.ok(&["volume", "load", "corpus", data]);

    let (ro, snap) = (dir.path().join("ro"), dir.path().join("snap"));
    write_shared_bundle(&ro, "reader.localhost", "ro");
    write_shared_bundle(&snap, "snapper.localhost", "snapshot");
    node.ok(&["deploy", "reader", ro.to_str().unwrap()]);
    let out = node.ok(&["deploy", "snapper", snap.to_str().unwrap()]);
    assert!(out.contains("snapshotted shared volume `corpus`"), "{out}");
    let (reader_port, snapper_port) = (
        node.inspect("reader")["port"].as_u64().unwrap(),
        node.inspect("snapper")["port"].as_u64().unwrap(),
    );
    assert_eq!(get(reader_port), "corpus one");
    assert_eq!(get(snapper_port), "corpus one");
    let list = node.ok(&["volume", "list"]);
    assert!(list.contains("reader (ro), snapper (snapshot)"), "{list}");

    // Loading is refused while an app mounts it read-only; deleting while any app mounts it.
    let err = node.fails(&["volume", "load", "corpus", data]);
    assert!(err.contains("while app `reader` mounts it"), "{err}");
    let err = node.fails(&["volume", "delete", "corpus"]);
    assert!(err.contains("while app `reader`, `snapper` mounts it"), "{err}");
    // A snapshot can't be deleted while its app is deployed.
    let err = node.fails(&["volume", "delete-snapshot", "corpus", "snapper"]);
    assert!(err.contains("while the app is deployed"), "{err}");

    // Without the reader, a new load is allowed; the snapshot keeps what it had.
    node.ok(&["delete", "reader"]);
    std::fs::write(dir.path().join("data/index.html"), "corpus two").unwrap();
    node.ok(&["volume", "load", "corpus", data]);
    node.ok(&["deploy", "reader", ro.to_str().unwrap()]);
    let reader_port = node.inspect("reader")["port"].as_u64().unwrap();
    assert_eq!(get(reader_port), "corpus two");
    assert_eq!(get(snapper_port), "corpus one");

    // A kept snapshot still blocks deleting the volume, until it's deleted itself.
    node.ok(&["delete", "reader"]);
    node.ok(&["delete", "snapper", "--keep-data"]);
    let err = node.fails(&["volume", "delete", "corpus"]);
    assert!(err.contains("while snapshots of it exist, for app `snapper`"), "{err}");
    node.ok(&["volume", "delete-snapshot", "corpus", "snapper"]);
    node.ok(&["volume", "delete", "corpus"]);
    assert!(!node.volume("shared/corpus").exists());
}

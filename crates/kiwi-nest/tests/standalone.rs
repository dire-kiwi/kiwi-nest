//! End-to-end tests of a standalone node and the CLI, through the real binary.
//!
//! The tests that need Docker only run with `KIWI_NEST_INTEGRATION=1`. Each uses a random Compose project prefix,
//! so concurrent runs don't collide, and removes only the projects with its own prefix.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
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

fn random_prefix() -> String {
    let n = RandomState::new().hash_one((std::process::id(), SystemTime::now()));
    format!("knit{:08x}-", n as u32)
}

/// Removes the Compose projects whose names start with a prefix, when dropped.
struct Cleanup(String);

impl Drop for Cleanup {
    fn drop(&mut self) {
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
            if let Some(name) = p["Name"].as_str().filter(|n| n.starts_with(&self.0)) {
                eprintln!("cleaning up project {name}");
                let _ = Command::new("docker")
                    .args(["compose", "-p", name, "down", "--remove-orphans"])
                    .status();
            }
        }
    }
}

/// A standalone node running in the background, killed when dropped.
struct Node {
    child: Child,
    url: String,
    prefix: String,
    dir: tempfile::TempDir,
}

impl Node {
    fn start(prefix: &str) -> Node {
        let dir = tempfile::tempdir().unwrap();
        let mut child = Command::new(BIN)
            .args(["node", "--standalone", "--listen", "127.0.0.1:0", "--up-timeout", "2m"])
            .arg("--state-dir")
            .arg(dir.path().join("state"))
            .arg("--volumes-dir")
            .arg(dir.path().join("volumes"))
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
        }
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
    if !integration() {
        return;
    }
    let prefix = random_prefix();
    let _cleanup = Cleanup(prefix.clone());
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
    assert!(!node.dir.path().join("volumes/apps/shop").exists());
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
    if !integration() {
        return;
    }
    let (pa, pb) = (random_prefix(), random_prefix());
    let (_ca, _cb) = (Cleanup(pa.clone()), Cleanup(pb.clone()));
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

/// Doesn't need Docker: the version, and the refusals that happen before anything runs.
#[test]
fn refuses_bad_requests_without_docker() {
    let out = Command::new(BIN).arg("version").output().unwrap();
    let version = String::from_utf8(out.stdout).unwrap();
    assert!(
        version.starts_with(&format!("kiwi-nest {} (commit ", env!("CARGO_PKG_VERSION"))),
        "{version}"
    );

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

    // A non-loopback address is refused outright.
    let out = Command::new(BIN)
        .args(["node", "--standalone", "--listen", "0.0.0.0:0", "--state-dir"])
        .arg(bundle.path().join("state"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("only listens on loopback"));
}

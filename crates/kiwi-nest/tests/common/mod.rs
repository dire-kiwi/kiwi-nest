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
use std::net::TcpStream;
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

//! End-to-end tests of backups and restores: a server and nodes from the real binary, Docker, btrfs, and a backup
//! target in a local directory.
//!
//! They need `KIWI_NEST_INTEGRATION=1` and `KIWI_NEST_TEST_BTRFS_DIR`. The `send` format needs `btrfs send`, so
//! root: run as root, those test it too; otherwise they test `tar`.

mod common;

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::*;

fn is_root() -> bool {
    String::from_utf8(Command::new("id").arg("-u").output().unwrap().stdout)
        .unwrap()
        .trim()
        == "0"
}

/// A targets file with one target, `local`, in `store`.
fn targets(dir: &Path, store: &Path, format: &str) -> PathBuf {
    write_targets(dir, &format!("dir: {{ path: {} }}", store.display()), format)
}

fn write_targets(dir: &Path, storage: &str, format: &str) -> PathBuf {
    let path = dir.join("targets.yaml");
    std::fs::write(
        &path,
        format!("targets:\n  - name: local\n    {storage}\n    format: {format}\n    keep: {{ last: 5 }}\n"),
    )
    .unwrap();
    path
}

/// `KIWI_NEST_TEST_S3` (`ENDPOINT BUCKET ACCESS SECRET`) as a target's storage, under a prefix of its own.
fn s3_storage() -> Option<String> {
    let v = std::env::var("KIWI_NEST_TEST_S3").ok()?;
    let p: Vec<&str> = v.split_whitespace().collect();
    let [endpoint, bucket, access, secret] = p[..] else {
        panic!("KIWI_NEST_TEST_S3 should be 'ENDPOINT BUCKET ACCESS SECRET'")
    };
    Some(format!(
        "s3: {{ endpoint: '{endpoint}', bucket: {bucket}, region: us-east-1, access_key: {access}, secret_key: {secret} }}\n    prefix: {}",
        random_prefix().replace('-', "/")
    ))
}

/// A busybox web server at `domain` with a volume, mounting shared volume `corpus` read-only.
fn write_bundle(dir: &Path, domain: &str, page: &str) {
    let user = uid_gid();
    std::fs::create_dir_all(dir.join("site")).unwrap();
    std::fs::write(dir.join("site/index.html"), page).unwrap();
    std::fs::write(
        dir.join("compose.yaml"),
        format!(
            r#"x-kiwi-nest:
  domain: {domain}
  service: web
  port: 8080
  shared:
    corpus: {{ target: /corpus }}
services:
  web:
    image: busybox:1.37
    command: ["sh", "-c", "exec httpd -f -p 8080 -h /www"]
    user: "{user}"
    init: true
    volumes: ["./site:/www", "data:/data"]
    healthcheck:
      test: ["CMD", "wget", "-q", "-O", "/dev/null", "http://127.0.0.1:8080/"]
      interval: 1s
      retries: 60
volumes:
  data: {{}}
"#
        ),
    )
    .unwrap();
}

/// What a restore must keep of a tree: each entry's type, mode, owner, size, link target and contents.
fn describe(root: &Path) -> Vec<String> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let mut es: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap()).collect();
        es.sort_by_key(|e| e.file_name());
        for e in es {
            let p = e.path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            let rel = p.strip_prefix(root).unwrap().display().to_string();
            let what = if m.is_file() {
                String::from_utf8_lossy(&std::fs::read(&p).unwrap()).into_owned()
            } else if m.file_type().is_symlink() {
                std::fs::read_link(&p).unwrap().display().to_string()
            } else {
                String::new()
            };
            out.push(format!("{rel} {:o} {}:{} {what}", m.mode(), m.uid(), m.gid()));
            if m.is_dir() {
                go(root, &p, out);
            }
        }
    }
    let mut out = Vec::new();
    go(root, root, &mut out);
    out
}

fn fill(dir: &Path, note: &str) {
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("note"), note).unwrap();
    std::fs::write(dir.join("sub/secret"), "s").unwrap();
    std::fs::set_permissions(dir.join("sub/secret"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::set_permissions(dir.join("sub"), std::fs::Permissions::from_mode(0o750)).unwrap();
    let _ = std::fs::remove_file(dir.join("link"));
    std::os::unix::fs::symlink("note", dir.join("link")).unwrap();
}

fn backups(server: &Server, args: &[&str]) -> Vec<serde_json::Value> {
    let mut a = vec!["backup", "list"];
    a.extend_from_slice(args);
    a.push("--json");
    serde_json::from_str(&server.ok(&a)).unwrap()
}

/// Kills a node, waits for the server to see it offline, and forgets it.
fn lose(server: &Server, node: &mut ClusterNode) {
    node.kill();
    server.wait_online(&node.name, false);
    server.ok(&["node", "forget", &node.name, "--yes"]);
}

fn recover_a_lost_nest(format: &str, storage: Option<String>) {
    let work = tempfile::tempdir().unwrap();
    let store = work.path().join("store");
    let t = match &storage {
        Some(s) => write_targets(work.path(), s, format),
        None => targets(work.path(), &store, format),
    };
    let server = Server::start_with(&["--backup-targets", t.to_str().unwrap()]);
    let prefix = random_prefix();
    let mut a = ClusterNode::join(&server, "a", &format!("{prefix}a-"));
    let b = ClusterNode::join(&server, "b", &format!("{prefix}b-"));
    server.wait_online("a", true);
    server.wait_online("b", true);

    // A nest on node a: a shared volume, the app that mounts it, and a preview of the app.
    server.ok(&["volume", "create", "corpus", "--node", "a"]);
    fill(&a.volumes_dir().join("shared/corpus/data"), "words");
    let bundle = work.path().join("bundle");
    write_bundle(&bundle, "notes.localhost", "notes");
    let bp = bundle.to_str().unwrap();
    server.ok(&["deploy", "notes", bp]);
    let preview = work.path().join("preview");
    write_bundle(&preview, "notes-pr1.localhost", "preview");
    server.ok(&["deploy", "notes-pr1", preview.to_str().unwrap(), "--from", "notes"]);
    let data = a.volumes_dir().join("apps/notes/data");
    fill(&data, "one");

    // Backed up, changed, backed up again.
    let out = server.ok(&["backup", "volume", "corpus"]);
    assert!(out.contains("backed up to local: volumes/corpus/"), "{out}");
    let out = server.ok(&["backup", "app", "notes"]);
    assert!(out.contains("backed up to local: apps/notes/"), "{out}");
    std::thread::sleep(Duration::from_millis(1100));
    fill(&data, "two");
    let want = describe(&data);
    let out = server.ok(&["backup", "app", "notes"]);
    if format == "send" {
        assert!(out.contains(".after-") && out.contains("incremental"), "{out}");
    } else {
        assert!(out.contains(".tar.zst"), "{out}");
    }
    let list = backups(&server, &["app", "notes"]);
    assert_eq!(list.len(), 2, "{list:?}");
    let key = list.last().unwrap()["key"].as_str().unwrap().to_string();
    let vkey = backups(&server, &["volume"])[0]["key"].as_str().unwrap().to_string();
    let table = server.ok(&["backup", "list"]);
    assert!(table.contains(&key) && table.contains(&vkey), "{table}");

    // Node a is lost; the nest comes back on b, from the backups.
    lose(&server, &mut a);
    let out = server.ok(&["volume", "create", "corpus", "--restore", &vkey, "--node", "b"]);
    assert!(out.contains("on node b"), "{out}");
    let out = server.ok(&["deploy", "notes", bp, "--restore", &key]);
    assert!(out.contains("on node b"), "it joins the volume's nest: {out}");
    assert!(out.contains("restored `data`"), "{out}");
    let restored = b.volumes_dir().join("apps/notes/data");
    assert_eq!(describe(&restored), want);
    assert_eq!(
        std::fs::read_to_string(b.volumes_dir().join("shared/corpus/data/note")).unwrap(),
        "words"
    );
    let info = server.inspect("notes");
    assert_eq!(get(info["port"].as_u64().unwrap()), "notes");
    assert!(info["restored_from"].as_str().unwrap().ends_with(&key), "{info}");
    // The preview is made again from the restored app.
    let out = server.ok(&["deploy", "notes-pr1", preview.to_str().unwrap(), "--from", "notes"]);
    assert!(
        out.contains("on node b") && out.contains("snapshotted volume `data`"),
        "{out}"
    );
    assert_eq!(
        std::fs::read_to_string(b.volumes_dir().join("apps/notes-pr1/data/note")).unwrap(),
        "two"
    );

    if format == "tar" && storage.is_none() {
        // Plain tar, on a filesystem that isn't btrfs, gives the same files, owners and modes included.
        let plain = tempfile::tempdir().unwrap();
        let file = store.join(&key);
        let st = Command::new("tar")
            .args(["--zstd", "-xpf"])
            .arg(&file)
            .arg("-C")
            .arg(plain.path())
            .status()
            .unwrap();
        assert!(st.success());
        assert_eq!(describe(&plain.path().join("data")), want);
    }
}

#[test]
fn a_lost_nodes_nest_comes_back_from_tar_backups() {
    if !docker_and_btrfs() {
        return;
    }
    recover_a_lost_nest("tar", None);
}

#[test]
fn a_lost_nodes_nest_comes_back_from_an_s3_bucket() {
    if !docker_and_btrfs() {
        return;
    }
    let Some(s3) = s3_storage() else {
        eprintln!("skipped: set KIWI_NEST_TEST_S3 to 'ENDPOINT BUCKET ACCESS SECRET' to test against S3");
        return;
    };
    recover_a_lost_nest(if is_root() { "send" } else { "tar" }, Some(s3));
}

#[test]
fn a_lost_nodes_nest_comes_back_from_send_backups() {
    if !docker_and_btrfs() {
        return;
    }
    if !is_root() {
        assert!(
            std::env::var_os("KIWI_NEST_REQUIRE_ROOT").is_none(),
            "btrfs send needs root, and KIWI_NEST_REQUIRE_ROOT is set"
        );
        eprintln!("skipped: btrfs send needs root");
        return;
    }
    recover_a_lost_nest("send", None);
}

#[test]
fn the_server_comes_back_from_its_backup() {
    if !docker_and_btrfs() {
        return;
    }
    let work = tempfile::tempdir().unwrap();
    let store = work.path().join("store");
    let t = targets(work.path(), &store, "tar");
    let tp = t.to_str().unwrap();
    let mut server = Server::start_with(&["--backup-targets", tp]);
    let prefix = random_prefix();
    let a = ClusterNode::join(&server, "a", &format!("{prefix}a-"));
    server.wait_online("a", true);
    server.ok(&["volume", "create", "corpus"]);
    let bundle = work.path().join("bundle");
    write_bundle(&bundle, "notes.localhost", "notes");
    server.ok(&["deploy", "notes", bundle.to_str().unwrap()]);

    let out = server.ok(&["backup", "server"]);
    assert!(out.contains("backed up to local: server/"), "{out}");
    let key = backups(&server, &["server"])[0]["key"].as_str().unwrap().to_string();

    // The server's host is lost: its state comes back from the backup, on the same address.
    let addr = server.url.trim_start_matches("http://").to_string();
    server.kill();
    let fresh = tempfile::tempdir().unwrap();
    let state = fresh.path().join("server");
    let out = Command::new(BIN)
        .args(["server", "restore", &key, "--backup-targets", tp])
        .arg("--state-dir")
        .arg(&state)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.contains("restored the server's state"), "{said}");
    // Again into the same directory: refused.
    let again = Command::new(BIN)
        .args(["server", "restore", &key, "--backup-targets", tp])
        .arg("--state-dir")
        .arg(&state)
        .output()
        .unwrap();
    assert!(!again.status.success());

    let restored = Server::start_restored(fresh, &server.admin, &addr, &["--backup-targets", tp]);
    // The node reconnects with its token, and everything is as it was.
    restored.wait_online("a", true);
    let info = restored.inspect("notes");
    assert_eq!(info["node"], "a");
    assert_eq!(get(info["port"].as_u64().unwrap()), "notes");
    assert!(restored.ok(&["volume", "list"]).contains("corpus"));
    let deadline = Instant::now() + Duration::from_secs(30);
    while restored.inspect("notes")["stale"] == true {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(200));
    }
    drop(a);
}

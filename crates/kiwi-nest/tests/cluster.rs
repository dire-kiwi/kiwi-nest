//! End-to-end tests of a cluster: a server and nodes run from the real binary, on this machine, sharing the Docker
//! Engine with their own project prefixes, and the CLI going through the server.
//!
//! Nodes keep their volumes on btrfs, so these need `KIWI_NEST_TEST_BTRFS_DIR`; the ones that run apps also need
//! `KIWI_NEST_INTEGRATION=1` (Docker).

mod common;

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;

/// One web service (busybox httpd serving `./site`) at `domain`, with a volume. A slow one takes a few seconds to
/// become healthy, so a deploy spends that long in `docker compose up`.
fn write_bundle(dir: &Path, domain: &str, page: &str, slow: bool) {
    let user = uid_gid();
    std::fs::create_dir_all(dir.join("site")).unwrap();
    std::fs::write(dir.join("site/index.html"), page).unwrap();
    let delay = if slow { "sleep 6; " } else { "" };
    std::fs::write(
        dir.join("compose.yaml"),
        format!(
            r#"x-kiwi-nest:
  domain: {domain}
  service: web
  port: 8080
services:
  web:
    image: busybox:1.37
    command: ["sh", "-c", "{delay}exec httpd -f -p 8080 -h /www"]
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

fn container_id(project: &str) -> String {
    let out = Command::new("docker")
        .args(["ps", "-q", "--no-trunc", "--filter"])
        .arg(format!("label=com.docker.compose.project={project}"))
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

#[test]
fn two_nodes_join_and_report_domains_are_unique_and_deploy_tokens_are_limited() {
    if !docker_and_btrfs() {
        return;
    }
    let mut server = Server::start();
    let prefix = random_prefix();
    let a = ClusterNode::join(&server, "a", &format!("{prefix}a-"));
    let b = ClusterNode::join(&server, "b", &format!("{prefix}b-"));
    server.wait_online("a", true);
    server.wait_online("b", true);
    let nodes = server.nodes();
    assert_eq!(nodes.len(), 2);
    for n in &nodes {
        let f = &n["facts"];
        assert!(f["memory_bytes"].as_u64().unwrap() > 0, "{n}");
        assert!(
            f["volumes"]["unallocated_bytes"].as_u64().is_some(),
            "btrfs space is reported: {n}"
        );
        assert_eq!(f["version"], env!("CARGO_PKG_VERSION"));
    }
    assert!(a.state("agent.json").contains("\"token\""));

    let bundle = tempfile::tempdir().unwrap();
    let p = bundle.path().to_str().unwrap();
    write_bundle(bundle.path(), "blog.localhost", "blog one", false);
    let out = server.ok(&["deploy", "blog", p, "--node", "a"]);
    assert!(out.contains("deployed blog (generation 1) on node a"), "{out}");
    let info = server.inspect("blog");
    assert_eq!(info["node"], "a");
    assert_eq!(get(info["port"].as_u64().unwrap()), "blog one");

    // The domain is taken across the cluster.
    let err = server.fails(&["deploy", "other", p, "--node", "b"]);
    assert!(
        err.contains("domain `blog.localhost` is already used by app `blog` on node `a`"),
        "{err}"
    );
    let list = server.ok(&["list"]);
    assert!(!list.contains("other"), "{list}");

    // A deploy token for previews can't touch blog, but can preview it.
    let ci = server
        .ok(&[
            "token",
            "create",
            "deploy",
            "--pattern",
            "blog-pr*",
            "--from",
            "blog",
            "--name",
            "ci",
        ])
        .trim()
        .to_string();
    for args in [
        vec!["deploy", "blog", p],
        vec!["delete", "blog"],
        vec!["suspend", "blog"],
        vec!["logs", "blog"],
        vec!["inspect", "blog"],
    ] {
        let err = server.fails_as(&ci, &args);
        assert!(err.contains("can't manage app `blog`"), "{args:?}: {err}");
    }
    let preview = tempfile::tempdir().unwrap();
    write_bundle(preview.path(), "blog-pr1.localhost", "preview", false);
    let pp = preview.path().to_str().unwrap();
    let out = server.ok_as(&ci, &["deploy", "blog-pr1", pp, "--from", "blog"]);
    assert!(
        out.contains("on node a"),
        "a preview goes to its production's node: {out}"
    );
    assert!(out.contains("snapshotted volume `data` from `blog`"), "{out}");
    let list = server.ok_as(&ci, &["list"]);
    assert!(
        !list.lines().any(|l| l.starts_with("blog ")),
        "the token only sees its own apps: {list}"
    );
    assert!(
        list.lines().any(|l| l.starts_with("blog-pr1 ") && l.contains(" blog ")),
        "the preview is in production's nest: {list}"
    );

    // Restarting the server loses nothing: apps, tokens, nodes.
    server.restart();
    server.wait_online("a", true);
    server.wait_online("b", true);
    let list = server.ok(&["list"]);
    assert!(
        list.lines().any(|l| l.starts_with("blog ") && l.contains("running")),
        "{list}"
    );
    assert!(list.lines().any(|l| l.starts_with("blog-pr1 ")), "{list}");
    write_bundle(preview.path(), "blog-pr1.localhost", "preview two", false);
    let out = server.ok_as(&ci, &["deploy", "blog-pr1", pp]);
    assert!(out.contains("generation 2"), "{out}");
    assert_eq!(get(server.inspect("blog-pr1")["port"].as_u64().unwrap()), "preview two");
    server.ok_as(&ci, &["delete", "blog-pr1"]);
    server.ok(&["delete", "blog"]);
    assert_eq!(server.bundles(), 0);
    drop(b);
}

#[test]
fn a_node_killed_mid_deploy_finishes_that_deploy_once_when_it_comes_back() {
    if !docker_and_btrfs() {
        return;
    }
    let server = Server::start();
    let prefix = random_prefix();
    let mut a = ClusterNode::join(&server, "a", &prefix);
    server.wait_online("a", true);
    let bundle = tempfile::tempdir().unwrap();
    write_bundle(bundle.path(), "slow.localhost", "slow", true);

    // The deploy waits on the node, which is killed while it's in `docker compose up`.
    let deploy = Command::new(BIN)
        .args(["deploy", "slow", bundle.path().to_str().unwrap()])
        .env("KIWI_NEST_SERVER", &server.url)
        .env("KIWI_NEST_TOKEN", &server.admin)
        .env("KIWI_NEST_CONFIG_DIR", server.config.path())
        .env_remove("KIWI_NEST_NODE_API")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !a.state("state.json").contains("\"deploying\"") || container_id(&format!("{prefix}slow")).is_empty() {
        assert!(Instant::now() < deadline, "the node should start deploying");
        std::thread::sleep(Duration::from_millis(100));
    }
    a.kill();
    assert!(
        !a.state("agent.json").contains("\"slow\""),
        "the cut-off deploy isn't recorded as applied"
    );
    server.wait_online("a", false);
    let info = server.inspect("slow");
    assert_eq!(info["pending"], 1, "{info}");
    // Down for a while, as the waiting CLI notices.
    std::thread::sleep(Duration::from_secs(2));

    // Back again: it finishes the deploy, and the CLI that was waiting sees it.
    a.start();
    let out = deploy.wait_with_output().unwrap();
    let (stdout, stderr) = (
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
    assert!(stdout.contains("deployed slow (generation 1) on node a"), "{stdout}");
    assert!(stderr.contains("offline; waiting for it"), "{stderr}");
    let info = server.inspect("slow");
    assert_eq!(
        (info["status"].as_str(), info["generation"].as_u64()),
        (Some("running"), Some(1))
    );
    assert_eq!(get(info["port"].as_u64().unwrap()), "slow");
    let id = container_id(&format!("{prefix}slow"));

    // And only once: another restart leaves the app as it is.
    a.kill();
    a.start();
    server.wait_online("a", true);
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(
        container_id(&format!("{prefix}slow")),
        id,
        "the deploy didn't run again"
    );
    assert_eq!(server.inspect("slow")["generation"], 1);
}

/// Needs btrfs (to start a node), not Docker: the refusal happens before anything runs.
#[test]
fn a_deploy_to_an_offline_node_is_refused_and_saves_nothing() {
    if btrfs_dir().is_none() {
        return;
    }
    let server = Server::start();
    let prefix = random_prefix();
    let mut a = ClusterNode::join(&server, "a", &prefix);
    server.wait_online("a", true);
    a.kill();
    server.wait_online("a", false);

    let bundle = tempfile::tempdir().unwrap();
    write_bundle(bundle.path(), "blog.localhost", "blog", false);
    let p = bundle.path().to_str().unwrap();
    let err = server.fails(&["deploy", "blog", p, "--node", "a"]);
    assert!(err.contains("node `a` is offline, so the deploy wasn't saved"), "{err}");
    let err = server.fails(&["deploy", "blog", p]);
    assert!(err.contains("no node is online"), "{err}");
    let list = server.ok(&["list", "--json"]);
    assert_eq!(list.trim(), "[]");
    assert_eq!(server.bundles(), 0);
    assert_eq!(server.nodes()[0]["apps"], 0);

    // Queued, it's saved for when the node is back.
    let out = server.ok(&["deploy", "blog", p, "--node", "a", "--queue"]);
    assert!(out.contains("queued blog (generation 1) for node a"), "{out}");
    assert_eq!(server.bundles(), 1);
    let list = server.ok(&["list"]);
    assert!(list.contains("pending (gen 1)"), "{list}");
    if integration() {
        a.start();
        let deadline = Instant::now() + Duration::from_secs(60);
        while server.inspect("blog")["status"] != "running" {
            assert!(Instant::now() < deadline, "the queued deploy should be applied");
            std::thread::sleep(Duration::from_millis(250));
        }
    }
}

#[test]
fn a_forgotten_node_never_runs_its_apps_again() {
    if !docker_and_btrfs() {
        return;
    }
    let mut server = Server::start();
    let prefix = random_prefix();
    let mut a = ClusterNode::join(&server, "a", &format!("{prefix}a-"));
    let b = ClusterNode::join(&server, "b", &format!("{prefix}b-"));
    server.wait_online("a", true);
    server.wait_online("b", true);
    let bundle = tempfile::tempdir().unwrap();
    let p = bundle.path().to_str().unwrap();
    write_bundle(bundle.path(), "blog.localhost", "blog", false);
    let out = server.ok(&["deploy", "blog", p, "--node", "a"]);
    assert!(out.contains("on node a"), "{out}");
    let info = server.inspect("blog");
    assert_eq!(info["nest"], "blog");
    assert!(
        info["placement"]
            .as_str()
            .unwrap()
            .contains("went to node a when `blog` was deployed: --node a"),
        "{info}"
    );
    let nests = server.ok(&["nest", "list"]);
    assert!(
        nests.lines().any(|l| l.starts_with("blog ") && l.contains(" a ")),
        "{nests}"
    );
    let data = a.volumes_dir().join("apps/blog/data");
    std::fs::write(data.join("posts"), "the only copy").unwrap();
    let old = container_id(&format!("{prefix}a-blog"));
    assert!(!old.is_empty());

    // A plain outage takes nothing down.
    server.down_for(Duration::from_secs(5));
    server.wait_online("a", true);
    assert_eq!(
        container_id(&format!("{prefix}a-blog")),
        old,
        "the outage changed nothing"
    );
    assert!(!a.state("agent.json").contains("forgotten"));

    // a is lost; it's refused while online, and then forgotten.
    let err = server.fails(&["node", "forget", "a", "--yes"]);
    assert!(err.contains("is online"), "{err}");
    a.kill();
    server.wait_online("a", false);
    let err = server.fails(&["node", "forget", "a"]);
    assert!(err.contains("Make sure node a is really gone first"), "{err}");
    assert!(err.contains("pass --yes"), "{err}");
    let out = server.ok(&["node", "forget", "a", "--yes"]);
    assert!(
        out.contains("forgot node a") && out.contains("released apps: blog"),
        "{out}"
    );
    // Cut off from the server, its always-on app still runs (which is why you make sure the node is gone).
    assert_eq!(container_id(&format!("{prefix}a-blog")), old);

    // The app is deployed again on b.
    let out = server.ok(&["deploy", "blog", p]);
    assert!(out.contains("deployed blog (generation 2) on node b"), "{out}");
    let port = server.inspect("blog")["port"].as_u64().unwrap();
    assert_eq!(get(port), "blog");

    // a comes back: it takes its app down, sets its data aside (not deleted) and stops.
    a.start();
    assert!(!a.exits().success());
    assert!(
        container_id(&format!("{prefix}a-blog")).is_empty(),
        "a's containers are removed"
    );
    let aside: Vec<_> = std::fs::read_dir(a.volumes_dir().join("forgotten"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(aside.len(), 1, "{aside:?}");
    assert_eq!(
        std::fs::read_to_string(aside[0].join("apps/blog/data/posts")).unwrap(),
        "the only copy"
    );
    assert!(aside[0].join("state.json").exists());
    // Its agent won't start again without a fresh join token.
    let out = a.run_once(None);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("forgot this node") && stderr.contains("fresh join token"),
        "{stderr}"
    );
    // The app only ever runs on b.
    assert!(container_id(&format!("{prefix}a-blog")).is_empty());
    assert!(!container_id(&format!("{prefix}b-blog")).is_empty());
    assert_eq!(server.inspect("blog")["node"], "b");

    // With a fresh join token, it's a new node, with nothing on it.
    a.join_again(&server);
    server.wait_online("a", true);
    let n = server.nodes().into_iter().find(|n| n["name"] == "a").unwrap();
    assert_eq!(n["apps"], 0, "{n}");
    std::thread::sleep(Duration::from_secs(2));
    assert!(container_id(&format!("{prefix}a-blog")).is_empty());
    drop(b);
}

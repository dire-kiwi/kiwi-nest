//! End-to-end test of the image registry: a server running `registry:3` behind real Caddy, two nodes pulling from
//! it with their own credentials, `kiwi-nest push`, garbage collection and pruning.
//!
//! It needs `KIWI_NEST_INTEGRATION=1`, `KIWI_NEST_TEST_BTRFS_DIR` (see `common`) and Caddy (`KIWI_NEST_TEST_CADDY`,
//! or `caddy` on the PATH). The ingress serves plain HTTP on a port of its own, and the registry is at `localhost`
//! on that port, which Docker reaches without TLS, as it does any registry on loopback. The registry's container
//! has a name of the test's own, and is removed at the end, as are the images the test tagged.

mod common;

use std::process::Command;
use std::time::Duration;

use base64::Engine;
use common::*;

/// What a client accepts as a manifest.
const MANIFESTS: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, \
                         application/vnd.docker.distribution.manifest.list.v2+json, \
                         application/vnd.docker.distribution.manifest.v2+json";

/// Removes the registry's container and the test's images (those whose names start with one of `images`) when
/// dropped.
struct Cleanup {
    container: String,
    images: Vec<String>,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = Command::new("docker").args(["rm", "-f", &self.container]).output();
        if let Ok(out) = Command::new("docker")
            .args(["image", "ls", "--format", "{{.Repository}}:{{.Tag}}"])
            .output()
        {
            for image in String::from_utf8_lossy(&out.stdout).lines() {
                if self.images.iter().any(|p| image.starts_with(p)) {
                    let _ = Command::new("docker").args(["image", "rm", image]).output();
                }
            }
        }
    }
}

fn docker(args: &[&str]) -> std::process::Output {
    Command::new("docker").args(args).output().unwrap()
}

fn docker_ok(args: &[&str]) -> String {
    let out = docker(args);
    assert!(
        out.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn has_image(reference: &str) -> bool {
    docker(&["image", "inspect", reference]).status.success()
}

/// Pulls an image from Docker Hub, unless it's here already.
fn pulled(reference: &str) -> &str {
    if !has_image(reference) {
        docker_ok(&["pull", "--quiet", reference]);
    }
    reference
}

/// The `user:password` a node's Docker configuration holds for `host`.
fn node_login(node: &ClusterNode, host: &str) -> String {
    let config: serde_json::Value =
        serde_json::from_str(&node.state("docker/config.json")).expect("the node's Docker configuration");
    let auth = config["auths"][host]["auth"]
        .as_str()
        .expect("a login for the registry");
    String::from_utf8(base64::engine::general_purpose::STANDARD.decode(auth).unwrap()).unwrap()
}

fn deploy(server: &Server, name: &str, node: &str, image: &str) {
    let bundle = tempfile::tempdir().unwrap();
    write_python_bundle_with(bundle.path(), &format!("{name}.localhost"), "0", "", image);
    let out = server.ok(&["deploy", name, bundle.path().to_str().unwrap(), "--node", node]);
    assert!(out.contains(&format!("on node {node}")), "{out}");
}

fn gc(server: &Server, args: &[&str]) -> serde_json::Value {
    let mut all = vec!["registry", "gc", "--json"];
    all.extend_from_slice(args);
    serde_json::from_str(&server.ok(&all)).unwrap()
}

fn why(report: &serde_json::Value, image: &str) -> String {
    report["kept"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| format!("{}:{}", t["repository"].as_str().unwrap(), t["tag"].as_str().unwrap()) == image)
        .and_then(|t| t["why"].as_str())
        .unwrap_or_else(|| panic!("{image} should be kept: {report}"))
        .to_string()
}

fn tags(registry: u16, repository: &str) -> Vec<String> {
    let out = Command::new("curl")
        .args([
            "-fsS",
            "--max-time",
            "30",
            &format!("http://127.0.0.1:{registry}/v2/{repository}/tags/list"),
        ])
        .output()
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let mut tags: Vec<String> = v["tags"]
        .as_array()
        .map(|a| a.iter().map(|t| t.as_str().unwrap().to_string()).collect())
        .unwrap_or_default();
    tags.sort();
    tags
}

/// What the ingress answers a request to the registry with this login.
fn status(http: u16, host: &str, login: &str, method: &str, path: &str) -> u16 {
    let mut c = Command::new("curl");
    c.args([
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "-u",
        login,
        "--max-time",
        "30",
    ]);
    if method == "HEAD" {
        c.arg("--head");
    } else {
        c.args(["-X", method]);
    }
    let out = c
        .args(["-H", &format!("Host: {host}"), "-H", &format!("Accept: {MANIFESTS}")])
        .arg(format!("http://127.0.0.1:{http}{path}"))
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).parse().unwrap_or(0)
}

#[test]
fn images_are_pushed_once_pulled_by_every_node_and_collected_when_nothing_uses_them() {
    if !docker_and_btrfs() {
        return;
    }
    let Some(bin) = caddy_bin() else { return };
    let caddy = Caddy::start(bin);
    let prefix = random_prefix();
    let (http, registry) = (free_port(), free_port());
    let host = format!("localhost:{http}");
    let container = format!("{prefix}registry");
    let _cleanup = Cleanup {
        container: container.clone(),
        images: vec![format!("{host}/"), prefix.clone()],
    };
    let mut server = Server::start_with(&[
        "--ingress",
        "http",
        "--caddy-admin",
        &format!("http://127.0.0.1:{}", caddy.admin),
        "--http-port",
        &http.to_string(),
        "--https-port",
        &free_port().to_string(),
        "--ingress-sync",
        "1s",
        "--registry-domain",
        "localhost",
        "--registry-listen",
        &format!("127.0.0.1:{registry}"),
        "--registry-container",
        &container,
    ]);
    // The CLI's own Docker configuration, so the test never logs the user's Docker in anywhere.
    let docker_config = tempfile::tempdir().unwrap();
    server
        .env
        .push(("DOCKER_CONFIG".into(), docker_config.path().to_path_buf()));
    let admin = format!("kiwi-nest:{}", server.admin);
    wait_for(
        "the registry answers through the ingress",
        Duration::from_secs(120),
        || status(http, &host, &admin, "GET", "/v2/") == 200,
    );
    assert_eq!(server.ok(&["registry", "host"]).trim(), host);
    let a = ClusterNode::join_with(&server, "a", &format!("{prefix}a-"), &["--prune-interval", "0"]);
    let _b = ClusterNode::join_with(&server, "b", &format!("{prefix}b-"), &["--prune-interval", "0"]);
    server.wait_online("a", true);
    server.wait_online("b", true);

    // An image of the test's own (so no other tag shares it), pushed once with the CLI's admin token, then deployed
    // on both nodes, which pull it with their own credentials: the local tags are removed first, so they have to.
    let context = tempfile::tempdir().unwrap();
    std::fs::write(
        context.path().join("Dockerfile"),
        format!("FROM {}\nLABEL kiwi-nest-test={prefix}\n", pulled("python:3.13-alpine")),
    )
    .unwrap();
    let local = format!("{prefix}app:dev");
    docker_ok(&["build", "--quiet", "--tag", &local, context.path().to_str().unwrap()]);
    let image = format!("{host}/app:1");
    let out = server.ok(&["push", &local, "app:1"]);
    assert_eq!(out.trim(), image);
    docker_ok(&["image", "rm", &local, &image]);
    assert!(!has_image(&image));
    deploy(&server, "blog-a", "a", &image);
    deploy(&server, "blog-b", "b", &image);
    assert!(has_image(&image), "the node pulled it");
    for n in ["blog-a", "blog-b"] {
        assert_eq!(server.inspect(n)["status"], "running");
    }

    // A node's credential may pull, but not push.
    let login = node_login(&a, &host);
    assert!(login.starts_with("node-a:knr_"), "{login}");
    assert_eq!(status(http, &host, &login, "HEAD", "/v2/app/manifests/1"), 200);
    assert_eq!(status(http, &host, &login, "POST", "/v2/app/blobs/uploads/"), 403);
    assert_eq!(status(http, &host, &login, "PUT", "/v2/app/manifests/2"), 403);
    let node_docker = a.dir.path().join("state/docker");
    let pushed = format!("{host}/app:from-a-node");
    docker_ok(&["tag", pulled("busybox:1.37"), &pushed]);
    let out = Command::new("docker")
        .env("DOCKER_CONFIG", &node_docker)
        .args(["push", &pushed])
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!out.status.success(), "a node's credential pushed: {text}");
    assert!(text.contains("may only pull"), "{text}");
    assert_eq!(tags(registry, "app"), vec!["1"]);

    // Something nothing uses, to collect.
    assert_eq!(
        server.ok(&["push", "busybox:1.37", "old:1"]).trim(),
        format!("{host}/old:1")
    );

    // Only node b uses app:1 now; without a grace period, old:1 goes and app:1 stays.
    server.ok(&["delete", "blog-a"]);
    let r = gc(&server, &["--grace", "0"]);
    assert_eq!(why(&r, "app:1"), "used by app blog-b on node b");
    let deleted: Vec<String> = r["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| format!("{}:{}", t["repository"].as_str().unwrap(), t["tag"].as_str().unwrap()))
        .collect();
    assert_eq!(deleted, vec!["old:1"]);
    assert_eq!(r["collected"], true, "{r}");
    assert!(tags(registry, "old").is_empty());
    // The registry was stopped to collect, and started again.
    assert_eq!(status(http, &host, &admin, "HEAD", "/v2/app/manifests/1"), 200);
    // Node a stopped using app:1, but b's container still does, so it stays.
    let p: serde_json::Value =
        serde_json::from_str(&server.ok(&["node", "prune", "a", "--grace", "0", "--json"])).unwrap();
    assert!(p["removed"].as_array().unwrap().is_empty(), "{p}");
    assert!(has_image(&image));

    // Deleted, then deployed again within the grace period: nothing is pulled, so it works with the registry
    // down.
    server.ok(&["delete", "blog-b"]);
    let r = gc(&server, &[]);
    assert_eq!(why(&r, "app:1"), "released just now");
    let p: serde_json::Value = serde_json::from_str(&server.ok(&["node", "prune", "b", "--json"])).unwrap();
    assert!(p["removed"].as_array().unwrap().is_empty(), "{p}");
    assert!(has_image(&image));
    docker_ok(&["stop", &container]);
    deploy(&server, "blog-b", "b", &image);
    docker_ok(&["start", &container]);
    wait_for("the registry is back", Duration::from_secs(60), || {
        status(http, &host, &admin, "GET", "/v2/") == 200
    });

    // Once the grace period is over and nothing uses it, the node removes it, and the next deploy pulls it again.
    server.ok(&["delete", "blog-b"]);
    let p: serde_json::Value =
        serde_json::from_str(&server.ok(&["node", "prune", "b", "--grace", "0", "--json"])).unwrap();
    assert_eq!(p["removed"][0], image.as_str(), "{p}");
    assert!(!has_image(&image));
    deploy(&server, "blog-b", "b", &image);
    assert!(has_image(&image));
}

//! The agent against a real server, in this process, with the fake runtime and the fake subvolume backend.
//!
//! Each node runs on a tokio runtime of its own, so a test can kill one (dropping every task it has, wherever it
//! was) and start it again on the same state, as a node restart would. The fake runtime stands for Docker, so it
//! outlives the node.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kn_proto::api::{Accepted, AppInfo, AppSummary, ChangeStatus, CreatedToken, ErrorBody, NodeInfo};
use kn_proto::state::AppStatus;
use reqwest::{Method, StatusCode};
use serde_json::json;

use super::*;
use crate::NodeConfig;
use crate::testing::{Behaviour, Fake};

struct Cluster {
    dir: tempfile::TempDir,
    url: String,
    admin: String,
    http: reqwest::Client,
}

impl Cluster {
    async fn start() -> Cluster {
        install_tls();
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = kn_server::ServerConfig::new(dir.path().join("server"));
        cfg.poll_timeout = Duration::from_secs(1);
        cfg.offline_after = Duration::from_millis(1500);
        cfg.command_timeout = Duration::from_secs(20);
        let (server, admin) = kn_server::Server::open(cfg).await.unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(kn_server::serve(listener, server, std::future::pending()));
        Cluster {
            dir,
            url,
            admin: admin.unwrap(),
            http: reqwest::Client::new(),
        }
    }

    fn req(&self, method: Method, path: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{}{path}", self.url, kn_proto::API_PREFIX))
            .bearer_auth(&self.admin)
    }

    async fn join_token(&self) -> String {
        let r = self
            .req(Method::POST, "/tokens")
            .json(&json!({"kind": "join"}))
            .send()
            .await
            .unwrap();
        r.json::<CreatedToken>().await.unwrap().token
    }

    /// A node, not started yet, with its own directories, fake Docker and fake subvolumes.
    fn node(&self, name: &str, ports: std::ops::RangeInclusive<u16>) -> Node {
        let dir = self.dir.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        Node {
            name: name.into(),
            server: self.url.clone(),
            dir,
            ports,
            fake: Fake::new(),
            volumes: Arc::new(crate::volumes::fake::Fake::default()),
            rt: None,
        }
    }

    async fn nodes(&self) -> Vec<NodeInfo> {
        self.req(Method::GET, "/nodes")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    /// Waits until `pred` holds for the nodes.
    async fn until_nodes(&self, what: &str, pred: impl Fn(&[NodeInfo]) -> bool) -> Vec<NodeInfo> {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let nodes = self.nodes().await;
            if pred(&nodes) {
                return nodes;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}: {nodes:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn online(&self, name: &str, online: bool) {
        self.until_nodes(&format!("{name} online: {online}"), |ns| {
            ns.iter().any(|n| n.name == name && n.online == online)
        })
        .await;
    }

    async fn deploy(&self, name: &str, yaml: &str, query: &str) -> reqwest::Response {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compose.yaml"), yaml).unwrap();
        let zip = kn_bundle::dir::pack(dir.path(), &kn_bundle::Limits::default()).unwrap();
        self.req(Method::PUT, &format!("/apps/{name}?{query}"))
            .header("content-type", "application/zip")
            .body(zip)
            .send()
            .await
            .unwrap()
    }

    async fn deployed(&self, name: &str, yaml: &str, query: &str) -> Accepted {
        let r = self.deploy(name, yaml, query).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED, "{}", r.text().await.unwrap());
        r.json().await.unwrap()
    }

    /// Waits for a change to be done.
    async fn wait(&self, name: &str, generation: u64) -> ChangeStatus {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let st: ChangeStatus = self
                .req(Method::GET, &format!("/apps/{name}/changes/{generation}?wait=5"))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            if st.done {
                return st;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {name} {generation}: {st:?}"
            );
        }
    }

    async fn apps(&self) -> Vec<AppSummary> {
        self.req(Method::GET, "/apps")
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn info(&self, name: &str) -> AppInfo {
        let r = self.req(Method::GET, &format!("/apps/{name}")).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    fn bundle_files(&self) -> usize {
        std::fs::read_dir(self.dir.path().join("server/bundles"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".zip"))
            .count()
    }
}

async fn error(r: reqwest::Response) -> (StatusCode, String) {
    let status = r.status();
    let body: ErrorBody = r.json().await.unwrap();
    (status, body.error)
}

struct Node {
    name: String,
    server: String,
    dir: PathBuf,
    ports: std::ops::RangeInclusive<u16>,
    fake: Arc<Fake>,
    volumes: Arc<crate::volumes::fake::Fake>,
    rt: Option<tokio::runtime::Runtime>,
}

impl Node {
    fn config(&self) -> NodeConfig {
        let mut cfg = NodeConfig::new(self.dir.join("state"));
        cfg.volumes_dir = self.dir.join("volumes");
        cfg.project_prefix = format!("kn-{}-", self.name);
        cfg.ports = self.ports.clone();
        cfg.start_timeout = Duration::from_secs(5);
        cfg
    }

    fn project(&self, app: &str) -> String {
        format!("kn-{}-{app}", self.name)
    }

    /// Starts the node on a runtime of its own; returns once it has joined (or read its identity).
    async fn start(&mut self, join_token: Option<String>) {
        assert!(self.rt.is_none());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (cfg, fake, volumes) = (self.config(), self.fake.clone(), self.volumes.clone());
        let mut acfg = AgentConfig::new(self.server.clone(), self.name.clone());
        acfg.join_token = join_token;
        acfg.poll_wait = Duration::from_secs(1);
        acfg.max_backoff = Duration::from_millis(200);
        let (tx, rx) = tokio::sync::oneshot::channel();
        rt.spawn(async move {
            let started = async {
                let apps = Apps::open_with_runtime(cfg, volumes, fake).await?;
                Agent::start(apps, acfg).await
            }
            .await;
            match started {
                Ok(agent) => {
                    let _ = tx.send(Ok(()));
                    agent.run(CancellationToken::new()).await;
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        });
        rx.await.unwrap().unwrap();
        self.rt = Some(rt);
    }

    /// Kills the node: every one of its tasks stops wherever it is. Its apps (in the fake runtime) carry on.
    fn kill(&mut self) {
        self.rt.take().expect("a running node").shutdown_background();
    }

    fn agent_file(&self) -> AgentFile {
        serde_json::from_slice(&std::fs::read(self.dir.join("state/agent.json")).unwrap()).unwrap()
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

fn yaml(domain: &str, volumes: &[&str]) -> String {
    let mounts: String = volumes.iter().map(|v| format!(", \"{v}:/{v}\"")).collect();
    let declared: String = volumes.iter().map(|v| format!("  {v}: {{}}\n")).collect();
    let declared = if declared.is_empty() {
        String::new()
    } else {
        format!("volumes:\n{declared}")
    };
    format!(
        "x-kiwi-nest: {{ domain: {domain}, service: web, port: 80 }}\n\
         services:\n  web:\n    image: x\n    volumes: [\"./compose.yaml:/c.yaml\"{mounts}]\n{declared}"
    )
}

fn files_in(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |d| d.count())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_join_report_and_run_apps_through_the_server() {
    let c = Cluster::start().await;
    let mut a = c.node("a", 31000..=31499);
    let mut b = c.node("b", 31500..=31999);
    a.start(Some(c.join_token().await)).await;
    b.start(Some(c.join_token().await)).await;
    let nodes = c
        .until_nodes("both nodes to report", |ns| {
            ns.len() == 2
                && ns
                    .iter()
                    .all(|n| n.online && n.facts.as_ref().is_some_and(|f| f.memory_bytes > 0))
        })
        .await;
    let f = nodes[0].facts.as_ref().unwrap();
    assert_eq!(f.version, kn_proto::VERSION);
    assert_eq!(f.project_prefix, "kn-a-");
    assert!(f.state.total_bytes > 0);
    // The node's token is kept, and only there.
    assert_eq!(a.agent_file().name, "a");

    let acc = c.deployed("blog", &yaml("blog.test", &["data"]), "node=a").await;
    assert_eq!((acc.node.as_str(), acc.generation), ("a", 1));
    let st = c.wait("blog", 1).await;
    assert!(st.ok, "{st:?}");
    assert_eq!(a.fake.calls("up", &a.project("blog")), 1);
    let apps = c.apps().await;
    assert_eq!(apps.len(), 1);
    assert_eq!(
        (
            apps[0].status,
            apps[0].generation,
            apps[0].node.as_deref(),
            apps[0].pending
        ),
        (AppStatus::Running, 1, Some("a"), None)
    );

    // A domain is unique across nodes.
    let (status, msg) = error(c.deploy("other", &yaml("blog.test", &[]), "node=b").await).await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    assert!(msg.contains("on node `a`"), "{msg}");

    // Inspect asks the node.
    let info = c.info("blog").await;
    assert_eq!(info.project, a.project("blog"));
    assert_eq!(info.node.as_deref(), Some("a"));
    assert!(!info.stale && !info.containers.is_empty());

    // Suspend and resume are commands, answered through the link.
    let r = c.req(Method::POST, "/apps/blog/suspend").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.json::<AppInfo>().await.unwrap().status, AppStatus::Suspended);
    let r = c.req(Method::POST, "/apps/blog/resume").send().await.unwrap();
    assert_eq!(r.json::<AppInfo>().await.unwrap().status, AppStatus::Running);
    // The node's errors come back as they are.
    let (status, msg) = error(c.req(Method::GET, "/apps/blog/logs").send().await.unwrap()).await;
    assert!(status.is_server_error() && msg.contains("no logs"), "{status} {msg}");

    // A preview goes to production's node, and starts from its data.
    let acc = c.deployed("blog-pr1", &yaml("pr1.test", &["data"]), "from=blog").await;
    assert_eq!(acc.node, "a");
    let st = c.wait("blog-pr1", 1).await;
    assert!(st.ok, "{st:?}");
    assert!(
        st.notes
            .iter()
            .any(|n| n.contains("snapshotted volume `data` from `blog`")),
        "{st:?}"
    );

    // A redeploy, then a delete.
    c.deployed("blog", &yaml("blog.test", &["data"]), "").await;
    assert!(c.wait("blog", 2).await.ok);
    assert_eq!(c.info("blog").await.generation, 2);
    for app in ["blog-pr1", "blog"] {
        let r = c.req(Method::DELETE, &format!("/apps/{app}")).send().await.unwrap();
        let acc: Accepted = r.json().await.unwrap();
        let st = c.wait(app, acc.generation).await;
        assert!(st.ok, "{st:?}");
    }
    assert!(c.apps().await.is_empty());
    assert_eq!(
        a.fake.calls("down", &a.project("blog")),
        2,
        "the redeploy, and the delete"
    );
    assert_eq!(c.bundle_files(), 0);
    // The domain is free again, on either node.
    let acc = c.deployed("other", &yaml("blog.test", &[]), "node=b").await;
    assert!(c.wait("other", acc.generation).await.ok);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_killed_mid_deploy_finishes_it_once_when_it_comes_back() {
    let c = Cluster::start().await;
    let mut a = c.node("a", 32000..=32499);
    a.start(Some(c.join_token().await)).await;
    c.online("a", true).await;
    a.fake.set_behaviour(Behaviour {
        up_delay: Duration::from_secs(30),
        ..Default::default()
    });
    let project = a.project("blog");
    c.deployed("blog", &yaml("blog.test", &["data"]), "").await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while a.fake.calls("up", &project) == 0 {
        assert!(Instant::now() < deadline, "the node should start the deploy");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    a.kill();
    c.online("a", false).await;
    let st: ChangeStatus = c
        .req(Method::GET, "/apps/blog/changes/1")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(!st.done && !st.node_online, "{st:?}");
    assert!(
        a.agent_file().applied.apps.is_empty(),
        "the cut-off deploy wasn't recorded"
    );

    // Back again: it finishes the deploy, once.
    a.fake.set_behaviour(Behaviour::default());
    a.start(None).await;
    let st = c.wait("blog", 1).await;
    assert!(st.ok, "{st:?}");
    assert_eq!(
        a.fake.calls("up", &project),
        2,
        "the cut-off attempt and the one that finished"
    );
    let info = c.info("blog").await;
    assert_eq!((info.status, info.generation), (AppStatus::Running, 1));
    assert_eq!(a.agent_file().applied.apps["blog"].generation, 1);

    // Restarting again changes nothing: the deploy is recorded, so it never runs again.
    a.kill();
    a.start(None).await;
    c.online("a", true).await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(a.fake.calls("up", &project), 2);
    assert_eq!(c.info("blog").await.generation, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deploy_to_an_offline_node_is_refused_and_leaves_nothing() {
    let c = Cluster::start().await;
    let mut a = c.node("a", 32500..=32999);
    a.start(Some(c.join_token().await)).await;
    c.online("a", true).await;
    a.kill();
    c.online("a", false).await;

    let (status, msg) = error(c.deploy("blog", &yaml("blog.test", &[]), "node=a").await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(msg.contains("--queue"), "{msg}");
    let (status, _) = error(c.deploy("blog", &yaml("blog.test", &[]), "").await).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(c.apps().await.is_empty());
    assert_eq!(c.bundle_files(), 0);
    assert_eq!(files_in(&c.dir.path().join("server/bundles/tmp")), 0);
    let nodes = c.nodes().await;
    assert_eq!(nodes[0].apps, 0);

    // With --queue it's saved, and done when the node is back.
    let acc = c.deployed("blog", &yaml("blog.test", &[]), "node=a&queue=true").await;
    assert!(!acc.node_online);
    assert_eq!(c.apps().await[0].pending, Some(1));
    a.start(None).await;
    assert!(c.wait("blog", 1).await.ok);
    assert_eq!(c.apps().await[0].pending, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_poll_loop_carries_on_while_a_long_deploy_and_commands_run() {
    let c = Cluster::start().await;
    let mut a = c.node("a", 33000..=33499);
    a.start(Some(c.join_token().await)).await;
    c.online("a", true).await;
    c.deployed("notes", &yaml("notes.test", &[]), "").await;
    assert!(c.wait("notes", 1).await.ok);

    a.fake.set_behaviour(Behaviour {
        up_delay: Duration::from_secs(4),
        ..Default::default()
    });
    let started = Instant::now();
    c.deployed("blog", &yaml("blog.test", &[]), "").await;
    while a.fake.calls("up", &a.project("blog")) == 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // While blog's deploy is in `up`, other apps' commands and the node's reports go on.
    let r = c.req(Method::POST, "/apps/notes/suspend").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let info = c.info("notes").await;
    assert_eq!(info.status, AppStatus::Suspended);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "commands waited for the deploy"
    );
    assert!(c.nodes().await[0].online);
    assert!(c.wait("blog", 1).await.ok);
}

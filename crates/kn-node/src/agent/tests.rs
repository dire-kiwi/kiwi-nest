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
            ingress: String::new(),
            rt: None,
            exited: Arc::new(std::sync::Mutex::new(None)),
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
    /// The ingress address it reports.
    ingress: String,
    rt: Option<tokio::runtime::Runtime>,
    /// Why the agent stopped, once it has (it was forgotten).
    exited: Arc<std::sync::Mutex<Option<String>>>,
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
        if let Err(e) = self.try_start(join_token).await {
            panic!("node {} didn't start: {e}", self.name);
        }
    }

    /// Starts the node, or says why it refused to.
    async fn try_start(&mut self, join_token: Option<String>) -> Result<(), String> {
        assert!(self.rt.is_none());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (cfg, fake, volumes) = (self.config(), self.fake.clone(), self.volumes.clone());
        let mut acfg = AgentConfig::new(self.server.clone(), self.name.clone());
        acfg.join_token = join_token;
        acfg.ingress = self.ingress.clone();
        acfg.poll_wait = Duration::from_secs(1);
        acfg.max_backoff = Duration::from_millis(200);
        let (tx, rx) = tokio::sync::oneshot::channel();
        let exited = self.exited.clone();
        *exited.lock().unwrap() = None;
        rt.spawn(async move {
            let started = async {
                prepare(&cfg, &*fake, acfg.join_token.is_some()).await?;
                let apps = Apps::open_with_runtime(cfg, volumes, fake).await?;
                Agent::start(apps, acfg).await
            }
            .await;
            match started {
                Ok(agent) => {
                    let _ = tx.send(Ok(()));
                    let why = match agent.run(CancellationToken::new()).await {
                        Ok(()) => "stopped".to_string(),
                        Err(e) => format!("{e:#}"),
                    };
                    *exited.lock().unwrap() = Some(why);
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                }
            }
        });
        match rx.await.unwrap() {
            Ok(()) => {
                self.rt = Some(rt);
                Ok(())
            }
            Err(e) => {
                rt.shutdown_background();
                Err(format!("{e:#}"))
            }
        }
    }

    /// Waits for the agent to stop by itself; returns why.
    async fn exits(&mut self) -> String {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let why = self.exited.lock().unwrap().clone();
            if let Some(why) = why {
                self.kill();
                return why;
            }
            assert!(Instant::now() < deadline, "node {} should have stopped", self.name);
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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
    // The node's token is kept, and only there, with its ingress token.
    assert_eq!(a.agent_file().name, "a");
    assert!(a.agent_file().ingress_token.unwrap().starts_with("kni_"));

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

/// The applier on its own, with a server that can't be reached (so every download is retried).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_change_replaced_by_a_later_one_wakes_whoever_waits_on_it() {
    install_tls();
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut cfg = NodeConfig::new(root.join("state"));
    cfg.volumes_dir = root.join("volumes");
    let apps = Apps::open_with_runtime(cfg, Arc::new(crate::volumes::fake::Fake::default()), Fake::new())
        .await
        .unwrap();
    // Port 1 refuses connections: the agent's downloads fail, and it retries them.
    let server = "http://127.0.0.1:1";
    let file = AgentFile {
        version: FILE_VERSION,
        server: server.into(),
        node_id: 1,
        name: "a".into(),
        token: "knn_test".into(),
        ingress_token: None,
        applied: Applied::default(),
        forgotten: None,
    };
    std::fs::write(root.join("state/agent.json"), serde_json::to_vec(&file).unwrap()).unwrap();
    let agent = Agent::start(apps, AgentConfig::new(server, "a")).await.unwrap();
    let inner = &agent.inner;
    let deploy = |generation, seq, shared: &[&str]| {
        Want::App(AppChange {
            name: "x".into(),
            generation,
            seq,
            want: AppWant::Deploy {
                bundle_digest: format!("sha256:{}", "0".repeat(64)),
                from: None,
                shared: shared.iter().map(|s| s.to_string()).collect(),
            },
        })
    };
    // x's deploy (seq 4) can't download its bundle, so it stays pending.
    inner.want(Key::App("x".into()), deploy(1, 4, &[]));
    // Volume v (seq 5) waits for every earlier change, so for x's.
    inner.want(
        Key::Volume("v".into()),
        Want::Volume(VolumeChange {
            name: "v".into(),
            generation: 1,
            seq: 5,
            want: VolumeWant::Present { data_digest: None },
        }),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(inner.applied().volumes.is_empty(), "v waits for x's earlier deploy");
    // x is redeployed (seq 7) mounting v: x now waits for v, and v must notice that x's change is no longer
    // earlier than its own.
    inner.want(Key::App("x".into()), deploy(2, 7, &["v"]));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !inner.applied().volumes.contains_key("v") {
        assert!(Instant::now() < deadline, "v and x waited on each other");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(inner.applied().volumes["v"].ok);
    assert!(inner.applied().apps.is_empty(), "x is still retrying its download");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_joined_before_ingress_tokens_asks_for_its_own() {
    let c = Cluster::start().await;
    let mut a = c.node("a", 31000..=31099);
    a.start(Some(c.join_token().await)).await;
    let token = a.agent_file().ingress_token.expect("given when it joins");
    a.kill();
    // As if it joined before the server gave out ingress tokens.
    let path = a.dir.join("state/agent.json");
    let mut f = a.agent_file();
    f.ingress_token = None;
    std::fs::write(&path, serde_json::to_vec(&f).unwrap()).unwrap();
    // Without an ingress address it doesn't need one.
    a.start(None).await;
    assert_eq!(a.agent_file().ingress_token, None);
    a.kill();
    a.ingress = "127.0.0.1:7382".into();
    a.start(None).await;
    assert_eq!(a.agent_file().ingress_token, Some(token));
    // And it reports where the ingress reaches it.
    c.until_nodes("a's ingress address", |ns| {
        ns.iter()
            .any(|n| n.facts.as_ref().is_some_and(|f| f.ingress == "127.0.0.1:7382"))
    })
    .await;
}

/// The directories under `VOLUMES/forgotten/` on a node.
fn set_aside(n: &Node) -> Vec<PathBuf> {
    let dir = n.dir.join("volumes/forgotten");
    let mut out: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|d| d.map(|e| e.unwrap().path()).collect())
        .unwrap_or_default();
    out.sort();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forgotten_node_takes_its_apps_down_sets_its_data_aside_and_wont_start_again() {
    let c = Cluster::start().await;
    let mut a = c.node("a", 32000..=32499);
    let mut b = c.node("b", 32500..=32999);
    a.start(Some(c.join_token().await)).await;
    b.start(Some(c.join_token().await)).await;
    c.online("a", true).await;
    c.online("b", true).await;
    let old_id = a.agent_file().node_id;

    // On a: production with its data, a preview of it, and a shared volume.
    c.deployed("blog", &yaml("blog.test", &["data"]), "node=a").await;
    assert!(c.wait("blog", 1).await.ok);
    let r = c.req(Method::PUT, "/volumes/corpus?node=a").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    c.deployed("blog-pr1", &yaml("blog-pr1.test", &["data"]), "from=blog")
        .await;
    assert!(c.wait("blog-pr1", 1).await.ok);
    std::fs::write(a.dir.join("volumes/apps/blog/data/posts"), "hello").unwrap();
    assert_eq!(a.fake.state(&a.project("blog"), "web").as_deref(), Some("running"));

    // a goes away, and is forgotten.
    a.kill();
    c.online("a", false).await;
    let r = c.req(Method::POST, "/nodes/a/forget").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text().await.unwrap());
    let f: kn_proto::api::Forgot = r.json().await.unwrap();
    assert_eq!(f.apps, ["blog", "blog-pr1"]);
    assert_eq!(f.volumes, ["corpus"]);
    assert_eq!(f.nests, ["blog", "corpus"]);

    // One of its always-on apps is deployed again, on b (the only node left).
    let acc = c.deployed("blog", &yaml("blog.test", &["data"]), "").await;
    assert_eq!(acc.node, "b");
    assert!(acc.generation > 1, "a name's generations only grow");
    assert!(c.wait("blog", acc.generation).await.ok);
    assert_eq!(b.fake.state(&b.project("blog"), "web").as_deref(), Some("running"));

    // a comes back: it hears it was forgotten, takes its apps down, sets its data aside and stops.
    a.start(None).await;
    let why = a.exits().await;
    assert!(why.contains("forgot this node"), "{why}");
    for app in ["blog", "blog-pr1"] {
        assert_eq!(
            a.fake.state(&a.project(app), "web"),
            None,
            "{app}'s containers are removed"
        );
        assert!(a.fake.calls("down", &a.project(app)) >= 1);
    }
    let aside = set_aside(&a);
    assert_eq!(aside.len(), 1, "{aside:?}");
    let aside = &aside[0];
    assert_eq!(
        std::fs::read_to_string(aside.join("apps/blog/data/posts")).unwrap(),
        "hello",
        "the data is set aside, not deleted"
    );
    assert!(aside.join("apps/blog-pr1/data").is_dir());
    assert!(aside.join("shared/corpus/data").is_dir());
    assert!(aside.join("state.json").is_file());
    assert!(aside.join("bundles/blog").is_dir());
    assert!(aside.join("forgotten.json").is_file());
    assert!(!a.dir.join("state/state.json").exists());
    assert!(!a.dir.join("volumes/apps").exists());
    let file = a.agent_file();
    let rec = file.forgotten.expect("recorded as forgotten");
    assert!(rec.done && file.token.is_empty(), "{rec:?}");
    assert_eq!(rec.node_id, old_id);

    // It won't start again as that node.
    let err = a.try_start(None).await.unwrap_err();
    assert!(err.contains("fresh join token"), "{err}");
    assert!(a.fake.calls("up", &a.project("blog")) == 1, "nothing was started again");
    // The app runs only on b.
    let apps = c.apps().await;
    let blog = apps.iter().find(|x| x.name == "blog").unwrap();
    assert_eq!(blog.node.as_deref(), Some("b"));
    assert!(
        !apps.iter().any(|x| x.name == "blog-pr1"),
        "its other apps were released"
    );

    // With a fresh join token it's a new node, with nothing of the old one's.
    a.start(Some(c.join_token().await)).await;
    let nodes = c
        .until_nodes("the new a to report", |ns| ns.iter().any(|n| n.name == "a" && n.online))
        .await;
    let new = nodes.iter().find(|n| n.name == "a").unwrap();
    assert_ne!(new.id, old_id, "a new identity");
    assert_eq!(new.apps, 0);
    assert_eq!(a.agent_file().node_id, new.id);
    assert!(a.agent_file().forgotten.is_none());
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        a.fake.calls("up", &a.project("blog")),
        1,
        "its old apps aren't sent to it"
    );
    // And the volume and the preview's names are free again.
    let r = c.req(Method::PUT, "/volumes/corpus").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED, "{}", r.text().await.unwrap());
}

/// A node forgotten while it's online (`--force`) hears at once, through its open poll.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_node_forgotten_while_online_hears_through_its_poll() {
    let c = Cluster::start().await;
    let mut a = c.node("a", 33000..=33499);
    a.start(Some(c.join_token().await)).await;
    c.online("a", true).await;
    c.deployed("blog", &yaml("blog.test", &["data"]), "").await;
    assert!(c.wait("blog", 1).await.ok);
    let (status, msg) = error(c.req(Method::POST, "/nodes/a/forget").send().await.unwrap()).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("is online"), "{msg}");
    let r = c.req(Method::POST, "/nodes/a/forget?force=true").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let why = a.exits().await;
    assert!(why.contains("forgot this node"), "{why}");
    assert_eq!(a.fake.state(&a.project("blog"), "web"), None);
    assert_eq!(set_aside(&a).len(), 1);
}

/// A node cut off in the middle of setting its data aside finishes before it opens its apps, so it never adopts
/// (and starts) one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setting_aside_cut_off_is_finished_before_the_apps_open() {
    let c = Cluster::start().await;
    let mut a = c.node("a", 33500..=33999);
    a.start(Some(c.join_token().await)).await;
    c.deployed("blog", &yaml("blog.test", &["data"]), "").await;
    assert!(c.wait("blog", 1).await.ok);
    a.kill();
    // As if the node had recorded that it was forgotten, and then died.
    let mut file = a.agent_file();
    let cfg = a.config();
    file.forgotten = Some(Forgotten::new(&cfg, &c.url, file.node_id, "a", 1, now()));
    file.token.clear();
    std::fs::write(a.dir.join("state/agent.json"), serde_json::to_vec(&file).unwrap()).unwrap();
    // The app's containers stopped meanwhile (say, the machine rebooted); an always-on app would be started.
    a.fake.add_project(&a.project("blog"), 33500, &[("web", "exited", 0)]);
    let err = a.try_start(None).await.unwrap_err();
    assert!(err.contains("fresh join token"), "{err}");
    assert_eq!(a.fake.state(&a.project("blog"), "web"), None);
    assert_eq!(a.fake.calls("start", &a.project("blog")), 0, "never started again");
    assert!(a.agent_file().forgotten.unwrap().done);
    assert!(set_aside(&a)[0].join("apps/blog/data").is_dir());
}

/// A server that answers every request with `status` and `body`, as a stand-in for a proxy or a broken server.
async fn answering(status: &'static str, body: String) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let body = body.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 1 << 16];
                let mut got = Vec::new();
                // Read the head and whatever body arrives with it; the answer doesn't depend on it.
                while !got.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                    }
                }
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    url
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_outage_a_wrong_token_or_a_look_alike_answer_takes_nothing_down() {
    let c = Cluster::start().await;
    let mut a = c.node("a", 34000..=34499);
    a.start(Some(c.join_token().await)).await;
    c.deployed("blog", &yaml("blog.test", &["data"]), "").await;
    assert!(c.wait("blog", 1).await.ok);
    a.kill();
    let good = a.agent_file();
    let other_node = serde_json::to_string(&kn_proto::agent::ForgottenBody {
        error: kn_proto::agent::FORGOTTEN.into(),
        node_id: good.node_id + 100,
        name: "a".into(),
        forgotten_at: 1,
    })
    .unwrap();
    let servers = [
        ("a server that can't be reached", "http://127.0.0.1:1".to_string()),
        (
            "a 401",
            answering("401 Unauthorized", r#"{"error":"the node token isn't valid"}"#.into()).await,
        ),
        (
            "a proxy's 410",
            answering("410 Gone", r#"{"error":"gone"}"#.into()).await,
        ),
        ("a 410 for another node", answering("410 Gone", other_node).await),
        ("a 502", answering("502 Bad Gateway", "bad gateway".into()).await),
    ];
    for (what, url) in servers {
        let mut file = a.agent_file();
        file.server = url.clone();
        std::fs::write(a.dir.join("state/agent.json"), serde_json::to_vec(&file).unwrap()).unwrap();
        a.server = url;
        a.start(None).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(a.exited.lock().unwrap().is_none(), "{what}: the agent carries on");
        a.kill();
        assert_eq!(
            a.fake.state(&a.project("blog"), "web").as_deref(),
            Some("running"),
            "{what}"
        );
        assert_eq!(a.fake.calls("down", &a.project("blog")), 0, "{what}");
        assert!(a.agent_file().forgotten.is_none(), "{what}");
        assert!(set_aside(&a).is_empty(), "{what}");
    }
    // A wrong token against the real server: an ordinary 401, which takes nothing down either.
    let mut file = good.clone();
    file.token = "knn_wrong".into();
    std::fs::write(a.dir.join("state/agent.json"), serde_json::to_vec(&file).unwrap()).unwrap();
    a.server = c.url.clone();
    a.start(None).await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(a.exited.lock().unwrap().is_none());
    a.kill();
    assert_eq!(a.fake.state(&a.project("blog"), "web").as_deref(), Some("running"));
    assert!(a.agent_file().forgotten.is_none());
    // With its own token back, it carries on as before.
    std::fs::write(a.dir.join("state/agent.json"), serde_json::to_vec(&good).unwrap()).unwrap();
    a.start(None).await;
    c.online("a", true).await;
    assert_eq!(c.info("blog").await.node.as_deref(), Some("a"));
}

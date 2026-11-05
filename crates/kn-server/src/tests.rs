//! The server over HTTP, with nodes played by the test: joining and reporting, reserving domains across nodes,
//! refusing changes for offline nodes, deploy tokens, commands, deletes and restarts.

use std::time::Duration;

use kn_proto::agent::{
    Action, AppReport, AppWant, CommandResult, JoinRequest, JoinResponse, NodeFacts, Outcome, PollRequest,
    PollResponse, Report,
};
use kn_proto::api::{Accepted, AppSummary, ChangeStatus, CreatedToken, ErrorBody, NodeInfo};
use kn_proto::state::AppStatus;
use reqwest::StatusCode;
use serde_json::json;

use super::*;

pub(crate) struct Env {
    pub(crate) dir: tempfile::TempDir,
    /// Backup targets the server is given.
    targets: Vec<kn_proto::backup::TargetConfig>,
    /// The running server, for what tests reach into.
    server: Option<Server>,
    ingress: Option<ingress::IngressConfig>,
    /// The registry, and a fake for its container.
    registry: Option<(registry::RegistryConfig, Arc<dyn registry::container::Container>)>,
    /// The releases the server offers nodes.
    releases: Vec<upgrades::Offer>,
    pub(crate) url: String,
    pub(crate) admin: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    pub(crate) http: reqwest::Client,
}

fn config(dir: &std::path::Path, targets: &[kn_proto::backup::TargetConfig]) -> ServerConfig {
    let mut cfg = ServerConfig::new(dir.join("server"));
    cfg.poll_timeout = Duration::from_millis(500);
    cfg.offline_after = Duration::from_millis(1500);
    cfg.command_timeout = Duration::from_secs(10);
    cfg.backup_targets = targets.to_vec();
    cfg.backup_check_interval = Duration::from_millis(200);
    cfg.upgrade_check_interval = Duration::from_millis(100);
    cfg.upgrade_restart_timeout = Duration::from_secs(2);
    cfg
}

impl Env {
    async fn new() -> Env {
        Env::build(None, Vec::new(), None).await
    }

    async fn with_ingress(ingress: Option<ingress::IngressConfig>) -> Env {
        Env::build(ingress, Vec::new(), None).await
    }

    async fn with_targets(targets: Vec<kn_proto::backup::TargetConfig>) -> Env {
        Env::build(None, targets, None).await
    }

    /// A server with an ingress and a registry, whose container is a fake.
    pub(crate) async fn with_registry(
        ingress: Option<ingress::IngressConfig>,
        registry: Option<(registry::RegistryConfig, Arc<dyn registry::container::Container>)>,
    ) -> Env {
        Env::build(ingress, Vec::new(), registry).await
    }

    async fn build(
        ingress: Option<ingress::IngressConfig>,
        targets: Vec<kn_proto::backup::TargetConfig>,
        registry: Option<(registry::RegistryConfig, Arc<dyn registry::container::Container>)>,
    ) -> Env {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let mut env = Env {
            dir,
            targets,
            server: None,
            ingress,
            registry,
            releases: Vec::new(),
            url: String::new(),
            admin: String::new(),
            stop: None,
            task: None,
            http: reqwest::Client::new(),
        };
        let first = env.start().await;
        env.admin = first.expect("the first start makes an admin token");
        env
    }

    async fn start(&mut self) -> Option<String> {
        let mut cfg = config(self.dir.path(), &self.targets);
        cfg.ingress = self.ingress.clone();
        cfg.registry = self.registry.as_ref().map(|r| r.0.clone());
        cfg.releases = self.releases.clone();
        let (server, first) = Server::open_with(cfg, self.registry.as_ref().map(|r| r.1.clone()))
            .await
            .unwrap();
        self.server = Some(server.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        self.url = format!("http://{}{}", listener.local_addr().unwrap(), kn_proto::API_PREFIX);
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.stop = Some(tx);
        self.task = Some(tokio::spawn(async move {
            serve(listener, server, async {
                let _ = rx.await;
            })
            .await
            .unwrap();
        }));
        first
    }

    /// Stops the server and starts it again on the same state.
    async fn restart(&mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.take().unwrap().await.unwrap();
        assert!(self.start().await.is_none(), "a restart makes no new admin token");
    }

    pub(crate) fn req(&self, method: reqwest::Method, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(token)
    }

    pub(crate) async fn token(&self, body: serde_json::Value) -> String {
        let r = self
            .req(reqwest::Method::POST, "/tokens", &self.admin)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{}", r.text().await.unwrap());
        r.json::<CreatedToken>().await.unwrap().token
    }

    pub(crate) async fn join(&self, name: &str, memory: u64) -> FakeNode {
        self.join_with(name, facts(memory)).await
    }

    /// Joins a node that reports `facts`.
    pub(crate) async fn join_with(&self, name: &str, node_facts: NodeFacts) -> FakeNode {
        let join = self.token(json!({"kind": "join"})).await;
        let r = self
            .http
            .post(format!("{}/agent/join", self.url))
            .json(&JoinRequest {
                token: join.clone(),
                name: name.into(),
                facts: node_facts.clone(),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let j: JoinResponse = r.json().await.unwrap();
        // A join token works once.
        let again = self
            .http
            .post(format!("{}/agent/join", self.url))
            .json(&JoinRequest {
                token: join,
                name: format!("{name}-again"),
                facts: node_facts.clone(),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
        assert!(j.ingress_token.starts_with("kni_") && j.ingress_token.len() == 68);
        FakeNode {
            registry: j.registry,
            ingress_token: j.ingress_token,
            token: j.token,
            id: j.node_id,
            facts: node_facts.clone(),
            since: 0,
            report: Report {
                facts: node_facts,
                ..Default::default()
            },
        }
    }

    pub(crate) async fn poll(&self, n: &mut FakeNode) -> PollResponse {
        n.report.facts = n.facts.clone();
        let r = self
            .req(reqwest::Method::POST, "/agent/poll", &n.token)
            .json(&PollRequest {
                protocol: kn_proto::agent::PROTOCOL,
                since: n.since,
                wait_secs: Some(0),
                report: n.report.clone(),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{}", r.text().await.unwrap());
        let resp: PollResponse = r.json().await.unwrap();
        n.since = resp.seq;
        resp
    }

    pub(crate) async fn deploy(&self, token: &str, name: &str, domain: &str, query: &str) -> reqwest::Response {
        self.req(reqwest::Method::PUT, &format!("/apps/{name}?{query}"), token)
            .header("content-type", "application/zip")
            .body(bundle(domain))
            .send()
            .await
            .unwrap()
    }

    pub(crate) async fn apps(&self) -> Vec<AppSummary> {
        self.req(reqwest::Method::GET, "/apps", &self.admin)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    pub(crate) async fn status(&self, name: &str, generation: u64) -> ChangeStatus {
        let r = self
            .req(
                reqwest::Method::GET,
                &format!("/apps/{name}/changes/{generation}"),
                &self.admin,
            )
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    /// The bundle files in the store.
    fn bundle_files(&self) -> usize {
        std::fs::read_dir(self.dir.path().join("server/bundles"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".zip"))
            .count()
    }
}

pub(crate) struct FakeNode {
    pub(crate) token: String,
    ingress_token: String,
    /// What the join answered about the registry.
    pub(crate) registry: Option<kn_proto::agent::RegistryLogin>,
    pub(crate) id: u64,
    pub(crate) facts: NodeFacts,
    since: u64,
    pub(crate) report: Report,
}

impl FakeNode {
    /// Records that a change is done, and the app as the node has it.
    pub(crate) fn applied(&mut self, name: &str, generation: u64, ok: bool, deleted: bool) {
        self.report.applied.apps.insert(
            name.into(),
            Outcome {
                generation,
                ok,
                error: (!ok).then(|| "it broke".to_string()),
                problems: vec![],
                notes: vec![],
                deleted,
                data_digest: None,
                finished_at: 0,
            },
        );
        self.report.apps.retain(|a| a.name != name);
        if !deleted {
            self.report.apps.push(AppReport {
                name: name.into(),
                status: AppStatus::Running,
                generation,
                error: None,
                domain: format!("{name}.test"),
                port: 30000,
                bundle_digest: String::new(),
                containers: "running(1)".into(),
                starting: false,
                last_request_at: None,
                open_requests: 0,
                space: Default::default(),
            });
        }
    }
}

pub(crate) fn facts(memory: u64) -> NodeFacts {
    NodeFacts {
        arch: "amd64".into(),
        memory_bytes: memory,
        memory_available_bytes: memory,
        version: kn_proto::VERSION.into(),
        commit: "test".into(),
        protocol: kn_proto::agent::PROTOCOL,
        ..Default::default()
    }
}

/// A bundle ZIP with one service at `domain`.
fn bundle(domain: &str) -> Vec<u8> {
    bundle_with(domain, "")
}

/// A bundle ZIP with one service at `domain`, and more of `x-kiwi-nest`.
fn bundle_with(domain: &str, extra: &str) -> Vec<u8> {
    bundle_image(domain, extra, "x")
}

/// A bundle ZIP with one service at `domain` running `image`, and more of `x-kiwi-nest`.
pub(crate) fn bundle_image(domain: &str, extra: &str, image: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("compose.yaml"),
        format!(
            "x-kiwi-nest: {{ domain: {domain}, service: web, port: 80{extra} }}\n\
             services:\n  web:\n    image: {image}\n    mem_limit: 128m\n"
        ),
    )
    .unwrap();
    kn_bundle::dir::pack(dir.path(), &kn_bundle::Limits::default()).unwrap()
}

pub(crate) async fn error(r: reqwest::Response) -> (StatusCode, String) {
    let status = r.status();
    let body: ErrorBody = r.json().await.unwrap();
    (status, body.error)
}

#[tokio::test]
async fn nodes_join_and_report() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    let mut b = env.join("b", 8 << 30).await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    let nodes: Vec<NodeInfo> = env
        .req(reqwest::Method::GET, "/nodes", &env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(nodes.len(), 2);
    assert!(nodes.iter().all(|n| n.online));
    assert_eq!(nodes[1].facts.as_ref().unwrap().memory_bytes, 8 << 30);
    // A node's token doesn't work on the CLI's API, nor a wrong token anywhere.
    let r = env.req(reqwest::Method::GET, "/apps", &a.token).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = env.req(reqwest::Method::GET, "/apps", "kna_nope").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let r = env
        .req(reqwest::Method::POST, "/agent/poll", &env.admin)
        .json(&PollRequest {
            protocol: 1,
            since: 0,
            wait_secs: Some(0),
            report: Report::default(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    // A node too new for the server is told so.
    let r = env
        .req(reqwest::Method::POST, "/agent/poll", &a.token)
        .json(&PollRequest {
            protocol: 99,
            since: 0,
            wait_secs: Some(0),
            report: Report::default(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UPGRADE_REQUIRED);
}

#[tokio::test]
async fn deploys_go_to_the_most_free_memory_and_reach_their_node() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    let mut b = env.join("b", 8 << 30).await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    let r = env.deploy(&env.admin, "blog", "blog.test", "").await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let acc: Accepted = r.json().await.unwrap();
    assert_eq!((acc.node.as_str(), acc.generation, acc.node_online), ("b", 1, true));
    let st = env.status("blog", 1).await;
    assert!(!st.done);

    // b gets it; a doesn't.
    assert!(env.poll(&mut a).await.apps.is_empty());
    let resp = env.poll(&mut b).await;
    assert_eq!(resp.apps.len(), 1);
    let change = &resp.apps[0];
    assert_eq!((change.name.as_str(), change.generation), ("blog", 1));
    let AppWant::Deploy { bundle_digest, .. } = &change.want else {
        panic!("a deploy")
    };
    // Only b may download the bundle.
    let get = |token: &str| {
        env.req(reqwest::Method::GET, &format!("/agent/bundles/{bundle_digest}"), token)
            .send()
    };
    let r = get(&b.token).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.bytes().await.unwrap().as_ref(), bundle("blog.test").as_slice());
    assert_eq!(get(&a.token).await.unwrap().status(), StatusCode::NOT_FOUND);
    // Nothing new on the next poll.
    assert!(env.poll(&mut b).await.apps.is_empty());

    b.applied("blog", 1, true, false);
    env.poll(&mut b).await;
    let st = env.status("blog", 1).await;
    assert!(st.done && st.ok, "{st:?}");
    let apps = env.apps().await;
    assert_eq!(apps[0].node.as_deref(), Some("b"));
    assert_eq!(apps[0].pending, None);
    assert_eq!(apps[0].status, AppStatus::Running);

    // The next app goes to b too: 8 GiB less blog's 128 MiB still beats 4 GiB.
    let acc: Accepted = env
        .deploy(&env.admin, "shop", "shop.test", "")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(acc.node, "b");
    let acc: Accepted = env
        .deploy(&env.admin, "notes", "notes.test", "node=a")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(acc.node, "a");
    // A redeploy stays on its node, and `--node` can't move it.
    let r = env.deploy(&env.admin, "notes", "notes.test", "node=b").await;
    let (status, msg) = error(r).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("is on node `a`"), "{msg}");
    // A preview goes to its `--from` app's node.
    let acc: Accepted = env
        .deploy(&env.admin, "blog-pr1", "blog-pr1.test", "from=blog")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(acc.node, "b");
    let resp = env.poll(&mut b).await;
    let pr = resp.apps.iter().find(|c| c.name == "blog-pr1").unwrap();
    assert!(matches!(&pr.want, AppWant::Deploy { from: Some(f), .. } if f == "blog"));
}

#[tokio::test]
async fn a_domain_is_refused_across_nodes_and_nothing_is_saved() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    let mut b = env.join("b", 4 << 30).await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    let r = env.deploy(&env.admin, "blog", "blog.test", "node=a").await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let files = env.bundle_files();
    let r = env.deploy(&env.admin, "other", "blog.test", "node=b").await;
    let (status, msg) = error(r).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("already used by app `blog` on node `a`"), "{msg}");
    assert_eq!(env.apps().await.len(), 1);
    assert!(
        env.poll(&mut b).await.apps.is_empty(),
        "b's desired state didn't change"
    );
    assert_eq!(env.bundle_files(), files);
    // create-only refuses an existing app.
    let r = env.deploy(&env.admin, "blog", "blog.test", "create_only=true").await;
    assert_eq!(r.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_change_for_an_offline_node_is_refused_unless_queued() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    tokio::time::sleep(Duration::from_millis(1700)).await;
    let r = env.deploy(&env.admin, "blog", "blog.test", "").await;
    let (status, msg) = error(r).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(msg.contains("no node is online"), "{msg}");
    let r = env.deploy(&env.admin, "blog", "blog.test", "node=a").await;
    let (status, msg) = error(r).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(msg.contains("node `a` is offline"), "{msg}");
    // Nothing was saved: no app, no bundle, no change for the node.
    assert!(env.apps().await.is_empty());
    assert_eq!(env.bundle_files(), 0);
    assert_eq!(
        std::fs::read_dir(env.dir.path().join("server/bundles/tmp"))
            .unwrap()
            .count(),
        0,
        "no upload is left behind"
    );
    // Queued, it's saved, and the node gets it when it's back.
    let r = env.deploy(&env.admin, "blog", "blog.test", "node=a&queue=true").await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let acc: Accepted = r.json().await.unwrap();
    assert!(!acc.node_online);
    assert_eq!(env.apps().await[0].pending, Some(1));
    let resp = env.poll(&mut a).await;
    assert_eq!(resp.apps.len(), 1);
}

#[tokio::test]
async fn a_deploy_token_only_touches_its_own_apps() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    assert_eq!(
        env.deploy(&env.admin, "blog", "blog.test", "").await.status(),
        StatusCode::ACCEPTED
    );
    a.applied("blog", 1, true, false);
    env.poll(&mut a).await;
    let ci = env
        .token(json!({"kind": "deploy", "name": "ci", "patterns": ["blog-pr*"], "from": ["blog"]}))
        .await;
    let limited = env.token(json!({"kind": "deploy", "patterns": ["blog-pr*"]})).await;

    // It can't deploy, delete, suspend, inspect or read the logs of blog.
    let (status, msg) = error(env.deploy(&ci, "blog", "blog.test", "").await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(msg.contains("limited to blog-pr*"), "{msg}");
    for (method, path) in [
        (reqwest::Method::DELETE, "/apps/blog"),
        (reqwest::Method::POST, "/apps/blog/suspend"),
        (reqwest::Method::POST, "/apps/blog/resume"),
        (reqwest::Method::GET, "/apps/blog"),
        (reqwest::Method::GET, "/apps/blog/logs"),
        (reqwest::Method::GET, "/apps/blog/changes/1"),
        (reqwest::Method::PUT, "/volumes/corpus"),
        (reqwest::Method::GET, "/nodes"),
        (reqwest::Method::GET, "/tokens"),
    ] {
        let r = env.req(method.clone(), path, &ci).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{method} {path}");
    }
    // Nor does `list` show it.
    let apps: Vec<AppSummary> = env
        .req(reqwest::Method::GET, "/apps", &ci)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(apps.is_empty());
    // It can make a preview from blog; a token without `from` can't.
    let (status, _) = error(env.deploy(&limited, "blog-pr1", "pr1.test", "from=blog").await).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        env.deploy(&ci, "blog-pr1", "pr1.test", "from=blog").await.status(),
        StatusCode::ACCEPTED
    );
    let r = env
        .req(reqwest::Method::DELETE, "/apps/blog-pr1", &ci)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    // blog's desired state never changed.
    let resp = env.poll(&mut a).await;
    assert!(resp.apps.iter().all(|c| c.name == "blog-pr1"), "{resp:?}");
}

#[tokio::test]
async fn deletes_release_the_name_and_domain_once_the_node_is_done() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    env.deploy(&env.admin, "blog", "blog.test", "").await;
    env.poll(&mut a).await;
    a.applied("blog", 1, true, false);
    env.poll(&mut a).await;
    assert_eq!(env.bundle_files(), 1);

    let r = env
        .req(reqwest::Method::DELETE, "/apps/blog?keep_data=true", &env.admin)
        .send()
        .await
        .unwrap();
    let acc: Accepted = r.json().await.unwrap();
    assert_eq!(acc.generation, 2);
    let resp = env.poll(&mut a).await;
    assert_eq!(resp.apps[0].want, AppWant::Delete { keep_data: true });
    // Until the node is done, the domain is still taken.
    let (status, _) = error(env.deploy(&env.admin, "other", "blog.test", "").await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    a.applied("blog", 2, true, true);
    env.poll(&mut a).await;
    let st = env.status("blog", 2).await;
    assert!(st.done && st.ok);
    assert!(env.apps().await.is_empty());
    assert_eq!(env.bundle_files(), 0, "a deleted app's bundle is removed");
    assert_eq!(
        env.deploy(&env.admin, "other", "blog.test", "").await.status(),
        StatusCode::ACCEPTED
    );
    // The name starts again with a higher generation, so the node can't mistake it for the old app.
    let acc: Accepted = env
        .deploy(&env.admin, "blog", "blog2.test", "")
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(acc.generation, 3);
}

#[tokio::test]
async fn commands_are_delivered_once_and_lost_ones_fail() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    env.deploy(&env.admin, "blog", "blog.test", "").await;
    env.poll(&mut a).await;
    a.applied("blog", 1, true, false);
    env.poll(&mut a).await;

    // A suspend: the node gets it once, and its result goes back to the caller.
    let suspend = tokio::spawn({
        let r = env.req(reqwest::Method::POST, "/apps/blog/suspend", &env.admin);
        async move { r.send().await.unwrap() }
    });
    let command = loop {
        let resp = env.poll(&mut a).await;
        if let Some(c) = resp.commands.into_iter().next() {
            break c;
        }
    };
    assert_eq!(command.action, Action::Suspend { app: "blog".into() });
    a.report.running_commands.push(command.id);
    assert!(env.poll(&mut a).await.commands.is_empty(), "delivered at most once");
    let info = serde_json::to_value(kn_proto::api::AppInfo {
        name: "blog".into(),
        status: AppStatus::Suspended,
        generation: 1,
        error: None,
        domain: "blog.test".into(),
        service: "web".into(),
        container_port: 80,
        port: 30000,
        idle_timeout_secs: 0,
        project: "kn-blog".into(),
        bundle_digest: String::new(),
        volumes: vec![],
        shared: vec![],
        from: None,
        restored_from: None,
        notes: vec![],
        created_at: String::new(),
        updated_at: String::new(),
        containers: vec![],
        open_requests: 0,
        last_request_at: None,
        space: Default::default(),
        node: None,
        nest: None,
        placement: None,
        pending: None,
        stale: false,
    })
    .unwrap();
    let r = env
        .req(
            reqwest::Method::POST,
            &format!("/agent/commands/{}/result", command.id),
            &a.token,
        )
        .json(&CommandResult::Ok { value: info })
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    a.report.running_commands.clear();
    let r = suspend.await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let info: kn_proto::api::AppInfo = r.json().await.unwrap();
    assert_eq!(info.status, AppStatus::Suspended);
    assert_eq!(info.node.as_deref(), Some("a"));

    // A resume whose poll response never reached the node (its next report doesn't list it) fails.
    let resume = tokio::spawn({
        let r = env.req(reqwest::Method::POST, "/apps/blog/resume", &env.admin);
        async move { r.send().await.unwrap() }
    });
    loop {
        if !env.poll(&mut a).await.commands.is_empty() {
            break;
        }
    }
    env.poll(&mut a).await;
    let (status, msg) = error(resume.await.unwrap()).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert!(msg.contains("didn't get the command"), "{msg}");

    // An error from the node is passed on with its status.
    let inspect = tokio::spawn({
        let r = env.req(reqwest::Method::GET, "/apps/blog", &env.admin);
        async move { r.send().await.unwrap() }
    });
    let command = loop {
        if let Some(c) = env.poll(&mut a).await.commands.into_iter().next() {
            break c;
        }
    };
    env.req(
        reqwest::Method::POST,
        &format!("/agent/commands/{}/result", command.id),
        &a.token,
    )
    .json(&CommandResult::Err {
        status: 409,
        error: ErrorBody {
            error: "busy".into(),
            problems: vec![],
        },
    })
    .send()
    .await
    .unwrap();
    let (status, msg) = error(inspect.await.unwrap()).await;
    assert_eq!((status, msg.as_str()), (StatusCode::CONFLICT, "busy"));
}

#[tokio::test]
async fn a_restart_loses_nothing() {
    let mut env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    let ci = env.token(json!({"kind": "deploy", "patterns": ["blog*"]})).await;
    env.deploy(&ci, "blog", "blog.test", "").await;
    let first = env.poll(&mut a).await;
    a.applied("blog", 1, true, false);
    env.poll(&mut a).await;
    env.deploy(&env.admin, "shop", "shop.test", "").await;

    env.restart().await;
    // Tokens still work, and the node's desired state is as it was: polling from 0 gives the same changes.
    let mut again = FakeNode {
        token: a.token.clone(),
        ingress_token: a.ingress_token.clone(),
        registry: None,
        id: a.id,
        facts: a.facts.clone(),
        since: 0,
        report: a.report.clone(),
    };
    let all = env.poll(&mut again).await;
    assert_eq!(all.apps[0], first.apps[0]);
    assert_eq!(all.apps.len(), 2);
    assert_eq!(env.poll(&mut a).await.apps.len(), 1, "only shop is new to a");
    let apps: Vec<AppSummary> = env
        .req(reqwest::Method::GET, "/apps", &ci)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].status, AppStatus::Running);
    assert!(env.status("blog", 1).await.done);
    assert_eq!(env.bundle_files(), 2);
}

#[tokio::test]
async fn only_admins_make_tokens_and_the_last_admin_token_stays() {
    let env = Env::new().await;
    let r = env
        .req(reqwest::Method::POST, "/tokens", &env.admin)
        .json(&json!({"kind": "deploy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST, "a deploy token needs a pattern");
    let r = env
        .req(reqwest::Method::POST, "/tokens", &env.admin)
        .json(&json!({"kind": "deploy", "patterns": ["Blog/*"]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let tokens: Vec<kn_proto::api::TokenInfo> = env
        .req(reqwest::Method::GET, "/tokens", &env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tokens.len(), 1);
    let r = env
        .req(
            reqwest::Method::DELETE,
            &format!("/tokens/{}", tokens[0].id),
            &env.admin,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    // No token is stored in the clear.
    let needle = env.admin.as_bytes();
    for f in ["server.sqlite", "server.sqlite-wal"] {
        let db = std::fs::read(env.dir.path().join("server").join(f)).unwrap_or_default();
        assert!(!db.windows(needle.len()).any(|w| w == needle), "{f}");
    }
}

#[tokio::test]
async fn json_bodies_are_limited_even_before_a_token_is_checked() {
    let env = Env::new().await;
    let huge = format!(r#"{{"token":"{}","name":"a","facts":{{}}}}"#, "x".repeat(MAX_JSON));
    let r = env
        .http
        .post(format!("{}/agent/join", env.url))
        .header("content-type", "application/json")
        .body(huge)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

/// A bundle at `domain` mounting shared volume `corpus` in `mode`.
fn shared_bundle(domain: &str, mode: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("compose.yaml"),
        format!(
            "x-kiwi-nest: {{ domain: {domain}, service: web, port: 80, shared: {{ corpus: {{ target: /c, mode: {mode} }} }} }}\n\
             services:\n  web:\n    image: x\n"
        ),
    )
    .unwrap();
    kn_bundle::dir::pack(dir.path(), &kn_bundle::Limits::default()).unwrap()
}

#[tokio::test]
async fn a_deploy_token_cant_mount_a_shared_volume_read_write() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    let r = env
        .req(reqwest::Method::PUT, "/volumes/corpus", &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let ci = env.token(json!({"kind": "deploy", "patterns": ["blog-pr*"]})).await;
    let up = |mode: &'static str, token: String| {
        env.req(reqwest::Method::PUT, &format!("/apps/blog-pr-{mode}"), &token)
            .header("content-type", "application/zip")
            .body(shared_bundle(&format!("{mode}.test"), mode))
            .send()
    };
    let (status, msg) = error(up("rw", ci.clone()).await.unwrap()).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(msg.contains("read-write (corpus)"), "{msg}");
    assert_eq!(up("ro", ci.clone()).await.unwrap().status(), StatusCode::ACCEPTED);
    assert_eq!(up("snapshot", ci).await.unwrap().status(), StatusCode::ACCEPTED);
    assert_eq!(
        up("rw", env.admin.clone()).await.unwrap().status(),
        StatusCode::ACCEPTED
    );
    // Each goes to the volume's node, and the volume can't be deleted while they mount it.
    let r = env
        .req(reqwest::Method::DELETE, "/volumes/corpus", &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_report_that_arrives_late_doesnt_replace_a_newer_one() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    env.deploy(&env.admin, "blog", "blog.test", "").await;
    env.poll(&mut a).await;
    a.applied("blog", 1, true, false);
    a.report.serial = 10;
    let r = env
        .req(reqwest::Method::POST, "/agent/report", &a.token)
        .json(&a.report)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert!(env.status("blog", 1).await.done);
    // An older report (built before the change finished) arriving afterwards changes nothing.
    let mut old = a.report.clone();
    old.serial = 9;
    old.applied.apps.clear();
    let r = env
        .req(reqwest::Method::POST, "/agent/report", &a.token)
        .json(&old)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert!(env.status("blog", 1).await.done);
}

/// A bundle ZIP of this `compose.yaml`.
fn zip_of(yaml: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("compose.yaml"), yaml).unwrap();
    kn_bundle::dir::pack(dir.path(), &kn_bundle::Limits::default()).unwrap()
}

/// A one-service app at `domain`, with more `x-kiwi-nest` settings (such as `nest: blog`), using `mem` memory.
fn app_yaml(domain: &str, extra: &str, mem: &str) -> String {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!(", {extra}")
    };
    format!(
        "x-kiwi-nest: {{ domain: {domain}, service: web, port: 80{extra} }}\n\
         services:\n  web:\n    image: x\n    mem_limit: {mem}\n"
    )
}

impl Env {
    async fn put(&self, name: &str, yaml: &str, query: &str) -> reqwest::Response {
        self.req(reqwest::Method::PUT, &format!("/apps/{name}?{query}"), &self.admin)
            .header("content-type", "application/zip")
            .body(zip_of(yaml))
            .send()
            .await
            .unwrap()
    }

    async fn placed(&self, name: &str, yaml: &str, query: &str) -> Accepted {
        let r = self.put(name, yaml, query).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED, "{name}: {}", r.text().await.unwrap());
        r.json().await.unwrap()
    }

    async fn volume(&self, name: &str, query: &str) -> reqwest::Response {
        self.req(reqwest::Method::PUT, &format!("/volumes/{name}?{query}"), &self.admin)
            .send()
            .await
            .unwrap()
    }

    async fn post(&self, path: &str) -> reqwest::Response {
        self.req(reqwest::Method::POST, path, &self.admin).send().await.unwrap()
    }

    async fn nests(&self) -> Vec<kn_proto::api::NestInfo> {
        self.req(reqwest::Method::GET, "/nests", &self.admin)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn nodes(&self) -> Vec<NodeInfo> {
        self.req(reqwest::Method::GET, "/nodes", &self.admin)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn inspect(&self, name: &str) -> kn_proto::api::AppInfo {
        let r = self
            .req(reqwest::Method::GET, &format!("/apps/{name}"), &self.admin)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }
}

#[tokio::test]
async fn a_shared_volume_its_production_app_and_previews_share_a_nest_and_its_node() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    let mut b = env.join("b", 16 << 30).await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    // The volume goes to a (b has more room, but --node chooses), and starts a nest named after it.
    let r = env.volume("corpus", "node=a").await;
    let acc: Accepted = r.json().await.unwrap();
    assert_eq!((acc.node.as_str(), acc.nest.as_deref()), ("a", Some("corpus")));
    let mounts = "shared: { corpus: { target: /c, mode: ro } }";
    let acc = env.placed("blog", &app_yaml("blog.test", mounts, "128m"), "").await;
    assert_eq!((acc.node.as_str(), acc.nest.as_deref()), ("a", Some("corpus")));
    for i in 1..=3 {
        let acc = env
            .placed(
                &format!("blog-pr{i}"),
                &app_yaml(&format!("pr{i}.test"), "idle_timeout: 10m", "128m"),
                "from=blog",
            )
            .await;
        assert_eq!(
            (acc.node.as_str(), acc.nest.as_deref()),
            ("a", Some("corpus")),
            "preview {i}"
        );
    }
    // They all reach a, and nothing reaches b.
    assert_eq!(env.poll(&mut a).await.apps.len(), 4);
    assert!(env.poll(&mut b).await.apps.is_empty());
    let nests = env.nests().await;
    assert_eq!(nests.len(), 1);
    let n = &nests[0];
    assert_eq!((n.name.as_str(), n.node.as_str()), ("corpus", "a"));
    assert_eq!(n.apps, ["blog", "blog-pr1", "blog-pr2", "blog-pr3"]);
    assert_eq!(n.volumes, ["corpus"]);
    assert_eq!(n.memory_bytes, 4 * (128 << 20));
    assert!(
        n.placement
            .contains("when shared volume `corpus` was created: --node a"),
        "{}",
        n.placement
    );
    // Inspect says how each got there.
    let info = env.inspect("blog-pr2").await;
    assert_eq!(info.nest.as_deref(), Some("corpus"));
    let p = info.placement.unwrap();
    assert!(p.starts_with("through --from blog; nest corpus went to node a"), "{p}");
    let info = env.inspect("blog").await;
    assert!(info.placement.unwrap().starts_with("through shared volume `corpus`"));
    // A shared volume made with --nest joins that nest.
    let acc: Accepted = env.volume("extra", "nest=corpus").await.json().await.unwrap();
    assert_eq!((acc.node.as_str(), acc.nest.as_deref()), ("a", Some("corpus")));
    // --node can't split a nest.
    let (status, msg) = error(env.volume("more", "nest=corpus&node=b").await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        msg.contains("nest `corpus` is on node `a`, so --node `b` can't apply"),
        "{msg}"
    );
    let (status, msg) = error(
        env.put("blog-pr4", &app_yaml("pr4.test", "", "1m"), "from=blog&node=b")
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("a nest never splits"), "{msg}");
    // A volume whose name can't name a nest needs --nest.
    let (status, msg) = error(env.volume("Big_Data", "").await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(msg.contains("pass --nest"), "{msg}");
}

#[tokio::test]
async fn a_bundle_naming_two_nests_is_refused_and_nothing_is_saved() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    let mut b = env.join("b", 4 << 30).await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    env.volume("corpus", "node=a").await;
    env.placed("blog", &app_yaml("blog.test", "", "128m"), "node=b").await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    let files = env.bundle_files();
    let cases = [
        (
            "a shared volume and an explicit nest",
            app_yaml("x.test", "nest: other, shared: { corpus: { target: /c } }", "1m"),
            "",
            "shared volume `corpus` is in nest `corpus`, but the bundle's `x-kiwi-nest.nest` is `other`",
        ),
        (
            "--from and a shared volume",
            app_yaml("x.test", "shared: { corpus: { target: /c } }", "1m"),
            "from=blog",
            "`--from` app `blog` is in nest `blog`, but shared volume `corpus` is in nest `corpus`",
        ),
        (
            "--from and an explicit nest",
            app_yaml("x.test", "nest: corpus", "1m"),
            "from=blog",
            "`--from` app `blog` is in nest `blog`, but the bundle's `x-kiwi-nest.nest` is `corpus`",
        ),
    ];
    for (what, yaml, query, want) in cases {
        let (status, msg) = error(env.put("x", &yaml, query).await).await;
        assert_eq!(status, StatusCode::CONFLICT, "{what}");
        assert!(msg.contains(want), "{what}: {msg}");
    }
    assert_eq!(env.apps().await.len(), 1, "nothing was saved");
    assert_eq!(env.bundle_files(), files);
    assert!(env.poll(&mut a).await.apps.is_empty() && env.poll(&mut b).await.apps.is_empty());
    // A redeploy can't move an app to another nest either.
    let (status, msg) = error(env.put("blog", &app_yaml("blog.test", "nest: corpus", "1m"), "").await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("app `blog` is in nest `blog`"), "{msg}");
    // An explicit nest that agrees is fine, and joins it: on its node.
    let acc = env
        .placed("blog-admin", &app_yaml("admin.test", "nest: blog", "1m"), "")
        .await;
    assert_eq!((acc.node.as_str(), acc.nest.as_deref()), ("b", Some("blog")));
}

#[tokio::test]
async fn new_nests_spread_by_memory_and_a_full_nest_warns() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    let mut b = env.join("b", 4 << 30).await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    let mut on = std::collections::BTreeMap::<String, Vec<String>>::new();
    for i in 0..6 {
        let name = format!("app{i}");
        let acc = env
            .placed(&name, &app_yaml(&format!("{name}.test"), "", "512m"), "")
            .await;
        assert_eq!(
            acc.nest.as_deref(),
            Some(name.as_str()),
            "a nest of its own, named after it"
        );
        assert!(acc.warnings.is_empty(), "{:?}", acc.warnings);
        on.entry(acc.node).or_default().push(name);
    }
    assert_eq!(on["a"].len(), 3, "{on:?}");
    assert_eq!(on["b"].len(), 3, "{on:?}");
    // Apps that idle out count for half: four idling 512m apps on b weigh what two always-on ones on a do.
    let nodes = env.nodes().await;
    assert_eq!(nodes[0].headroom_bytes, nodes[1].headroom_bytes);
    assert_eq!(nodes[0].headroom_bytes, (4 << 30) - (512 << 20) - 3 * (512 << 20));
    for i in 0..2 {
        let acc = env
            .placed(
                &format!("idle{i}"),
                &app_yaml(&format!("idle{i}.test"), "idle_timeout: 5m", "512m"),
                "",
            )
            .await;
        assert_eq!(acc.node, if i == 0 { "a" } else { "b" });
    }
    let nodes = env.nodes().await;
    assert_eq!(
        nodes[0].headroom_bytes,
        (4 << 30) - (512 << 20) - 3 * (512 << 20) - (256 << 20)
    );
    // A nest's node takes a new member even when it's full, and the deploy warns.
    let acc = env
        .placed("app0-big", &app_yaml("big.test", "nest: app0", "4g"), "")
        .await;
    let node = on
        .iter()
        .find(|(_, apps)| apps.contains(&"app0".to_string()))
        .unwrap()
        .0;
    assert_eq!(&acc.node, node);
    assert_eq!(acc.warnings.len(), 1, "{:?}", acc.warnings);
    assert!(
        acc.warnings[0].contains(&format!("node `{node}` is out of memory headroom with `app0-big`"))
            && acc.warnings[0].contains("it goes there anyway, as nest `app0` does"),
        "{}",
        acc.warnings[0]
    );
}

#[tokio::test]
async fn a_cordoned_node_gets_no_new_nests_but_keeps_its_own() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    let mut b = env.join("b", 64 << 30).await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    env.placed("blog", &app_yaml("blog.test", "", "128m"), "").await;
    let r = env.post("/nodes/b/cordon").await;
    assert_eq!(r.status(), StatusCode::OK);
    let n: NodeInfo = r.json().await.unwrap();
    assert!(n.cordoned);
    assert_eq!(n.nests, ["blog"]);
    for i in 0..3 {
        let acc = env
            .placed(&format!("new{i}"), &app_yaml(&format!("new{i}.test"), "", "128m"), "")
            .await;
        assert_eq!(acc.node, "a", "b is cordoned, however much room it has");
    }
    // Its nest still takes members.
    let acc = env
        .placed("blog-pr1", &app_yaml("pr1.test", "", "128m"), "from=blog")
        .await;
    assert_eq!(acc.node, "b");
    // --node can't put a new nest on it, as the filter still applies.
    let (status, msg) = error(env.put("forced", &app_yaml("forced.test", "", "1m"), "node=b").await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("node `b` can't take new nest `forced`: cordoned"), "{msg}");
    let (status, msg) = error(env.volume("corpus", "node=b").await).await;
    assert_eq!(status, StatusCode::CONFLICT, "{msg}");
    // With a cordoned too, nothing qualifies, and the deploy says why.
    env.post("/nodes/a/cordon").await;
    let (status, msg) = error(env.put("none", &app_yaml("none.test", "", "1m"), "").await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        msg.contains("no node can take new nest `none`: a (cordoned), b (cordoned)"),
        "{msg}"
    );
    let n: NodeInfo = env.post("/nodes/b/uncordon").await.json().await.unwrap();
    assert!(!n.cordoned);
    assert_eq!(env.placed("none", &app_yaml("none.test", "", "1m"), "").await.node, "b");
    assert_eq!(env.post("/nodes/zz/cordon").await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn placement_follows_arch_labels_and_host_paths() {
    let env = Env::new().await;
    let mut big = env.join("big", 64 << 30).await;
    let mut pi = env
        .join_with(
            "pi",
            NodeFacts {
                arch: "arm64".into(),
                ..facts(2 << 30)
            },
        )
        .await;
    env.poll(&mut big).await;
    env.poll(&mut pi).await;
    // An arm64 app never lands on the amd64 node, however roomy.
    for i in 0..3 {
        let acc = env
            .placed(
                &format!("arm{i}"),
                &app_yaml(&format!("arm{i}.test"), "arch: arm64", "512m"),
                "",
            )
            .await;
        assert_eq!(acc.node, "pi");
    }
    let (status, msg) = error(
        env.put("forced", &app_yaml("forced.test", "arch: arm64", "1m"), "node=big")
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        msg.contains("node `big` can't take new nest `forced`: arch `amd64`, not `arm64`"),
        "{msg}"
    );
    // Nor does one join an arm64 app's nest on the amd64 node.
    env.placed("amd", &app_yaml("amd.test", "", "1m"), "node=big").await;
    let (status, msg) = error(
        env.put("amd-arm", &app_yaml("amd-arm.test", "arch: arm64, nest: amd", "1m"), "")
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        msg.contains("nest `amd` is on node `big`, which can't take this: arch `amd64`"),
        "{msg}"
    );
    // Labels: set with `node label`, and a node's own.
    let (status, msg) = error(
        env.put("ssd", &app_yaml("ssd.test", "node_labels: { disk: ssd }", "1m"), "")
            .await,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(
        msg.contains("big (no label disk=ssd)") && msg.contains("pi (no label disk=ssd)"),
        "{msg}"
    );
    let r = env
        .req(reqwest::Method::POST, "/nodes/pi/labels", &env.admin)
        .json(&json!({"set": {"disk": "ssd"}}))
        .send()
        .await
        .unwrap();
    let n: NodeInfo = r.json().await.unwrap();
    assert_eq!(n.labels.get("disk").map(String::as_str), Some("ssd"));
    assert_eq!(
        env.placed("ssd", &app_yaml("ssd.test", "node_labels: { disk: ssd }", "1m"), "")
            .await
            .node,
        "pi"
    );
    let r = env
        .req(reqwest::Method::POST, "/nodes/pi/labels", &env.admin)
        .json(&json!({"remove": ["disk"]}))
        .send()
        .await
        .unwrap();
    assert!(r.json::<NodeInfo>().await.unwrap().labels.is_empty());
    big.facts.labels.insert("disk".into(), "ssd".into());
    env.poll(&mut big).await;
    assert_eq!(
        env.placed("ssd2", &app_yaml("ssd2.test", "node_labels: { disk: ssd }", "1m"), "")
            .await
            .node,
        "big"
    );
    let r = env
        .req(reqwest::Method::POST, "/nodes/pi/labels", &env.admin)
        .json(&json!({"set": {"bad key": "x"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // Host paths go only where they're allowed.
    pi.facts.allowed_host_paths = vec!["/srv/media".into()];
    env.poll(&mut pi).await;
    let yaml = "x-kiwi-nest: { domain: media.test, service: web, port: 80 }\n\
                services:\n  web:\n    image: x\n    volumes: [\"/srv/media/films:/films:ro\"]\n";
    assert_eq!(env.placed("media", yaml, "").await.node, "pi");
}

async fn poll_status(env: &Env, token: &str) -> (StatusCode, String) {
    let r = env
        .req(reqwest::Method::POST, "/agent/poll", token)
        .json(&PollRequest {
            protocol: kn_proto::agent::PROTOCOL,
            since: 0,
            wait_secs: Some(0),
            report: Report::default(),
        })
        .send()
        .await
        .unwrap();
    (r.status(), r.text().await.unwrap())
}

#[tokio::test]
async fn a_forgotten_node_is_told_so_and_its_names_are_released() {
    let mut env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    let mut b = env.join("b", 4 << 30).await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    env.volume("corpus", "node=a").await;
    env.placed("blog", &app_yaml("blog.test", "", "128m"), "node=a").await;
    env.placed("blog-pr1", &app_yaml("pr1.test", "", "128m"), "from=blog")
        .await;
    // A deploy follows the change: it's still waiting when the node is forgotten.
    let st = env.status("blog", 1).await;
    assert!(!st.done);

    // Online, it's refused without --force; a dry run says what would be released, and changes nothing.
    let (status, msg) = error(env.post("/nodes/a/forget").await).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("is online"), "{msg}");
    let plan: kn_proto::api::Forgot = env.post("/nodes/a/forget?dry_run=true").await.json().await.unwrap();
    assert!(plan.online && plan.dry_run);
    assert_eq!(plan.apps, ["blog", "blog-pr1"]);
    assert_eq!(plan.volumes, ["corpus"]);
    assert_eq!(plan.nests, ["blog", "corpus"]);
    assert_eq!(env.apps().await.len(), 2);
    tokio::time::sleep(Duration::from_millis(1700)).await;
    env.poll(&mut b).await;
    let r = env.post("/nodes/a/forget").await;
    assert_eq!(r.status(), StatusCode::OK);
    let f: kn_proto::api::Forgot = r.json().await.unwrap();
    assert!(!f.online && !f.dry_run);

    // The forgotten node gets an explicit answer, on every route, distinct from an ordinary 401.
    let (status, body) = poll_status(&env, &a.token).await;
    assert_eq!(status, StatusCode::GONE);
    let gone: kn_proto::agent::ForgottenBody = serde_json::from_str(&body).unwrap();
    assert_eq!(gone.error, kn_proto::agent::FORGOTTEN);
    assert_eq!((gone.node_id, gone.name.as_str()), (a.id, "a"));
    let r = env
        .req(reqwest::Method::POST, "/agent/report", &a.token)
        .json(&a.report)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::GONE);
    for token in ["knn_never-issued", &env.admin] {
        let (status, body) = poll_status(&env, token).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert!(!body.contains(kn_proto::agent::FORGOTTEN), "{body}");
    }
    // The waiting deploy is told, rather than left waiting.
    let st = env.status("blog", 1).await;
    assert!(st.done && !st.ok, "{st:?}");
    assert!(st.error.unwrap().contains("was forgotten"));

    // Its names, domains, volumes and nests are free; its tombstone stays out of the listings.
    assert!(env.apps().await.is_empty());
    assert!(env.nests().await.is_empty());
    let nodes = env.nodes().await;
    assert_eq!(nodes.iter().map(|n| n.name.as_str()).collect::<Vec<_>>(), ["b"]);
    let acc = env.placed("blog", &app_yaml("blog.test", "", "128m"), "").await;
    assert_eq!((acc.node.as_str(), acc.generation), ("b", 2));
    assert_eq!(env.volume("corpus", "").await.status(), StatusCode::ACCEPTED);
    let tokens: Vec<kn_proto::api::TokenInfo> = env
        .req(reqwest::Method::GET, "/tokens", &env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        !tokens.iter().any(|t| t.node.as_deref() == Some("a")),
        "its token is revoked"
    );
    assert_eq!(env.post("/nodes/a/forget").await.status(), StatusCode::NOT_FOUND);

    // A fresh join may take its name, as a new node with nothing of the old one's.
    let mut again = env.join("a", 4 << 30).await;
    assert_ne!(again.id, a.id);
    let resp = env.poll(&mut again).await;
    assert!(resp.apps.is_empty() && resp.volumes.is_empty(), "{resp:?}");
    let (status, _) = poll_status(&env, &a.token).await;
    assert_eq!(
        status,
        StatusCode::GONE,
        "the old token still gets the forgotten answer"
    );
    // And all of it survives a restart.
    env.restart().await;
    let (status, _) = poll_status(&env, &a.token).await;
    assert_eq!(status, StatusCode::GONE);
    assert!(env.poll(&mut again).await.apps.is_empty());
}

/// A backup target in a local directory.
fn dir_target(dir: &std::path::Path, every: Option<&str>) -> kn_proto::backup::TargetConfig {
    kn_proto::backup::TargetConfig {
        name: "local".into(),
        s3: None,
        ssh: None,
        dir: Some(kn_proto::backup::DirConfig {
            path: dir.to_string_lossy().into_owned(),
        }),
        prefix: "home/".into(),
        format: kn_proto::backup::Format::Tar,
        every: every.map(str::to_string),
        full_every: None,
        keep: Default::default(),
    }
}

impl Env {
    async fn backup_server(&self) -> kn_proto::backup::BackupResult {
        let r = self
            .req(reqwest::Method::POST, "/backups/server", &self.admin)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{}", r.text().await.unwrap());
        r.json().await.unwrap()
    }

    /// Restores a server backup into a fresh state directory, and returns it.
    async fn restore_server(&self, key: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let targets = kn_backup::target::open_all(self.targets.clone()).unwrap();
        backups::restore(&dir.path().join("server"), &targets, None, key)
            .await
            .unwrap();
        dir
    }
}

/// The apps and bundle references in a database, and whether every bundle it refers to is in `bundles`.
fn restored(state: &std::path::Path) -> (Vec<String>, Vec<String>) {
    let c = rusqlite::Connection::open(state.join("server.sqlite")).unwrap();
    let check: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap();
    assert_eq!(check, "ok");
    let apps: Vec<String> = c
        .prepare("SELECT name FROM apps WHERE state = 'active' ORDER BY name")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let digests: Vec<String> = c
        .prepare(
            "SELECT bundle_digest FROM apps WHERE bundle_digest IS NOT NULL
             UNION SELECT digest FROM bundles",
        )
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let missing: Vec<String> = digests
        .into_iter()
        .filter(|d| {
            !state
                .join("bundles")
                .join(format!("{}.zip", d.replace(':', "-")))
                .exists()
        })
        .collect();
    (apps, missing)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_servers_backup_is_consistent_while_deploys_write() {
    let store = tempfile::tempdir().unwrap();
    let mut env = Env::with_targets(vec![dir_target(store.path(), None)]).await;
    let mut a = env.join("a", 64 << 30).await;
    env.poll(&mut a).await;
    let node_token = a.token.clone();
    // Keep the node online while deploys run.
    let (url, http) = (env.url.clone(), env.http.clone());
    let keepalive = tokio::spawn(async move {
        loop {
            let _ = http
                .post(format!("{url}/agent/report"))
                .bearer_auth(&node_token)
                .json(&Report::default())
                .send()
                .await;
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    });

    // Deploys as fast as they go, each recording when it was saved.
    let saved = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, std::time::Instant)>::new()));
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut writers = Vec::new();
    for w in 0..3 {
        let (url, http, admin, saved, stop) = (
            env.url.clone(),
            env.http.clone(),
            env.admin.clone(),
            saved.clone(),
            stop.clone(),
        );
        writers.push(tokio::spawn(async move {
            let mut i = 0;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let name = format!("app-{w}-{i}");
                let r = http
                    .put(format!("{url}/apps/{name}"))
                    .bearer_auth(&admin)
                    .header("content-type", "application/zip")
                    .body(bundle(&format!("{name}.test")))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(r.status(), StatusCode::ACCEPTED, "{}", r.text().await.unwrap());
                saved.lock().unwrap().push((name, std::time::Instant::now()));
                i += 1;
            }
        }));
    }
    let mut checks = Vec::new();
    for _ in 0..4 {
        tokio::time::sleep(Duration::from_millis(150)).await;
        let began = std::time::Instant::now();
        let before: Vec<String> = saved
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, t)| *t < began)
            .map(|(n, _)| n.clone())
            .collect();
        let r = env.backup_server().await;
        assert!(r.failed.is_empty(), "{r:?}");
        checks.push((r.made[0].key.clone(), before));
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    for w in writers {
        w.await.unwrap();
    }
    keepalive.abort();
    assert!(saved.lock().unwrap().len() > 8, "the deploys should have kept writing");

    for (key, before) in &checks {
        let dir = env.restore_server(key).await;
        let (apps, missing) = restored(&dir.path().join("server"));
        assert!(missing.is_empty(), "{key} lacks bundles {missing:?}");
        for n in before {
            assert!(apps.contains(n), "{key} lacks {n}, deployed before it began");
        }
    }

    // A server started on a restored state has the apps, and its admin token still works.
    let (key, before) = checks.last().unwrap();
    let dir = env.restore_server(key).await;
    env.dir = dir;
    env.restart().await;
    let names: Vec<String> = env.apps().await.into_iter().map(|a| a.name).collect();
    for n in before {
        assert!(names.contains(n), "{n}");
    }
    // Restoring over a database is refused.
    let targets = kn_backup::target::open_all(env.targets.clone()).unwrap();
    let e = backups::restore(&env.dir.path().join("server"), &targets, None, key)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("already has a database"), "{e}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delete_during_the_servers_backup_never_loses_a_bundle_it_refers_to() {
    let store = tempfile::tempdir().unwrap();
    let env = Env::with_targets(vec![dir_target(store.path(), None)]).await;
    let mut a = env.join("a", 8 << 30).await;
    env.poll(&mut a).await;
    for (name, domain) in [("blog", "blog.test"), ("shop", "shop.test")] {
        assert_eq!(
            env.deploy(&env.admin, name, domain, "").await.status(),
            StatusCode::ACCEPTED
        );
        a.applied(name, 1, true, false);
    }
    env.poll(&mut a).await;
    assert_eq!(env.bundle_files(), 2);

    // Right after the database is copied (so the copy refers to both bundles), blog is redeployed with a new
    // bundle (so its old one has nothing referring to it) and shop is deleted, all the way.
    let (url, http, admin, token) = (env.url.clone(), env.http.clone(), env.admin.clone(), a.token.clone());
    let mut report = a.report.clone();
    let hook: Box<dyn FnOnce() + Send> = Box::new(move || {
        tokio::runtime::Handle::current().block_on(async move {
            let r = http
                .put(format!("{url}/apps/blog"))
                .bearer_auth(&admin)
                .header("content-type", "application/zip")
                .body(bundle("blog2.test"))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::ACCEPTED);
            let r = http
                .delete(format!("{url}/apps/shop"))
                .bearer_auth(&admin)
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), StatusCode::ACCEPTED);
            report.applied.apps.insert(
                "shop".into(),
                Outcome {
                    generation: 2,
                    ok: true,
                    error: None,
                    problems: vec![],
                    notes: vec![],
                    deleted: true,
                    data_digest: None,
                    finished_at: 0,
                },
            );
            report.serial = u64::MAX;
            let r = http
                .post(format!("{url}/agent/report"))
                .bearer_auth(&token)
                .json(&report)
                .send()
                .await
                .unwrap();
            assert!(r.status().is_success());
        });
    });
    *env.server.as_ref().unwrap().backups.after_copy.lock().unwrap() = Some(hook);
    let r = env.backup_server().await;
    assert!(r.failed.is_empty(), "{r:?}");
    assert!(r.notes.iter().any(|n| n.contains("2 bundles")), "{r:?}");
    assert!(!r.notes.iter().any(|n| n.contains("couldn't be copied")), "{r:?}");
    // Deletion resumed afterwards: blog's old bundle and shop's are gone, blog's new one is there.
    assert_eq!(env.bundle_files(), 1);

    let dir = env.restore_server(&r.made[0].key).await;
    let (apps, missing) = restored(&dir.path().join("server"));
    assert_eq!(apps, ["blog", "shop"]);
    assert!(missing.is_empty(), "{missing:?}");
}

#[tokio::test]
async fn restores_are_checked_before_anything_is_saved() {
    let store = tempfile::tempdir().unwrap();
    let env = Env::with_targets(vec![dir_target(store.path(), None)]).await;
    let mut a = env.join("a", 8 << 30).await;
    env.poll(&mut a).await;
    // A backup of `blog`, as a node would have made it.
    let key = "apps/blog/20261106T030000Z.tar.zst";
    let t = kn_backup::target::open_all(env.targets.clone()).unwrap();
    let k = kn_backup::Key::parse(key).unwrap();
    t[0].upload(&k, Box::new(|_| Ok(()))).await.unwrap();

    let r = env
        .deploy(&env.admin, "blog", "blog.test", &format!("restore={key}"))
        .await;
    assert_eq!(r.status(), StatusCode::ACCEPTED, "{}", r.text().await.unwrap());
    let resp = env.poll(&mut a).await;
    match &resp.apps[0].want {
        AppWant::Deploy { restore, .. } => {
            let r = restore.as_ref().unwrap();
            assert_eq!((r.target.as_str(), r.key.as_str()), ("local", key));
        }
        w => panic!("{w:?}"),
    }
    // A key that isn't there, a broken one, or one of the wrong kind: refused, and nothing saved.
    for (q, want) in [
        ("restore=apps/blog/20261107T030000Z.tar.zst", StatusCode::NOT_FOUND),
        ("restore=apps/blog/nope", StatusCode::BAD_REQUEST),
        ("restore=volumes/blog/20261106T030000Z.tar.zst", StatusCode::BAD_REQUEST),
        (&format!("restore={key}&target=other"), StatusCode::NOT_FOUND),
        (&format!("restore={key}&from=blog"), StatusCode::BAD_REQUEST),
    ] {
        let r = env.deploy(&env.admin, "copy", "copy.test", q).await;
        assert_eq!(r.status(), want, "{q}: {}", r.text().await.unwrap());
    }
    assert!(!env.apps().await.iter().any(|a| a.name == "copy"));
    // A node from before backups would deploy the app empty, ignoring the restore: refused, and nothing saved.
    let mut old = env
        .join_with(
            "old",
            NodeFacts {
                protocol: 0,
                ..facts(8 << 30)
            },
        )
        .await;
    env.poll(&mut old).await;
    let r = env
        .deploy(&env.admin, "copy", "copy.test", &format!("restore={key}&node=old"))
        .await;
    let (status, msg) = error(r).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("from before backups"), "{msg}");
    assert!(!env.apps().await.iter().any(|a| a.name == "copy"));
    assert_eq!(
        env.deploy(&env.admin, "legacy", "legacy.test", "node=old")
            .await
            .status(),
        StatusCode::ACCEPTED
    );
    let r = env
        .req(reqwest::Method::POST, "/backups/apps/legacy", &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::CONFLICT);
    // A deploy token may restore another app's backup only where it may use that app as --from.
    let t1 = env.token(json!({"kind": "deploy", "patterns": ["blog-pr*"]})).await;
    let r = env.deploy(&t1, "blog-pr1", "pr1.test", &format!("restore={key}")).await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let t2 = env
        .token(json!({"kind": "deploy", "patterns": ["blog-pr*"], "from": ["blog"]}))
        .await;
    let r = env.deploy(&t2, "blog-pr1", "pr1.test", &format!("restore={key}")).await;
    assert_eq!(r.status(), StatusCode::ACCEPTED, "{}", r.text().await.unwrap());
    // And deploy tokens list only their own apps' backups.
    let list: Vec<kn_proto::backup::BackupInfo> = env
        .req(reqwest::Method::GET, "/backups", &t2)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(list.is_empty(), "{list:?}");
    let list: Vec<kn_proto::backup::BackupInfo> = env
        .req(reqwest::Method::GET, "/backups?kind=app&name=blog", &env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!((list[0].key.as_str(), list[0].target.as_str()), (key, "local"));
}

#[tokio::test]
async fn nodes_get_the_contents_of_the_files_targets_name() {
    let files = tempfile::tempdir().unwrap();
    let key = files.path().join("backup-key");
    std::fs::write(&key, "-----BEGIN OPENSSH PRIVATE KEY-----\nk\n").unwrap();
    let kh = files.path().join("known_hosts");
    std::fs::write(&kh, "nas ssh-ed25519 AAAA\n").unwrap();
    let secret = files.path().join("r2-secret");
    std::fs::write(&secret, "s3cret\n").unwrap();
    let ssh = kn_proto::backup::TargetConfig {
        name: "nas".into(),
        dir: None,
        ssh: Some(kn_proto::backup::SshConfig {
            host: "backup@nas".into(),
            port: None,
            path: "/srv/kn".into(),
            identity: Some(key.to_string_lossy().into_owned()),
            known_hosts: Some(kh.to_string_lossy().into_owned()),
            identity_data: None,
            known_hosts_data: None,
        }),
        ..dir_target(files.path(), None)
    };
    let s3 = kn_proto::backup::TargetConfig {
        name: "r2".into(),
        dir: None,
        s3: Some(kn_proto::backup::S3Config {
            endpoint: "https://r2.example.com".into(),
            bucket: "b".into(),
            region: "auto".into(),
            access_key: "a".into(),
            secret_key: None,
            secret_key_file: Some(secret.to_string_lossy().into_owned()),
            virtual_host: false,
        }),
        ..dir_target(files.path(), None)
    };
    let env = Env::with_targets(vec![ssh, s3]).await;
    let mut a = env.join("a", 8 << 30).await;
    env.poll(&mut a).await;
    let r = env
        .req(reqwest::Method::GET, "/agent/backup-targets", &a.token)
        .send()
        .await
        .unwrap();
    let cfgs: Vec<kn_proto::backup::TargetConfig> = r.json().await.unwrap();
    let ssh = cfgs[0].ssh.as_ref().unwrap();
    assert_eq!((ssh.identity.as_deref(), ssh.known_hosts.as_deref()), (None, None));
    assert!(ssh.identity_data.as_deref().unwrap().contains("PRIVATE KEY"));
    assert_eq!(ssh.known_hosts_data.as_deref(), Some("nas ssh-ed25519 AAAA\n"));
    let s3 = cfgs[1].s3.as_ref().unwrap();
    assert_eq!(
        (s3.secret_key.as_deref(), s3.secret_key_file.as_deref()),
        (Some("s3cret"), None)
    );
    // So a node opens them without any of the files.
    drop(files);
    let (ok, broken) = kn_backup::target::open_each(cfgs);
    assert!(broken.is_empty(), "{broken:?}");
    assert_eq!(ok.len(), 2);
}

#[tokio::test]
async fn scheduled_backups_go_to_nodes_and_skip_apps_that_opt_out() {
    let store = tempfile::tempdir().unwrap();
    let env = Env::with_targets(vec![dir_target(store.path(), Some("1h"))]).await;
    let mut a = env.join("a", 8 << 30).await;
    env.poll(&mut a).await;
    assert_eq!(
        env.deploy(&env.admin, "blog", "blog.test", "").await.status(),
        StatusCode::ACCEPTED
    );
    let preview = {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("compose.yaml"),
            "x-kiwi-nest: { domain: pr.test, service: web, port: 80, backups: false }\nservices:\n  web:\n    image: x\n",
        )
        .unwrap();
        kn_bundle::dir::pack(dir.path(), &kn_bundle::Limits::default()).unwrap()
    };
    let r = env
        .req(reqwest::Method::PUT, "/apps/blog-pr1", &env.admin)
        .header("content-type", "application/zip")
        .body(preview)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    a.applied("blog", 1, true, false);
    a.applied("blog-pr1", 1, true, false);
    env.poll(&mut a).await;

    // Within a few checks: a backup command for blog (not the preview), naming the due target; and the server
    // backed up itself.
    let mut got = None;
    for _ in 0..50 {
        let resp = env.poll(&mut a).await;
        if let Some(c) = resp
            .commands
            .into_iter()
            .find(|c| matches!(c.action, Action::Backup { .. }))
        {
            got = Some(c);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let c = got.expect("a scheduled backup");
    assert_eq!(
        c.action,
        Action::Backup {
            what: kn_proto::backup::Kind::App,
            name: "blog".into(),
            targets: vec!["local".into()],
            full: false,
        }
    );
    let t = kn_backup::target::open_all(env.targets.clone()).unwrap();
    let mut server_backups = Vec::new();
    for _ in 0..50 {
        server_backups = t[0].list(Some((kn_proto::backup::Kind::Server, ""))).await.unwrap();
        if !server_backups.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(server_backups.len(), 1);
    // Nodes get the targets, credentials included; the CLI's tokens don't.
    let r = env
        .req(reqwest::Method::GET, "/agent/backup-targets", &a.token)
        .send()
        .await
        .unwrap();
    let cfgs: Vec<kn_proto::backup::TargetConfig> = r.json().await.unwrap();
    assert_eq!(cfgs, env.targets);
    let r = env
        .req(reqwest::Method::GET, "/agent/backup-targets", &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    let _ = CommandResult::Ok { value: json!(null) };
}

const HASH: &str = "$2y$05$abcdefghijklmnopqrstuuG3dNw1n3eO1n2Cx4M3jW7sXkEw0LZ6u";

#[tokio::test]
async fn the_ingress_routes_apps_to_their_nodes_and_the_api_never_shows_hashes() {
    let (caddy, url) = ingress::tests::FakeCaddy::start().await;
    let mut cfg = ingress::IngressConfig::new(url, ingress::Tls::Acme { email: None, ca: None });
    cfg.api_domain = Some("kn.test".into());
    cfg.sync_interval = Duration::from_millis(200);
    let env = Env::with_ingress(Some(cfg)).await;
    caddy.until("the first load", |c| !c.is_null()).await;
    let mut a = env.join("a", 4 << 30).await;
    let mut b = env.join("b", 8 << 30).await;
    assert_ne!(a.ingress_token, b.ingress_token);
    // A node that joined before ingress tokens asks for its own.
    let r = env
        .req(reqwest::Method::GET, "/agent/ingress-token", &a.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let t: kn_proto::agent::IngressToken = r.json().await.unwrap();
    assert_eq!(t.ingress_token, a.ingress_token);
    let r = env
        .req(reqwest::Method::GET, "/agent/ingress-token", &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    // b reports where the ingress reaches it; a doesn't.
    b.facts.ingress = "10.0.0.6:7382".into();
    env.poll(&mut a).await;
    env.poll(&mut b).await;

    // A deploy with basic auth: Caddy gets the route, to b's ingress address with b's token, and the hash.
    let r = env
        .req(reqwest::Method::PUT, "/apps/blog-pr1?node=b", &env.admin)
        .header("content-type", "application/zip")
        .body(bundle_with(
            "blog-pr1.test",
            &format!(", auth: [{{ user: preview, hash: '{HASH}' }}]"),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED, "{}", r.text().await.unwrap());
    caddy
        .until("blog-pr1 is routed", |c| c.to_string().contains("blog-pr1.test"))
        .await;
    let config = caddy.current().to_string();
    assert!(config.contains("10.0.0.6:7382"), "{config}");
    assert!(config.contains(&b.ingress_token), "{config}");
    assert!(config.contains(HASH), "{config}");
    assert!(!config.contains(&a.ingress_token), "{config}");

    // An app on a node the ingress can't reach isn't routed.
    let r = env.deploy(&env.admin, "shop", "shop.test", "node=a").await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!caddy.current().to_string().contains("shop.test"));

    // The API never shows the hashes.
    for path in ["/apps", "/apps/blog-pr1"] {
        let text = env
            .req(reqwest::Method::GET, path, &env.admin)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(!text.contains("$2y$") && !text.contains("preview"), "{path}: {text}");
    }

    // The server's API domain is kept for it.
    let r = env.deploy(&env.admin, "sneaky", "kn.test", "node=b").await;
    let (status, msg) = error(r).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(msg.contains("the server's API domain"), "{msg}");

    // A deleted app's route goes once the delete is saved.
    let r = env
        .req(reqwest::Method::DELETE, "/apps/blog-pr1", &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    caddy
        .until("blog-pr1 isn't routed", |c| !c.to_string().contains("blog-pr1.test"))
        .await;

    // So do the routes of a node that's forgotten.
    let r = env.deploy(&env.admin, "docs", "docs.test", "node=b").await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    caddy
        .until("docs is routed", |c| c.to_string().contains("docs.test"))
        .await;
    let r = env.post("/nodes/b/forget?force=true").await;
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text().await.unwrap());
    caddy
        .until("b's routes are gone", |c| {
            let c = c.to_string();
            !c.contains("docs.test") && !c.contains(&b.ingress_token)
        })
        .await;
}

#[tokio::test]
async fn apps_deployed_before_the_server_kept_auth_get_it_from_their_bundles() {
    let mut env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    let r = env
        .req(reqwest::Method::PUT, "/apps/blog", &env.admin)
        .header("content-type", "application/zip")
        .body(bundle_with(
            "blog.test",
            &format!(", auth: [{{ user: preview, hash: '{HASH}' }}]"),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    let r = env.deploy(&env.admin, "shop", "shop.test", "").await;
    assert_eq!(r.status(), StatusCode::ACCEPTED);
    // As if they were deployed before migration 3; shop's bundle has gone missing too.
    let db = env.dir.path().join("server/server.sqlite");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.execute("UPDATE apps SET auth = NULL", []).unwrap();
    let shop: String = c
        .query_row("SELECT bundle_digest FROM apps WHERE name = 'shop'", [], |r| r.get(0))
        .unwrap();
    drop(c);
    std::fs::remove_file(
        env.dir
            .path()
            .join("server/bundles")
            .join(format!("{}.zip", shop.replace(':', "-"))),
    )
    .unwrap();
    env.restart().await;
    let c = rusqlite::Connection::open(&db).unwrap();
    let auth = |name: &str| -> Option<String> {
        c.query_row("SELECT auth FROM apps WHERE name = ?1", [name], |r| r.get(0))
            .unwrap()
    };
    let blog = auth("blog").expect("read from blog's bundle");
    assert!(blog.contains(HASH), "{blog}");
    assert_eq!(auth("shop"), None, "unknown, so the ingress won't route it");
}

/// The server's own origin, for the web UI's requests.
fn origin(env: &Env) -> String {
    env.url.trim_end_matches(kn_proto::API_PREFIX).to_string()
}

/// A request as the web UI's page makes it: with the session cookie, from the server's own origin, as JSON.
fn ui_req(env: &Env, method: reqwest::Method, path: &str, cookie: &str) -> reqwest::RequestBuilder {
    let mut r = env
        .http
        .request(method.clone(), format!("{}{path}", env.url))
        .header("cookie", format!("{}={cookie}", kn_proto::api::SESSION_COOKIE))
        .header("origin", origin(env))
        .header("sec-fetch-site", "same-origin");
    if method != reqwest::Method::GET {
        r = r.header("content-type", "application/json").body("{}");
    }
    r
}

/// Logs in; returns the session cookie's value.
async fn login(env: &Env, user: &str, password: &str) -> Result<String, (StatusCode, String)> {
    let r = env
        .http
        .post(format!("{}/session", env.url))
        .header("origin", origin(env))
        .json(&json!({"user": user, "password": password}))
        .send()
        .await
        .unwrap();
    if r.status() != StatusCode::OK {
        return Err(error(r).await);
    }
    let cookie = r.headers()["set-cookie"].to_str().unwrap().to_string();
    for part in ["HttpOnly", "Secure", "SameSite=Strict", "Path=/"] {
        assert!(cookie.split("; ").any(|p| p == part), "{part} in {cookie}");
    }
    assert!(!cookie.contains("Domain"), "host-only: {cookie}");
    let value = cookie
        .split(';')
        .next()
        .unwrap()
        .strip_prefix(&format!("{}=", kn_proto::api::SESSION_COOKIE))
        .unwrap();
    assert!(value.starts_with("kns_"));
    Ok(value.to_string())
}

#[tokio::test]
async fn the_web_ui_is_served_and_its_users_log_in() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;

    // The page, with a policy that allows only the server's own scripts, and an unknown API route's error.
    let r = env.http.get(format!("{}/", origin(&env))).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    let csp = r.headers()["content-security-policy"].to_str().unwrap();
    assert!(
        csp.contains("script-src 'self'") && csp.contains("frame-ancestors 'none'"),
        "{csp}"
    );
    assert_eq!(r.headers()["x-frame-options"], "DENY");
    let r = env
        .req(reqwest::Method::GET, "/nothing-here", &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(error(r).await.0, StatusCode::NOT_FOUND);

    // Users are made with an admin token, with passwords long enough.
    let add = |name: &str, password: &str, token: &str| {
        env.req(reqwest::Method::POST, "/users", token)
            .json(&json!({"name": name, "password": password}))
            .send()
    };
    assert_eq!(
        add("ivan", "short", &env.admin).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        add("Ivan", "long enough pw", &env.admin).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        add("ivan", "long enough pw", &env.admin).await.unwrap().status(),
        StatusCode::CREATED
    );
    assert_eq!(
        add("ivan", "another long one", &env.admin).await.unwrap().status(),
        StatusCode::CONFLICT
    );
    let deploy = env.token(json!({"kind": "deploy", "patterns": ["blog-pr*"]})).await;
    assert_eq!(
        add("kim", "long enough pw", &deploy).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );

    // A wrong password, or a user that doesn't exist, is refused alike.
    assert_eq!(
        login(&env, "ivan", "long enough px").await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        login(&env, "nobody", "long enough pw").await.unwrap_err().0,
        StatusCode::UNAUTHORIZED
    );
    let cookie = login(&env, "ivan", "long enough pw").await.unwrap();

    // The session stands for an admin, on the same API as the CLI.
    let r = ui_req(&env, reqwest::Method::GET, "/session", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(r.json::<kn_proto::api::SessionInfo>().await.unwrap().user, "ivan");
    let who: kn_proto::api::WhoAmI = ui_req(&env, reqwest::Method::GET, "/whoami", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        (who.kind, who.user.as_deref()),
        (kn_proto::api::TokenKind::Admin, Some("ivan"))
    );
    let r = ui_req(&env, reqwest::Method::POST, "/nodes/a/cordon", &cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(r.json::<NodeInfo>().await.unwrap().cordoned);
    let users: Vec<kn_proto::api::UserInfo> = ui_req(&env, reqwest::Method::GET, "/users", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(users[0].last_login_at.is_some());

    // With the cookie, nothing from another site or origin is taken, not even reading: an app's page on a sibling
    // domain is "same-site".
    let with_cookie = |method: reqwest::Method, path: &str| {
        env.http
            .request(method, format!("{}{path}", env.url))
            .header("cookie", format!("{}={cookie}", kn_proto::api::SESSION_COOKIE))
    };
    for site in ["cross-site", "same-site"] {
        let r = with_cookie(reqwest::Method::GET, "/nodes")
            .header("sec-fetch-site", site)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{site}");
    }
    let r = with_cookie(reqwest::Method::GET, "/nodes")
        .header("origin", "http://blog.test")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    // A change must say it's from the server's own page, and be JSON.
    let uncordon = || with_cookie(reqwest::Method::POST, "/nodes/a/uncordon");
    let r = uncordon()
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN, "no Origin or Sec-Fetch-Site");
    for ct in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data"),
    ] {
        let mut r = uncordon().header("origin", origin(&env));
        if let Some(ct) = ct {
            r = r.header("content-type", ct);
        }
        assert_eq!(r.send().await.unwrap().status(), StatusCode::BAD_REQUEST, "{ct:?}");
    }
    assert!(
        env.req(reqwest::Method::GET, "/nodes", &env.admin)
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let nodes: Vec<NodeInfo> = env
        .req(reqwest::Method::GET, "/nodes", &env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(nodes[0].cordoned, "none of those uncordoned it");
    // Nor is a login from elsewhere, or not as JSON.
    let r = env
        .http
        .post(format!("{}/session", env.url))
        .json(&json!({"user": "ivan", "password": "long enough pw"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    let r = env
        .http
        .post(format!("{}/session", env.url))
        .header("origin", origin(&env))
        .header("content-type", "application/x-www-form-urlencoded")
        .body("user=ivan&password=long+enough+pw")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    // A made-up session is refused, and a token still works without one.
    let r = ui_req(&env, reqwest::Method::GET, "/nodes", "kns_00")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

    // Logging out ends the session, and clears the cookie.
    let other = login(&env, "ivan", "long enough pw").await.unwrap();
    let r = ui_req(&env, reqwest::Method::DELETE, "/session", &other)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert!(r.headers()["set-cookie"].to_str().unwrap().contains("Max-Age=0"));
    let r = ui_req(&env, reqwest::Method::GET, "/session", &other)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        ui_req(&env, reqwest::Method::GET, "/session", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK,
        "only that session ended"
    );

    // A new password ends the user's sessions.
    let r = env
        .req(reqwest::Method::PUT, "/users/ivan/password", &env.admin)
        .json(&json!({"password": "a new long password"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        ui_req(&env, reqwest::Method::GET, "/session", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(login(&env, "ivan", "long enough pw").await.is_err());
    let cookie = login(&env, "ivan", "a new long password").await.unwrap();

    // Sessions are kept across restarts.
    let mut env = env;
    env.restart().await;
    assert_eq!(
        ui_req(&env, reqwest::Method::GET, "/session", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );

    // After a few failed logins, a name is locked out for a while, even with the right password.
    for _ in 0..5 {
        assert_eq!(
            login(&env, "ivan", "wrong password!").await.unwrap_err().0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        login(&env, "ivan", "a new long password").await.unwrap_err().0,
        StatusCode::TOO_MANY_REQUESTS
    );

    // Deleting the user ends their sessions.
    let r = env
        .req(reqwest::Method::DELETE, "/users/ivan", &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        ui_req(&env, reqwest::Method::GET, "/session", &cookie)
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
}

/// Answers the next command the node gets with `result`; returns its action.
async fn answer(env: &Env, n: &mut FakeNode, result: CommandResult) -> Action {
    let command = loop {
        if let Some(c) = env.poll(n).await.commands.into_iter().next() {
            break c;
        }
    };
    let r = env
        .req(
            reqwest::Method::POST,
            &format!("/agent/commands/{}/result", command.id),
            &n.token,
        )
        .json(&result)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    command.action
}

#[tokio::test]
async fn an_apps_commands_are_its_history() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    env.deploy(&env.admin, "blog", "blog.test", "").await;
    env.poll(&mut a).await;
    a.applied("blog", 1, true, false);
    env.poll(&mut a).await;
    env.req(reqwest::Method::POST, "/users", &env.admin)
        .json(&json!({"name": "ivan", "password": "long enough pw"}))
        .send()
        .await
        .unwrap();
    let cookie = login(&env, "ivan", "long enough pw").await.unwrap();
    let ci = env
        .token(json!({"kind": "deploy", "name": "ci", "patterns": ["blog*"]}))
        .await;
    let mut info = serde_json::to_value(env.apps().await[0].clone()).unwrap();
    // Enough of an AppInfo for the server to read.
    for (k, v) in [
        ("service", json!("web")),
        ("container_port", json!(80)),
        ("idle_timeout_secs", json!(0)),
        ("project", json!("kn-blog")),
        ("bundle_digest", json!("")),
        ("volumes", json!([])),
        ("created_at", json!("")),
        ("containers", json!([])),
    ] {
        info[k] = v;
    }

    // A suspend from the UI, a resume with a deploy token that fails on the node, and an inspect (not listed).
    let suspend = tokio::spawn(ui_req(&env, reqwest::Method::POST, "/apps/blog/suspend", &cookie).send());
    let ok = CommandResult::Ok { value: info.clone() };
    assert_eq!(
        answer(&env, &mut a, ok.clone()).await,
        Action::Suspend { app: "blog".into() }
    );
    assert_eq!(suspend.await.unwrap().unwrap().status(), StatusCode::OK);
    let resume = tokio::spawn(env.req(reqwest::Method::POST, "/apps/blog/resume", &ci).send());
    let failed = CommandResult::Err {
        status: 409,
        error: ErrorBody {
            error: "it won't start".into(),
            problems: vec![],
        },
    };
    answer(&env, &mut a, failed).await;
    assert_eq!(resume.await.unwrap().unwrap().status(), StatusCode::CONFLICT);
    let inspect = tokio::spawn(env.req(reqwest::Method::GET, "/apps/blog", &env.admin).send());
    answer(&env, &mut a, ok).await;
    assert_eq!(inspect.await.unwrap().unwrap().status(), StatusCode::OK);

    let history: Vec<kn_proto::api::CommandInfo> = ui_req(&env, reqwest::Method::GET, "/apps/blog/commands", &cookie)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let seen: Vec<_> = history
        .iter()
        .map(|c| {
            (
                c.action.as_str(),
                c.by.as_str(),
                c.state.as_str(),
                c.ok,
                c.error.as_deref(),
                c.node.as_str(),
            )
        })
        .collect();
    assert_eq!(
        seen,
        vec![
            ("resume", "token ci", "done", Some(false), Some("it won't start"), "a"),
            ("suspend", "user ivan", "done", Some(true), None, "a"),
        ]
    );
    assert!(history.iter().all(|c| c.finished_at.is_some()));
    // `limit` keeps the newest, a deploy token sees only its own apps' history, and a new app of the same name
    // starts with none.
    let one: Vec<kn_proto::api::CommandInfo> = env
        .req(reqwest::Method::GET, "/apps/blog/commands?limit=1", &env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(one.len(), 1);
    assert_eq!(one[0].action, "resume");
    let other = env.token(json!({"kind": "deploy", "patterns": ["shop*"]})).await;
    let r = env
        .req(reqwest::Method::GET, "/apps/blog/commands", &other)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    env.req(reqwest::Method::DELETE, "/apps/blog", &env.admin)
        .send()
        .await
        .unwrap();
    env.poll(&mut a).await;
    a.applied("blog", 2, true, true);
    env.poll(&mut a).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        env.deploy(&env.admin, "blog", "blog.test", "").await.status(),
        StatusCode::ACCEPTED
    );
    let fresh: Vec<kn_proto::api::CommandInfo> = env
        .req(reqwest::Method::GET, "/apps/blog/commands", &env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(fresh.is_empty(), "{fresh:?}");
}

#[tokio::test]
async fn nodes_report_their_space_and_their_apps_activity() {
    let env = Env::new().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    for (name, query) in [("blog", ""), ("blog-pr1", "from=blog"), ("notes", "")] {
        let r = env.deploy(&env.admin, name, &format!("{name}.test"), query).await;
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        env.poll(&mut a).await;
        a.applied(name, 1, true, false);
        env.poll(&mut a).await;
    }
    let space = kn_proto::api::Space {
        bytes: 10 << 20,
        exclusive_bytes: 1 << 20,
    };
    for app in &mut a.report.apps {
        if app.name == "blog" {
            app.last_request_at = Some(1_800_000_000);
            app.open_requests = 2;
            app.space.insert("data".into(), space);
        }
    }
    a.report.usage = Some(kn_proto::agent::Usage {
        measured_at: 1_800_000_100,
        groups: vec![
            kn_proto::agent::UsageGroup {
                apps: vec!["blog".into(), "blog-pr1".into()],
                volumes: vec![],
                bytes: 11 << 20,
            },
            kn_proto::agent::UsageGroup {
                apps: vec!["notes".into()],
                volumes: vec![],
                bytes: 3 << 20,
            },
        ],
        backups: space,
    });
    env.poll(&mut a).await;

    let apps = env.apps().await;
    let blog = apps.iter().find(|x| x.name == "blog").unwrap();
    assert_eq!(blog.last_request_at.as_deref(), Some("2027-01-15T08:00:00Z"));
    assert_eq!(blog.open_requests, 2);
    assert_eq!(blog.space["data"], space);
    assert_eq!(blog.idle_timeout_secs, 0);
    let notes = apps.iter().find(|x| x.name == "notes").unwrap();
    assert_eq!((notes.last_request_at.as_ref(), notes.open_requests), (None, 0));

    let nests: Vec<kn_proto::api::NestInfo> = env
        .req(reqwest::Method::GET, "/nests", &env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let disk: Vec<_> = nests.iter().map(|n| (n.name.as_str(), n.disk_bytes)).collect();
    assert_eq!(disk, vec![("blog", Some(11 << 20)), ("notes", Some(3 << 20))]);
    let nodes: Vec<NodeInfo> = env
        .req(reqwest::Method::GET, "/nodes", &env.admin)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let usage = nodes[0].usage.clone().unwrap();
    assert_eq!(usage.data_bytes, 14 << 20);
    assert_eq!(usage.backup_snapshots, space);
    assert_eq!(usage.measured_at, "2027-01-15T08:01:40Z");
}

#[tokio::test]
async fn older_clis_are_refused_with_a_clear_error() {
    let env = Env::new().await;
    let with_ua = |ua: &str, path: &str| {
        env.req(reqwest::Method::GET, path, &env.admin)
            .header(reqwest::header::USER_AGENT, ua)
    };
    let r = with_ua("kiwi-nest/0.0.1", "/apps").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UPGRADE_REQUIRED);
    let body: ErrorBody = r.json().await.unwrap();
    assert!(
        body.error.contains("the CLI must be at least as new as the server"),
        "{}",
        body.error
    );
    assert!(
        body.error.contains(&format!("kiwi-nest {}", kn_proto::VERSION)),
        "{}",
        body.error
    );
    // The users' routes too; but an old CLI can still ask the server's version.
    let r = with_ua("kiwi-nest/0.0.1", "/users").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::UPGRADE_REQUIRED);
    let r = with_ua("kiwi-nest/0.0.1", "/version").send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    // The same version, a newer one, and other clients are served.
    let newer = format!("kiwi-nest/{}.0.0", Version::current().major + 1);
    for ua in [format!("kiwi-nest/{}", kn_proto::VERSION), newer, "curl/8.0".into()] {
        let r = with_ua(&ua, "/apps").send().await.unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{ua}");
    }
    let r = env.req(reqwest::Method::GET, "/apps", &env.admin).send().await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
}

use kn_proto::version::Version;

impl Env {
    async fn upgrade_status(&self) -> kn_proto::api::UpgradeStatus {
        let r = self
            .req(reqwest::Method::GET, "/upgrade", &self.admin)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        r.json().await.unwrap()
    }

    /// Polls `n` until it's given a command, which it then lists as running.
    async fn next_command(&self, n: &mut FakeNode) -> kn_proto::agent::Command {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let mut resp = self.poll(n).await;
            if let Some(c) = resp.commands.pop() {
                n.report.running_commands = vec![c.id];
                return c;
            }
            assert!(tokio::time::Instant::now() < deadline, "no command came");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Polls `n` for a while, checking it's given no command.
    async fn no_command(&self, n: &mut FakeNode) {
        for _ in 0..6 {
            assert!(self.poll(n).await.commands.is_empty(), "a command came");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn answer(&self, n: &mut FakeNode, id: u64, result: CommandResult) {
        let r = self
            .req(reqwest::Method::POST, &format!("/agent/commands/{id}/result"), &n.token)
            .json(&result)
            .send()
            .await
            .unwrap();
        assert!(r.status().is_success());
        n.report.running_commands.clear();
    }

    async fn until_rollout(&self, what: &str, pred: impl Fn(&kn_proto::api::UpgradeStatus) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let st = self.upgrade_status().await;
            if pred(&st) {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for {what}: {st:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn state_of(st: &kn_proto::api::UpgradeStatus, node: &str) -> kn_proto::api::UpgradeState {
    st.nodes.iter().find(|n| n.node == node).unwrap().state
}

#[tokio::test]
async fn the_server_upgrades_older_nodes_one_at_a_time_and_a_failure_halts_it() {
    use kn_proto::api::UpgradeState::*;
    let mut env = Env::new().await;
    let release = env.dir.path().join("release");
    std::fs::write(&release, b"the new kiwi-nest").unwrap();
    let sha256 = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(b"the new kiwi-nest"));
    env.releases = vec![upgrades::Offer {
        arch: "amd64".into(),
        path: release,
        sha256: sha256.clone(),
        size: 17,
    }];
    env.restart().await;

    let old = |protocol: u32, self_upgrade: bool| NodeFacts {
        version: "0.0.1".into(),
        protocol,
        self_upgrade,
        ..facts(4 << 30)
    };
    let mut a = env.join_with("a", old(kn_proto::agent::PROTOCOL, true)).await;
    let mut b = env.join_with("b", old(kn_proto::agent::PROTOCOL, true)).await;
    let mut c = env
        .join_with("c", old(kn_proto::agent::UPGRADE_PROTOCOL - 1, true))
        .await;
    let mut d = env.join_with("d", old(kn_proto::agent::PROTOCOL, false)).await;
    let mut e = env.join("e", 4 << 30).await;
    for n in [&mut c, &mut d, &mut e] {
        env.poll(n).await;
    }

    // `a` first, and `b` waits its turn.
    let cmd = env.next_command(&mut a).await;
    let Action::Upgrade {
        version,
        sha256: sum,
        size,
    } = cmd.action.clone()
    else {
        panic!("{cmd:?}")
    };
    assert_eq!(
        (version.as_str(), sum.as_str(), size),
        (kn_proto::VERSION, sha256.as_str(), 17)
    );
    env.no_command(&mut b).await;
    let st = env.upgrade_status().await;
    assert_eq!(st.version, kn_proto::VERSION);
    assert_eq!(st.arches, ["amd64"]);
    assert_eq!(
        ["a", "b", "c", "d", "e"].map(|n| state_of(&st, n)),
        [Upgrading, Pending, Manual, Manual, Current]
    );
    // It downloads the release (only the server's own version), and answers.
    let r = env
        .req(
            reqwest::Method::GET,
            &format!("/agent/releases/{version}/amd64"),
            &a.token,
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.bytes().await.unwrap().as_ref(), b"the new kiwi-nest");
    for path in [
        "/agent/releases/0.0.1/amd64".to_string(),
        format!("/agent/releases/{version}/arm64"),
    ] {
        let r = env.req(reqwest::Method::GET, &path, &a.token).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "{path}");
    }
    let ok = CommandResult::Ok {
        value: json!({ "version": kn_proto::VERSION, "restarting": true }),
    };
    env.answer(&mut a, cmd.id, ok.clone()).await;
    // It comes back running the server's version: then it's `b`'s turn.
    a.facts.version = kn_proto::VERSION.into();
    env.poll(&mut a).await;
    let cmd = env.next_command(&mut b).await;
    assert_eq!(state_of(&env.upgrade_status().await, "a"), Current);

    // `b` refuses: the rollout halts.
    let refused = CommandResult::Err {
        status: 500,
        error: ErrorBody {
            error: "release 9 doesn't match its checksum".into(),
            problems: vec![],
        },
    };
    env.answer(&mut b, cmd.id, refused).await;
    env.until_rollout("b to fail", |st| st.halted && state_of(st, "b") == Failed)
        .await;
    let st = env.upgrade_status().await;
    let detail = st.nodes.iter().find(|n| n.node == "b").unwrap().detail.clone().unwrap();
    assert!(detail.contains("doesn't match its checksum"), "{detail}");
    env.no_command(&mut b).await;

    // Carrying on tries it again; this time it says it restarts, but never comes back running the new release.
    let r = env
        .req(reqwest::Method::POST, "/upgrade", &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let cmd = env.next_command(&mut b).await;
    env.answer(&mut b, cmd.id, ok.clone()).await;
    for _ in 0..30 {
        env.poll(&mut b).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    env.until_rollout("b not to come back", |st| st.halted).await;
    let st = env.upgrade_status().await;
    let detail = st.nodes.iter().find(|n| n.node == "b").unwrap().detail.clone().unwrap();
    assert!(detail.contains("didn't come back"), "{detail}");

    // Again, and it does.
    env.req(reqwest::Method::POST, "/upgrade", &env.admin)
        .send()
        .await
        .unwrap();
    let cmd = env.next_command(&mut b).await;
    env.answer(&mut b, cmd.id, ok).await;
    b.facts.version = kn_proto::VERSION.into();
    env.poll(&mut b).await;
    env.until_rollout("b to be upgraded", |st| !st.halted && state_of(st, "b") == Current)
        .await;
    // Those that can't be upgraded by the server get nothing.
    env.no_command(&mut c).await;
    env.no_command(&mut d).await;
    let st = env.upgrade_status().await;
    let detail = |n: &str| st.nodes.iter().find(|x| x.node == n).unwrap().detail.clone().unwrap();
    assert!(detail("c").contains("before upgrades"), "{}", detail("c"));
    assert!(detail("d").contains("installed release"), "{}", detail("d"));

    // Deploy tokens can't see or start the rollout.
    let deploy = env.token(json!({"kind": "deploy", "patterns": ["x*"]})).await;
    for m in [reqwest::Method::GET, reqwest::Method::POST] {
        let r = env.req(m, "/upgrade", &deploy).send().await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);
    }
}

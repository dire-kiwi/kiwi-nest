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

struct Env {
    dir: tempfile::TempDir,
    url: String,
    admin: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    http: reqwest::Client,
}

fn config(dir: &std::path::Path) -> ServerConfig {
    let mut cfg = ServerConfig::new(dir.join("server"));
    cfg.poll_timeout = Duration::from_millis(500);
    cfg.offline_after = Duration::from_millis(1500);
    cfg.command_timeout = Duration::from_secs(10);
    cfg
}

impl Env {
    async fn new() -> Env {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tempfile::tempdir().unwrap();
        let mut env = Env {
            dir,
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
        let (server, first) = Server::open(config(self.dir.path())).await.unwrap();
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

    fn req(&self, method: reqwest::Method, path: &str, token: &str) -> reqwest::RequestBuilder {
        self.http
            .request(method, format!("{}{path}", self.url))
            .bearer_auth(token)
    }

    async fn token(&self, body: serde_json::Value) -> String {
        let r = self
            .req(reqwest::Method::POST, "/tokens", &self.admin)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK, "{}", r.text().await.unwrap());
        r.json::<CreatedToken>().await.unwrap().token
    }

    async fn join(&self, name: &str, memory: u64) -> FakeNode {
        let join = self.token(json!({"kind": "join"})).await;
        let r = self
            .http
            .post(format!("{}/agent/join", self.url))
            .json(&JoinRequest {
                token: join.clone(),
                name: name.into(),
                facts: facts(memory),
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
                facts: facts(memory),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(again.status(), StatusCode::UNAUTHORIZED);
        FakeNode {
            token: j.token,
            memory,
            since: 0,
            report: Report {
                facts: facts(memory),
                ..Default::default()
            },
        }
    }

    async fn poll(&self, n: &mut FakeNode) -> PollResponse {
        n.report.facts = facts(n.memory);
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

    async fn deploy(&self, token: &str, name: &str, domain: &str, query: &str) -> reqwest::Response {
        self.req(reqwest::Method::PUT, &format!("/apps/{name}?{query}"), token)
            .header("content-type", "application/zip")
            .body(bundle(domain))
            .send()
            .await
            .unwrap()
    }

    async fn apps(&self) -> Vec<AppSummary> {
        self.req(reqwest::Method::GET, "/apps", &self.admin)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn status(&self, name: &str, generation: u64) -> ChangeStatus {
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

struct FakeNode {
    token: String,
    memory: u64,
    since: u64,
    report: Report,
}

impl FakeNode {
    /// Records that a change is done, and the app as the node has it.
    fn applied(&mut self, name: &str, generation: u64, ok: bool, deleted: bool) {
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
            });
        }
    }
}

fn facts(memory: u64) -> NodeFacts {
    NodeFacts {
        arch: "amd64".into(),
        memory_bytes: memory,
        memory_available_bytes: memory,
        version: kn_proto::VERSION.into(),
        commit: "test".into(),
        ..Default::default()
    }
}

/// A bundle ZIP with one service at `domain`.
fn bundle(domain: &str) -> Vec<u8> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("compose.yaml"),
        format!(
            "x-kiwi-nest: {{ domain: {domain}, service: web, port: 80 }}\n\
             services:\n  web:\n    image: x\n    mem_limit: 128m\n"
        ),
    )
    .unwrap();
    kn_bundle::dir::pack(dir.path(), &kn_bundle::Limits::default()).unwrap()
}

async fn error(r: reqwest::Response) -> (StatusCode, String) {
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
        notes: vec![],
        created_at: String::new(),
        updated_at: String::new(),
        containers: vec![],
        open_requests: 0,
        node: None,
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
        memory: a.memory,
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

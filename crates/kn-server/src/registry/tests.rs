//! The registry through the server: the credentials the ingress has checked, the nodes' credentials, the platform
//! check on deploy, and garbage collection, against a fake registry (its HTTP API, in memory) and a fake container.

use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, StatusCode as AxumStatus, Uri};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use futures_util::future::BoxFuture;
use kn_proto::api::{Accepted, GcReport};
use reqwest::StatusCode;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::container::Container;
use super::*;
use crate::ingress::{IngressConfig, Tls};
use crate::tests::{Env, FakeNode, bundle_image, error, facts};

const HOST: &str = "registry.test";

/// The registry's HTTP API, in memory.
#[derive(Default)]
struct Fake {
    /// repository → tag → digest.
    tags: BTreeMap<String, BTreeMap<String, String>>,
    /// (repository, digest) → (media type, body).
    manifests: HashMap<(String, String), (String, Vec<u8>)>,
    blobs: HashMap<String, Vec<u8>>,
}

fn digest_of(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

#[derive(Clone, Default)]
struct FakeRegistry(Arc<Mutex<Fake>>);

impl FakeRegistry {
    fn put_manifest(&self, repo: &str, tag: &str, media_type: &str, body: serde_json::Value) -> String {
        let bytes = serde_json::to_vec(&body).unwrap();
        let d = digest_of(&bytes);
        let mut f = self.0.lock().unwrap();
        f.manifests.insert((repo.into(), d.clone()), (media_type.into(), bytes));
        f.tags.entry(repo.into()).or_default().insert(tag.into(), d.clone());
        d
    }

    /// An image for one architecture.
    fn image(&self, repo: &str, tag: &str, arch: &str) -> String {
        // Each image is its own.
        let config =
            serde_json::to_vec(&json!({ "architecture": arch, "os": "linux", "created": format!("{repo}:{tag}") }))
                .unwrap();
        let cd = digest_of(&config);
        self.0.lock().unwrap().blobs.insert(cd.clone(), config);
        self.put_manifest(
            repo,
            tag,
            "application/vnd.oci.image.manifest.v1+json",
            json!({
                "schemaVersion": 2,
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "config": { "mediaType": "application/vnd.oci.image.config.v1+json", "digest": cd, "size": 1 },
                "layers": [],
            }),
        )
    }

    /// An index of images for several architectures, with an attestation.
    fn index(&self, repo: &str, tag: &str, archs: &[&str]) -> String {
        let mut manifests: Vec<serde_json::Value> = archs
            .iter()
            .map(|a| {
                json!({
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": digest_of(a.as_bytes()),
                    "size": 1,
                    "platform": { "architecture": a, "os": "linux" },
                })
            })
            .collect();
        manifests.push(json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": digest_of(b"attestation"),
            "size": 1,
            "platform": { "architecture": "unknown", "os": "unknown" },
        }));
        self.put_manifest(
            repo,
            tag,
            "application/vnd.oci.image.index.v1+json",
            json!({ "schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": manifests }),
        )
    }

    /// Another tag for a manifest.
    fn alias(&self, repo: &str, tag: &str, digest: &str) {
        let mut f = self.0.lock().unwrap();
        f.tags.entry(repo.into()).or_default().insert(tag.into(), digest.into());
    }

    fn tags(&self, repo: &str) -> Vec<String> {
        let f = self.0.lock().unwrap();
        f.tags
            .get(repo)
            .map(|t| t.keys().cloned().collect())
            .unwrap_or_default()
    }

    async fn serve(&self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let this = self.clone();
        let app = axum::Router::new().fallback(move |method: Method, uri: Uri| {
            let this = this.clone();
            async move { this.handle(method, uri) }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    }

    fn handle(&self, method: Method, uri: Uri) -> Response {
        let mut f = self.0.lock().unwrap();
        let path = uri.path();
        if path == "/v2/" {
            return axum::Json(json!({})).into_response();
        }
        if path == "/v2/_catalog" {
            let repos: Vec<&String> = f.tags.iter().filter(|(_, t)| !t.is_empty()).map(|(r, _)| r).collect();
            return axum::Json(json!({ "repositories": repos })).into_response();
        }
        let rest = path.strip_prefix("/v2/").unwrap_or("");
        if let Some(repo) = rest.strip_suffix("/tags/list") {
            let tags: Vec<String> = f
                .tags
                .get(repo)
                .map(|t| t.keys().cloned().collect())
                .unwrap_or_default();
            return axum::Json(json!({ "name": repo, "tags": tags })).into_response();
        }
        if let Some((repo, reference)) = rest.rsplit_once("/manifests/") {
            let digest = if refs::valid_digest(reference) {
                Some(reference.to_string())
            } else {
                f.tags.get(repo).and_then(|t| t.get(reference)).cloned()
            };
            let Some(d) = digest.filter(|d| f.manifests.contains_key(&(repo.to_string(), d.clone()))) else {
                return AxumStatus::NOT_FOUND.into_response();
            };
            if method == Method::DELETE {
                f.manifests.remove(&(repo.to_string(), d.clone()));
                if let Some(t) = f.tags.get_mut(repo) {
                    t.retain(|_, x| x != &d);
                }
                return AxumStatus::ACCEPTED.into_response();
            }
            let (media, body) = f.manifests[&(repo.to_string(), d.clone())].clone();
            let mut headers = HeaderMap::new();
            headers.insert("content-type", media.parse().unwrap());
            headers.insert("docker-content-digest", d.parse().unwrap());
            if method == Method::HEAD {
                return (headers, Bytes::new()).into_response();
            }
            return (headers, body).into_response();
        }
        if let Some((_, digest)) = rest.rsplit_once("/blobs/")
            && let Some(b) = f.blobs.get(digest)
        {
            return b.clone().into_response();
        }
        AxumStatus::NOT_FOUND.into_response()
    }
}

/// The registry's container, recording what it's asked to do.
#[derive(Default)]
struct FakeContainer {
    calls: Mutex<Vec<String>>,
    /// How long collecting takes; when it isn't instant, its end is recorded as `collected`.
    collect_takes: Duration,
}

impl FakeContainer {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn call(&self, c: &str) -> BoxFuture<'_, anyhow::Result<()>> {
        self.calls.lock().unwrap().push(c.into());
        Box::pin(async { Ok(()) })
    }
}

impl Container for FakeContainer {
    fn ensure(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        self.call("ensure")
    }

    fn stop(&self) -> BoxFuture<'_, anyhow::Result<()>> {
        self.call("stop")
    }

    fn collect(&self, delete_untagged: bool) -> BoxFuture<'_, anyhow::Result<String>> {
        let c = if delete_untagged {
            "collect --delete-untagged"
        } else {
            "collect"
        };
        self.calls.lock().unwrap().push(c.into());
        Box::pin(async move {
            if !self.collect_takes.is_zero() {
                tokio::time::sleep(self.collect_takes).await;
                self.calls.lock().unwrap().push("collected".into());
            }
            Ok("0 blobs marked, 0 blobs and 0 manifests eligible for deletion".into())
        })
    }
}

struct Setup {
    env: Env,
    registry: FakeRegistry,
    container: Arc<FakeContainer>,
}

async fn setup() -> Setup {
    setup_with(Duration::from_secs(30), FakeContainer::default()).await
}

/// With the registry checked every `check_interval`, and `container` for its container.
async fn setup_with(check_interval: Duration, container: FakeContainer) -> Setup {
    let registry = FakeRegistry::default();
    let addr = registry.serve().await;
    let mut cfg = RegistryConfig::new(HOST);
    cfg.listen = addr;
    cfg.idle_wait = Duration::from_millis(500);
    cfg.check_interval = check_interval;
    let container = Arc::new(container);
    // Caddy isn't there; the ingress just keeps trying.
    let ingress = IngressConfig::new("http://127.0.0.1:9", Tls::Http);
    let env = Env::with_registry(Some(ingress), Some((cfg, container.clone()))).await;
    Setup {
        env,
        registry,
        container,
    }
}

fn basic(password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("someone:{password}"))
    )
}

/// What the server answers the ingress about a request.
async fn ask(env: &Env, authorization: Option<&str>, method: &str, uri: &str) -> (StatusCode, String) {
    let mut req = env
        .http
        .get(format!("{}/registry/auth", env.url))
        .header("x-forwarded-method", method)
        .header("x-forwarded-uri", uri);
    if let Some(a) = authorization {
        req = req.header("authorization", a);
    }
    let r = req.send().await.unwrap();
    let status = r.status();
    let challenge = r
        .headers()
        .get("www-authenticate")
        .map(|v| v.to_str().unwrap().to_string())
        .unwrap_or_default();
    let body = r.text().await.unwrap();
    (status, if challenge.is_empty() { body } else { challenge })
}

async fn gc(env: &Env, query: &str) -> GcReport {
    let r = env
        .req(reqwest::Method::POST, &format!("/registry/gc?{query}"), &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK, "{}", r.text().await.unwrap());
    r.json().await.unwrap()
}

fn names(tags: &[kn_proto::api::GcTag]) -> Vec<String> {
    let mut v: Vec<String> = tags.iter().map(|t| format!("{}:{}", t.repository, t.tag)).collect();
    v.sort();
    v
}

fn why(r: &GcReport, name: &str) -> String {
    r.kept
        .iter()
        .find(|t| format!("{}:{}", t.repository, t.tag) == name)
        .map(|t| t.why.clone())
        .unwrap_or_else(|| panic!("{name} should be kept: {r:?}"))
}

/// Deploys `name` with `image` from the registry, and has its node report it applied.
async fn deploy(env: &Env, node: &mut FakeNode, name: &str, image: &str) {
    let r = env
        .req(
            reqwest::Method::PUT,
            &format!("/apps/{name}?node={}", node_name(node)),
            &env.admin,
        )
        .header("content-type", "application/zip")
        .body(bundle_image(&format!("{name}.test"), "", image))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::ACCEPTED, "{}", r.text().await.unwrap());
    let acc: Accepted = r.json().await.unwrap();
    env.poll(node).await;
    node.applied(name, acc.generation, true, false);
    env.poll(node).await;
}

fn node_name(n: &FakeNode) -> String {
    n.registry
        .as_ref()
        .unwrap()
        .username
        .trim_start_matches("node-")
        .to_string()
}

#[tokio::test]
async fn the_server_checks_every_request_to_the_registry() {
    let Setup { env, .. } = setup().await;
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    let login = a
        .registry
        .clone()
        .expect("a node gets a registry credential when it joins");
    assert_eq!(login.host, HOST);
    assert!(login.password.starts_with("knr_") && login.password.len() == 68);
    let node = basic(&login.password);
    let deploy = basic(&env.token(json!({"kind": "deploy", "patterns": ["blog-pr*"]})).await);
    let admin = basic(&env.admin);
    let manifest = "/v2/team/blog/manifests/3";

    // Without a credential, or with a wrong one, a client is asked to log in.
    let (s, challenge) = ask(&env, None, "GET", "/v2/").await;
    assert_eq!(
        (s, challenge.as_str()),
        (StatusCode::UNAUTHORIZED, "Basic realm=\"kiwi-nest registry\"")
    );
    assert_eq!(
        ask(&env, Some(&basic("knd_nope")), "GET", "/v2/").await.0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        ask(&env, Some(&format!("Bearer {}", env.admin)), "GET", "/v2/").await.0,
        StatusCode::UNAUTHORIZED
    );
    // A node's token isn't a registry credential.
    assert_eq!(
        ask(&env, Some(&basic(&a.token)), "GET", "/v2/").await.0,
        StatusCode::UNAUTHORIZED
    );

    // A node may pull, and nothing else.
    for (method, uri) in [
        ("GET", "/v2/"),
        ("HEAD", manifest),
        ("GET", manifest),
        ("GET", "/v2/team/blog/blobs/sha256:00"),
    ] {
        assert_eq!(
            ask(&env, Some(&node), method, uri).await.0,
            StatusCode::OK,
            "{method} {uri}"
        );
    }
    for (method, uri) in [
        ("PUT", manifest),
        ("POST", "/v2/team/blog/blobs/uploads/"),
        ("PATCH", "/v2/team/blog/blobs/uploads/abc"),
        ("PUT", "/v2/team/blog/blobs/uploads/abc?digest=sha256:00"),
        ("DELETE", manifest),
    ] {
        let (s, body) = ask(&env, Some(&node), method, uri).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{method} {uri}");
        assert!(body.contains("DENIED") && body.contains("may only pull"), "{body}");
    }
    // A deploy token may pull and push, but not delete.
    for (method, uri) in [
        ("GET", manifest),
        ("POST", "/v2/team/blog/blobs/uploads/"),
        ("PUT", manifest),
    ] {
        assert_eq!(
            ask(&env, Some(&deploy), method, uri).await.0,
            StatusCode::OK,
            "{method} {uri}"
        );
    }
    assert_eq!(
        ask(&env, Some(&deploy), "DELETE", manifest).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(ask(&env, Some(&admin), "DELETE", manifest).await.0, StatusCode::OK);
    // Only the registry's API.
    assert_eq!(
        ask(&env, Some(&admin), "GET", "/debug/vars").await.0,
        StatusCode::FORBIDDEN
    );

    // Every start replaces the node's credential.
    let r = env
        .req(reqwest::Method::POST, "/agent/registry", &a.token)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let again: kn_proto::agent::RegistryCredential = r.json().await.unwrap();
    let again = again.registry.unwrap();
    assert_ne!(again.password, login.password);
    assert_eq!(ask(&env, Some(&node), "GET", "/v2/").await.0, StatusCode::UNAUTHORIZED);
    let node = basic(&again.password);
    assert_eq!(ask(&env, Some(&node), "GET", "/v2/").await.0, StatusCode::OK);
    // A forgotten node's credential is no good.
    let r = env
        .req(reqwest::Method::POST, "/nodes/a/forget?force=true", &env.admin)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(ask(&env, Some(&node), "GET", "/v2/").await.0, StatusCode::UNAUTHORIZED);

    // The CLI learns the registry's host; the registry's domain is kept from apps.
    let r = env
        .req(reqwest::Method::GET, "/registry", &env.admin)
        .send()
        .await
        .unwrap();
    let info: kn_proto::api::RegistryInfo = r.json().await.unwrap();
    assert_eq!(info.host, HOST);
    let mut b = env.join("b", 4 << 30).await;
    env.poll(&mut b).await;
    let (s, msg) = error(env.deploy(&env.admin, "squat", HOST, "").await).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(msg.contains("image registry"), "{msg}");
}

#[tokio::test]
async fn deploys_check_the_platforms_of_images_the_registry_holds() {
    let Setup { env, registry, .. } = setup().await;
    registry.image("app", "amd64", "amd64");
    registry.index("app", "multi", &["amd64", "arm64"]);
    let mut a = env.join("a", 4 << 30).await;
    let mut arm = facts(8 << 30);
    arm.arch = "arm64".into();
    let mut b = env.join_with("b", arm).await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    let put = |name: &str, image: &str, query: &str| {
        env.req(reqwest::Method::PUT, &format!("/apps/{name}?{query}"), &env.admin)
            .header("content-type", "application/zip")
            .body(bundle_image(&format!("{name}.test"), "", image))
            .send()
    };
    let amd64 = format!("{HOST}/app:amd64");
    let (s, msg) = error(put("x", &amd64, "node=b").await.unwrap()).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(
        msg.contains(&format!("arch `arm64`, and image `{amd64}` is only built for amd64")),
        "{msg}"
    );
    // A new nest goes where its images run, though b has more memory.
    let acc: Accepted = put("x", &amd64, "").await.unwrap().json().await.unwrap();
    assert_eq!(acc.node, "a");
    let acc: Accepted = put("y", &format!("{HOST}/app:multi"), "")
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(acc.node, "b");
    // Images the registry doesn't hold, and images elsewhere, aren't checked.
    for (name, image) in [
        ("z", format!("{HOST}/app:missing")),
        ("w", "other.example.com/app:1".to_string()),
    ] {
        let r = put(name, &image, "node=b").await.unwrap();
        assert_eq!(r.status(), StatusCode::ACCEPTED, "{image}");
    }
    // A redeploy on b can't switch to an amd64-only image.
    let (s, msg) = error(put("y", &amd64, "").await.unwrap()).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(msg.contains("nest `y` is on node `b`, which can't take this"), "{msg}");
}

#[tokio::test]
async fn garbage_collection_keeps_what_any_node_uses() {
    let Setup {
        env,
        registry,
        container,
    } = setup().await;
    let one = registry.image("app", "1", "amd64");
    let two = registry.image("app", "2", "amd64");
    registry.alias("app", "2-alias", &two);
    registry.image("app", "3", "amd64");
    registry.image("other", "1", "amd64");
    let mut a = env.join("a", 4 << 30).await;
    let mut b = env.join("b", 4 << 30).await;
    env.poll(&mut a).await;
    env.poll(&mut b).await;
    deploy(&env, &mut a, "one", &format!("{HOST}/app:1")).await;
    // Used only on node b.
    deploy(&env, &mut b, "two", &format!("{HOST}/app:2")).await;

    // Everything is new to garbage collection, so it's all within the grace period.
    let r = gc(&env, "dry_run=true").await;
    assert!(r.deleted.is_empty() && r.dry_run, "{r:?}");
    assert_eq!(why(&r, "app:3"), "first seen just now");

    let r = gc(&env, "grace_secs=0").await;
    assert_eq!(names(&r.deleted), vec!["app:3", "other:1"]);
    assert_eq!(why(&r, "app:1"), "used by app one on node a");
    assert_eq!(why(&r, "app:2"), "used by app two on node b");
    assert_eq!(
        why(&r, "app:2-alias"),
        "shares its manifest with app:2, which is used by app two on node b"
    );
    assert!(r.collected && r.postponed.is_none(), "{r:?}");
    assert_eq!(registry.tags("app"), vec!["1", "2", "2-alias"]);
    assert!(registry.tags("other").is_empty());
    // Stopped, collected, started again.
    let calls = container.calls();
    let n = calls.len();
    assert_eq!(calls[n - 3..], ["stop", "collect --delete-untagged", "ensure"]);

    // Nothing more to delete, so the registry isn't stopped again.
    let r = gc(&env, "grace_secs=0").await;
    assert!(r.deleted.is_empty() && !r.collected, "{r:?}");
    assert_eq!(container.calls().len(), n);
    assert!(!one.is_empty());
}

#[tokio::test]
async fn garbage_collection_keeps_what_was_pushed_or_released_within_the_grace_period() {
    let Setup { env, registry, .. } = setup().await;
    registry.image("app", "1", "amd64");
    registry.image("app", "2", "amd64");
    registry.image("app", "3", "amd64");
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    // Garbage collection has seen them all.
    let r = gc(&env, "grace_secs=3600").await;
    assert_eq!(why(&r, "app:3"), "first seen just now");
    // An app uses app:1, and is deleted: the image is released.
    deploy(&env, &mut a, "one", &format!("{HOST}/app:1")).await;
    let r = env
        .req(reqwest::Method::DELETE, "/apps/one", &env.admin)
        .send()
        .await
        .unwrap();
    let acc: Accepted = r.json().await.unwrap();
    env.poll(&mut a).await;
    a.applied("one", acc.generation, true, true);
    env.poll(&mut a).await;
    // app:2 is pushed through the ingress.
    let deploy_token = basic(&env.token(json!({"kind": "deploy", "patterns": ["*"]})).await);
    assert_eq!(
        ask(&env, Some(&deploy_token), "PUT", "/v2/app/manifests/2").await.0,
        StatusCode::OK
    );

    let r = gc(&env, "grace_secs=3600").await;
    assert!(r.deleted.is_empty(), "{r:?}");
    assert_eq!(why(&r, "app:1"), "released just now");
    assert_eq!(why(&r, "app:2"), "pushed just now");
    let r = gc(&env, "grace_secs=0").await;
    assert_eq!(names(&r.deleted), vec!["app:1", "app:2", "app:3"]);
}

#[tokio::test]
async fn the_registry_isnt_stopped_while_an_app_may_be_pulling() {
    let Setup {
        env,
        registry,
        container,
    } = setup().await;
    registry.image("app", "1", "amd64");
    registry.image("app", "2", "amd64");
    let mut a = env.join("a", 4 << 30).await;
    env.poll(&mut a).await;
    deploy(&env, &mut a, "one", &format!("{HOST}/app:1")).await;
    // The node says the app is starting (a request woke it, say).
    a.report.apps[0].starting = true;
    env.poll(&mut a).await;
    let r = gc(&env, "grace_secs=0").await;
    assert_eq!(names(&r.deleted), vec!["app:2"]);
    assert!(!r.collected, "{r:?}");
    assert!(
        r.postponed
            .as_deref()
            .unwrap_or("")
            .contains("app `one` is starting on node `a`"),
        "{r:?}"
    );
    assert!(!container.calls().contains(&"stop".to_string()));

    // A deploy on its way to the node counts too.
    a.report.apps[0].starting = false;
    env.poll(&mut a).await;
    let r = env
        .req(reqwest::Method::PUT, "/apps/one?", &env.admin)
        .header("content-type", "application/zip")
        .body(bundle_image("one.test", "", &format!("{HOST}/app:1")))
        .send()
        .await
        .unwrap();
    let acc: Accepted = r.json().await.unwrap();
    let r = gc(&env, "grace_secs=0").await;
    assert!(
        r.postponed
            .as_deref()
            .unwrap_or("")
            .contains("app `one` is being deployed on node `a`"),
        "{r:?}"
    );
    // Once it's done, the collection that was owed runs.
    env.poll(&mut a).await;
    a.applied("one", acc.generation, true, false);
    env.poll(&mut a).await;
    let r = gc(&env, "grace_secs=0").await;
    assert!(r.collected && r.deleted.is_empty(), "{r:?}");
    assert!(container.calls().contains(&"stop".to_string()));
}

#[test]
fn reference_hosts_carry_the_ingress_port_unless_its_the_default() {
    let mut i = IngressConfig::new("http://127.0.0.1:2019", Tls::Internal);
    assert_eq!(reference_host("r.example.com", &i), "r.example.com");
    i.https_port = 7443;
    assert_eq!(reference_host("r.example.com", &i), "r.example.com:7443");
    i.tls = Tls::Http;
    assert_eq!(reference_host("r.example.com", &i), "r.example.com");
    i.http_port = 8080;
    assert_eq!(reference_host("localhost", &i), "localhost:8080");
}

#[test]
fn registry_config_is_checked() {
    let ok = RegistryConfig::new("registry.example.com");
    ok.validate().unwrap();
    let bad = |f: fn(&mut RegistryConfig)| {
        let mut c = ok.clone();
        f(&mut c);
        c.validate().is_err()
    };
    assert!(bad(|c| c.domain = "Registry".into()));
    assert!(bad(|c| c.listen = "0.0.0.0:5000".parse().unwrap()));
    assert!(bad(|c| c.container = "-x".into()));
    assert!(bad(|c| c.image = "registry 3".into()));
    let cfg = container::config_file("s3cret");
    let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(&cfg).unwrap();
    assert_eq!(parsed["storage"]["delete"]["enabled"], serde_yaml_ng::Value::Bool(true));
    assert_eq!(parsed["http"]["secret"].as_str(), Some("s3cret"));
}

#[test]
fn credentials_allow_what_their_kind_may_do() {
    let admin = Who::Admin("first".into());
    let ci = Who::Deploy("ci".into());
    let node = Who::Node("a".into());
    let m = refs::target("/v2/app/manifests/1");
    for who in [&admin, &ci, &node] {
        assert!(allowed(who, "GET", &m).is_ok());
        assert!(allowed(who, "HEAD", &Target::Base).is_ok());
        assert!(allowed(who, "GET", &Target::Other).is_err());
    }
    assert!(allowed(&ci, "PUT", &m).is_ok());
    assert!(allowed(&ci, "PATCH", &Target::Repository("app".into())).is_ok());
    assert!(allowed(&ci, "DELETE", &m).is_err());
    assert!(allowed(&ci, "POST", &Target::Catalog).is_err());
    assert!(allowed(&node, "PUT", &m).is_err());
    assert!(allowed(&admin, "DELETE", &m).is_ok());
}

#[tokio::test]
async fn the_registry_isnt_started_again_in_the_middle_of_a_collection_asked_for_through_the_api() {
    // The check that keeps the registry running comes round every 10 ms, and collecting takes a while.
    let Setup {
        env,
        registry,
        container,
    } = setup_with(
        Duration::from_millis(10),
        FakeContainer {
            collect_takes: Duration::from_millis(300),
            ..Default::default()
        },
    )
    .await;
    registry.image("app", "1", "amd64");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let r = gc(&env, "grace_secs=0").await;
    assert!(r.collected, "{r:?}");
    let calls = container.calls();
    let stop = calls.iter().position(|c| c == "stop").expect("stopped");
    let done = calls.iter().position(|c| c == "collected").expect("collected");
    assert_eq!(calls[stop + 1], "collect --delete-untagged");
    assert!(
        !calls[stop..done].iter().any(|c| c == "ensure"),
        "the registry was started while it was being collected: {calls:?}"
    );
    // It's started again right after, and the check carries on.
    assert_eq!(calls[done + 1], "ensure");
    assert!(
        calls[..stop].iter().filter(|c| *c == "ensure").count() > 5,
        "the check was running: {calls:?}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(container.calls().len() > calls.len() + 3, "the check carries on");
}

#[tokio::test]
async fn a_node_from_before_the_registry_isnt_sent_a_prune() {
    let Setup { env, .. } = setup().await;
    let mut old = env
        .join_with(
            "old",
            kn_proto::agent::NodeFacts {
                protocol: kn_proto::agent::BACKUPS_PROTOCOL,
                ..facts(4 << 30)
            },
        )
        .await;
    env.poll(&mut old).await;
    let r = env
        .req(reqwest::Method::POST, "/nodes/old/prune", &env.admin)
        .send()
        .await
        .unwrap();
    let (s, msg) = error(r).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(msg.contains("from before the image registry"), "{msg}");
}

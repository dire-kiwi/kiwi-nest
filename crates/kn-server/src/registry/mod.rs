//! The cluster's image registry.
//!
//! - **The container.** The server runs a `registry:3` container (the image is configurable) with its storage in
//!   the server's state directory, published only on loopback ([`container`]). The ingress serves it at the
//!   registry's domain, such as `registry.example.com`.
//! - **Credentials are checked by the server, in the ingress.** Caddy asks [`auth`] (`GET /registry/auth`, with the
//!   request's credentials, method and path) about every request before passing it to the registry, which checks
//!   nothing itself. People and CI log in with a deploy or admin token, and may pull and push; each node has its own
//!   credential (`knr_…`, given when it joins and replaced whenever it starts), which may only pull. Only an admin
//!   token may delete. Every manifest pushed through the ingress is recorded, for garbage collection's grace period.
//! - **Garbage collection** ([`gc`]) deletes the manifests whose tags no app on any node uses and that weren't
//!   pushed, released or first seen within the grace period, keeping every tag that shares a manifest with one
//!   that's kept. Then, once nothing may be pulling (no app deploying or starting, and no request to the registry
//!   for a while), it stops the registry, runs its offline garbage collection, which frees the layers, and starts it
//!   again, whatever happened. It runs every `gc_interval`, and on `registry gc`.
//! - **Platforms.** A deploy checks each image the registry holds against the node's architecture
//!   ([`Registry::platforms`]), so an amd64-only image is refused for an arm64 node, and a new nest goes where its
//!   images can run.

pub mod client;
pub mod container;
pub mod gc;
pub mod refs;

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use base64::Engine;
use kn_proto::api::{GcReport, TokenKind};
use tokio_util::sync::CancellationToken;

use crate::Server;
use crate::error::{Error, Result};
use crate::store::{self, registry::Listed};
use refs::{ImageRef, Target};

/// No request to the registry for this long, and nothing pulls (as far as the server can tell).
pub const QUIET: Duration = Duration::from_secs(10);

/// The realm in the registry's login challenge.
const REALM: &str = "kiwi-nest registry";

/// How the server runs the registry.
#[derive(Debug, Clone)]
pub struct RegistryConfig {
    /// The domain the ingress serves it at, such as `registry.example.com`.
    pub domain: String,
    /// The registry's image.
    pub image: String,
    /// Where it's published, on loopback, for the ingress and the server.
    pub listen: SocketAddr,
    /// Its container's name.
    pub container: String,
    /// The Docker CLI.
    pub docker: PathBuf,
    /// How long a tag is kept after it was pushed or released.
    pub grace: Duration,
    /// How often garbage collection runs.
    pub gc_interval: Duration,
    /// How long garbage collection waits for nothing to be pulling, before it leaves stopping the registry to free
    /// space to its next run.
    pub idle_wait: Duration,
}

impl RegistryConfig {
    pub fn new(domain: impl Into<String>) -> Self {
        RegistryConfig {
            domain: domain.into(),
            image: "registry:3".into(),
            listen: SocketAddr::from(([127, 0, 0, 1], 5000)),
            container: "kiwi-nest-registry".into(),
            docker: "docker".into(),
            grace: Duration::from_secs(24 * 3600),
            gc_interval: Duration::from_secs(6 * 3600),
            idle_wait: Duration::from_secs(600),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if !kn_bundle::manifest::is_valid_domain(&self.domain) {
            anyhow::bail!(
                "the registry's domain must be a hostname in lowercase, not {:?}",
                self.domain
            );
        }
        if !self.listen.ip().is_loopback() {
            anyhow::bail!(
                "the registry checks no credentials itself, so it's only published on loopback, not {}",
                self.listen
            );
        }
        let c = self.container.as_bytes();
        if c.is_empty()
            || !c[0].is_ascii_alphanumeric()
            || !c
                .iter()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'))
        {
            anyhow::bail!("{:?} isn't a container name", self.container);
        }
        if self.image.is_empty() || self.image.contains(char::is_whitespace) || self.image.starts_with('-') {
            anyhow::bail!("{:?} isn't an image", self.image);
        }
        if self.gc_interval.is_zero() {
            anyhow::bail!("garbage collection needs an interval");
        }
        Ok(())
    }
}

/// The host image references name: the domain, with the ingress's port unless it's the default one.
pub fn reference_host(domain: &str, ingress: &crate::ingress::IngressConfig) -> String {
    let (port, default) = if ingress.tls == crate::ingress::Tls::Http {
        (ingress.http_port, 80)
    } else {
        (ingress.https_port, 443)
    };
    if port == default {
        domain.to_string()
    } else {
        format!("{domain}:{port}")
    }
}

/// The registry, as the server sees it.
pub struct Registry {
    cfg: RegistryConfig,
    host: String,
    client: client::Client,
    container: Arc<dyn container::Container>,
    /// Held (shared) while a deploy is saved, and exclusively while garbage collection decides what to delete and
    /// while the registry is stopped, so a deploy never names a tag that's being deleted, and one saved while the
    /// registry is stopped waits.
    gc_lock: tokio::sync::RwLock<()>,
    /// One garbage collection at a time.
    collecting: tokio::sync::Mutex<()>,
    /// When the ingress last asked about a request.
    last_request: Mutex<Option<Instant>>,
}

/// Who a registry credential belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Who {
    Admin(String),
    Deploy(String),
    Node(String),
}

impl Who {
    fn describe(&self) -> String {
        match self {
            Who::Admin(n) => format!("admin token `{n}`"),
            Who::Deploy(n) => format!("deploy token `{n}`"),
            Who::Node(n) => format!("node `{n}`"),
        }
    }
}

/// Whether `who` may make a request with this method, to this target; why not, if not.
pub fn allowed(who: &Who, method: &str, target: &Target) -> std::result::Result<(), String> {
    let read = matches!(method, "GET" | "HEAD");
    match (who, target) {
        (_, Target::Other) => Err("that isn't a registry request".into()),
        (Who::Admin(_), _) => Ok(()),
        (_, Target::Base | Target::Catalog) if read => Ok(()),
        (Who::Node(_), _) if read => Ok(()),
        (Who::Node(n), _) => Err(format!("node `{n}`'s credential may only pull")),
        (Who::Deploy(_), _) if method == "DELETE" => Err("only an admin token may delete from the registry".into()),
        (Who::Deploy(_), Target::Manifest { .. } | Target::Repository(_)) => Ok(()),
        (Who::Deploy(_), _) => Err("a deploy token may only pull and push".into()),
    }
}

/// The password of a Basic `Authorization` header.
fn basic_password(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, rest) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(rest.trim()).ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (_, password) = text.split_once(':')?;
    Some(password.to_string())
}

/// An answer in the registry's own error format.
fn registry_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = serde_json::json!({ "errors": [{ "code": code, "message": message }] });
    let mut resp = (status, axum::Json(body)).into_response();
    if status == StatusCode::UNAUTHORIZED {
        resp.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            format!("Basic realm=\"{REALM}\"").parse().expect("a header value"),
        );
    }
    resp
}

/// Makes a node's registry credential (`knr_…`): stored only as a hash.
pub fn generate_credential() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the system's random number generator");
    format!("knr_{}", hex::encode(bytes))
}

pub fn router() -> Router<Server> {
    let p = kn_proto::API_PREFIX;
    Router::new().route(&format!("{p}/registry/auth"), get(auth))
}

/// `GET /registry/auth`: the ingress asks whether to pass a request on to the registry, with its credentials in
/// `Authorization` and what it is in `X-Forwarded-Method` and `X-Forwarded-Uri`. Answers 200 to let it through,
/// or the 401 (with the login challenge) or 403 that the client should get.
async fn auth(State(s): State<Server>, headers: HeaderMap) -> Response {
    let Some(registry) = s.registry.clone() else {
        return registry_error(StatusCode::NOT_FOUND, "UNSUPPORTED", "this cluster has no registry");
    };
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string()
    };
    let (method, uri) = (
        header("x-forwarded-method").to_ascii_uppercase(),
        header("x-forwarded-uri"),
    );
    let Some(password) = basic_password(&headers) else {
        return registry_error(
            StatusCode::UNAUTHORIZED,
            "UNAUTHORIZED",
            "log in with a kiwi-nest token",
        );
    };
    let h = crate::auth::hash(&password);
    let who = s
        .store
        .call(move |c| {
            if let Some(t) = store::tokens::lookup(c, &h)? {
                return Ok(match t.kind {
                    TokenKind::Admin => Some(Who::Admin(t.name)),
                    TokenKind::Deploy => Some(Who::Deploy(t.name)),
                    _ => None,
                });
            }
            Ok(store::registry::node_by_credential(c, &h)?.map(Who::Node))
        })
        .await;
    let who = match who {
        Ok(Some(w)) => w,
        Ok(None) => {
            return registry_error(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "that credential isn't valid");
        }
        Err(e) => {
            tracing::warn!(error = %e, "couldn't check a registry credential");
            return registry_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", "try again");
        }
    };
    let target = refs::target(&uri);
    if let Err(why) = allowed(&who, &method, &target) {
        tracing::debug!(who = who.describe(), method, uri, why, "refused a registry request");
        return registry_error(StatusCode::FORBIDDEN, "DENIED", &why);
    }
    *registry.last_request.lock().expect("registry") = Some(Instant::now());
    if method == "PUT"
        && let Target::Manifest { repository, reference } = &target
        && !refs::valid_digest(reference)
    {
        let (repo, tag) = (repository.clone(), reference.clone());
        tracing::info!(
            who = who.describe(),
            image = format!("{}/{repo}:{tag}", registry.host),
            "push"
        );
        if let Err(e) = s
            .store
            .call(move |c| store::registry::record_push(c, &repo, &tag, store::now()))
            .await
        {
            tracing::warn!(error = %e, "couldn't record a push");
            return registry_error(StatusCode::SERVICE_UNAVAILABLE, "UNAVAILABLE", "try again");
        }
    }
    StatusCode::OK.into_response()
}

/// An image the registry holds, and the architectures it's built for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platforms {
    pub image: String,
    pub archs: Vec<String>,
}

impl Registry {
    pub fn new(
        cfg: RegistryConfig,
        ingress: &crate::ingress::IngressConfig,
        state_dir: &std::path::Path,
    ) -> anyhow::Result<Registry> {
        cfg.validate()?;
        let container = Arc::new(container::Docker::new(cfg.clone(), state_dir)?);
        Registry::with_container(cfg, ingress, container)
    }

    /// A registry whose container is run by `container` (a fake, in tests).
    pub fn with_container(
        cfg: RegistryConfig,
        ingress: &crate::ingress::IngressConfig,
        container: Arc<dyn container::Container>,
    ) -> anyhow::Result<Registry> {
        Ok(Registry {
            host: reference_host(&cfg.domain, ingress),
            client: client::Client::new(cfg.listen)?,
            cfg,
            container,
            gc_lock: tokio::sync::RwLock::new(()),
            collecting: tokio::sync::Mutex::new(()),
            last_request: Mutex::new(None),
        })
    }

    pub fn config(&self) -> &RegistryConfig {
        &self.cfg
    }

    /// The host image references name, such as `registry.example.com`.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Held while a deploy is saved, so garbage collection doesn't decide what to delete meanwhile.
    pub async fn hold(&self) -> tokio::sync::RwLockReadGuard<'_, ()> {
        self.gc_lock.read().await
    }

    /// The architectures of each of `images` that the registry holds. Images elsewhere, and any the registry
    /// doesn't have (yet), aren't checked; nor is anything if the registry can't be reached.
    pub async fn platforms(&self, images: &[String]) -> Vec<Platforms> {
        let mut out = Vec::new();
        for image in images {
            let Some(r) = ImageRef::parse(image).filter(|r| r.is_on(&self.host)) else {
                continue;
            };
            match self.client.platforms(&r.repository, r.reference()).await {
                Ok(Some(archs)) if !archs.is_empty() => out.push(Platforms {
                    image: image.clone(),
                    archs,
                }),
                Ok(_) => {}
                Err(e) => tracing::warn!(image, error = %e, "couldn't read an image's platforms from the registry"),
            }
        }
        out
    }

    /// Keeps the registry running, and collects garbage every `gc_interval`, until `stop` is cancelled.
    pub async fn run(&self, server: Server, stop: CancellationToken) {
        let mut next_gc = Instant::now() + self.cfg.gc_interval;
        let mut failing: Option<String> = None;
        loop {
            match self.container.ensure().await {
                Ok(()) => {
                    if failing.take().is_some() {
                        tracing::info!(container = self.cfg.container, "the registry is running again");
                    }
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    if failing.as_deref() != Some(&msg) {
                        tracing::warn!(container = self.cfg.container, error = msg, "couldn't run the registry");
                    }
                    failing = Some(msg);
                }
            }
            if failing.is_none() && Instant::now() >= next_gc {
                match self.gc(&server, false, None).await {
                    Ok(r) => tracing::info!(
                        deleted = r.deleted.len(),
                        kept = r.kept.len(),
                        collected = r.collected,
                        postponed = r.postponed,
                        "collected the registry's garbage"
                    ),
                    Err(e) => tracing::warn!(error = %e, "the registry's garbage collection failed"),
                }
                next_gc = Instant::now() + self.cfg.gc_interval;
            }
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_secs(30)) => {}
            }
        }
    }

    /// Every tag the registry holds, with its digest.
    async fn listed(&self) -> Result<Vec<Listed>> {
        let mut out = Vec::new();
        let unreachable = |e: anyhow::Error| Error::Offline(format!("couldn't read the registry: {e:#}"));
        for repository in self.client.repositories().await.map_err(unreachable)? {
            for tag in self.client.tags(&repository).await.map_err(unreachable)? {
                if let Some(digest) = self.client.digest(&repository, &tag).await.map_err(unreachable)? {
                    out.push(Listed {
                        repository: repository.clone(),
                        tag,
                        digest,
                    });
                }
            }
        }
        Ok(out)
    }

    /// Why something may be pulling now, if it may.
    async fn busy(&self, server: &Server) -> Result<Option<String>> {
        if let Some(t) = *self.last_request.lock().expect("registry")
            && t.elapsed() < QUIET
        {
            return Ok(Some("the registry was used in the last few seconds".into()));
        }
        let online = server.online();
        server.store.call(move |c| store::registry::busy(c, &online)).await
    }

    /// Collects the registry's garbage (see [`gc`]): deletes what's unused, and frees its layers. `grace` replaces
    /// the configured grace period. A dry run only says what it would delete.
    pub async fn gc(&self, server: &Server, dry_run: bool, grace: Option<Duration>) -> Result<GcReport> {
        let _one = self.collecting.lock().await;
        let grace = grace.unwrap_or(self.cfg.grace).as_secs();
        let keep = self.cfg.grace.as_secs();
        let listed = self.listed().await?;

        let lock = self.gc_lock.write().await;
        let now = store::now();
        let (l, host) = (listed.clone(), self.host.clone());
        let (inputs, unknown, moved) = server
            .store
            .call(move |c| {
                let times = store::registry::tag_times(c)?;
                let uses = store::registry::uses(c)?;
                let mut inputs = gc::Inputs {
                    tags: l,
                    now,
                    grace,
                    ..Default::default()
                };
                for (app, node, images) in uses.apps {
                    for image in images {
                        if let Some(r) = ImageRef::parse(&image).filter(|r| r.is_on(&host)) {
                            inputs.uses.push(gc::Use {
                                app: app.clone(),
                                node: node.clone(),
                                image: r,
                            });
                        }
                    }
                }
                for (reference, at) in store::registry::releases(c)? {
                    if let Some(r) = ImageRef::parse(&reference).filter(|r| r.is_on(&host)) {
                        inputs.event(&r.repository, r.reference(), "released", at);
                    }
                }
                for ((repo, tag), (pushed, _, _)) in &times {
                    if let Some(p) = pushed {
                        inputs.event(repo, tag, "pushed", *p);
                    }
                }
                // When each tag was first seen at its digest: now, for one not seen there before.
                let seen: Vec<(String, String, u64)> = inputs
                    .tags
                    .iter()
                    .map(|l| {
                        let at = match times.get(&(l.repository.clone(), l.tag.clone())) {
                            Some((_, Some(d), Some(at))) if d == &l.digest => *at,
                            _ => now,
                        };
                        (l.repository.clone(), l.tag.clone(), at)
                    })
                    .collect();
                for (repo, tag, at) in seen {
                    inputs.event(&repo, &tag, "first seen", at);
                }
                let moved = if dry_run {
                    false
                } else {
                    store::registry::see(c, &inputs.tags, keep, now)?
                };
                Ok((inputs, uses.unknown, moved))
            })
            .await?;
        let plan = gc::plan(&inputs);
        let mut report = GcReport {
            dry_run,
            ..Default::default()
        };
        if !unknown.is_empty() {
            report.kept = listed
                .iter()
                .map(|l| kn_proto::api::GcTag {
                    repository: l.repository.clone(),
                    tag: l.tag.clone(),
                    digest: l.digest.clone(),
                    why: "nothing is deleted while some apps' images aren't known".into(),
                })
                .collect();
            report.postponed = Some(format!(
                "the images of app {} aren't known (deployed before the server recorded them); deploy {} again",
                unknown.join(", "),
                if unknown.len() == 1 { "it" } else { "them" }
            ));
            return Ok(report);
        }
        report.kept = plan.kept;
        report.deleted = plan.deleted;
        if dry_run {
            return Ok(report);
        }
        for (repo, digest) in &plan.delete {
            if let Err(e) = self.client.delete(repo, digest).await {
                drop(lock);
                return Err(Error::Internal(e));
            }
            tracing::info!(repository = repo, digest, "deleted from the registry");
        }
        if !plan.delete.is_empty() || moved {
            server
                .store
                .call(move |c| store::registry::set_collect_pending(c, true, now))
                .await?;
        }
        let pending = server.store.call(|c| store::registry::collect_pending(c)).await?;
        drop(lock);
        if !pending {
            return Ok(report);
        }

        // The registry's own collection, once nothing may be pulling: with the lock held, so no deploy is saved
        // while the registry is stopped.
        let deadline = Instant::now() + self.cfg.idle_wait;
        let lock = loop {
            let lock = self.gc_lock.write().await;
            match self.busy(server).await? {
                None => break lock,
                Some(why) if Instant::now() >= deadline => {
                    report.postponed = Some(format!(
                        "{why}, so the registry wasn't stopped to free space; next time"
                    ));
                    return Ok(report);
                }
                Some(_) => {
                    drop(lock);
                    let left = deadline.saturating_duration_since(Instant::now());
                    tokio::time::sleep(left.min(Duration::from_secs(2))).await;
                }
            }
        };
        // Manifests that apps use by digest alone, and no tag names, would go with the untagged ones.
        let tagged: HashSet<(&str, &str)> = listed
            .iter()
            .map(|l| (l.repository.as_str(), l.digest.as_str()))
            .collect();
        let untagged_in_use = inputs.uses.iter().any(|u| {
            u.image
                .digest
                .as_deref()
                .is_some_and(|d| !tagged.contains(&(u.image.repository.as_str(), d)))
        });
        let collected = self.collect(!untagged_in_use).await;
        drop(lock);
        collected?;
        server
            .store
            .call(move |c| store::registry::set_collect_pending(c, false, store::now()))
            .await?;
        report.collected = true;
        Ok(report)
    }

    /// Stops the registry, collects, and always starts it again.
    async fn collect(&self, delete_untagged: bool) -> Result<()> {
        tracing::info!(
            container = self.cfg.container,
            "stopping the registry to collect its garbage"
        );
        let result = match self.container.stop().await {
            Ok(()) => self.container.collect(delete_untagged).await.map(|out| {
                let summary = out
                    .lines()
                    .rev()
                    .find(|l| l.contains("blobs marked"))
                    .unwrap_or("")
                    .trim();
                tracing::info!(summary, "the registry's garbage collection ran");
            }),
            Err(e) => Err(e),
        };
        let mut started = self.container.ensure().await;
        for _ in 0..4 {
            if started.is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
            started = self.container.ensure().await;
        }
        if let Err(e) = &started {
            tracing::error!(error = %e, "couldn't start the registry again after collecting its garbage");
        }
        result.map_err(Error::Internal)?;
        started.map_err(Error::Internal)
    }
}

#[cfg(test)]
pub(crate) mod tests;

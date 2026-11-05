//! The server's API for the CLI. See [`kn_proto::api`] for the routes.
//!
//! Changes (deploy, delete, `volume create|load|delete`) are saved and answered with `202 Accepted` at once; the
//! CLI then waits on `…/changes/{generation}` until the node reports them done. One-off actions (inspect, logs,
//! suspend, resume, …) are commands, run on the node while the caller waits.

use std::collections::HashMap;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete as delete_route, get, post, put};
use kn_proto::agent::{Action, CommandResult};
use kn_proto::api::{
    AppInfo, AppSummary, ChangeStatus, CreateToken, CreatedToken, DeleteQuery, DeployQuery, LogsQuery, NodeInfo,
    TokenInfo, TokenKind, VersionInfo, VolumeInfo, VolumeQuery, WaitQuery, WhoAmI,
};
use kn_proto::state::AppStatus;
use serde::de::DeserializeOwned;

use crate::Server;
use crate::auth::{self, Caller};
use crate::bundles;
use crate::error::{Error, Result};
use crate::hub::Outcome;
use crate::store::apps::{self, AppRow, Kind, VolumeRow};
use crate::store::nodes::{self, Node};
use crate::store::{self, rfc3339, tokens};

/// The longest a caller may wait on `…/changes/{generation}` in one request.
const MAX_WAIT: Duration = Duration::from_secs(60);

pub fn router() -> Router<Server> {
    let p = kn_proto::API_PREFIX;
    Router::new()
        .route(&format!("{p}/version"), get(version))
        .route(&format!("{p}/whoami"), get(whoami))
        .route(&format!("{p}/apps"), get(list))
        .route(&format!("{p}/apps/{{name}}"), get(inspect).put(deploy).delete(delete))
        .route(&format!("{p}/apps/{{name}}/changes/{{generation}}"), get(app_change))
        .route(&format!("{p}/apps/{{name}}/logs"), get(logs))
        .route(&format!("{p}/apps/{{name}}/suspend"), post(suspend))
        .route(&format!("{p}/apps/{{name}}/resume"), post(resume))
        .route(&format!("{p}/volumes"), get(volume_list))
        .route(
            &format!("{p}/volumes/{{name}}"),
            get(volume_inspect).put(volume_create).delete(volume_delete),
        )
        .route(&format!("{p}/volumes/{{name}}/data"), put(volume_load))
        .route(
            &format!("{p}/volumes/{{name}}/changes/{{generation}}"),
            get(volume_change),
        )
        .route(
            &format!("{p}/volumes/{{name}}/snapshots/{{app}}"),
            delete_route(volume_delete_snapshot),
        )
        .route(&format!("{p}/nodes"), get(node_list))
        .route(&format!("{p}/tokens"), get(token_list).post(token_create))
        .route(&format!("{p}/tokens/{{id}}"), delete_route(token_revoke))
}

async fn version() -> axum::Json<VersionInfo> {
    axum::Json(VersionInfo {
        version: kn_proto::VERSION.into(),
        commit: kn_proto::GIT_COMMIT.into(),
    })
}

async fn whoami(caller: Caller) -> axum::Json<WhoAmI> {
    axum::Json(caller.whoami())
}

fn check_app_name(name: &str) -> Result<()> {
    if kn_bundle::manifest::is_valid_app_name(name) {
        Ok(())
    } else {
        Err(Error::BadRequest(format!(
            "`{name}` isn't a valid app name: use lowercase letters, digits and `-`, at most 50 characters"
        )))
    }
}

fn check_volume_name(name: &str) -> Result<()> {
    if kn_bundle::compose::is_valid_object_name(name) {
        Ok(())
    } else {
        Err(Error::BadRequest(format!(
            "`{name}` isn't a valid volume name: use letters, digits, `_`, `.` and `-`, at most 63 characters"
        )))
    }
}

/// An upload must be a ZIP.
fn check_zip(headers: &HeaderMap) -> Result<()> {
    let ct = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "application/zip" | "application/octet-stream" => Ok(()),
        "application/json" => Err(Error::BadRequest(
            "the server can't read a directory on a node; upload the bundle as a ZIP".into(),
        )),
        other => Err(Error::BadRequest(format!(
            "send a ZIP as application/zip, not {other:?}"
        ))),
    }
}

/// An app and its node, as the server sees them.
struct View {
    app: AppRow,
    node: Option<Node>,
    online: bool,
}

impl View {
    fn report(&self) -> Option<&kn_proto::agent::AppReport> {
        self.node
            .as_ref()?
            .report
            .as_ref()?
            .apps
            .iter()
            .find(|a| a.name == self.app.name)
    }

    fn applied(&self) -> u64 {
        self.node
            .as_ref()
            .and_then(|n| n.report.as_ref())
            .and_then(|r| r.applied.apps.get(&self.app.name))
            .map_or(0, |o| o.generation)
    }

    fn pending(&self) -> Option<u64> {
        (self.app.generation > self.applied()).then_some(self.app.generation)
    }

    fn summary(&self) -> AppSummary {
        let r = self.report();
        let containers = match r {
            Some(_) if !self.online => "unknown (node offline)".to_string(),
            Some(r) => r.containers.clone(),
            None => "absent".to_string(),
        };
        AppSummary {
            name: self.app.name.clone(),
            status: r.map_or(AppStatus::Deploying, |r| r.status),
            generation: r.map_or(0, |r| r.generation),
            domain: self
                .app
                .domain
                .clone()
                .or_else(|| r.map(|r| r.domain.clone()))
                .unwrap_or_default(),
            port: r.map_or(0, |r| r.port),
            containers,
            updated_at: rfc3339(self.app.updated_at),
            node: self.node.as_ref().map(|n| n.name.clone()),
            pending: self.pending(),
        }
    }

    /// What the server knows without asking the node.
    fn info(&self) -> AppInfo {
        let r = self.report();
        AppInfo {
            name: self.app.name.clone(),
            status: r.map_or(AppStatus::Deploying, |r| r.status),
            generation: r.map_or(0, |r| r.generation),
            error: r.and_then(|r| r.error.clone()),
            domain: self.app.domain.clone().unwrap_or_default(),
            service: String::new(),
            container_port: 0,
            port: r.map_or(0, |r| r.port),
            idle_timeout_secs: 0,
            project: String::new(),
            bundle_digest: r
                .map(|r| r.bundle_digest.clone())
                .or_else(|| self.app.bundle_digest.clone())
                .unwrap_or_default(),
            volumes: Vec::new(),
            shared: Vec::new(),
            from: self.app.from_app.clone(),
            notes: Vec::new(),
            created_at: rfc3339(self.app.created_at),
            updated_at: rfc3339(self.app.updated_at),
            containers: Vec::new(),
            open_requests: 0,
            node: self.node.as_ref().map(|n| n.name.clone()),
            pending: self.pending(),
            stale: true,
        }
    }
}

async fn view(s: &Server, name: &str) -> Result<View> {
    let n = name.to_string();
    let (app, node) = s
        .store
        .call(move |c| {
            let app = apps::live_app(c, &n)?;
            let node = app.node_id.map(|id| nodes::get(c, id)).transpose()?;
            Ok((app, node))
        })
        .await?;
    let online = node.as_ref().is_some_and(|n| s.hub.is_online(n.id));
    Ok(View { app, node, online })
}

/// A command's JSON result.
fn result<T: DeserializeOwned>(o: Outcome) -> Result<T> {
    match o {
        Outcome::Result(CommandResult::Ok { value }) => Ok(serde_json::from_value(value)?),
        Outcome::Result(CommandResult::Err { status, error }) => Err(Error::Node(
            StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            error,
        )),
        Outcome::Stream(_) => Err(Error::Internal(anyhow::anyhow!("the node streamed an answer"))),
        Outcome::Lost => Err(Error::Offline("the command was lost".into())),
    }
}

/// Asks an app's node for it, adding what the server knows.
async fn inspect_on_node(s: &Server, v: &View) -> Result<AppInfo> {
    let node = v
        .node
        .as_ref()
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("no node")))?;
    let mut info: AppInfo = result(
        s.command(
            node.id,
            &node.name,
            Action::Inspect {
                app: v.app.name.clone(),
            },
        )
        .await?,
    )?;
    info.node = Some(node.name.clone());
    info.pending = v.pending();
    Ok(info)
}

async fn list(State(s): State<Server>, caller: Caller) -> Result<axum::Json<Vec<AppSummary>>> {
    let (rows, nodes) = s.store.call(|c| Ok((apps::apps(c)?, nodes::list(c)?))).await?;
    let nodes: HashMap<u64, Node> = nodes.into_iter().map(|n| (n.id, n)).collect();
    Ok(axum::Json(
        rows.into_iter()
            .filter(|a| caller.may_manage(&a.name))
            .map(|app| {
                let node = app.node_id.and_then(|id| nodes.get(&id).cloned());
                let online = node.as_ref().is_some_and(|n| s.hub.is_online(n.id));
                View { app, node, online }.summary()
            })
            .collect(),
    ))
}

async fn inspect(State(s): State<Server>, caller: Caller, Path(name): Path<String>) -> Result<axum::Json<AppInfo>> {
    caller.manage(&name)?;
    let v = view(&s, &name).await?;
    if v.online && v.report().is_some() {
        match inspect_on_node(&s, &v).await {
            Ok(info) => return Ok(axum::Json(info)),
            Err(Error::Offline(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(axum::Json(v.info()))
}

async fn deploy(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    Query(q): Query<DeployQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    check_app_name(&name)?;
    caller.manage(&name)?;
    if let Some(from) = &q.from {
        check_app_name(from)?;
        caller.may_use_from(from)?;
    }
    check_zip(&headers)?;
    let limits = s.cfg.limits.clone();
    let upload = s.bundles.temp_file()?;
    let size = bundles::receive(body, &upload, limits.max_archive_size).await?;
    let scratch = s.bundles.temp_dir()?;
    let (zip, dir) = (upload.path().to_path_buf(), scratch.path().to_path_buf());
    let checked = tokio::task::spawn_blocking(move || bundles::check_app(&zip, &dir, &limits))
        .await
        .map_err(|e| Error::Internal(e.into()))??;
    drop(scratch);
    if !caller.is_admin() && !checked.shared_rw.is_empty() {
        return Err(Error::Forbidden(format!(
            "a deploy token can't mount shared volumes read-write ({}); mount them `ro` or `snapshot`, or deploy \
             with an admin token",
            checked.shared_rw.join(", ")
        )));
    }
    let (b, online) = (s.bundles.clone(), s.online());
    let d = apps::Deploy {
        name: name.clone(),
        bundle: checked,
        upload: upload.path().to_path_buf(),
        size,
        create_only: q.create_only,
        from: q.from,
        node: q.node,
        queue: q.queue,
    };
    let saved = s
        .store
        .call(move |c| apps::deploy(c, &b, &online, d, store::now()))
        .await?;
    drop(upload);
    tracing::info!(
        app = name,
        node = saved.accepted.node,
        generation = saved.accepted.generation,
        "deploy saved"
    );
    s.hub.wake(saved.node_id);
    Ok((StatusCode::ACCEPTED, axum::Json(saved.accepted)).into_response())
}

async fn delete(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Result<Response> {
    check_app_name(&name)?;
    caller.manage(&name)?;
    let online = s.online();
    let saved = s
        .store
        .call(move |c| apps::delete(c, &online, &name, q.keep_data, q.queue, store::now()))
        .await?;
    s.hub.wake(saved.node_id);
    Ok((StatusCode::ACCEPTED, axum::Json(saved.accepted)).into_response())
}

/// Waits (up to `wait`) for a change to be done on its node.
async fn wait_change(s: &Server, kind: Kind, name: String, generation: u64, wait: u64) -> Result<ChangeStatus> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(wait).min(MAX_WAIT);
    let status = |name: String| {
        let store = s.store.clone();
        async move {
            store
                .call(move |c| apps::change_status(c, kind, &name, generation))
                .await
        }
    };
    let (_, node_id) = status(name.clone()).await?;
    let mut reports = s.hub.node(node_id).reports.subscribe();
    let was_online = s.hub.is_online(node_id);
    loop {
        let (mut st, node_id) = status(name.clone()).await?;
        st.node_online = s.hub.is_online(node_id);
        // Answer early when the node goes offline, so the caller can say so.
        if st.done || tokio::time::Instant::now() >= deadline || (was_online && !st.node_online) {
            return Ok(st);
        }
        tokio::select! {
            r = reports.changed() => { if r.is_err() { return Ok(st); } }
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            _ = tokio::time::sleep_until(deadline) => {}
            _ = s.stop.cancelled() => return Ok(st),
        }
    }
}

async fn app_change(
    State(s): State<Server>,
    caller: Caller,
    Path((name, generation)): Path<(String, u64)>,
    Query(q): Query<WaitQuery>,
) -> Result<axum::Json<ChangeStatus>> {
    caller.manage(&name)?;
    Ok(axum::Json(wait_change(&s, Kind::App, name, generation, q.wait).await?))
}

async fn logs(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Result<Response> {
    caller.manage(&name)?;
    let v = view(&s, &name).await?;
    let node = v
        .node
        .as_ref()
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("no node")))?;
    let outcome = s
        .command(node.id, &node.name, Action::Logs { app: name, query })
        .await?;
    let text = [(header::CONTENT_TYPE, "text/plain; charset=utf-8")];
    match outcome {
        Outcome::Stream(rx) => {
            let stream = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|x| (x, rx)) });
            Ok((text, Body::from_stream(stream)).into_response())
        }
        o => {
            let _: serde_json::Value = result(o)?;
            Ok((text, "").into_response())
        }
    }
}

async fn app_command(s: &Server, caller: &Caller, name: String, action: Action) -> Result<axum::Json<AppInfo>> {
    caller.manage(&name)?;
    let v = view(s, &name).await?;
    let node = v
        .node
        .as_ref()
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("no node")))?;
    let mut info: AppInfo = result(s.command(node.id, &node.name, action).await?)?;
    info.node = Some(node.name.clone());
    info.pending = v.pending();
    Ok(axum::Json(info))
}

async fn suspend(State(s): State<Server>, caller: Caller, Path(name): Path<String>) -> Result<axum::Json<AppInfo>> {
    let action = Action::Suspend { app: name.clone() };
    app_command(&s, &caller, name, action).await
}

async fn resume(State(s): State<Server>, caller: Caller, Path(name): Path<String>) -> Result<axum::Json<AppInfo>> {
    let action = Action::Resume { app: name.clone() };
    app_command(&s, &caller, name, action).await
}

/// A shared volume as the server sees it: what its node last reported, plus where it is.
fn volume_view(v: &VolumeRow, node: Option<&Node>) -> VolumeInfo {
    let report = node.and_then(|n| n.report.as_ref());
    let reported = report.and_then(|r| r.volumes.iter().find(|x| x.name == v.name));
    let applied = report
        .and_then(|r| r.applied.volumes.get(&v.name))
        .map_or(0, |o| o.generation);
    let failed = report
        .and_then(|r| r.applied.volumes.get(&v.name))
        .filter(|o| !o.ok)
        .and_then(|o| o.error.clone());
    let mut info = reported.cloned().unwrap_or_else(|| VolumeInfo {
        name: v.name.clone(),
        created_at: rfc3339(v.created_at),
        loaded_at: None,
        mounts: Vec::new(),
        snapshots: Vec::new(),
        problem: None,
        node: None,
        pending: None,
    });
    if info.problem.is_none() {
        info.problem = failed;
    }
    info.node = node.map(|n| n.name.clone());
    info.pending = (v.generation > applied).then_some(v.generation);
    info
}

async fn volume_list(State(s): State<Server>, _caller: Caller) -> Result<axum::Json<Vec<VolumeInfo>>> {
    let (rows, nodes) = s.store.call(|c| Ok((apps::volumes(c)?, nodes::list(c)?))).await?;
    let nodes: HashMap<u64, Node> = nodes.into_iter().map(|n| (n.id, n)).collect();
    Ok(axum::Json(
        rows.iter()
            .map(|v| volume_view(v, v.node_id.and_then(|id| nodes.get(&id))))
            .collect(),
    ))
}

async fn volume_inspect(
    State(s): State<Server>,
    _caller: Caller,
    Path(name): Path<String>,
) -> Result<axum::Json<VolumeInfo>> {
    check_volume_name(&name)?;
    let n = name.clone();
    let (v, node) = s
        .store
        .call(move |c| {
            let v = apps::live_volume(c, &n)?;
            let node = v.node_id.map(|id| nodes::get(c, id)).transpose()?;
            Ok((v, node))
        })
        .await?;
    let mut view = volume_view(&v, node.as_ref());
    if let Some(node) = &node
        && s.hub.is_online(node.id)
        && view.pending.is_none()
    {
        match s.command(node.id, &node.name, Action::VolumeInfo { name }).await {
            Ok(o) => {
                let mut live: VolumeInfo = result(o)?;
                live.node = view.node.take();
                live.pending = view.pending;
                return Ok(axum::Json(live));
            }
            Err(Error::Offline(_)) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(axum::Json(view))
}

async fn volume_create(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    Query(q): Query<VolumeQuery>,
) -> Result<Response> {
    caller.admin()?;
    check_volume_name(&name)?;
    let online = s.online();
    let saved = s
        .store
        .call(move |c| apps::create_volume(c, &online, &name, q.node.as_deref(), q.queue, store::now()))
        .await?;
    s.hub.wake(saved.node_id);
    Ok((StatusCode::ACCEPTED, axum::Json(saved.accepted)).into_response())
}

async fn volume_load(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    Query(q): Query<VolumeQuery>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response> {
    caller.admin()?;
    check_volume_name(&name)?;
    check_zip(&headers)?;
    let limits = s.cfg.load_limits.clone();
    let upload = s.bundles.temp_file()?;
    let size = bundles::receive(body, &upload, limits.max_archive_size).await?;
    let scratch = s.bundles.temp_dir()?;
    let (zip, dir) = (upload.path().to_path_buf(), scratch.path().to_path_buf());
    let digest = tokio::task::spawn_blocking(move || bundles::check_data(&zip, &dir, &limits))
        .await
        .map_err(|e| Error::Internal(e.into()))??;
    drop(scratch);
    let (b, online, path) = (s.bundles.clone(), s.online(), upload.path().to_path_buf());
    let saved = s
        .store
        .call(move |c| apps::load_volume(c, &b, &online, &name, &digest, &path, size, q.queue, store::now()))
        .await?;
    drop(upload);
    s.hub.wake(saved.node_id);
    Ok((StatusCode::ACCEPTED, axum::Json(saved.accepted)).into_response())
}

async fn volume_delete(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    Query(q): Query<VolumeQuery>,
) -> Result<Response> {
    caller.admin()?;
    check_volume_name(&name)?;
    let online = s.online();
    let saved = s
        .store
        .call(move |c| apps::delete_volume(c, &online, &name, q.queue, store::now()))
        .await?;
    s.hub.wake(saved.node_id);
    Ok((StatusCode::ACCEPTED, axum::Json(saved.accepted)).into_response())
}

async fn volume_change(
    State(s): State<Server>,
    _caller: Caller,
    Path((name, generation)): Path<(String, u64)>,
    Query(q): Query<WaitQuery>,
) -> Result<axum::Json<ChangeStatus>> {
    Ok(axum::Json(
        wait_change(&s, Kind::Volume, name, generation, q.wait).await?,
    ))
}

async fn volume_delete_snapshot(
    State(s): State<Server>,
    caller: Caller,
    Path((name, app)): Path<(String, String)>,
) -> Result<StatusCode> {
    caller.admin()?;
    check_volume_name(&name)?;
    let n = name.clone();
    let node = s
        .store
        .call(move |c| {
            let v = apps::live_volume(c, &n)?;
            v.node_id.map(|id| nodes::get(c, id)).transpose()
        })
        .await?
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("no node")))?;
    let _: serde_json::Value = result(
        s.command(node.id, &node.name, Action::DeleteSnapshot { name, app })
            .await?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}

async fn node_list(State(s): State<Server>, caller: Caller) -> Result<axum::Json<Vec<NodeInfo>>> {
    caller.admin()?;
    let (nodes, placed) = s.store.call(|c| Ok((nodes::list_facts(c)?, apps::placed(c)?))).await?;
    let placed: HashMap<u64, (u64, u64)> = placed.into_iter().map(|(n, m, c)| (n, (m, c))).collect();
    Ok(axum::Json(
        nodes
            .into_iter()
            .map(|n| {
                let (mem, count) = placed.get(&n.id).copied().unwrap_or_default();
                NodeInfo {
                    id: n.id,
                    online: s.hub.is_online(n.id),
                    name: n.name,
                    joined_at: rfc3339(n.joined_at),
                    last_seen: n.last_seen.map(rfc3339),
                    facts: n.facts,
                    apps: count,
                    app_memory_bytes: mem,
                }
            })
            .collect(),
    ))
}

async fn token_list(State(s): State<Server>, caller: Caller) -> Result<axum::Json<Vec<TokenInfo>>> {
    caller.admin()?;
    Ok(axum::Json(s.store.call(|c| tokens::list(c)).await?))
}

async fn token_create(
    State(s): State<Server>,
    caller: Caller,
    axum::Json(req): axum::Json<CreateToken>,
) -> Result<axum::Json<CreatedToken>> {
    caller.admin()?;
    match req.kind {
        TokenKind::Node => {
            return Err(Error::BadRequest(
                "node tokens are made when a node joins; make a join token instead".into(),
            ));
        }
        TokenKind::Deploy if req.patterns.is_empty() => {
            return Err(Error::BadRequest(
                "a deploy token needs at least one pattern of app names, such as `blog-pr*`".into(),
            ));
        }
        TokenKind::Admin | TokenKind::Join if !req.patterns.is_empty() || !req.from.is_empty() => {
            return Err(Error::BadRequest("only deploy tokens have patterns".into()));
        }
        _ => {}
    }
    if let Some(bad) = req.patterns.iter().chain(&req.from).find(|p| !auth::valid_pattern(p)) {
        return Err(Error::BadRequest(format!(
            "`{bad}` isn't a valid pattern: use lowercase letters, digits, `-` and `*`"
        )));
    }
    let token = auth::generate(req.kind);
    let h = auth::hash(&token);
    let kind = req.kind;
    let id = s
        .store
        .call(move |c| tokens::create(c, kind, &h, &req.name, &req.patterns, &req.from, None, store::now()))
        .await?;
    tracing::info!(id, kind = %kind, by = caller.id, "made a token");
    Ok(axum::Json(CreatedToken { id, kind, token }))
}

async fn token_revoke(State(s): State<Server>, caller: Caller, Path(id): Path<u64>) -> Result<StatusCode> {
    caller.admin()?;
    s.store.call(move |c| tokens::revoke(c, id, store::now())).await?;
    tracing::info!(id, by = caller.id, "revoked a token");
    Ok(StatusCode::NO_CONTENT)
}

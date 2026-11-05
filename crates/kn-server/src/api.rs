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
    AppInfo, AppSummary, ChangeStatus, CommandInfo, CommandsQuery, CreateToken, CreatedToken, DeleteQuery, DeployQuery,
    ForgetQuery, Forgot, LabelRequest, LogsQuery, NestInfo, NodeInfo, NodeUsage, TokenInfo, TokenKind, VersionInfo,
    VolumeInfo, VolumeQuery, WaitQuery, WhoAmI,
};
use kn_proto::backup::{BackupInfo, BackupQuery, BackupResult, Kind as BackupKind, ListQuery, Restore};
use kn_proto::state::AppStatus;
use serde::de::DeserializeOwned;

use crate::Server;
use crate::auth::{self, Caller};
use crate::bundles;
use crate::error::{Error, Result};
use crate::hub::Outcome;
use crate::scheduler;
use crate::store::apps::{self, AppRow, Kind, VolumeRow};
use crate::store::nests::{self, Nest};
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
        .route(&format!("{p}/apps/{{name}}/commands"), get(app_commands))
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
        .route(&format!("{p}/backups"), get(backup_list))
        .route(&format!("{p}/backups/apps/{{name}}"), post(backup_app))
        .route(&format!("{p}/backups/volumes/{{name}}"), post(backup_volume))
        .route(&format!("{p}/backups/server"), post(backup_server))
        .route(&format!("{p}/nests"), get(nest_list))
        .route(&format!("{p}/nodes"), get(node_list))
        .route(&format!("{p}/nodes/{{name}}/labels"), post(node_label))
        .route(&format!("{p}/nodes/{{name}}/cordon"), post(node_cordon))
        .route(&format!("{p}/nodes/{{name}}/uncordon"), post(node_uncordon))
        .route(&format!("{p}/nodes/{{name}}/forget"), post(node_forget))
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
    nest: Option<Nest>,
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

    /// How the app came to be in its nest, and why the nest's node was chosen.
    fn placement(&self) -> Option<String> {
        let nest = self.nest.as_ref()?;
        let node = self.node.as_ref().map_or("?", |n| n.name.as_str());
        let via = if self.app.nest_via.is_empty() {
            String::new()
        } else {
            format!("{}; ", self.app.nest_via)
        };
        Some(format!(
            "{via}nest {} went to node {node} {}",
            nest.name, nest.placement
        ))
    }

    /// Adds what only the server knows to what the node says.
    fn annotate(&self, info: &mut AppInfo) {
        info.node = self.node.as_ref().map(|n| n.name.clone());
        info.pending = self.pending();
        info.nest = self.app.nest.clone();
        info.placement = self.placement();
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
            error: r.and_then(|r| r.error.clone()),
            idle_timeout_secs: self.app.idle_timeout_secs,
            last_request_at: r.and_then(|r| r.last_request_at).map(rfc3339),
            // What it last reported; an offline node's requests can't be known.
            open_requests: r.filter(|_| self.online).map_or(0, |r| r.open_requests),
            space: r.map(|r| r.space.clone()).unwrap_or_default(),
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
            nest: self.app.nest.clone(),
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
            idle_timeout_secs: self.app.idle_timeout_secs,
            project: String::new(),
            bundle_digest: r
                .map(|r| r.bundle_digest.clone())
                .or_else(|| self.app.bundle_digest.clone())
                .unwrap_or_default(),
            volumes: Vec::new(),
            shared: Vec::new(),
            from: self.app.from_app.clone(),
            restored_from: self.app.restore.as_ref().map(|r| format!("{}:{}", r.target, r.key)),
            notes: Vec::new(),
            created_at: rfc3339(self.app.created_at),
            updated_at: rfc3339(self.app.updated_at),
            containers: Vec::new(),
            open_requests: 0,
            last_request_at: r.and_then(|r| r.last_request_at).map(rfc3339),
            space: r.map(|r| r.space.clone()).unwrap_or_default(),
            node: self.node.as_ref().map(|n| n.name.clone()),
            nest: self.app.nest.clone(),
            placement: self.placement(),
            pending: self.pending(),
            stale: true,
        }
    }
}

async fn view(s: &Server, name: &str) -> Result<View> {
    let n = name.to_string();
    let node = s
        .store
        .call(move |c| {
            let app = apps::live_app(c, &n)?;
            let node = app.node_id.map(|id| nodes::get(c, id)).transpose()?;
            let nest = match &app.nest {
                Some(n) => nests::live(c, n)?,
                None => None,
            };
            Ok((app, node, nest))
        })
        .await?;
    let (app, node, nest) = node;
    let online = node.as_ref().is_some_and(|n| s.hub.is_online(n.id));
    Ok(View {
        app,
        node,
        online,
        nest,
    })
}

/// A command's JSON result.
pub(crate) fn result<T: DeserializeOwned>(o: Outcome) -> Result<T> {
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
async fn inspect_on_node(s: &Server, v: &View, caller: &Caller) -> Result<AppInfo> {
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
            &caller.by(),
        )
        .await?,
    )?;
    v.annotate(&mut info);
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
                View {
                    app,
                    node,
                    online,
                    nest: None,
                }
                .summary()
            })
            .collect(),
    ))
}

async fn inspect(State(s): State<Server>, caller: Caller, Path(name): Path<String>) -> Result<axum::Json<AppInfo>> {
    caller.manage(&name)?;
    let v = view(&s, &name).await?;
    if v.online && v.report().is_some() {
        match inspect_on_node(&s, &v, &caller).await {
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
    if q.from.is_some() && q.restore.is_some() {
        return Err(Error::BadRequest(
            "an app's volumes start either `--from` another app or `--restore`d from a backup, not both".into(),
        ));
    }
    let restore = match &q.restore {
        Some(key) => Some(resolve_restore(&s, &caller, BackupKind::App, &name, key, q.target.as_deref()).await?),
        None => None,
    };
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
    if let Some(reserved) = s.reserved_domain(&checked.domain) {
        return Err(Error::Conflict(format!(
            "domain `{}` is {reserved}, so no app can use it",
            checked.domain
        )));
    }
    if !caller.is_admin() && !checked.shared_rw.is_empty() {
        return Err(Error::Forbidden(format!(
            "a deploy token can't mount shared volumes read-write ({}); mount them `ro` or `snapshot`, or deploy \
             with an admin token",
            checked.shared_rw.join(", ")
        )));
    }
    let (b, online, settings) = (s.bundles.clone(), s.online(), s.cfg.scheduler);
    let d = apps::Deploy {
        name: name.clone(),
        bundle: checked,
        upload: upload.path().to_path_buf(),
        size,
        create_only: q.create_only,
        from: q.from,
        node: q.node,
        queue: q.queue,
        restore,
    };
    let saved = s
        .store
        .call(move |c| apps::deploy(c, &b, &online, &settings, d, store::now()))
        .await?;
    drop(upload);
    tracing::info!(
        app = name,
        node = saved.accepted.node,
        nest = saved.accepted.nest,
        generation = saved.accepted.generation,
        "deploy saved"
    );
    s.hub.wake(saved.node_id);
    s.ingress_changed();
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
    s.ingress_changed();
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
        .command(node.id, &node.name, Action::Logs { app: name, query }, &caller.by())
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
    let mut info: AppInfo = result(s.command(node.id, &node.name, action, &caller.by()).await?)?;
    v.annotate(&mut info);
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

/// The app's recent commands, newest first.
async fn app_commands(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    Query(q): Query<CommandsQuery>,
) -> Result<axum::Json<Vec<CommandInfo>>> {
    check_app_name(&name)?;
    caller.manage(&name)?;
    let limit = q.limit.unwrap_or(20).clamp(1, 200);
    Ok(axum::Json(
        s.store
            .call(move |c| {
                let app = apps::live_app(c, &name)?;
                store::commands::history(c, &name, app.created_at, limit)
            })
            .await?,
    ))
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
        space: None,
        snapshot_space: Default::default(),
        node: None,
        nest: None,
        pending: None,
    });
    if info.problem.is_none() {
        info.problem = failed;
    }
    info.node = node.map(|n| n.name.clone());
    info.nest = v.nest.clone();
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
    caller: Caller,
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
        match s
            .command(node.id, &node.name, Action::VolumeInfo { name }, &caller.by())
            .await
        {
            Ok(o) => {
                let mut live: VolumeInfo = result(o)?;
                live.node = view.node.take();
                live.nest = view.nest.take();
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
    let restore = match &q.restore {
        Some(key) => Some(resolve_restore(&s, &caller, BackupKind::Volume, &name, key, q.target.as_deref()).await?),
        None => None,
    };
    let (online, settings) = (s.online(), s.cfg.scheduler);
    let saved = s
        .store
        .call(move |c| {
            apps::create_volume(
                c,
                &online,
                &settings,
                &name,
                q.node.as_deref(),
                q.nest.as_deref(),
                q.queue,
                restore.as_ref(),
                store::now(),
            )
        })
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
        s.command(node.id, &node.name, Action::DeleteSnapshot { name, app }, &caller.by())
            .await?,
    )?;
    Ok(StatusCode::NO_CONTENT)
}

/// Every node that isn't forgotten, as `node list` shows it.
async fn nodes_info(s: &Server) -> Result<Vec<NodeInfo>> {
    let settings = s.cfg.scheduler;
    let (nodes, loads, nests) = s
        .store
        .call(|c| Ok((nodes::list(c)?, scheduler::loads(c, None)?, nests::list(c)?)))
        .await?;
    Ok(nodes
        .into_iter()
        .map(|n| {
            let load = loads.get(&n.id).copied().unwrap_or_default();
            let facts = nodes::facts_of(&n);
            let usage = n.report.as_ref().and_then(|r| r.usage.as_ref()).map(|u| NodeUsage {
                measured_at: rfc3339(u.measured_at),
                data_bytes: u.groups.iter().map(|g| g.bytes).sum(),
                backup_snapshots: u.backups,
            });
            NodeInfo {
                usage,
                id: n.id,
                online: s.hub.is_online(n.id),
                joined_at: rfc3339(n.joined_at),
                last_seen: n.last_seen.map(rfc3339),
                cordoned: n.cordoned,
                labels: nodes::labels_of(&n),
                nests: nests
                    .iter()
                    .filter(|m| m.nest.node_id == n.id)
                    .map(|m| m.nest.name.clone())
                    .collect(),
                apps: load.apps,
                app_memory_bytes: load.always_on + load.idle,
                headroom_bytes: scheduler::headroom(facts.memory_bytes, load, &settings)
                    .clamp(i64::MIN.into(), i64::MAX.into()) as i64,
                facts: n.facts,
                name: n.name,
            }
        })
        .collect())
}

async fn node_info(s: &Server, name: &str) -> Result<axum::Json<NodeInfo>> {
    nodes_info(s)
        .await?
        .into_iter()
        .find(|n| n.name == name)
        .map(axum::Json)
        .ok_or_else(|| Error::NotFound(format!("there's no node `{name}`")))
}

async fn node_list(State(s): State<Server>, caller: Caller) -> Result<axum::Json<Vec<NodeInfo>>> {
    caller.admin()?;
    Ok(axum::Json(nodes_info(&s).await?))
}

/// Whether a label's key or value is acceptable: printable, without `=` or spaces, and not too long.
fn valid_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.' | b'/'))
}

async fn node_label(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    axum::Json(req): axum::Json<LabelRequest>,
) -> Result<axum::Json<NodeInfo>> {
    caller.admin()?;
    if let Some(bad) = req
        .set
        .iter()
        .flat_map(|(k, v)| [k, v])
        .chain(&req.remove)
        .find(|x| !valid_label(x))
    {
        return Err(Error::BadRequest(format!(
            "`{bad}` isn't a valid label key or value: use letters, digits, `-`, `_`, `.` and `/`, at most 63"
        )));
    }
    let n = name.clone();
    s.store
        .call(move |c| nodes::set_labels(c, &n, &req.set, &req.remove))
        .await?;
    node_info(&s, &name).await
}

async fn cordon(s: &Server, caller: &Caller, name: String, cordoned: bool) -> Result<axum::Json<NodeInfo>> {
    caller.admin()?;
    let n = name.clone();
    s.store.call(move |c| nodes::set_cordoned(c, &n, cordoned)).await?;
    tracing::info!(node = name, cordoned, "cordon changed");
    node_info(s, &name).await
}

async fn node_cordon(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<axum::Json<NodeInfo>> {
    cordon(&s, &caller, name, true).await
}

async fn node_uncordon(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
) -> Result<axum::Json<NodeInfo>> {
    cordon(&s, &caller, name, false).await
}

async fn node_forget(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    Query(q): Query<ForgetQuery>,
) -> Result<axum::Json<Forgot>> {
    caller.admin()?;
    let online = s.online();
    let f = s
        .store
        .call(move |c| nodes::forget(c, &name, &online, q.force, q.dry_run, store::now()))
        .await?;
    if !q.dry_run {
        for id in &f.lost_commands {
            s.hub.settle(*id, Outcome::Lost);
        }
        let (b, gone) = (s.bundles.clone(), f.forgot.apps.len() + f.forgot.volumes.len());
        s.store.call(move |c| apps::gc(c, &b)).await?;
        // An open poll (with --force) is answered at once, and says the node was forgotten.
        s.hub.wake(f.node_id);
        s.hub.reported(f.node_id);
        s.ingress_changed();
        tracing::warn!(node = f.forgot.node, released = gone, by = caller.id, "forgot a node");
    }
    Ok(axum::Json(f.forgot))
}

async fn nest_list(State(s): State<Server>, caller: Caller) -> Result<axum::Json<Vec<NestInfo>>> {
    caller.admin()?;
    let (list, all) = s.store.call(|c| Ok((nests::list(c)?, nodes::list(c)?))).await?;
    let names: HashMap<u64, String> = all.iter().map(|n| (n.id, n.name.clone())).collect();
    let usage: HashMap<u64, &kn_proto::agent::Usage> = all
        .iter()
        .filter_map(|n| Some((n.id, n.report.as_ref()?.usage.as_ref()?)))
        .collect();
    Ok(axum::Json(
        list.into_iter()
            .map(|m| NestInfo {
                disk_bytes: usage.get(&m.nest.node_id).map(|u| nest_disk(u, &m.apps, &m.volumes)),
                node: names.get(&m.nest.node_id).cloned().unwrap_or_default(),
                node_online: s.hub.is_online(m.nest.node_id),
                memory_bytes: m.apps.iter().map(|(_, b)| b).sum(),
                apps: m.apps.into_iter().map(|(n, _)| n).collect(),
                volumes: m.volumes,
                placement: m.nest.placement,
                created_at: rfc3339(m.nest.created_at),
                name: m.nest.name,
            })
            .collect(),
    ))
}

/// The space a nest's members take: the sum of the node's groups of subvolumes that hold any of them (a group is
/// always within one nest).
fn nest_disk(u: &kn_proto::agent::Usage, apps: &[(String, u64)], volumes: &[String]) -> u64 {
    u.groups
        .iter()
        .filter(|g| {
            g.apps.iter().any(|a| apps.iter().any(|(n, _)| n == a)) || g.volumes.iter().any(|v| volumes.contains(v))
        })
        .map(|g| g.bytes)
        .sum()
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

/// Checks a `--restore` key, and finds the target that has it. A deploy token may restore another app's backup only
/// if it may use that app as `--from`.
async fn resolve_restore(
    s: &Server,
    caller: &Caller,
    kind: BackupKind,
    name: &str,
    key: &str,
    target: Option<&str>,
) -> Result<Restore> {
    let k = kn_backup::parse_restore_key(key, kind).map_err(|e| Error::BadRequest(format!("{e:#}")))?;
    if kind == BackupKind::App && k.name != name {
        caller.may_use_from(&k.name)?;
    }
    if s.backups.targets.is_empty() {
        return Err(Error::BadRequest(
            "the server has no backup targets to restore from: give it --backup-targets".into(),
        ));
    }
    let t = kn_backup::find(&s.backups.targets, target, &k)
        .await
        .map_err(|e| Error::NotFound(format!("{e:#}")))?;
    // The whole chain must be there before anything is saved.
    kn_backup::chain_for(t, &k)
        .await
        .map_err(|e| Error::NotFound(format!("{e:#}")))?;
    Ok(Restore {
        target: t.name().into(),
        key: k.to_string(),
    })
}

async fn backup_list(
    State(s): State<Server>,
    caller: Caller,
    Query(q): Query<ListQuery>,
) -> Result<axum::Json<Vec<BackupInfo>>> {
    if s.backups.targets.is_empty() {
        return Err(Error::BadRequest(
            "the server has no backup targets: give it --backup-targets".into(),
        ));
    }
    let mut all = list_targets(&s, &q).await?;
    if !caller.is_admin() {
        all.retain(|b| b.kind == BackupKind::App && caller.may_manage(&b.name));
    }
    Ok(axum::Json(all))
}

/// Lists the targets' backups, as the node does.
async fn list_targets(s: &Server, q: &ListQuery) -> Result<Vec<BackupInfo>> {
    let chosen = s.backups.chosen(&q.target.iter().cloned().collect::<Vec<_>>())?;
    let mut out = Vec::new();
    for t in chosen {
        let scope = match (q.kind, &q.name) {
            (Some(BackupKind::Server), _) => Some((BackupKind::Server, "")),
            (Some(k), Some(n)) => Some((k, n.as_str())),
            _ => None,
        };
        let infos = t.infos(scope).await.map_err(|e| Error::Offline(format!("{e:#}")))?;
        out.extend(
            infos
                .into_iter()
                .filter(|i| q.kind.is_none_or(|k| k == i.kind))
                .filter(|i| q.name.as_ref().is_none_or(|n| *n == i.name)),
        );
    }
    Ok(out)
}

async fn backup_app(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    Query(q): Query<BackupQuery>,
) -> Result<axum::Json<BackupResult>> {
    check_app_name(&name)?;
    caller.manage(&name)?;
    let targets: Vec<String> = q.target.into_iter().collect();
    s.backups.chosen(&targets)?;
    let v = view(&s, &name).await?;
    let node = v
        .node
        .as_ref()
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("no node")))?;
    nodes::require_backups(node)?;
    let action = Action::Backup {
        what: BackupKind::App,
        name,
        targets,
        full: q.full,
    };
    let o = s
        .command_with(node.id, &node.name, action, &caller.by(), s.cfg.backup_timeout)
        .await?;
    Ok(axum::Json(result(o)?))
}

async fn backup_volume(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    Query(q): Query<BackupQuery>,
) -> Result<axum::Json<BackupResult>> {
    caller.admin()?;
    check_volume_name(&name)?;
    let targets: Vec<String> = q.target.into_iter().collect();
    s.backups.chosen(&targets)?;
    let n = name.clone();
    let node = s
        .store
        .call(move |c| {
            let v = apps::live_volume(c, &n)?;
            v.node_id.map(|id| nodes::get(c, id)).transpose()
        })
        .await?
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("no node")))?;
    nodes::require_backups(&node)?;
    let action = Action::Backup {
        what: BackupKind::Volume,
        name,
        targets,
        full: q.full,
    };
    let o = s
        .command_with(node.id, &node.name, action, &caller.by(), s.cfg.backup_timeout)
        .await?;
    Ok(axum::Json(result(o)?))
}

async fn backup_server(
    State(s): State<Server>,
    caller: Caller,
    Query(q): Query<BackupQuery>,
) -> Result<axum::Json<BackupResult>> {
    caller.admin()?;
    let targets: Vec<String> = q.target.into_iter().collect();
    Ok(axum::Json(s.backup_self(&targets).await?))
}

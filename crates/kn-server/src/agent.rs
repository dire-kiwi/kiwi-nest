//! The agent link, the server's side: joining, long polls, bundle downloads and command results. See
//! [`kn_proto::agent`] for the protocol.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures_util::StreamExt;
use kn_proto::agent::{
    CommandResult, IngressToken, JoinRequest, JoinResponse, OLDEST_PROTOCOL, PROTOCOL, PollRequest, PollResponse,
};
use kn_proto::api::TokenKind;
use tokio::sync::mpsc;
use tokio_util::io::ReaderStream;

use crate::Server;
use crate::auth::{self, NodeCaller};
use crate::error::{Error, Result};
use crate::hub::Outcome;
use crate::store::{self, apps, commands, nodes};

pub fn router() -> Router<Server> {
    let p = kn_proto::API_PREFIX;
    Router::new()
        .route(&format!("{p}/agent/join"), post(join))
        .route(&format!("{p}/agent/ingress-token"), get(ingress_token))
        .route(&format!("{p}/agent/poll"), post(poll))
        .route(&format!("{p}/agent/report"), post(report))
        .route(&format!("{p}/agent/bundles/{{digest}}"), get(bundle))
        .route(&format!("{p}/agent/commands/{{id}}/result"), post(command_result))
        .route(&format!("{p}/agent/commands/{{id}}/output"), post(command_output))
}

async fn join(State(s): State<Server>, axum::Json(req): axum::Json<JoinRequest>) -> Result<axum::Json<JoinResponse>> {
    if !kn_bundle::manifest::is_valid_app_name(&req.name) {
        return Err(Error::BadRequest(format!(
            "`{}` isn't a valid node name: use lowercase letters, digits and `-`, at most 50 characters",
            req.name
        )));
    }
    let token = auth::generate(TokenKind::Node);
    let ingress_token = auth::generate_ingress();
    let (join_hash, node_hash, name, facts) = (auth::hash(&req.token), auth::hash(&token), req.name, req.facts);
    let (n, i) = (name.clone(), ingress_token.clone());
    let node_id = s
        .store
        .call(move |c| nodes::join(c, &join_hash, &n, &facts, &node_hash, &i, store::now()))
        .await?;
    tracing::info!(node = name, node_id, "a node joined");
    s.ingress_changed();
    Ok(axum::Json(JoinResponse {
        node_id,
        name,
        token,
        ingress_token,
    }))
}

/// The node's ingress token, for a node that joined before the server gave them out.
async fn ingress_token(State(s): State<Server>, node: NodeCaller) -> Result<axum::Json<IngressToken>> {
    let id = node.node_id;
    let ingress_token = s.store.call(move |c| nodes::ingress_token(c, id)).await?;
    Ok(axum::Json(IngressToken { ingress_token }))
}

/// A long poll: records the report, then answers with what changed after `since` and any commands, as soon as
/// there's something, or empty once the wait is over.
async fn poll(
    State(s): State<Server>,
    node: NodeCaller,
    axum::Json(req): axum::Json<PollRequest>,
) -> Result<axum::Json<PollResponse>> {
    if req.protocol < OLDEST_PROTOCOL || req.protocol > PROTOCOL {
        return Err(Error::UpgradeRequired(format!(
            "this server speaks versions {OLDEST_PROTOCOL} to {PROTOCOL} of the agent link, and the node speaks {}",
            req.protocol
        )));
    }
    let id = node.node_id;
    let open = s.hub.open_poll(id);
    let running = req.report.running_commands.clone();
    let (b, report) = (s.bundles.clone(), req.report);
    s.store
        .call(move |c| apps::record_report(c, &b, id, &report, store::now()))
        .await?;
    s.hub.reported(id);
    // The node's ingress address may have changed, or a delete finished.
    s.ingress_changed();

    let wait = Duration::from_secs(req.wait_secs.unwrap_or(u64::MAX)).min(s.cfg.poll_timeout);
    let deadline = tokio::time::Instant::now() + wait;
    let since = req.since;
    let mut first = true;
    loop {
        let notified = open.node().wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if open.superseded() {
            // The node has given up on this poll; whatever this answers goes nowhere.
            return Ok(axum::Json(PollResponse {
                seq: since,
                ..Default::default()
            }));
        }
        let running = if first { Some(running.clone()) } else { None };
        first = false;
        let superseded = open.ticket();
        let ((seq, apps, volumes), reset, lost, commands) = s
            .store
            .call(move |c| {
                // Checked again here, where nothing else can hand out commands meanwhile.
                if superseded() {
                    return Ok(((since, Vec::new(), Vec::new()), false, Vec::new(), Vec::new()));
                }
                let mut desired = apps::desired_since(c, id, since)?;
                // A node that asks from a sequence number the server hasn't reached (the server's database was
                // restored from an older backup) gets everything again.
                let reset = since > desired.0;
                if reset {
                    desired = apps::desired_since(c, id, 0)?;
                }
                let (lost, commands) = match running {
                    Some(r) => commands::take(c, id, &r, store::now())?,
                    None => (Vec::new(), commands::take_pending(c, id, store::now())?),
                };
                Ok((desired, reset, lost, commands))
            })
            .await?;
        for command in lost {
            tracing::warn!(node = node.name, command, "a command was lost on the way to the node");
            s.hub.settle(command, Outcome::Lost);
        }
        if reset || !apps.is_empty() || !volumes.is_empty() || !commands.is_empty() {
            return Ok(axum::Json(PollResponse {
                seq,
                apps,
                volumes,
                commands,
            }));
        }
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(axum::Json(PollResponse { seq, ..Default::default() }));
            }
            _ = s.stop.cancelled() => {
                return Ok(axum::Json(PollResponse { seq, ..Default::default() }));
            }
        }
    }
}

/// A report on its own, sent when a change finishes, so the server hears without waiting for the poll.
async fn report(
    State(s): State<Server>,
    node: NodeCaller,
    axum::Json(report): axum::Json<kn_proto::agent::Report>,
) -> Result<StatusCode> {
    let (b, id) = (s.bundles.clone(), node.node_id);
    s.store
        .call(move |c| apps::record_report(c, &b, id, &report, store::now()))
        .await?;
    s.hub.reported(id);
    s.ingress_changed();
    Ok(StatusCode::NO_CONTENT)
}

/// A bundle, for a node that has an app or volume using it.
async fn bundle(State(s): State<Server>, node: NodeCaller, Path(digest): Path<String>) -> Result<Response> {
    let path = s
        .bundles
        .path(&digest)
        .ok_or_else(|| Error::BadRequest(format!("`{digest}` isn't a bundle digest")))?;
    let (id, d) = (node.node_id, digest.clone());
    let wanted: bool = s
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS (SELECT 1 FROM apps WHERE node_id = ?1 AND bundle_digest = ?2)
                     OR EXISTS (SELECT 1 FROM volumes WHERE node_id = ?1 AND data_digest = ?2)",
                rusqlite::params![id, d],
                |r| r.get(0),
            )?)
        })
        .await?;
    let missing = || {
        Error::NotFound(format!(
            "no app or volume of node `{}` uses bundle `{digest}`",
            node.name
        ))
    };
    if !wanted {
        return Err(missing());
    }
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(missing()),
        Err(e) => return Err(e.into()),
    };
    let len = file.metadata().await?.len();
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (header::CONTENT_LENGTH, len.to_string()),
        ],
        Body::from_stream(ReaderStream::new(file)),
    )
        .into_response())
}

async fn command_result(
    State(s): State<Server>,
    node: NodeCaller,
    Path(id): Path<u64>,
    axum::Json(result): axum::Json<CommandResult>,
) -> Result<StatusCode> {
    let json = serde_json::to_string(&result)?;
    let node_id = node.node_id;
    let was = s
        .store
        .call(move |c| commands::finish(c, id, node_id, &json, store::now()))
        .await?;
    if was {
        s.hub.settle(id, Outcome::Result(result));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// A command's output, streamed through to whoever is waiting for it.
async fn command_output(
    State(s): State<Server>,
    node: NodeCaller,
    Path(id): Path<u64>,
    body: Body,
) -> Result<StatusCode> {
    let node_id = node.node_id;
    let delivered: bool = s
        .store
        .call(move |c| {
            Ok(c.query_row(
                "SELECT EXISTS (SELECT 1 FROM commands WHERE id = ?1 AND node_id = ?2 AND state = 'delivered')",
                rusqlite::params![id, node_id],
                |r| r.get(0),
            )?)
        })
        .await?;
    if !delivered {
        return Err(Error::NotFound(format!("command {id} isn't waiting for output")));
    }
    let (tx, rx) = mpsc::channel(16);
    if !s.hub.settle(id, Outcome::Stream(rx)) {
        let _ = s
            .store
            .call(move |c| {
                commands::finish(
                    c,
                    id,
                    node_id,
                    r#"{"kind":"err","status":410,"error":{"error":"nobody was waiting"}}"#,
                    store::now(),
                )
            })
            .await;
        return Ok(StatusCode::GONE);
    }
    let mut stream = body.into_data_stream();
    let mut gone = false;
    while let Some(chunk) = stream.next().await {
        let item = chunk.map_err(std::io::Error::other);
        let failed = item.is_err();
        if tx.send(item).await.is_err() {
            gone = true;
            break;
        }
        if failed {
            break;
        }
    }
    let _ = s
        .store
        .call(move |c| commands::finish(c, id, node_id, r#"{"kind":"ok","value":"streamed"}"#, store::now()))
        .await;
    Ok(if gone { StatusCode::GONE } else { StatusCode::NO_CONTENT })
}

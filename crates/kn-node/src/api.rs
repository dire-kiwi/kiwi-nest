//! The standalone node's HTTP API, for the CLI on the same machine. See [`kn_proto::api`] for the routes.
//!
//! It has no authentication, so it only listens on loopback.

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use kn_proto::api::{AppInfo, AppSummary, DeployDirectory, DeployQuery, ErrorBody, LogsQuery, VersionInfo};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdout};
use tokio_util::io::ReaderStream;

use crate::apps::{Apps, AppsError, Logs};

/// The largest JSON body accepted.
const MAX_JSON: usize = 64 * 1024;

/// An error response.
pub struct ApiError(StatusCode, ErrorBody);

impl ApiError {
    fn new(status: StatusCode, error: impl Into<String>) -> Self {
        ApiError(
            status,
            ErrorBody {
                error: error.into(),
                problems: Vec::new(),
            },
        )
    }
}

impl From<AppsError> for ApiError {
    fn from(e: AppsError) -> Self {
        use kn_bundle::BundleError;
        let status = match &e {
            AppsError::InvalidName(_) => StatusCode::BAD_REQUEST,
            AppsError::Bundle(BundleError::TooLarge(_)) => StatusCode::PAYLOAD_TOO_LARGE,
            AppsError::Bundle(_) | AppsError::Refused(_) | AppsError::Unsupported(_) => {
                StatusCode::UNPROCESSABLE_ENTITY
            }
            AppsError::NotFound(_) | AppsError::NotFoundOther(_) => StatusCode::NOT_FOUND,
            AppsError::Exists(_) | AppsError::DomainTaken { .. } => StatusCode::CONFLICT,
            AppsError::DeployFailed { .. } | AppsError::Runtime(_) | AppsError::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        };
        match e {
            AppsError::Refused(problems) => ApiError(
                status,
                ErrorBody {
                    error: "the bundle was refused".into(),
                    problems: problems.0.iter().map(|p| p.to_string()).collect(),
                },
            ),
            e => ApiError::new(status, e.to_string()),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if self.0.is_server_error() {
            tracing::warn!(status = %self.0, error = %self.1.error, "request failed");
        }
        (self.0, axum::Json(self.1)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

/// The API's routes.
pub fn router(apps: Apps) -> Router {
    let p = kn_proto::API_PREFIX;
    Router::new()
        .route(&format!("{p}/version"), get(version))
        .route(&format!("{p}/apps"), get(list))
        .route(&format!("{p}/apps/{{name}}"), get(inspect).put(deploy).delete(delete))
        .route(&format!("{p}/apps/{{name}}/logs"), get(logs))
        // Uploads are streamed to disk under the bundle limits instead.
        .layer(DefaultBodyLimit::disable())
        .with_state(apps)
}

/// Serves the API until `shutdown` completes.
pub async fn serve(
    listener: tokio::net::TcpListener,
    apps: Apps,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    axum::serve(listener, router(apps))
        .with_graceful_shutdown(shutdown)
        .await
}

async fn version() -> axum::Json<VersionInfo> {
    axum::Json(VersionInfo {
        version: kn_proto::VERSION.into(),
        commit: kn_proto::GIT_COMMIT.into(),
    })
}

async fn list(State(apps): State<Apps>) -> ApiResult<axum::Json<Vec<AppSummary>>> {
    Ok(axum::Json(apps.list().await?))
}

async fn inspect(State(apps): State<Apps>, Path(name): Path<String>) -> ApiResult<axum::Json<AppInfo>> {
    Ok(axum::Json(apps.inspect(&name).await?))
}

async fn delete(State(apps): State<Apps>, Path(name): Path<String>) -> ApiResult<StatusCode> {
    apps.delete(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn deploy(
    State(apps): State<Apps>,
    Path(name): Path<String>,
    Query(q): Query<DeployQuery>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<axum::Json<AppInfo>> {
    if !kn_bundle::manifest::is_valid_app_name(&name) {
        return Err(AppsError::InvalidName(name).into());
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let staged = match content_type.as_str() {
        "application/zip" | "application/octet-stream" => {
            let upload = apps.upload_file()?;
            let limit = apps.config().limits.max_archive_size;
            let mut file = tokio::fs::File::from_std(upload.reopen().map_err(|e| AppsError::Internal(e.into()))?);
            let mut stream = body.into_data_stream();
            let mut n: u64 = 0;
            while let Some(chunk) = stream.next().await {
                let chunk =
                    chunk.map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("reading the upload: {e}")))?;
                n += chunk.len() as u64;
                if n > limit {
                    return Err(ApiError::new(
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!("the bundle is larger than {limit} bytes"),
                    ));
                }
                file.write_all(&chunk)
                    .await
                    .map_err(|e| AppsError::Internal(e.into()))?;
            }
            file.flush().await.map_err(|e| AppsError::Internal(e.into()))?;
            drop(file);
            let staged = apps.stage_zip(upload.path().to_path_buf()).await;
            drop(upload);
            staged?
        }
        "application/json" => {
            let bytes = axum::body::to_bytes(body, MAX_JSON)
                .await
                .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("reading the request: {e}")))?;
            let req: DeployDirectory = serde_json::from_slice(&bytes)
                .map_err(|e| ApiError::new(StatusCode::BAD_REQUEST, format!("bad request: {e}")))?;
            let dir = std::path::PathBuf::from(&req.directory);
            if !dir.is_absolute() {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "the directory must be an absolute path",
                ));
            }
            apps.stage_dir(dir).await?
        }
        other => {
            return Err(ApiError::new(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                format!("send a ZIP as application/zip, or a directory as JSON, not {other:?}"),
            ));
        }
    };
    Ok(axum::Json(apps.deploy(&name, staged, q.create_only).await?))
}

async fn logs(State(apps): State<Apps>, Path(name): Path<String>, Query(q): Query<LogsQuery>) -> ApiResult<Response> {
    let text = [(header::CONTENT_TYPE, "text/plain; charset=utf-8")];
    match apps.logs(&name, q).await? {
        Logs::Complete(bytes) => Ok((text, bytes).into_response()),
        Logs::Follow(mut child) => {
            let stdout = child
                .stdout
                .take()
                .ok_or_else(|| AppsError::Internal(anyhow::anyhow!("no logs output")))?;
            let stream = ChildStream {
                _child: child,
                inner: ReaderStream::new(stdout),
            };
            Ok((text, Body::from_stream(stream)).into_response())
        }
    }
}

/// A child's output as a stream; the child is killed when the stream is dropped (the client went away).
struct ChildStream {
    _child: Child,
    inner: ReaderStream<ChildStdout>,
}

impl Stream for ChildStream {
    type Item = std::io::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.get_mut().inner).poll_next(cx)
    }
}

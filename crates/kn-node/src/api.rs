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
use axum::routing::{delete as delete_route, get, post, put};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use kn_proto::api::{
    AppInfo, AppSummary, DeleteQuery, DeployDirectory, DeployQuery, ErrorBody, LogsQuery, VersionInfo, VolumeInfo,
};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdout};
use tokio_util::io::ReaderStream;

use crate::apps::{Apps, AppsError, DeployOptions, LoadSource, Logs};

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
            AppsError::Exists(_)
            | AppsError::DomainTaken { .. }
            | AppsError::Conflict(_)
            | AppsError::Mount(_)
            | AppsError::Busy(_) => StatusCode::CONFLICT,
            AppsError::DeployFailed { .. }
            | AppsError::StartFailed { .. }
            | AppsError::Runtime(_)
            | AppsError::Volume(_)
            | AppsError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
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

impl ApiError {
    /// The status and body it's answered with.
    pub fn into_parts(self) -> (StatusCode, ErrorBody) {
        (self.0, self.1)
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
        .route(&format!("{p}/apps/{{name}}/suspend"), post(suspend))
        .route(&format!("{p}/apps/{{name}}/resume"), post(resume))
        .route(&format!("{p}/volumes"), get(volume_list))
        .route(
            &format!("{p}/volumes/{{name}}"),
            get(volume_inspect).put(volume_create).delete(volume_delete),
        )
        .route(&format!("{p}/volumes/{{name}}/data"), put(volume_load))
        .route(
            &format!("{p}/volumes/{{name}}/snapshots/{{app}}"),
            delete_route(volume_delete_snapshot),
        )
        // Uploads are streamed to disk under the bundle limits instead.
        .layer(DefaultBodyLimit::disable())
        .layer(axum::middleware::from_fn(loopback_only))
        .with_state(apps)
}

/// Whether a `Host` header names this machine's loopback: `localhost`, `127.x.x.x` or `[::1]`, with any port.
pub fn is_loopback_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        match rest.split_once(']') {
            Some((ip, port)) if port.is_empty() || port.strip_prefix(':').is_some_and(is_port) => ip,
            _ => return false,
        }
    } else {
        match host.split_once(':') {
            Some((name, port)) if is_port(port) => name,
            Some(_) => return false,
            None => host,
        }
    };
    name.eq_ignore_ascii_case("localhost") || name.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn is_port(p: &str) -> bool {
    !p.is_empty() && p.parse::<u16>().is_ok()
}

/// Refuses requests that a web page could have made: listening on loopback isn't enough, because a page whose DNS
/// name is rebound to 127.0.0.1 is same-origin with it. Such a request names the page's host in `Host`, and a
/// browser adds `Origin` to it; the CLI does neither.
async fn loopback_only(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    if !is_loopback_host(host) {
        return ApiError::new(
            StatusCode::MISDIRECTED_REQUEST,
            "this API only answers requests for localhost",
        )
        .into_response();
    }
    if req.headers().contains_key(header::ORIGIN) {
        return ApiError::new(StatusCode::FORBIDDEN, "this API doesn't answer requests from web pages").into_response();
    }
    next.run(req).await
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

async fn suspend(State(apps): State<Apps>, Path(name): Path<String>) -> ApiResult<axum::Json<AppInfo>> {
    Ok(axum::Json(apps.suspend(&name).await?))
}

async fn resume(State(apps): State<Apps>, Path(name): Path<String>) -> ApiResult<axum::Json<AppInfo>> {
    Ok(axum::Json(apps.resume(&name).await?))
}

async fn delete(
    State(apps): State<Apps>,
    Path(name): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> ApiResult<StatusCode> {
    apps.delete(&name, q.keep_data).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// A request body that's either a ZIP (saved to the staging directory, within `limit`) or a directory on the node.
enum Received {
    Zip(tempfile::NamedTempFile),
    Dir(std::path::PathBuf),
}

async fn receive(apps: &Apps, headers: &HeaderMap, body: Body, limit: u64) -> ApiResult<Received> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match content_type.as_str() {
        "application/zip" | "application/octet-stream" => {
            let upload = apps.upload_file()?;
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
                        format!("the upload is larger than {limit} bytes"),
                    ));
                }
                file.write_all(&chunk)
                    .await
                    .map_err(|e| AppsError::Internal(e.into()))?;
            }
            file.flush().await.map_err(|e| AppsError::Internal(e.into()))?;
            Ok(Received::Zip(upload))
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
            Ok(Received::Dir(dir))
        }
        other => Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            format!("send a ZIP as application/zip, or a directory as JSON, not {other:?}"),
        )),
    }
}

async fn volume_list(State(apps): State<Apps>) -> ApiResult<axum::Json<Vec<VolumeInfo>>> {
    Ok(axum::Json(apps.volume_list().await?))
}

async fn volume_inspect(State(apps): State<Apps>, Path(name): Path<String>) -> ApiResult<axum::Json<VolumeInfo>> {
    Ok(axum::Json(apps.volume_info(&name).await?))
}

async fn volume_create(State(apps): State<Apps>, Path(name): Path<String>) -> ApiResult<Response> {
    Ok((StatusCode::CREATED, axum::Json(apps.volume_create(&name).await?)).into_response())
}

async fn volume_load(
    State(apps): State<Apps>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<axum::Json<VolumeInfo>> {
    let limit = apps.config().load_limits.max_archive_size;
    let info = match receive(&apps, &headers, body, limit).await? {
        Received::Zip(upload) => {
            let r = apps
                .volume_load(&name, LoadSource::Zip(upload.path().to_path_buf()))
                .await;
            drop(upload);
            r?
        }
        Received::Dir(dir) => apps.volume_load(&name, LoadSource::Dir(dir)).await?,
    };
    Ok(axum::Json(info))
}

async fn volume_delete(State(apps): State<Apps>, Path(name): Path<String>) -> ApiResult<StatusCode> {
    apps.volume_delete(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn volume_delete_snapshot(
    State(apps): State<Apps>,
    Path((name, app)): Path<(String, String)>,
) -> ApiResult<StatusCode> {
    apps.volume_delete_snapshot(&name, &app).await?;
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
    let limit = apps.config().limits.max_archive_size;
    let staged = match receive(&apps, &headers, body, limit).await? {
        Received::Zip(upload) => {
            let staged = apps.stage_zip(upload.path().to_path_buf()).await;
            drop(upload);
            staged?
        }
        Received::Dir(dir) => apps.stage_dir(dir).await?,
    };
    let opts = DeployOptions {
        create_only: q.create_only,
        from: q.from,
        generation: None,
    };
    Ok(axum::Json(apps.deploy(&name, staged, opts).await?))
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

#[cfg(test)]
mod tests {
    use super::is_loopback_host;

    #[test]
    fn loopback_hosts() {
        for ok in [
            "localhost",
            "LOCALHOST:7380",
            "127.0.0.1",
            "127.0.0.1:7380",
            "127.1.2.3:1",
            "[::1]",
            "[::1]:7380",
        ] {
            assert!(is_loopback_host(ok), "{ok}");
        }
        for bad in [
            "",
            "evil.example",
            "evil.example:7380",
            "localhost.evil.example",
            "127.0.0.1.evil.example",
            "10.0.0.1:7380",
            "[::2]:7380",
            "[::1]x",
            "localhost:",
            "localhost:99999",
            "0.0.0.0",
        ] {
            assert!(!is_loopback_host(bad), "{bad}");
        }
    }
}

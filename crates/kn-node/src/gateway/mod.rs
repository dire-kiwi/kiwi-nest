//! The gateway: a reverse proxy, keyed by Host, to each app's loopback port, which starts stopped apps for their
//! requests and lets idle ones be stopped.
//!
//! - **Routing.** A request goes to the app whose domain is its Host, compared case-insensitively and without the
//!   port. The request (method, path, query, headers and body) is passed on as it is, apart from hop-by-hop
//!   headers; WebSocket and other HTTP/1.1 upgrades are passed through as a byte stream.
//! - **Waking.** A request to a stopped app starts it, and waits; concurrent requests share the one start, which
//!   runs in the app's own task, so a client that goes away only stops its own wait. A browser's page load (`GET`
//!   with `Sec-Fetch-Mode: navigate`) that isn't answered within about a second gets the starting page instead
//!   (`503`, `Retry-After`, `no-store`), which polls with [`pages::STATUS_HEADER`] and reloads once the app is
//!   ready.
//! - **Leases.** A request holds a [`gate::Lease`] from when it's passed on until its response has been sent, a
//!   WebSocket until it closes. The app is stopped once its idle timeout has passed with no lease open.
//! - **Failures.** A connection to the app that fails gets `503`, isn't retried, and makes the next request check
//!   the app (and start it if needed). A start that failed is refused for a cooldown.
//!
//! - **Access.** The gateway listens on loopback, where anything on the node may use it, and (joined to a server)
//!   on the node's private address for the ingress ([`Access`]). There every request must carry the node's ingress
//!   token in [`INGRESS_HEADER`], or it's refused with `403` before anything else, so a machine on the private
//!   network can't skip the ingress's basic auth or wake an app. The token is removed before the request goes on.
//!   `X-Forwarded-For`, `-Proto` and `-Host` are passed on only from requests that carried the token, since the
//!   ingress sets them; on any other request they're replaced with what the gateway saw. `Forwarded`,
//!   `X-Forwarded-Port` and `X-Real-IP`, which the ingress doesn't set, are always removed.

pub mod gate;
pub mod pages;

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body as HttpBody, Frame, Incoming, SizeHint};
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, Request, Response, StatusCode, Uri, Version};
use hyper_util::rt::{TokioIo, TokioTimer};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::WaitForCancellationFutureOwned;

use crate::apps::Apps;
use gate::{Gate, Lease, Phase};
use kn_proto::agent::INGRESS_HEADER;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The body of every response the gateway sends.
pub type Body = http_body_util::combinators::BoxBody<Bytes, BoxError>;

/// How long a client may take to send a request's headers.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(30);

/// What a browser is told to wait before retrying, while an app starts.
const RETRY_AFTER_STARTING: u64 = 2;

/// Headers that belong to one connection, not to the request or response.
const HOP_BY_HOP: [HeaderName; 7] = [
    header::CONNECTION,
    HeaderName::from_static("keep-alive"),
    HeaderName::from_static("proxy-connection"),
    header::TE,
    header::TRAILER,
    header::TRANSFER_ENCODING,
    header::UPGRADE,
];

/// Headers that say where a request came from, which only the ingress is believed about.
const FORWARDING: [HeaderName; 6] = [
    HeaderName::from_static("forwarded"),
    HeaderName::from_static("x-forwarded-for"),
    HeaderName::from_static("x-forwarded-host"),
    HeaderName::from_static("x-forwarded-proto"),
    HeaderName::from_static("x-forwarded-port"),
    HeaderName::from_static("x-real-ip"),
];

/// Forwarding headers the ingress doesn't set, so a client's would get through it: removed even from the ingress's
/// requests.
const NOT_SET_BY_INGRESS: [HeaderName; 3] = [
    HeaderName::from_static("forwarded"),
    HeaderName::from_static("x-forwarded-port"),
    HeaderName::from_static("x-real-ip"),
];

/// Who may use a listener of the gateway.
#[derive(Clone)]
pub enum Access {
    /// On loopback: anything on the node. Nothing vouches for forwarding headers, so they're replaced.
    Local,
    /// On the node's private address, for the ingress: every request must carry this token.
    Ingress(std::sync::Arc<str>),
}

/// Compares two byte strings in time that depends only on their lengths.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What a listener's access makes of a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Admitted {
    /// It carried the ingress token: its forwarding headers are the ingress's.
    FromIngress,
    /// On loopback, without the token: nothing vouches for its forwarding headers.
    Local,
    Refused,
}

/// Checks a request against the listener's access, and removes the ingress token from it.
fn admit<B>(access: &Access, req: &mut Request<B>) -> Admitted {
    let given: Vec<HeaderValue> = match req.headers_mut().entry(INGRESS_HEADER) {
        header::Entry::Occupied(e) => e.remove_entry_mult().1.collect(),
        header::Entry::Vacant(_) => Vec::new(),
    };
    match access {
        Access::Ingress(token) => match given.as_slice() {
            [one] if !token.is_empty() && same(one.as_bytes(), token.as_bytes()) => Admitted::FromIngress,
            _ => Admitted::Refused,
        },
        Access::Local => Admitted::Local,
    }
}

/// Replaces a request's forwarding headers with what the gateway saw: the peer's address, plain HTTP, and the
/// Host.
fn own_forwarding<B>(req: &mut Request<B>, peer: std::net::SocketAddr) {
    let host = req.headers().get(header::HOST).cloned();
    let h = req.headers_mut();
    for name in &FORWARDING {
        h.remove(name);
    }
    if let Ok(v) = HeaderValue::from_str(&peer.ip().to_string()) {
        h.insert(HeaderName::from_static("x-forwarded-for"), v);
    }
    h.insert(
        HeaderName::from_static("x-forwarded-proto"),
        HeaderValue::from_static("http"),
    );
    if let Some(host) = host {
        h.insert(HeaderName::from_static("x-forwarded-host"), host);
    }
}

/// Serves the gateway on `listener`, to those `access` allows, until `shutdown` completes. Open connections are
/// dropped then.
pub async fn serve(
    listener: TcpListener,
    apps: Apps,
    access: Access,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let mut connections = tokio::task::JoinSet::new();
    tokio::pin!(shutdown);
    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(a) => a,
                Err(e) => {
                    // Out of file descriptors, say: don't spin.
                    tracing::warn!(error = %e, "gateway accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            () = &mut shutdown => break,
        };
        let _ = stream.set_nodelay(true);
        let (apps, access) = (apps.clone(), access.clone());
        connections.spawn(async move {
            let service = hyper::service::service_fn(move |mut req| {
                let (apps, access) = (apps.clone(), access.clone());
                async move {
                    match admit(&access, &mut req) {
                        Admitted::FromIngress => {
                            for name in &NOT_SET_BY_INGRESS {
                                req.headers_mut().remove(name);
                            }
                        }
                        Admitted::Local => own_forwarding(&mut req, peer),
                        Admitted::Refused => {
                            return Ok::<_, Infallible>(text(
                                StatusCode::FORBIDDEN,
                                "this address only takes requests from the cluster's ingress\n",
                            ));
                        }
                    }
                    Ok::<_, Infallible>(handle(&apps, req).await)
                }
            });
            let conn = hyper::server::conn::http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(HEADER_READ_TIMEOUT)
                .serve_connection(TokioIo::new(stream), service)
                .with_upgrades();
            if let Err(e) = conn.await {
                tracing::debug!(%peer, error = %e, "gateway connection ended");
            }
        });
        while connections.try_join_next().is_some() {}
    }
    Ok(())
}

/// A request's Host, in lowercase and without its port or a trailing dot.
pub fn request_host<B>(req: &Request<B>) -> Option<String> {
    let raw = match req.uri().host() {
        Some(h) => h,
        None => req.headers().get(header::HOST)?.to_str().ok()?,
    };
    normalize_host(raw)
}

fn normalize_host(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let name = if raw.starts_with('[') {
        &raw[..=raw.find(']')?]
    } else {
        match raw.rsplit_once(':') {
            Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => name,
            Some(_) => return None,
            None => raw,
        }
    };
    let name = name.strip_suffix('.').unwrap_or(name).to_ascii_lowercase();
    (!name.is_empty()).then_some(name)
}

/// Whether the request is a browser loading a page, which can be shown the starting page.
fn is_navigation<B>(req: &Request<B>) -> bool {
    req.method() == Method::GET
        && req
            .headers()
            .get("sec-fetch-mode")
            .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"navigate"))
}

fn is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(header::UPGRADE)
        && headers
            .get_all(header::CONNECTION)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(','))
            .any(|t| t.trim().eq_ignore_ascii_case("upgrade"))
}

async fn handle(apps: &Apps, req: Request<Incoming>) -> Response<Body> {
    let Some(host) = request_host(&req) else {
        return text(StatusCode::BAD_REQUEST, "the request has no valid Host\n");
    };
    let Some(gate) = apps.route(&host) else {
        return text(StatusCode::NOT_FOUND, "nothing is served at this address\n");
    };
    if req.method() == Method::GET && req.headers().contains_key(pages::STATUS_HEADER) {
        return status(apps, &gate);
    }
    let navigate = is_navigation(&req);
    let cfg = apps.config();
    let page_at = tokio::time::Instant::now() + cfg.starting_page_after;
    let give_up = tokio::time::Instant::now() + cfg.up_timeout + cfg.start_timeout + Duration::from_secs(30);
    let mut changes = gate.subscribe();
    loop {
        changes.borrow_and_update();
        let phase = match gate.lease() {
            Ok(lease) => return proxy(lease, req, navigate).await,
            Err(phase) => phase,
        };
        match phase {
            Phase::Stopped | Phase::Failed { .. } if apps.wake(&gate) => continue,
            Phase::Failed { retry_at } => {
                let wait = retry_at.saturating_duration_since(Instant::now()).as_secs().max(1);
                return refused(navigate, Refusal::Failed, Some(wait));
            }
            Phase::Suspended => return refused(navigate, Refusal::Suspended, None),
            Phase::Broken | Phase::Gone => return refused(navigate, Refusal::Unavailable, None),
            Phase::Stopped | Phase::Ready | Phase::Busy | Phase::Starting | Phase::Stopping => {}
        }
        tokio::select! {
            changed = changes.changed() => if changed.is_err() {
                return refused(navigate, Refusal::Unavailable, None);
            },
            () = tokio::time::sleep_until(page_at), if navigate => return starting_page(),
            () = tokio::time::sleep_until(give_up) => return refused(navigate, Refusal::TooSlow, None),
        }
    }
}

/// The answer to the starting page's poll. A stopped app is started, since a page is waiting for it; a failed
/// one isn't, so the page shows the failure rather than starting it again.
fn status(apps: &Apps, gate: &Gate) -> Response<Body> {
    let mut phase = gate.phase();
    if phase == Phase::Stopped && apps.wake(gate) {
        phase = Phase::Starting;
    }
    let state = match phase {
        Phase::Ready => "ready",
        Phase::Busy | Phase::Starting | Phase::Stopping | Phase::Stopped => "starting",
        Phase::Failed { .. } => "failed",
        Phase::Suspended => "suspended",
        Phase::Broken | Phase::Gone => "unavailable",
    };
    let mut resp = Response::new(full(format!("{{\"state\":\"{state}\"}}")));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn full(s: impl Into<Bytes>) -> Body {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn text(status: StatusCode, msg: &'static str) -> Response<Body> {
    let mut resp = Response::new(full(msg));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

fn unavailable(body: Body, content_type: &'static str, retry_after: Option<u64>) -> Response<Body> {
    let mut resp = Response::new(body);
    *resp.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    if let Some(secs) = retry_after {
        h.insert(header::RETRY_AFTER, HeaderValue::from(secs));
    }
    resp
}

fn starting_page() -> Response<Body> {
    unavailable(
        full(pages::starting()),
        "text/html; charset=utf-8",
        Some(RETRY_AFTER_STARTING),
    )
}

/// Why a request can't be served.
#[derive(Debug, Clone, Copy)]
enum Refusal {
    /// The app's last start failed, and it's cooling down.
    Failed,
    Suspended,
    /// Deleted, or its last deploy failed.
    Unavailable,
    /// Its connection failed.
    Unreachable,
    /// It took too long to start.
    TooSlow,
}

/// A `503`: a page for a browser, plain text otherwise. Neither says more than the app's state.
fn refused(navigate: bool, why: Refusal, retry_after: Option<u64>) -> Response<Body> {
    let (title, text) = match why {
        Refusal::Failed => (
            "This site couldn't start",
            "Something went wrong while starting it. Try again in a little while.",
        ),
        Refusal::Suspended => ("This site is suspended", "It has been switched off for now."),
        Refusal::Unavailable => ("This site is unavailable", "It isn't running at the moment."),
        Refusal::Unreachable => (
            "This site isn't answering",
            "It may have stopped. Try again in a moment.",
        ),
        Refusal::TooSlow => ("This site is taking too long to start", "Try again in a little while."),
    };
    if navigate {
        unavailable(
            full(pages::message(title, text)),
            "text/html; charset=utf-8",
            retry_after,
        )
    } else {
        unavailable(
            full(format!("{title}: {text}\n")),
            "text/plain; charset=utf-8",
            retry_after,
        )
    }
}

/// Removes hop-by-hop headers, and those the `Connection` header names. An upgrade keeps `Upgrade` and
/// `Connection: upgrade`.
fn strip_hop_by_hop(headers: &mut HeaderMap, upgrade: bool) {
    let named: Vec<HeaderName> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .filter_map(|n| HeaderName::from_bytes(n.trim().as_bytes()).ok())
        .collect();
    let protocol = headers.get(header::UPGRADE).cloned();
    for name in named.iter().chain(HOP_BY_HOP.iter()) {
        headers.remove(name);
    }
    if upgrade && let Some(protocol) = protocol {
        headers.insert(header::CONNECTION, HeaderValue::from_static("upgrade"));
        headers.insert(header::UPGRADE, protocol);
    }
}

/// The request as it goes to the app: the same, in origin form, without hop-by-hop headers.
fn upstream_request(req: Request<Incoming>, upgrade: bool) -> Request<Incoming> {
    let (mut parts, body) = req.into_parts();
    if !parts.headers.contains_key(header::HOST)
        && let Some(authority) = parts.uri.authority()
        && let Ok(v) = HeaderValue::from_str(authority.as_str())
    {
        parts.headers.insert(header::HOST, v);
    }
    let path = parts.uri.path_and_query().map_or("/", |p| p.as_str());
    parts.uri = path.parse().unwrap_or_else(|_| Uri::from_static("/"));
    parts.version = Version::HTTP_11;
    strip_hop_by_hop(&mut parts.headers, upgrade);
    Request::from_parts(parts, body)
}

/// Passes a request to the app, holding `lease` until the response is finished.
async fn proxy(lease: Lease, mut req: Request<Incoming>, navigate: bool) -> Response<Body> {
    let gate = lease.gate().clone();
    let client_upgrade = is_upgrade(req.headers()).then(|| hyper::upgrade::on(&mut req));
    let stream = match TcpStream::connect(("127.0.0.1", lease.port())).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(app = gate.name(), port = lease.port(), error = %e, "couldn't connect to the app");
            gate.upstream_failed();
            return refused(navigate, Refusal::Unreachable, None);
        }
    };
    let _ = stream.set_nodelay(true);
    let (mut sender, conn) = match hyper::client::conn::http1::handshake(TokioIo::new(stream)).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(app = gate.name(), error = %e, "couldn't talk to the app");
            gate.upstream_failed();
            return refused(navigate, Refusal::Unreachable, None);
        }
    };
    let name = gate.name().to_string();
    tokio::spawn(async move {
        if let Err(e) = conn.with_upgrades().await {
            tracing::debug!(app = name, error = %e, "connection to the app ended");
        }
    });
    let upgrade = client_upgrade.is_some();
    let mut resp = match sender.send_request(upstream_request(req, upgrade)).await {
        Ok(r) => r,
        // The client's request body failed: it went away, most likely.
        Err(e) if e.is_user() => {
            tracing::debug!(app = gate.name(), error = %e, "the request failed");
            return text(StatusCode::BAD_REQUEST, "the request couldn't be read\n");
        }
        Err(e) => {
            // The app accepted the connection but didn't answer: it may have stopped. Check it next time.
            tracing::warn!(app = gate.name(), error = %e, "the app didn't answer");
            gate.upstream_failed();
            return refused(navigate, Refusal::Unreachable, None);
        }
    };
    if resp.status() == StatusCode::SWITCHING_PROTOCOLS {
        let Some(client_upgrade) = client_upgrade else {
            tracing::warn!(app = gate.name(), "the app switched protocols without being asked");
            return text(
                StatusCode::BAD_GATEWAY,
                "the app answered with an upgrade nobody asked for\n",
            );
        };
        let app_upgrade = hyper::upgrade::on(&mut resp);
        tokio::spawn(tunnel(lease, client_upgrade, app_upgrade));
        let (mut parts, _) = resp.into_parts();
        strip_hop_by_hop(&mut parts.headers, true);
        return Response::from_parts(parts, Full::new(Bytes::new()).map_err(|never| match never {}).boxed());
    }
    let (mut parts, body) = resp.into_parts();
    strip_hop_by_hop(&mut parts.headers, false);
    Response::from_parts(parts, LeasedBody::new(body, lease).boxed())
}

/// Copies bytes both ways between an upgraded client connection and the app, holding the lease until both are
/// closed, or the lease is broken.
async fn tunnel(lease: Lease, client: hyper::upgrade::OnUpgrade, app: hyper::upgrade::OnUpgrade) {
    let cancelled = lease.cancelled();
    let copy = async {
        let (client, app) = tokio::try_join!(client, app)?;
        let (mut client, mut app) = (TokioIo::new(client), TokioIo::new(app));
        tokio::io::copy_bidirectional(&mut client, &mut app).await?;
        Ok::<_, BoxError>(())
    };
    tokio::select! {
        result = copy => if let Err(e) = result {
            tracing::debug!(app = lease.gate().name(), error = %e, "upgraded connection ended");
        },
        () = cancelled => tracing::debug!(app = lease.gate().name(), "upgraded connection cut off"),
    }
}

/// A response body from the app that holds its request's lease until it ends, and fails (cutting the connection
/// off) once the lease is broken.
struct LeasedBody {
    inner: Incoming,
    lease: Option<Lease>,
    cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
}

impl LeasedBody {
    fn new(inner: Incoming, lease: Lease) -> Self {
        LeasedBody {
            cancelled: Box::pin(lease.cancelled()),
            lease: Some(lease),
            inner,
        }
    }
}

impl HttpBody for LeasedBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, BoxError>>> {
        let this = self.get_mut();
        if this.lease.is_some() && this.cancelled.as_mut().poll(cx).is_ready() {
            this.lease = None;
            return Poll::Ready(Some(Err("the app was deleted".into())));
        }
        match Pin::new(&mut this.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(Some(Ok(frame))),
            Poll::Ready(Some(Err(e))) => {
                this.lease = None;
                Poll::Ready(Some(Err(e.into())))
            }
            Poll::Ready(None) => {
                this.lease = None;
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

#[cfg(test)]
mod tests;

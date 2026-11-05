//! The web UI: its files, embedded in the server, its users' logins and sessions, and the checks on requests that
//! carry a session cookie.
//!
//! The UI (`web/`, React and Vite) is built into `web/dist` by `make web`, and embedded with `rust-embed` (a page
//! saying it wasn't built, if it wasn't). It's served at `/` on the server's own listener, so on its API domain
//! through the ingress, and it uses the same API as the CLI.
//!
//! **Logins.** A user (`kiwi-nest user add NAME`) logs in with a password (`POST /session`), checked against its
//! Argon2id hash, and gets a session cookie ([`SESSION_COOKIE`]): a random token, `HttpOnly` (scripts can't read
//! it), `Secure`, `SameSite=Strict` and host-only through the `__Host-` prefix, so an app on a sibling domain can't
//! set one. The server keeps only the token's hash; see [`crate::store::users`] for how long a session lasts. After
//! a few failed logins for a name, the next is refused for a while, doubling each time; at most a few passwords are
//! checked at once, since each check takes memory on purpose.
//!
//! **Requests with the cookie** ([`check_cookie_request`]). Apps live on the cluster's other domains, which a browser
//! counts as the same *site* as the server's, so `SameSite` alone doesn't keep their pages from sending the cookie.
//! So a request with it is refused unless it comes from the server's own pages: `Sec-Fetch-Site` must be
//! `same-origin` (or `none`, for an address typed in), and `Origin`, if sent, must be the server's own. One that
//! changes anything must also say so positively (`Sec-Fetch-Site: same-origin`, or a matching `Origin`) and be JSON
//! (`Content-Type: application/json`): no form or plain request from another page can be, and anything else needs
//! a CORS preflight, which the server never grants. Logging in is checked the same way. A request with a token in
//! `Authorization` is judged by the token alone.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use argon2::{Argon2, PasswordHasher, PasswordVerifier};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, put};
use kn_proto::api::{CreateUser, Login, SESSION_COOKIE, SessionInfo, SetPassword, UserInfo};
use rust_embed::RustEmbed;

use crate::Server;
use crate::auth::{self, Caller};
use crate::error::{Error, Result};
use crate::store::{self, users};

/// The built UI, or a page saying it wasn't built (chosen by `build.rs`).
#[derive(RustEmbed)]
#[folder = "$KN_UI_DIR"]
struct Files;

/// The Content-Security-Policy of the UI's pages: only the server's own scripts, styles and API.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
                   connect-src 'self'; object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// Failed logins for one name before the next ones are slowed down.
const FREE_FAILURES: u32 = 5;
/// The longest a name is locked out for after failed logins.
const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);
/// Passwords checked at once.
const CONCURRENT_CHECKS: usize = 4;

pub fn router() -> Router<Server> {
    let p = kn_proto::API_PREFIX;
    Router::new()
        .route(&format!("{p}/session"), get(session_info).post(login).delete(logout))
        .route(&format!("{p}/users"), get(user_list).post(user_create))
        .route(&format!("{p}/users/{{name}}"), delete(user_delete))
        .route(&format!("{p}/users/{{name}}/password"), put(user_password))
}

/// Failed logins, by user name, and the limit on passwords checked at once.
pub struct Logins {
    failures: Mutex<HashMap<String, (u32, Instant)>>,
    checking: tokio::sync::Semaphore,
}

impl Default for Logins {
    fn default() -> Self {
        Logins {
            failures: Mutex::new(HashMap::new()),
            checking: tokio::sync::Semaphore::new(CONCURRENT_CHECKS),
        }
    }
}

impl Logins {
    /// Refuses a login for `name` while it's locked out after failures.
    fn check(&self, name: &str, now: Instant) -> Result<()> {
        let f = self.failures.lock().expect("logins lock");
        if let Some(&(n, last)) = f.get(name)
            && n >= FREE_FAILURES
        {
            let wait = lockout(n);
            let until = last + wait;
            if now < until {
                return Err(Error::TooManyRequests(format!(
                    "too many failed logins for `{name}`; try again in {}s",
                    (until - now).as_secs().max(1)
                )));
            }
        }
        Ok(())
    }

    fn failed(&self, name: &str, now: Instant) {
        let mut f = self.failures.lock().expect("logins lock");
        // Forget names that have been quiet for long enough, so the map can't grow without end.
        f.retain(|_, (_, last)| now.saturating_duration_since(*last) < MAX_LOCKOUT * 2);
        let e = f.entry(name.to_string()).or_insert((0, now));
        e.0 = e.0.saturating_add(1);
        e.1 = now;
    }

    fn succeeded(&self, name: &str) {
        self.failures.lock().expect("logins lock").remove(name);
    }
}

/// How long a name is locked out for after `n` failures in a row: a second after the free ones, doubling.
fn lockout(n: u32) -> Duration {
    let doublings = n.saturating_sub(FREE_FAILURES).min(20);
    Duration::from_secs(1u64 << doublings).min(MAX_LOCKOUT)
}

/// Hashes a password for storing (Argon2id, with a random salt, as a PHC string).
pub fn hash_password(password: &str) -> Result<String> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).expect("the system's random number generator");
    Argon2::default()
        .hash_password_with_salt(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| Error::Internal(anyhow::anyhow!("hashing a password: {e}")))
}

/// Whether `password` matches a stored hash.
fn verify_password(password: &str, hash: &str) -> bool {
    Argon2::default().verify_password(password.as_bytes(), hash).is_ok()
}

/// A hash no password is expected to match, checked for a name that isn't a user's, so the answer takes as long.
fn decoy_hash() -> &'static str {
    static DECOY: OnceLock<String> = OnceLock::new();
    DECOY.get_or_init(|| hash_password("there is no such user").expect("hashing"))
}

/// Refuses a password that's too short (or absurdly long).
fn check_password(password: &str) -> Result<()> {
    if password.chars().count() < 10 {
        return Err(Error::BadRequest("a password needs at least 10 characters".into()));
    }
    if password.len() > 1024 {
        return Err(Error::BadRequest("a password can't be longer than 1024 bytes".into()));
    }
    Ok(())
}

/// Makes a session token (`kns_…`).
fn new_session_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the system's random number generator");
    format!("kns_{}", hex::encode(bytes))
}

fn session_cookie(token: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; Secure; SameSite=Strict"
    ))
    .expect("a cookie of hex digits")
}

fn cleared_cookie() -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}=; Path=/; HttpOnly; Secure; SameSite=Strict; Max-Age=0"
    ))
    .expect("a valid cookie")
}

/// The values of the cookies called `name`.
fn cookies<'a>(headers: &'a HeaderMap, name: &str) -> Vec<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|c| c.trim().split_once('='))
        .filter(|(n, _)| *n == name)
        .map(|(_, v)| v.trim())
        .filter(|v| !v.is_empty())
        .collect()
}

/// `HOST[:PORT]`, lowercase, without the scheme's default port.
fn authority(host: &str, scheme: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    let default = match scheme {
        "https" => Some(":443"),
        "http" => Some(":80"),
        _ => None,
    };
    match default {
        Some(d) if host.ends_with(d) => host[..host.len() - d.len()].to_string(),
        _ => host,
    }
}

/// Whether `origin` (an `Origin` header) is the server's own, as the request's `Host` names it.
fn same_origin(origin: &str, host: &str) -> bool {
    let Some((scheme, rest)) = origin.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") || rest.contains('/') {
        return false;
    }
    !host.is_empty() && authority(rest, scheme) == authority(host, scheme)
}

/// Refuses a request carrying the session cookie (or logging in) that isn't from the server's own pages, and one
/// that changes anything without being JSON; see the module's documentation.
pub fn check_cookie_request(method: &Method, uri: &Uri, headers: &HeaderMap) -> Result<()> {
    let text = |h: header::HeaderName| headers.get(h).and_then(|v| v.to_str().ok());
    let site = text(header::HeaderName::from_static("sec-fetch-site"));
    if let Some(site) = site
        && !matches!(site, "same-origin" | "none")
    {
        return Err(Error::Forbidden(format!(
            "refused a {site} request: the web UI's session only works from the server's own pages"
        )));
    }
    let host = text(header::HOST)
        .map(str::to_string)
        .or_else(|| uri.authority().map(|a| a.to_string()))
        .unwrap_or_default();
    let origin = match text(header::ORIGIN) {
        None => None,
        Some(o) if same_origin(o, &host) => Some(true),
        Some(o) => {
            return Err(Error::Forbidden(format!(
                "refused a request from {o}: the web UI's session only works from the server's own pages"
            )));
        }
    };
    let changes = !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS);
    if changes {
        if site != Some("same-origin") && origin != Some(true) {
            return Err(Error::Forbidden(
                "a request that changes anything must come from the server's own pages (with Origin or \
                 Sec-Fetch-Site saying so)"
                    .into(),
            ));
        }
        let json = text(header::CONTENT_TYPE)
            .and_then(|ct| ct.split(';').next())
            .is_some_and(|ct| ct.trim().eq_ignore_ascii_case("application/json"));
        if !json {
            return Err(Error::BadRequest(
                "a request that changes anything must be JSON (Content-Type: application/json)".into(),
            ));
        }
    }
    Ok(())
}

/// A web UI user's session.
pub struct Session {
    pub user: users::User,
    pub info: SessionInfo,
    hash: String,
}

/// The session a request's cookie names, if it has the cookie; refused if the request fails
/// [`check_cookie_request`] or the session has ended.
pub async fn session(parts: &Parts, server: &Server) -> Result<Option<Session>> {
    let tokens: Vec<String> = cookies(&parts.headers, SESSION_COOKIE)
        .into_iter()
        .map(str::to_string)
        .collect();
    if tokens.is_empty() {
        return Ok(None);
    }
    check_cookie_request(&parts.method, &parts.uri, &parts.headers)?;
    let hashes: Vec<String> = tokens.iter().map(|t| auth::hash(t)).collect();
    let found = server
        .store
        .call(move |c| {
            let now = store::now();
            for h in hashes {
                if let Some((user, info)) = users::session(c, &h, now)? {
                    return Ok(Some(Session { user, info, hash: h }));
                }
            }
            Ok(None)
        })
        .await?;
    match found {
        Some(s) => Ok(Some(s)),
        None => Err(Error::Unauthorized("the session has ended; log in again".into())),
    }
}

async fn session_info(State(s): State<Server>, parts: Parts) -> Result<axum::Json<SessionInfo>> {
    match session(&parts, &s).await? {
        Some(session) => Ok(axum::Json(session.info)),
        None => Err(Error::Unauthorized("not logged in".into())),
    }
}

async fn login(State(s): State<Server>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> Result<Response> {
    check_cookie_request(&method, &uri, &headers)?;
    let req: Login = serde_json::from_slice(&body)
        .map_err(|e| Error::BadRequest(format!("the body must be {{\"user\": …, \"password\": …}}: {e}")))?;
    let name = req.user.trim().to_string();
    let wrong = || Error::Unauthorized("wrong user name or password".into());
    if !users::valid_name(&name) || req.password.len() > 1024 {
        return Err(wrong());
    }
    s.logins.check(&name, Instant::now())?;
    let n = name.clone();
    let user = s.store.call(move |c| users::get(c, &n)).await?;
    let ok = {
        let _permit = s
            .logins
            .checking
            .acquire()
            .await
            .map_err(|e| Error::Internal(e.into()))?;
        let (hash, password) = (
            user.as_ref()
                .map_or_else(|| decoy_hash().to_string(), |u| u.password_hash.clone()),
            req.password,
        );
        let matched = tokio::task::spawn_blocking(move || verify_password(&password, &hash))
            .await
            .map_err(|e| Error::Internal(e.into()))?;
        matched && user.is_some()
    };
    let Some(user) = user.filter(|_| ok) else {
        s.logins.failed(&name, Instant::now());
        tracing::warn!(user = name, "a failed login");
        return Err(wrong());
    };
    s.logins.succeeded(&name);
    let token = new_session_token();
    let (h, id) = (auth::hash(&token), user.id);
    let info = s
        .store
        .call(move |c| {
            let now = store::now();
            users::start_session(c, id, &h, now)?;
            users::session(c, &h, now)?
                .map(|(_, info)| info)
                .ok_or_else(|| Error::Internal(anyhow::anyhow!("the new session is gone")))
        })
        .await?;
    tracing::info!(user = user.name, "logged in");
    let mut resp = axum::Json(info).into_response();
    resp.headers_mut().insert(header::SET_COOKIE, session_cookie(&token));
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(resp)
}

async fn logout(State(s): State<Server>, parts: Parts) -> Result<Response> {
    // Refused (cross-site, not JSON) before anything, as any request with the cookie is.
    let found = match session(&parts, &s).await {
        Ok(found) => found,
        Err(Error::Unauthorized(_)) => None,
        Err(e) => return Err(e),
    };
    if let Some(session) = found {
        let h = session.hash.clone();
        s.store.call(move |c| users::end_session(c, &h)).await?;
        tracing::info!(user = session.user.name, "logged out");
    }
    let mut resp = StatusCode::NO_CONTENT.into_response();
    resp.headers_mut().insert(header::SET_COOKIE, cleared_cookie());
    Ok(resp)
}

fn check_user_name(name: &str) -> Result<()> {
    if users::valid_name(name) {
        Ok(())
    } else {
        Err(Error::BadRequest(format!(
            "`{name}` isn't a valid user name: use lowercase letters, digits, `-`, `_` and `.`, at most 64"
        )))
    }
}

async fn hash_blocking(s: &Server, password: String) -> Result<String> {
    let _permit = s
        .logins
        .checking
        .acquire()
        .await
        .map_err(|e| Error::Internal(e.into()))?;
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| Error::Internal(e.into()))?
}

async fn user_list(State(s): State<Server>, caller: Caller) -> Result<axum::Json<Vec<UserInfo>>> {
    caller.admin()?;
    Ok(axum::Json(s.store.call(|c| users::list(c)).await?))
}

async fn user_create(
    State(s): State<Server>,
    caller: Caller,
    axum::Json(req): axum::Json<CreateUser>,
) -> Result<(StatusCode, axum::Json<UserInfo>)> {
    caller.admin()?;
    check_user_name(&req.name)?;
    check_password(&req.password)?;
    let hash = hash_blocking(&s, req.password).await?;
    let name = req.name.clone();
    let info = s
        .store
        .call(move |c| users::create(c, &name, &hash, store::now()))
        .await?;
    tracing::info!(user = req.name, by = caller.by(), "made a user");
    Ok((StatusCode::CREATED, axum::Json(info)))
}

async fn user_password(
    State(s): State<Server>,
    caller: Caller,
    Path(name): Path<String>,
    axum::Json(req): axum::Json<SetPassword>,
) -> Result<StatusCode> {
    caller.admin()?;
    check_user_name(&name)?;
    check_password(&req.password)?;
    let hash = hash_blocking(&s, req.password).await?;
    let n = name.clone();
    s.store
        .call(move |c| users::set_password(c, &n, &hash, store::now()))
        .await?;
    s.logins.succeeded(&name);
    tracing::info!(
        user = name,
        by = caller.by(),
        "changed a user's password; their sessions ended"
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn user_delete(State(s): State<Server>, caller: Caller, Path(name): Path<String>) -> Result<StatusCode> {
    caller.admin()?;
    check_user_name(&name)?;
    let n = name.clone();
    s.store.call(move |c| users::delete(c, &n)).await?;
    tracing::info!(user = name, by = caller.by(), "deleted a user");
    Ok(StatusCode::NO_CONTENT)
}

/// Serves the UI's files, for every request no route took: `/` is `index.html`. An unknown API route gets the
/// API's 404.
pub async fn files(method: Method, uri: Uri) -> Response {
    let path = uri.path();
    let api = kn_proto::API_PREFIX;
    if path == api || path.starts_with(&format!("{api}/")) {
        return Error::NotFound(format!("there's no route {method} {path}")).into_response();
    }
    if method != Method::GET && method != Method::HEAD {
        return (StatusCode::METHOD_NOT_ALLOWED, "only GET\n").into_response();
    }
    let name = match path.trim_start_matches('/') {
        "" => "index.html",
        p => p,
    };
    let Some(file) = Files::get(name) else {
        return (StatusCode::NOT_FOUND, "not found\n").into_response();
    };
    let cache = if name.starts_with("assets/") {
        // Vite names them by their contents.
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let headers = [
        (header::CONTENT_TYPE, file.metadata.mimetype().to_string()),
        (header::CACHE_CONTROL, cache.to_string()),
        (header::CONTENT_SECURITY_POLICY, CSP.to_string()),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
        (header::X_FRAME_OPTIONS, "DENY".to_string()),
        (header::REFERRER_POLICY, "same-origin".to_string()),
    ];
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        Body::from(file.data.into_owned())
    };
    (headers, body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_compared_by_host_and_port() {
        assert!(same_origin("https://kn.example.com", "kn.example.com"));
        assert!(same_origin("https://kn.example.com", "KN.example.com:443"));
        assert!(same_origin("http://127.0.0.1:7400", "127.0.0.1:7400"));
        assert!(!same_origin("https://blog.example.com", "kn.example.com"));
        assert!(!same_origin("https://kn.example.com:8443", "kn.example.com"));
        assert!(!same_origin("null", "kn.example.com"));
        assert!(!same_origin("https://kn.example.com/x", "kn.example.com"));
        assert!(!same_origin("https://kn.example.com", ""));
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn cookie_requests_must_be_same_origin_and_changes_json() {
        let uri: Uri = "/v1/apps".parse().unwrap();
        let ok = |m: Method, h: &[(&'static str, &str)]| check_cookie_request(&m, &uri, &headers(h)).is_ok();
        let host = ("host", "kn.example.com");
        let json = ("content-type", "application/json");
        // Reading, from the page or an address typed in.
        assert!(ok(Method::GET, &[host]));
        assert!(ok(Method::GET, &[host, ("sec-fetch-site", "same-origin")]));
        assert!(ok(Method::GET, &[host, ("sec-fetch-site", "none")]));
        // Another site, even a sibling domain (an app's), is refused.
        assert!(!ok(Method::GET, &[host, ("sec-fetch-site", "same-site")]));
        assert!(!ok(Method::GET, &[host, ("sec-fetch-site", "cross-site")]));
        assert!(!ok(Method::GET, &[host, ("origin", "https://blog.example.com")]));
        // A change: JSON, and said to be from the server's own page.
        let page = ("origin", "https://kn.example.com");
        assert!(ok(Method::POST, &[host, page, json]));
        assert!(ok(Method::POST, &[host, ("sec-fetch-site", "same-origin"), json]));
        assert!(ok(
            Method::DELETE,
            &[host, page, ("content-type", "application/json; charset=utf-8")]
        ));
        assert!(!ok(Method::POST, &[host, json]), "nothing says where it's from");
        assert!(!ok(Method::POST, &[host, ("sec-fetch-site", "none"), json]));
        assert!(!ok(Method::POST, &[host, page]), "not JSON");
        assert!(!ok(Method::POST, &[host, page, ("content-type", "text/plain")]));
        assert!(!ok(
            Method::POST,
            &[host, page, ("content-type", "application/x-www-form-urlencoded")]
        ));
        assert!(!ok(Method::POST, &[host, ("origin", "https://blog.example.com"), json]));
        assert!(!ok(Method::POST, &[host, page, ("sec-fetch-site", "same-site"), json]));
    }

    #[test]
    fn cookies_are_found_by_name() {
        let h = headers(&[
            ("cookie", "a=1; __Host-kn-session=kns_x"),
            ("cookie", "__Host-kn-session=kns_y"),
        ]);
        assert_eq!(cookies(&h, SESSION_COOKIE), vec!["kns_x", "kns_y"]);
        assert!(cookies(&h, "b").is_empty());
    }

    #[test]
    fn failed_logins_lock_a_name_out_for_longer_each_time() {
        let l = Logins::default();
        let t = Instant::now();
        for _ in 0..FREE_FAILURES {
            assert!(l.check("ivan", t).is_ok());
            l.failed("ivan", t);
        }
        assert!(matches!(l.check("ivan", t), Err(Error::TooManyRequests(_))));
        assert!(l.check("kim", t).is_ok(), "other names aren't affected");
        assert!(l.check("ivan", t + Duration::from_secs(1)).is_ok());
        l.failed("ivan", t + Duration::from_secs(1));
        assert!(l.check("ivan", t + Duration::from_secs(2)).is_err(), "two seconds now");
        assert!(l.check("ivan", t + Duration::from_secs(3)).is_ok());
        assert_eq!(lockout(100), MAX_LOCKOUT);
        l.succeeded("ivan");
        assert!(l.check("ivan", t).is_ok());
    }

    #[test]
    fn passwords_hash_and_verify() {
        let h = hash_password("correct horse battery").unwrap();
        assert!(h.starts_with("$argon2id$"));
        assert!(verify_password("correct horse battery", &h));
        assert!(!verify_password("correct horse batterx", &h));
        assert!(!verify_password("x", "not a hash"));
        assert!(check_password("short").is_err());
        assert!(check_password("long enough!").is_ok());
    }
}

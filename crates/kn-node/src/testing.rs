//! A fake runtime for unit tests: each "project" is a small HTTP server on the app's loopback port, started and
//! stopped as `docker compose` would start and stop its containers.
//!
//! The server answers `/` with `hello from PROJECT`, `/echo` with the request, `/stream?n=N` with N lines a tenth
//! of a second apart, and `/ws` (an upgrade) by echoing bytes back.

use std::collections::HashMap;
use std::convert::Infallible;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures_util::future::BoxFuture;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Child;
use tokio::task::JoinHandle;

use crate::runtime::{ContainerState, LogsOptions, Project, ProjectState, Runtime, RuntimeError};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// How the fake behaves when asked to start a project.
#[derive(Debug, Clone, Default)]
pub struct Behaviour {
    /// How long `start` takes.
    pub start_delay: Duration,
    /// `start` succeeds, but the routed service crashes and keeps restarting.
    pub crash_on_start: bool,
    /// How long `up` takes.
    pub up_delay: Duration,
}

#[derive(Default)]
pub struct Fake {
    projects: Mutex<HashMap<String, FakeProject>>,
    calls: Mutex<Vec<(&'static str, String)>>,
    pub behaviour: Mutex<Behaviour>,
}

struct FakeProject {
    port: u16,
    routed: String,
    containers: Vec<ContainerState>,
    server: Option<JoinHandle<()>>,
}

impl FakeProject {
    fn set_all(&mut self, state: &str, exit_code: i64) {
        for c in &mut self.containers {
            c.state = state.into();
            c.exit_code = exit_code;
        }
    }

    fn stop_server(&mut self) {
        if let Some(s) = self.server.take() {
            s.abort();
        }
    }
}

impl Drop for FakeProject {
    fn drop(&mut self) {
        self.stop_server();
    }
}

fn container(project: &str, service: &str, state: &str, exit_code: i64) -> ContainerState {
    ContainerState {
        id: format!("{project}-{service}"),
        name: format!("{project}-{service}-1"),
        service: service.into(),
        state: state.into(),
        exit_code,
        ..Default::default()
    }
}

/// The services, the routed one and its published port, from a generated Compose file.
fn read_compose(file: &Path) -> (Vec<String>, String, u16) {
    let text = std::fs::read_to_string(file).expect("the generated Compose file");
    let doc: serde_yaml_ng::Value = serde_yaml_ng::from_str(&text).unwrap();
    let mut services = Vec::new();
    let (mut routed, mut port) = (String::new(), 0);
    for (name, svc) in doc["services"].as_mapping().unwrap() {
        let name = name.as_str().unwrap().to_string();
        if let Some(p) = svc["ports"][0]["published"].as_str() {
            routed = name.clone();
            port = p.parse().unwrap();
        }
        services.push(name);
    }
    (services, routed, port)
}

impl Fake {
    pub fn new() -> Arc<Fake> {
        Arc::new(Fake::default())
    }

    fn log(&self, call: &'static str, project: &str) {
        self.calls.lock().unwrap().push((call, project.to_string()));
    }

    /// How many times `call` (`up`, `start`, `stop`, `down`) was made for `project`.
    pub fn calls(&self, call: &str, project: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(c, p)| *c == call && p == project)
            .count()
    }

    pub fn set_behaviour(&self, b: Behaviour) {
        *self.behaviour.lock().unwrap() = b;
    }

    /// Adds a project as an earlier node agent left it: each service in `states` with its state and exit code.
    /// Its server runs if the routed service (the first) is running.
    pub fn add_project(&self, project: &str, port: u16, states: &[(&str, &str, i64)]) {
        let mut p = FakeProject {
            port,
            routed: states[0].0.to_string(),
            containers: states
                .iter()
                .map(|(s, state, code)| container(project, s, state, *code))
                .collect(),
            server: None,
        };
        if states[0].1 == "running" {
            p.server = Some(spawn_server(project.to_string(), port));
        }
        self.projects.lock().unwrap().insert(project.to_string(), p);
    }

    /// The state of one service's container.
    pub fn state(&self, project: &str, service: &str) -> Option<String> {
        let projects = self.projects.lock().unwrap();
        let p = projects.get(project)?;
        p.containers
            .iter()
            .find(|c| c.service == service)
            .map(|c| c.state.clone())
    }

    /// Stops the project's server, as if the app had stopped answering, while its containers still look running.
    pub fn kill_server(&self, project: &str) {
        if let Some(p) = self.projects.lock().unwrap().get_mut(project) {
            p.stop_server();
        }
    }

    fn run(&self, project: &str) {
        let mut projects = self.projects.lock().unwrap();
        let p = projects.get_mut(project).expect("a known project");
        p.set_all("running", 0);
        if p.server.is_none() {
            p.server = Some(spawn_server(project.to_string(), p.port));
        }
    }
}

fn ok() -> BoxFuture<'static, Result<(), RuntimeError>> {
    Box::pin(async { Ok(()) })
}

impl Runtime for Fake {
    fn up<'a>(&'a self, p: &'a Project, _wait: Duration) -> BoxFuture<'a, Result<(), RuntimeError>> {
        Box::pin(async move {
            self.log("up", &p.name);
            let delay = self.behaviour.lock().unwrap().up_delay;
            tokio::time::sleep(delay).await;
            let (services, routed, port) = read_compose(&p.file);
            {
                let mut projects = self.projects.lock().unwrap();
                let fp = projects.entry(p.name.clone()).or_insert_with(|| FakeProject {
                    port,
                    routed: routed.clone(),
                    containers: Vec::new(),
                    server: None,
                });
                fp.stop_server();
                fp.port = port;
                fp.routed = routed;
                fp.containers = services.iter().map(|s| container(&p.name, s, "running", 0)).collect();
            }
            self.run(&p.name);
            Ok(())
        })
    }

    fn start<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>> {
        Box::pin(async move {
            self.log("start", &p.name);
            let b = self.behaviour.lock().unwrap().clone();
            tokio::time::sleep(b.start_delay).await;
            if b.crash_on_start {
                let mut projects = self.projects.lock().unwrap();
                let fp = projects.get_mut(&p.name).expect("a known project");
                let routed = fp.routed.clone();
                for c in &mut fp.containers {
                    c.state = if c.service == routed { "restarting" } else { "running" }.into();
                    c.status = "Restarting (1) 1 second ago".into();
                }
                return Ok(());
            }
            self.run(&p.name);
            Ok(())
        })
    }

    fn stop<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>> {
        self.log("stop", &p.name);
        if let Some(fp) = self.projects.lock().unwrap().get_mut(&p.name) {
            fp.stop_server();
            fp.set_all("exited", 0);
        }
        ok()
    }

    fn down<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>> {
        self.log("down", &p.name);
        self.projects.lock().unwrap().remove(&p.name);
        ok()
    }

    fn pause<'a>(&'a self, _p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>> {
        ok()
    }

    fn unpause<'a>(&'a self, _p: &'a Project) -> BoxFuture<'a, Result<(), RuntimeError>> {
        ok()
    }

    fn ps<'a>(&'a self, p: &'a Project) -> BoxFuture<'a, Result<Vec<ContainerState>, RuntimeError>> {
        let cs = self
            .projects
            .lock()
            .unwrap()
            .get(&p.name)
            .map(|fp| fp.containers.clone())
            .unwrap_or_default();
        Box::pin(async move { Ok(cs) })
    }

    fn inspect<'a>(&'a self, _ids: &'a [String]) -> BoxFuture<'a, Result<serde_json::Value, RuntimeError>> {
        Box::pin(async { Ok(serde_json::json!([])) })
    }

    fn projects<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<ProjectState>, RuntimeError>> {
        let all = self
            .projects
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _)| name.starts_with(prefix))
            .map(|(name, fp)| ProjectState {
                name: name.clone(),
                status: format!("{}({})", fp.containers[0].state, fp.containers.len()),
            })
            .collect();
        Box::pin(async move { Ok(all) })
    }

    fn logs(&self, p: &Project, _opts: &LogsOptions) -> Result<Child, RuntimeError> {
        Err(RuntimeError::Output {
            command: format!("logs {}", p.name),
            message: "the fake runtime has no logs".into(),
        })
    }
}

/// Serves the fake app on a loopback port until aborted, which also closes every connection.
fn spawn_server(project: String, port: u16) -> JoinHandle<()> {
    tokio::spawn(async move {
        // A server that was just aborted may not have let go of the port yet.
        let listener = loop {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(l) => break l,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        let mut conns = tokio::task::JoinSet::new();
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let project = project.clone();
            conns.spawn(async move {
                let svc = hyper::service::service_fn(move |req| {
                    let project = project.clone();
                    async move { Ok::<_, Infallible>(app(&project, req).await) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), svc)
                    .with_upgrades()
                    .await;
            });
            while conns.try_join_next().is_some() {}
        }
    })
}

type AppBody = http_body_util::combinators::BoxBody<Bytes, BoxError>;

fn full(s: impl Into<Bytes>) -> AppBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

async fn app(project: &str, mut req: Request<Incoming>) -> Response<AppBody> {
    match req.uri().path() {
        "/ws" => {
            let on_upgrade = hyper::upgrade::on(&mut req);
            tokio::spawn(async move {
                if let Ok(upgraded) = on_upgrade.await {
                    let mut io = TokioIo::new(upgraded);
                    let mut buf = [0u8; 1024];
                    while let Ok(n) = io.read(&mut buf).await {
                        if n == 0 || io.write_all(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                }
            });
            Response::builder()
                .status(StatusCode::SWITCHING_PROTOCOLS)
                .header("connection", "upgrade")
                .header("upgrade", "websocket")
                .body(full(""))
                .unwrap()
        }
        "/echo" => {
            let head = format!(
                "{} {} host={} x-test={}\n",
                req.method(),
                req.uri().path_and_query().unwrap(),
                req.headers().get("host").and_then(|h| h.to_str().ok()).unwrap_or(""),
                req.headers().get("x-test").and_then(|h| h.to_str().ok()).unwrap_or(""),
            );
            let body = req.into_body().collect().await.unwrap().to_bytes();
            let mut out = head.into_bytes();
            out.extend_from_slice(&body);
            Response::new(full(out))
        }
        "/stream" => {
            let n: usize = req
                .uri()
                .query()
                .and_then(|q| q.strip_prefix("n="))
                .and_then(|n| n.parse().ok())
                .unwrap_or(3);
            let lines = futures_util::stream::unfold(0, move |i| async move {
                if i == n {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                Some((
                    Ok::<_, BoxError>(Frame::data(Bytes::from(format!("tick {i}\n")))),
                    i + 1,
                ))
            });
            Response::new(StreamBody::new(lines).boxed())
        }
        _ => Response::new(full(format!("hello from {project}"))),
    }
}

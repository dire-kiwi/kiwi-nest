//! Starting, stopping and adopting apps, run by each app's task.
//!
//! - **Starting** checks the app's mounts, as before every start, and runs `docker compose start` (or `up`, if some
//!   of the app's containers are missing). It then waits until every container is running and healthy and the
//!   app's port answers. A start that fails or times out is followed by `docker compose stop`, so a crashing app
//!   isn't left for Docker to restart over and over, and the gate refuses requests for the cooldown.
//! - **Idle stops** run `docker compose stop` once the reaper finds an app with no open requests for its timeout.
//! - **Suspending** is an explicit stop, refused while requests are open. Requests don't start a suspended app;
//!   a resume does.
//! - **Adopting**, when the node agent starts: an app whose containers are all running is left alone; one with none
//!   running is stopped (and an always-on one is started); one that's only partly running is stopped and suspended,
//!   since nobody knows what it was in the middle of.

use std::time::{Duration, Instant};

use kn_proto::api::AppInfo;
use kn_proto::state::AppStatus;
use tokio::io::AsyncReadExt;

use super::{App, AppRecord, AppsError, Result, resting_phase};
use crate::gateway::gate::Phase;
use crate::runtime::{ContainerState, Project};

/// How often a start checks the containers.
const READY_POLL: Duration = Duration::from_millis(200);

/// How long the port check waits for a connection to be accepted, then for it to be closed on us.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const SETTLE_TIMEOUT: Duration = Duration::from_millis(100);

/// What the containers say about a start.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Readiness {
    /// Every container is running (and healthy, if it has a healthcheck), or has finished cleanly.
    Up,
    Waiting,
    Failed(String),
}

/// Whether a start is done, from the app's containers. `routed` is the service that gets HTTP traffic, which must
/// be running; other services may also have exited cleanly (one-off jobs).
pub(crate) fn readiness(containers: &[ContainerState], services: &[String], routed: &str) -> Readiness {
    let mut waiting = false;
    for s in services {
        if !containers.iter().any(|c| &c.service == s) {
            return Readiness::Failed(format!("service `{s}` has no container"));
        }
    }
    for c in containers {
        let what = format!("service `{}`", c.service);
        match c.state.as_str() {
            "running" => match c.health.as_str() {
                "" | "healthy" => {}
                "unhealthy" => return Readiness::Failed(format!("{what} is unhealthy")),
                _ => waiting = true,
            },
            "exited" if c.exit_code == 0 && c.service != routed => {}
            "exited" | "dead" => {
                return Readiness::Failed(format!("{what} exited with code {}", c.exit_code));
            }
            "restarting" => return Readiness::Failed(format!("{what} keeps restarting ({})", c.status)),
            "paused" => return Readiness::Failed(format!("{what} is paused")),
            _ => waiting = true,
        }
    }
    if waiting { Readiness::Waiting } else { Readiness::Up }
}

/// What the agent finds of an app's containers when it starts.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Found {
    /// Every service's container is running (healthy or not) or has finished cleanly.
    Running,
    /// Nothing is running.
    Stopped,
    /// Some containers run and others don't.
    Partly,
}

pub(crate) fn found(containers: &[ContainerState], services: &[String]) -> Found {
    let running = |c: &ContainerState| c.state == "running";
    if !containers.iter().any(running) {
        return Found::Stopped;
    }
    let all_there = services.iter().all(|s| containers.iter().any(|c| &c.service == s));
    let all_fine = containers
        .iter()
        .all(|c| running(c) || (c.state == "exited" && c.exit_code == 0));
    if all_there && all_fine {
        Found::Running
    } else {
        Found::Partly
    }
}

/// Whether something answers on a loopback port: it accepts a connection and doesn't close it straight away. A
/// published port can accept connections for a container that isn't listening yet, and then drop them.
pub(crate) async fn port_answers(port: u16) -> bool {
    let connect = tokio::net::TcpStream::connect(("127.0.0.1", port));
    let Ok(Ok(mut stream)) = tokio::time::timeout(CONNECT_TIMEOUT, connect).await else {
        return false;
    };
    let mut buf = [0u8; 1];
    match tokio::time::timeout(SETTLE_TIMEOUT, stream.read(&mut buf)).await {
        // Still open, waiting for a request.
        Err(_) => true,
        // A server that speaks first.
        Ok(Ok(n)) => n > 0,
        Ok(Err(_)) => false,
    }
}

impl App<'_> {
    fn gate(&self) -> std::sync::Arc<crate::gateway::gate::Gate> {
        let r = self.inner.gate(self.name);
        r.unwrap_or_else(|| {
            // Every app with a record has a gate; this one is a stand-in that nothing routes to.
            crate::gateway::gate::Gate::new(self.name, 0, Duration::ZERO, Phase::Gone)
        })
    }

    /// Records the app's status (and error), logging rather than failing if the state file can't be written: the
    /// containers have already changed.
    async fn set_status(&self, status: AppStatus, error: Option<String>) -> Option<AppRecord> {
        match self
            .update(|r| {
                r.status = status;
                r.error = error;
            })
            .await
        {
            Ok(r) => Some(r),
            Err(e) => {
                tracing::warn!(app = self.name, error = %e, "couldn't record the app's status");
                None
            }
        }
    }

    /// Starts the app and waits until it's ready. See [`super::Op::Start`].
    pub(super) async fn start(&self, explicit: bool) -> Result<AppInfo> {
        let gate = self.gate();
        let r = match self.record().await {
            Ok(r) => r,
            Err(e) => {
                gate.set_phase(Phase::Gone);
                return Err(e);
            }
        };
        if !explicit {
            match r.status {
                AppStatus::Suspended | AppStatus::Failed => {
                    // Suspended or failed since the request found the app stopped.
                    gate.set_phase(resting_phase(r.status));
                    return Err(AppsError::Busy(format!("app `{}` is {}", self.name, r.status)));
                }
                _ if gate.phase() == Phase::Ready => return self.info(r).await,
                _ => {}
            }
        } else if gate.phase() == Phase::Ready && r.status == AppStatus::Running {
            return self.info(r).await;
        }
        gate.set_phase(Phase::Starting);
        let cfg = &self.inner.cfg;
        tracing::info!(app = self.name, "starting");
        let started = Instant::now();
        let project = self.project(&r);
        let result = match tokio::time::timeout(cfg.start_timeout, self.bring_up(&r, &project)).await {
            Ok(result) => result,
            Err(_) => Err(format!(
                "it wasn't ready within {}",
                humantime::format_duration(cfg.start_timeout)
            )),
        };
        match result {
            Ok(()) => {
                tracing::info!(app = self.name, elapsed = ?started.elapsed(), "started");
                let r = self.set_status(AppStatus::Running, None).await.unwrap_or(r);
                gate.set_phase(Phase::Ready);
                self.info(r).await
            }
            Err(message) => {
                tracing::warn!(app = self.name, %message, "starting failed; stopping it");
                if let Err(e) = self.inner.runtime.stop(&project).await {
                    tracing::warn!(app = self.name, error = %e, "couldn't stop the app after it failed to start");
                }
                self.set_status(AppStatus::Idle, Some(format!("starting failed: {message}")))
                    .await;
                gate.set_phase(Phase::Failed {
                    retry_at: Instant::now() + cfg.start_cooldown,
                });
                Err(AppsError::StartFailed {
                    name: self.name.into(),
                    message,
                })
            }
        }
    }

    async fn bring_up(&self, r: &AppRecord, project: &Project) -> std::result::Result<(), String> {
        let rt = &self.inner.runtime;
        // As before every start: the mounts must be the subvolumes kiwi-nest made, and the shared volumes it mounts
        // can't change meanwhile.
        let _locks = self.inner.lock_shared(r.shared.keys()).await;
        self.verify_mounts().await.map_err(|e| e.to_string())?;
        let containers = rt.ps(project).await.map_err(|e| e.to_string())?;
        let missing = r.services.iter().any(|s| !containers.iter().any(|c| &c.service == s));
        if missing {
            tracing::info!(app = self.name, "some containers are missing; creating them");
            rt.up(project, self.inner.cfg.start_timeout).await
        } else {
            rt.start(project).await
        }
        .map_err(|e| e.to_string())?;
        loop {
            let containers = rt.ps(project).await.map_err(|e| e.to_string())?;
            match readiness(&containers, &r.services, &r.service) {
                Readiness::Failed(message) => return Err(message),
                Readiness::Up if port_answers(r.port).await => return Ok(()),
                Readiness::Up | Readiness::Waiting => {}
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    /// Stops the app for being idle, if the reaper's decision still stands.
    pub(super) async fn idle_stop(&self) {
        let gate = self.gate();
        if gate.phase() != Phase::Stopping {
            return;
        }
        let Ok(r) = self.record().await else {
            gate.set_phase(Phase::Gone);
            return;
        };
        tracing::info!(
            app = self.name,
            idle_timeout_secs = r.idle_timeout_secs,
            "stopping: idle"
        );
        if let Err(e) = self.inner.runtime.stop(&self.project(&r)).await {
            // The next request starts it again, whatever state it's in.
            tracing::warn!(app = self.name, error = %e, "couldn't stop the idle app");
        }
        self.set_status(AppStatus::Idle, None).await;
        gate.set_phase(Phase::Stopped);
    }

    pub(super) async fn suspend(&self) -> Result<AppInfo> {
        let r = self.record().await?;
        let gate = self.gate();
        let before = gate.phase();
        gate.begin_suspend().map_err(|n| {
            AppsError::Busy(format!(
                "app `{}` has {n} open request{}; suspend it once {} finished",
                self.name,
                if n == 1 { "" } else { "s" },
                if n == 1 { "it has" } else { "they have" },
            ))
        })?;
        tracing::info!(app = self.name, "suspending");
        if let Err(e) = self.inner.runtime.stop(&self.project(&r)).await {
            gate.set_phase(if before == Phase::Ready { Phase::Stopped } else { before });
            return Err(e.into());
        }
        let r = self.set_status(AppStatus::Suspended, None).await.unwrap_or(r);
        gate.set_phase(Phase::Suspended);
        self.info(r).await
    }

    /// Takes over the app after the node agent started: see the module's documentation.
    pub(super) async fn adopt(&self) {
        let gate = self.gate();
        let Ok(r) = self.record().await else {
            gate.set_phase(Phase::Gone);
            return;
        };
        let project = self.project(&r);
        let containers = match self.inner.runtime.ps(&project).await {
            Ok(c) => c,
            Err(e) => {
                // The next request tries to start it.
                tracing::warn!(app = self.name, error = %e, "couldn't check the app's containers");
                gate.set_phase(resting_phase(r.status));
                return;
            }
        };
        let found = found(&containers, &r.services);
        match r.status {
            AppStatus::Running | AppStatus::Idle => match found {
                Found::Running => {
                    tracing::info!(app = self.name, "adopted: running");
                    if r.status != AppStatus::Running {
                        self.set_status(AppStatus::Running, r.error.clone()).await;
                    }
                    gate.set_phase(Phase::Ready);
                }
                Found::Stopped => {
                    if r.status != AppStatus::Idle {
                        self.set_status(AppStatus::Idle, r.error.clone()).await;
                    }
                    gate.set_phase(Phase::Stopped);
                    if r.idle_timeout_secs == 0 {
                        tracing::info!(app = self.name, "adopted: stopped, and always on, so starting it");
                        let _ = self.start(false).await;
                    } else {
                        tracing::info!(app = self.name, "adopted: stopped until a request");
                    }
                }
                Found::Partly => {
                    tracing::warn!(app = self.name, "found partly running; stopping and suspending it");
                    if let Err(e) = self.inner.runtime.stop(&project).await {
                        tracing::warn!(app = self.name, error = %e, "couldn't stop the app");
                    }
                    self.set_status(
                        AppStatus::Suspended,
                        Some(
                            "it was only partly running when the node agent started, so it was stopped; resume it \
                             to start it again"
                                .into(),
                        ),
                    )
                    .await;
                    gate.set_phase(Phase::Suspended);
                }
            },
            AppStatus::Suspended => {
                if found != Found::Stopped {
                    tracing::info!(app = self.name, "suspended, but running; stopping it");
                    if let Err(e) = self.inner.runtime.stop(&project).await {
                        tracing::warn!(app = self.name, error = %e, "couldn't stop the app");
                    }
                }
                gate.set_phase(Phase::Suspended);
            }
            // Its containers are left as they are, as after any failed deploy.
            AppStatus::Failed | AppStatus::Deploying => gate.set_phase(Phase::Broken),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(service: &str, state: &str, health: &str, exit_code: i64) -> ContainerState {
        ContainerState {
            service: service.into(),
            state: state.into(),
            health: health.into(),
            exit_code,
            ..Default::default()
        }
    }

    fn services(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn readiness_from_containers() {
        let svc = services(&["web", "db", "migrate"]);
        let up = [
            c("web", "running", "", 0),
            c("db", "running", "healthy", 0),
            c("migrate", "exited", "", 0),
        ];
        assert_eq!(readiness(&up, &svc, "web"), Readiness::Up);
        let starting = [
            c("web", "running", "", 0),
            c("db", "running", "starting", 0),
            c("migrate", "created", "", 0),
        ];
        assert_eq!(readiness(&starting, &svc, "web"), Readiness::Waiting);
        for (bad, why) in [
            (c("db", "running", "unhealthy", 0), "unhealthy"),
            (c("db", "exited", "", 1), "exited with code 1"),
            (c("db", "restarting", "", 0), "keeps restarting"),
            (c("db", "paused", "", 0), "paused"),
        ] {
            let cs = [c("web", "running", "", 0), bad, c("migrate", "exited", "", 0)];
            match readiness(&cs, &svc, "web") {
                Readiness::Failed(m) => assert!(m.contains(why), "{m}"),
                other => panic!("{other:?}"),
            }
        }
        // The routed service must keep running, and every service needs a container.
        let cs = [c("web", "exited", "", 0)];
        assert!(matches!(
            readiness(&cs, &services(&["web"]), "web"),
            Readiness::Failed(_)
        ));
        let cs = [c("web", "running", "", 0)];
        assert!(matches!(readiness(&cs, &svc, "web"), Readiness::Failed(m) if m.contains("no container")));
    }

    #[test]
    fn what_the_agent_finds() {
        let svc = services(&["web", "db"]);
        let all = [c("web", "running", "", 0), c("db", "running", "unhealthy", 0)];
        assert_eq!(found(&all, &svc), Found::Running);
        let none = [c("web", "exited", "", 0), c("db", "exited", "", 137)];
        assert_eq!(found(&none, &svc), Found::Stopped);
        assert_eq!(found(&[], &svc), Found::Stopped);
        let partly = [c("web", "running", "", 0), c("db", "exited", "", 137)];
        assert_eq!(found(&partly, &svc), Found::Partly);
        let missing = [c("web", "running", "", 0)];
        assert_eq!(found(&missing, &svc), Found::Partly);
        let restarting = [c("web", "running", "", 0), c("db", "restarting", "", 1)];
        assert_eq!(found(&restarting, &svc), Found::Partly);
    }

    #[tokio::test]
    async fn the_port_check_needs_a_listener_that_keeps_the_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let keep = tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                let (s, _) = listener.accept().await.unwrap();
                held.push(s);
            }
        });
        assert!(port_answers(port).await);
        keep.abort();
        let _ = keep.await;

        // Accepts, then hangs up straight away, like a published port whose container isn't listening.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let drop_all = tokio::spawn(async move {
            loop {
                let (s, _) = listener.accept().await.unwrap();
                drop(s);
            }
        });
        assert!(!port_answers(port).await);
        drop_all.abort();
        let _ = drop_all.await;
        assert!(!port_answers(port).await, "nothing listens any more");
    }
}

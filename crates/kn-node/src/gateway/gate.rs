//! Each app's gate: whether requests can go through now, and the leases of those that did.
//!
//! The gateway reads the gate; the app's task changes its phase as it starts, stops and deploys the app. Every
//! change happens under one lock together with the lease count, so an app is never stopped between a request
//! checking that it's ready and taking its lease.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// Where an app is, as far as requests are concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Being deployed, or checked after the node agent started: requests wait.
    Busy,
    /// A start is running: requests wait for it.
    Starting,
    /// Running: requests go through.
    Ready,
    /// Being stopped for being idle, or suspended: requests wait, then start it again (unless it was suspended).
    Stopping,
    /// Not running: the next request starts it.
    Stopped,
    /// The last start failed. Requests are refused until `retry_at`; the first one after that starts it again.
    Failed { retry_at: Instant },
    /// Stopped on purpose: requests are refused until a resume.
    Suspended,
    /// The last deploy failed: requests are refused until a deploy or a resume.
    Broken,
    /// Deleted.
    Gone,
}

impl Phase {
    /// Whether a request should wait for the phase to change.
    pub fn is_transient(self) -> bool {
        matches!(self, Phase::Busy | Phase::Starting | Phase::Stopping)
    }
}

/// One app's gate.
pub struct Gate {
    name: String,
    state: Mutex<State>,
    phase: watch::Sender<Phase>,
}

struct State {
    port: u16,
    /// Zero means never stopped for being idle.
    idle_timeout: Duration,
    leases: u64,
    /// When the last lease ended, or the app became ready.
    last_active: Instant,
    /// When a lease was last taken or ended: the app's last activity, as the UI shows it.
    last_request: Option<SystemTime>,
    /// Cancelled to break every open lease.
    cancel: CancellationToken,
}

impl Gate {
    pub fn new(name: &str, port: u16, idle_timeout: Duration, phase: Phase) -> Arc<Gate> {
        Arc::new(Gate {
            name: name.to_string(),
            state: Mutex::new(State {
                port,
                idle_timeout,
                leases: 0,
                last_active: Instant::now(),
                last_request: None,
                cancel: CancellationToken::new(),
            }),
            phase: watch::Sender::new(phase),
        })
    }

    /// The app's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().expect("gate lock")
    }

    pub fn phase(&self) -> Phase {
        *self.phase.borrow()
    }

    /// Changes to the phase. Mark the current one seen, then check the gate, then wait: nothing is missed.
    pub fn subscribe(&self) -> watch::Receiver<Phase> {
        self.phase.subscribe()
    }

    /// Where requests go, and when the app is stopped for being idle.
    pub fn configure(&self, port: u16, idle_timeout: Duration) {
        let mut s = self.lock();
        s.port = port;
        s.idle_timeout = idle_timeout;
    }

    pub fn set_phase(&self, phase: Phase) {
        let mut s = self.lock();
        if phase == Phase::Ready {
            // The idle timeout counts from when the app came up.
            s.last_active = Instant::now();
        }
        self.phase.send_replace(phase);
    }

    /// A lease for a request, if the app is ready; otherwise the phase it's in.
    pub fn lease(self: &Arc<Self>) -> Result<Lease, Phase> {
        let mut s = self.lock();
        let phase = *self.phase.borrow();
        if phase != Phase::Ready {
            return Err(phase);
        }
        s.leases += 1;
        s.last_request = Some(SystemTime::now());
        Ok(Lease {
            gate: self.clone(),
            port: s.port,
            cancel: s.cancel.clone(),
        })
    }

    pub fn open_leases(&self) -> u64 {
        self.lock().leases
    }

    /// When a request last began or ended, if one has since the gate was made, and how many are open.
    pub fn activity(&self) -> (Option<SystemTime>, u64) {
        let s = self.lock();
        (s.last_request, s.leases)
    }

    /// Moves a stopped app (or one whose cooldown is over) to `Starting`; whoever gets `true` must start it.
    pub fn begin_start(&self, now: Instant) -> bool {
        let _s = self.lock();
        let phase = *self.phase.borrow();
        let start = match phase {
            Phase::Stopped => true,
            Phase::Failed { retry_at } => now >= retry_at,
            _ => false,
        };
        if start {
            self.phase.send_replace(Phase::Starting);
        }
        start
    }

    /// Moves a ready app that has had no open leases for its idle timeout to `Stopping`; whoever gets `true` must
    /// stop it.
    pub fn begin_idle_stop(&self, now: Instant) -> bool {
        let s = self.lock();
        let stop = *self.phase.borrow() == Phase::Ready
            && s.leases == 0
            && !s.idle_timeout.is_zero()
            && now.saturating_duration_since(s.last_active) >= s.idle_timeout;
        if stop {
            self.phase.send_replace(Phase::Stopping);
        }
        stop
    }

    /// Moves the app to `Stopping` for a suspend, unless requests are open (it returns how many).
    pub fn begin_suspend(&self) -> Result<(), u64> {
        let s = self.lock();
        if s.leases > 0 {
            return Err(s.leases);
        }
        self.phase.send_replace(Phase::Stopping);
        Ok(())
    }

    /// A connection to the app failed: the next request checks it, and starts it if needed.
    pub fn upstream_failed(&self) {
        let _s = self.lock();
        if *self.phase.borrow() == Phase::Ready {
            self.phase.send_replace(Phase::Stopped);
        }
    }

    /// Ends every open lease: their responses and WebSockets are cut off.
    pub fn break_leases(&self) {
        let mut s = self.lock();
        let old = std::mem::replace(&mut s.cancel, CancellationToken::new());
        old.cancel();
    }
}

/// A request's hold on its app: while any lease is open, the app isn't stopped for being idle.
pub struct Lease {
    gate: Arc<Gate>,
    port: u16,
    cancel: CancellationToken,
}

impl Lease {
    /// The app's loopback port.
    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn gate(&self) -> &Arc<Gate> {
        &self.gate
    }

    /// Cancelled when the lease is broken (the app is being deleted).
    pub fn cancelled(&self) -> tokio_util::sync::WaitForCancellationFutureOwned {
        self.cancel.clone().cancelled_owned()
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut s = self.gate.lock();
        s.leases -= 1;
        s.last_active = Instant::now();
        s.last_request = Some(SystemTime::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leases_keep_an_app_from_idling() {
        let g = Gate::new("a", 1, Duration::from_millis(10), Phase::Ready);
        let lease = g.lease().unwrap();
        let later = Instant::now() + Duration::from_secs(1);
        assert!(!g.begin_idle_stop(later), "a lease is open");
        assert_eq!(g.begin_suspend(), Err(1));
        let (began, open) = g.activity();
        assert_eq!(open, 1);
        drop(lease);
        let (ended, open) = g.activity();
        assert_eq!(open, 0);
        assert!(
            began.is_some() && ended >= began,
            "a lease's start and end are activity"
        );
        assert_eq!(g.open_leases(), 0);
        assert!(
            !g.begin_idle_stop(Instant::now()),
            "the timeout counts from the last lease"
        );
        assert!(g.begin_idle_stop(later));
        assert_eq!(g.phase(), Phase::Stopping);
        assert!(matches!(g.lease(), Err(Phase::Stopping)));
    }

    #[test]
    fn a_zero_timeout_never_idles() {
        let g = Gate::new("a", 1, Duration::ZERO, Phase::Ready);
        assert!(!g.begin_idle_stop(Instant::now() + Duration::from_secs(86400)));
    }

    #[test]
    fn only_one_caller_starts_an_app() {
        let g = Gate::new("a", 1, Duration::ZERO, Phase::Stopped);
        let now = Instant::now();
        assert!(g.begin_start(now));
        assert!(!g.begin_start(now));
        assert_eq!(g.phase(), Phase::Starting);
    }

    #[test]
    fn a_failed_start_cools_down() {
        let now = Instant::now();
        let retry_at = now + Duration::from_secs(5);
        let g = Gate::new("a", 1, Duration::ZERO, Phase::Failed { retry_at });
        assert!(!g.begin_start(now));
        assert!(g.begin_start(retry_at));
    }

    #[test]
    fn a_failed_connection_marks_the_app_for_recovery() {
        let g = Gate::new("a", 1, Duration::ZERO, Phase::Ready);
        g.upstream_failed();
        assert_eq!(g.phase(), Phase::Stopped);
        let g = Gate::new("a", 1, Duration::ZERO, Phase::Suspended);
        g.upstream_failed();
        assert_eq!(g.phase(), Phase::Suspended);
    }

    #[tokio::test]
    async fn breaking_leases_cancels_them() {
        let g = Gate::new("a", 1, Duration::ZERO, Phase::Ready);
        let lease = g.lease().unwrap();
        let cancelled = lease.cancelled();
        g.break_leases();
        tokio::time::timeout(Duration::from_secs(1), cancelled).await.unwrap();
        // Later leases aren't affected.
        let lease2 = g.lease().unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), lease2.cancelled())
                .await
                .is_err()
        );
        drop(lease);
    }
}

//! What the server keeps in memory about nodes: which are online, waking their polls, the reports that waiters
//! follow, and the callers waiting for commands' results.
//!
//! None of it needs to survive a restart. A node is online while it has a poll open, or polled within
//! `offline_after`; after a restart, nodes are online again as soon as they poll.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use kn_proto::agent::CommandResult;
use tokio::sync::{Notify, mpsc, oneshot, watch};

/// How a command ended, for whoever is waiting for it.
pub enum Outcome {
    Result(CommandResult),
    /// The command's output, as the node streams it.
    Stream(mpsc::Receiver<std::io::Result<Bytes>>),
    /// The node didn't get the command, or restarted while running it.
    Lost,
}

pub struct Hub {
    offline_after: Duration,
    nodes: Mutex<HashMap<u64, Arc<NodeHub>>>,
    waiters: Mutex<HashMap<u64, oneshot::Sender<Outcome>>>,
}

/// One node's part of the hub.
pub struct NodeHub {
    /// Notified when the node's desired state changes or a command is queued for it, so its open poll answers; and
    /// when a newer poll arrives, so an older one still open answers and gets out of the way.
    pub wake: Notify,
    /// Counts the node's polls: only the newest may hand out changes and commands, so a poll whose connection the
    /// node has given up on can't take a command meant for the one that replaced it.
    epoch: AtomicU64,
    /// Bumped after each report is recorded, for callers waiting for a change to be done.
    pub reports: watch::Sender<u64>,
    open_polls: AtomicUsize,
    last_seen: Mutex<Option<Instant>>,
}

/// Held while a node's poll is open: the node counts as online.
pub struct OpenPoll {
    node: Arc<NodeHub>,
    epoch: u64,
}

impl OpenPoll {
    pub fn node(&self) -> &NodeHub {
        &self.node
    }

    /// Whether a newer poll from the same node has arrived.
    pub fn superseded(&self) -> bool {
        self.node.epoch.load(Ordering::SeqCst) != self.epoch
    }
}

impl Drop for OpenPoll {
    fn drop(&mut self) {
        *self.node.last_seen.lock().expect("last seen") = Some(Instant::now());
        self.node.open_polls.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Hub {
    pub fn new(offline_after: Duration) -> Hub {
        Hub {
            offline_after,
            nodes: Mutex::new(HashMap::new()),
            waiters: Mutex::new(HashMap::new()),
        }
    }

    pub fn node(&self, id: u64) -> Arc<NodeHub> {
        self.nodes
            .lock()
            .expect("nodes")
            .entry(id)
            .or_insert_with(|| {
                Arc::new(NodeHub {
                    wake: Notify::new(),
                    epoch: AtomicU64::new(0),
                    reports: watch::Sender::new(0),
                    open_polls: AtomicUsize::new(0),
                    last_seen: Mutex::new(None),
                })
            })
            .clone()
    }

    pub fn is_online(&self, id: u64) -> bool {
        let n = self.node(id);
        n.open_polls.load(Ordering::SeqCst) > 0
            || n.last_seen
                .lock()
                .expect("last seen")
                .is_some_and(|t| t.elapsed() < self.offline_after)
    }

    /// Marks a poll from the node as open, until the returned guard is dropped, and makes it the newest.
    pub fn open_poll(&self, id: u64) -> OpenPoll {
        let n = self.node(id);
        n.open_polls.fetch_add(1, Ordering::SeqCst);
        let epoch = n.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        *n.last_seen.lock().expect("last seen") = Some(Instant::now());
        n.wake.notify_waiters();
        OpenPoll { node: n, epoch }
    }

    /// Wakes the node's open poll, if any. (A poll checks for changes before it waits, so none is missed.)
    pub fn wake(&self, id: u64) {
        self.node(id).wake.notify_waiters();
    }

    pub fn reported(&self, id: u64) {
        self.node(id).reports.send_modify(|n| *n += 1);
    }

    /// Registers a waiter for a command's result.
    pub fn wait_for(&self, command: u64) -> oneshot::Receiver<Outcome> {
        let (tx, rx) = oneshot::channel();
        self.waiters.lock().expect("waiters").insert(command, tx);
        rx
    }

    /// Hands a command's outcome to its waiter; returns false if nobody was waiting.
    pub fn settle(&self, command: u64, outcome: Outcome) -> bool {
        let tx = self.waiters.lock().expect("waiters").remove(&command);
        match tx {
            Some(tx) => tx.send(outcome).is_ok(),
            None => false,
        }
    }

    pub fn forget_waiter(&self, command: u64) {
        self.waiters.lock().expect("waiters").remove(&command);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nodes_are_online_while_polling_and_for_a_while_after() {
        let hub = Hub::new(Duration::from_millis(50));
        assert!(!hub.is_online(1));
        let poll = hub.open_poll(1);
        std::thread::sleep(Duration::from_millis(80));
        assert!(hub.is_online(1), "a node with an open poll is online");
        drop(poll);
        assert!(hub.is_online(1));
        std::thread::sleep(Duration::from_millis(80));
        assert!(!hub.is_online(1));
    }
}

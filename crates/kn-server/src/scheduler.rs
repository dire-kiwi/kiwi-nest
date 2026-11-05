//! Picking a node for a new app or shared volume.
//!
//! For now: among the nodes that are online (and have the bundle's architecture, if it names one), the one with the
//! most memory left once its apps' memory is counted (each service's `mem_limit`, or 256 MiB without one). Ties go
//! to fewer apps, then to the name. Nests, the filters and the full score come in step 5.

use std::collections::HashMap;

use rusqlite::Connection;

use crate::error::{Error, Result};
use crate::store::{apps, nodes};

/// A node's memory left: its memory minus what its apps may use.
pub fn free_memory(memory: u64, placed: u64) -> i128 {
    i128::from(memory) - i128::from(placed)
}

pub fn pick(c: &Connection, online: &dyn Fn(u64) -> bool, arch: Option<&str>) -> Result<u64> {
    let all = nodes::list(c)?;
    if all.is_empty() {
        return Err(Error::Conflict(
            "no node has joined the cluster yet: make a join token with `kiwi-nest token create join`".into(),
        ));
    }
    let placed: HashMap<u64, (u64, u64)> = apps::placed(c)?
        .into_iter()
        .map(|(n, mem, count)| (n, (mem, count)))
        .collect();
    let best = all
        .iter()
        .filter(|n| online(n.id))
        .filter(|n| arch.is_none_or(|a| n.facts.as_ref().is_some_and(|f| f.arch == a)))
        .map(|n| {
            let (mem, count) = placed.get(&n.id).copied().unwrap_or_default();
            let total = n.facts.as_ref().map_or(0, |f| f.memory_bytes);
            (free_memory(total, mem), count, n)
        })
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)).then(b.2.name.cmp(&a.2.name)));
    match best {
        Some((_, _, n)) => Ok(n.id),
        None if arch.is_some() => Err(Error::Offline(format!(
            "no online node has arch `{}`; pass --node and --queue to deploy to an offline one",
            arch.unwrap_or_default()
        ))),
        None => Err(Error::Offline(
            "no node is online; pass --node and --queue to deploy to an offline one".into(),
        )),
    }
}

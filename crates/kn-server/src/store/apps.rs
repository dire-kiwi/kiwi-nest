//! Apps and shared volumes: placing them, the desired state of each node, and whether a change is done.
//!
//! Each change (deploy, delete, `volume create|load|delete`) is one transaction, which checks everything first and
//! saves nothing if it refuses: the name's permission was checked by the caller; here, the node (and that it's
//! online, unless the change is queued), the domain, `--from` and the shared volumes. A saved change bumps the node's
//! sequence number, and the app or volume records it with its new generation. Generations of a name only ever grow,
//! even across a delete, so a node never mistakes a new app for one it has already applied.
//!
//! A deleted app or volume stays `deleting` (keeping its name and domain) until its node reports the delete done.

use std::path::{Path, PathBuf};

use kn_proto::agent::{AppChange, AppWant, Outcome, Report, VolumeChange, VolumeWant};
use kn_proto::api::{Accepted, ChangeStatus};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::nodes::{self, Node};
use crate::bundles::{Bundles, CheckedApp};
use crate::error::{Error, Result};
use crate::scheduler;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Active,
    Deleting,
    Deleted,
}

impl State {
    fn parse(s: &str) -> State {
        match s {
            "active" => State::Active,
            "deleting" => State::Deleting,
            _ => State::Deleted,
        }
    }
}

/// An app as the server records it.
#[derive(Debug, Clone)]
pub struct AppRow {
    pub name: String,
    pub state: State,
    pub node_id: Option<u64>,
    pub domain: Option<String>,
    pub generation: u64,
    pub seq: u64,
    pub bundle_digest: Option<String>,
    pub from_app: Option<String>,
    pub shared: Vec<String>,
    pub memory_bytes: u64,
    pub keep_data: bool,
    pub created_at: u64,
    pub updated_at: u64,
}

/// A shared volume as the server records it.
#[derive(Debug, Clone)]
pub struct VolumeRow {
    pub name: String,
    pub state: State,
    pub node_id: Option<u64>,
    pub generation: u64,
    pub seq: u64,
    pub data_digest: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

const APP_COLUMNS: &str = "name, state, node_id, domain, generation, seq, bundle_digest, from_app, shared, \
                           memory_bytes, keep_data, created_at, updated_at";

fn app_row(r: &rusqlite::Row) -> rusqlite::Result<AppRow> {
    let shared: String = r.get(8)?;
    Ok(AppRow {
        name: r.get(0)?,
        state: State::parse(&r.get::<_, String>(1)?),
        node_id: r.get(2)?,
        domain: r.get(3)?,
        generation: r.get(4)?,
        seq: r.get(5)?,
        bundle_digest: r.get(6)?,
        from_app: r.get(7)?,
        shared: serde_json::from_str(&shared).unwrap_or_default(),
        memory_bytes: r.get(9)?,
        keep_data: r.get(10)?,
        created_at: r.get(11)?,
        updated_at: r.get(12)?,
    })
}

const VOLUME_COLUMNS: &str = "name, state, node_id, generation, seq, data_digest, created_at, updated_at";

fn volume_row(r: &rusqlite::Row) -> rusqlite::Result<VolumeRow> {
    Ok(VolumeRow {
        name: r.get(0)?,
        state: State::parse(&r.get::<_, String>(1)?),
        node_id: r.get(2)?,
        generation: r.get(3)?,
        seq: r.get(4)?,
        data_digest: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

pub fn app(c: &Connection, name: &str) -> Result<Option<AppRow>> {
    Ok(c.query_row(
        &format!("SELECT {APP_COLUMNS} FROM apps WHERE name = ?1"),
        [name],
        app_row,
    )
    .optional()?)
}

/// An app that exists (it may be being deleted).
pub fn live_app(c: &Connection, name: &str) -> Result<AppRow> {
    app(c, name)?
        .filter(|a| a.state != State::Deleted)
        .ok_or_else(|| Error::NotFound(format!("app `{name}` doesn't exist")))
}

/// Every app that exists, by name.
pub fn apps(c: &Connection) -> Result<Vec<AppRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {APP_COLUMNS} FROM apps WHERE state != 'deleted' ORDER BY name"
    ))?;
    let rows = st.query_map([], app_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn volume(c: &Connection, name: &str) -> Result<Option<VolumeRow>> {
    Ok(c.query_row(
        &format!("SELECT {VOLUME_COLUMNS} FROM volumes WHERE name = ?1"),
        [name],
        volume_row,
    )
    .optional()?)
}

pub fn live_volume(c: &Connection, name: &str) -> Result<VolumeRow> {
    volume(c, name)?
        .filter(|v| v.state != State::Deleted)
        .ok_or_else(|| Error::NotFound(format!("shared volume `{name}` doesn't exist")))
}

pub fn volumes(c: &Connection) -> Result<Vec<VolumeRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {VOLUME_COLUMNS} FROM volumes WHERE state != 'deleted' ORDER BY name"
    ))?;
    let rows = st.query_map([], volume_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A change the store saved, and the node it's for.
#[derive(Debug, Clone)]
pub struct Saved {
    pub accepted: Accepted,
    pub node_id: u64,
}

/// Bumps a node's sequence number; returns the new one.
fn bump(tx: &Transaction, node_id: u64) -> Result<u64> {
    Ok(tx.query_row(
        "UPDATE nodes SET seq = seq + 1 WHERE id = ?1 RETURNING seq",
        [node_id],
        |r| r.get(0),
    )?)
}

/// Refuses a change for an offline node, unless it's queued.
fn check_online(node: &Node, online: &dyn Fn(u64) -> bool, queue: bool, what: &str) -> Result<bool> {
    let up = online(node.id);
    if !up && !queue {
        return Err(Error::Offline(format!(
            "node `{}` is offline, so the {what} wasn't saved; pass --queue to save it for when the node is back",
            node.name
        )));
    }
    Ok(up)
}

/// One reason an app must go to a node.
struct Pin {
    node: u64,
    why: String,
}

/// The one node that every pin names, if any.
fn agree(pins: &[Pin]) -> Result<Option<u64>> {
    let Some(first) = pins.first() else { return Ok(None) };
    for p in &pins[1..] {
        if p.node != first.node {
            return Err(Error::Conflict(format!(
                "{}, but {}; an app has to be on the same node as its `--from` app and its shared volumes",
                first.why, p.why
            )));
        }
    }
    Ok(Some(first.node))
}

fn node_name(tx: &Transaction, id: u64) -> Result<String> {
    Ok(nodes::get(tx, id)?.name)
}

/// What a deploy asks for.
#[derive(Debug, Clone)]
pub struct Deploy {
    pub name: String,
    pub bundle: CheckedApp,
    /// The uploaded ZIP, moved into the bundle store if the deploy is saved.
    pub upload: PathBuf,
    pub size: u64,
    pub create_only: bool,
    pub from: Option<String>,
    pub node: Option<String>,
    pub queue: bool,
}

/// Saves a deploy: reserves the name and domain, picks the node, stores the bundle and bumps the node's desired
/// state, all in one transaction.
pub fn deploy(
    c: &mut Connection,
    bundles: &Bundles,
    online: &dyn Fn(u64) -> bool,
    d: Deploy,
    now: u64,
) -> Result<Saved> {
    let tx = c.transaction()?;
    let previous = app(&tx, &d.name)?;
    let live = previous.as_ref().filter(|a| a.state != State::Deleted);
    if let Some(a) = live {
        if a.state == State::Deleting {
            return Err(Error::Conflict(format!(
                "app `{}` is being deleted; deploy it again once that's done",
                d.name
            )));
        }
        if d.create_only {
            return Err(Error::Conflict(format!("app `{}` already exists", d.name)));
        }
    }

    // Where it has to go.
    let mut pins = Vec::new();
    if let Some(a) = live
        && let Some(n) = a.node_id
    {
        pins.push(Pin {
            node: n,
            why: format!("app `{}` is on node `{}`", d.name, node_name(&tx, n)?),
        });
    }
    if let Some(from) = &d.from {
        if from == &d.name {
            return Err(Error::BadRequest("an app can't be deployed `--from` itself".into()));
        }
        let src = app(&tx, from)?
            .filter(|a| a.state == State::Active)
            .ok_or_else(|| Error::NotFound(format!("app `{from}` (given as --from) doesn't exist")))?;
        if let Some(n) = src.node_id {
            pins.push(Pin {
                node: n,
                why: format!("`--from` app `{from}` is on node `{}`", node_name(&tx, n)?),
            });
        }
    }
    for v in &d.bundle.shared {
        let vol = volume(&tx, v)?.filter(|x| x.state == State::Active).ok_or_else(|| {
            Error::NotFound(format!(
                "the bundle mounts shared volume `{v}`, which doesn't exist; create it with `kiwi-nest volume create`"
            ))
        })?;
        if let Some(n) = vol.node_id {
            pins.push(Pin {
                node: n,
                why: format!("shared volume `{v}` is on node `{}`", node_name(&tx, n)?),
            });
        }
    }
    if let Some(name) = &d.node {
        let n = nodes::by_name(&tx, name)?;
        pins.push(Pin {
            node: n.id,
            why: format!("--node is `{name}`"),
        });
    }
    let node_id = match agree(&pins)? {
        Some(n) => n,
        None => scheduler::pick(&tx, online, d.bundle.arch.as_deref())?,
    };
    let node = nodes::get(&tx, node_id)?;
    let up = check_online(&node, online, d.queue, "deploy")?;
    let facts = nodes::facts_of(&node);
    if let Some(arch) = &d.bundle.arch
        && node.facts.is_some()
        && &facts.arch != arch
    {
        return Err(Error::Conflict(format!(
            "the bundle needs arch `{arch}`, and node `{}` is `{}`",
            node.name, facts.arch
        )));
    }
    let disallowed: Vec<String> = d
        .bundle
        .host_paths
        .iter()
        .filter(|p| !facts.allowed_host_paths.iter().any(|a| Path::new(p).starts_with(a)))
        .map(|p| {
            format!(
                "host path `{p}` isn't allowed on node `{}` (see `--allow-host-path`)",
                node.name
            )
        })
        .collect();
    if !disallowed.is_empty() {
        return Err(Error::Refused {
            message: "the bundle was refused".into(),
            problems: disallowed,
        });
    }
    let taken: Option<(String, Option<String>)> = tx
        .query_row(
            "SELECT a.name, n.name FROM apps a LEFT JOIN nodes n ON n.id = a.node_id
             WHERE a.domain = ?1 AND a.name != ?2",
            params![d.bundle.domain, d.name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((other, on)) = taken {
        return Err(Error::Conflict(format!(
            "domain `{}` is already used by app `{other}`{}",
            d.bundle.domain,
            on.map(|n| format!(" on node `{n}`")).unwrap_or_default()
        )));
    }

    let seq = bump(&tx, node_id)?;
    let generation = previous.as_ref().map_or(1, |a| a.generation + 1);
    tx.execute(
        "INSERT OR IGNORE INTO bundles (digest, size, created_at) VALUES (?1, ?2, ?3)",
        params![d.bundle.digest, d.size, now],
    )?;
    bundles.adopt(&d.upload, &d.bundle.digest)?;
    let created_at = live.map_or(now, |a| a.created_at);
    tx.execute(
        "INSERT INTO apps (name, state, node_id, domain, generation, seq, bundle_digest, from_app, shared,
                           memory_bytes, keep_data, created_at, updated_at, auth)
         VALUES (?1, 'active', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11, ?12)
         ON CONFLICT (name) DO UPDATE SET state = 'active', node_id = ?2, domain = ?3, generation = ?4, seq = ?5,
             bundle_digest = ?6, from_app = ?7, shared = ?8, memory_bytes = ?9, keep_data = 0, created_at = ?10,
             updated_at = ?11, auth = ?12",
        params![
            d.name,
            node_id,
            d.bundle.domain,
            generation,
            seq,
            d.bundle.digest,
            d.from,
            serde_json::to_string(&d.bundle.shared)?,
            d.bundle.memory_bytes,
            created_at,
            now,
            serde_json::to_string(&d.bundle.auth)?
        ],
    )?;
    tx.commit()?;
    gc(c, bundles)?;
    Ok(Saved {
        accepted: Accepted {
            name: d.name,
            node: node.name,
            generation,
            node_online: up,
        },
        node_id,
    })
}

/// Saves the delete of an app. Deleting one that's already being deleted answers with that delete.
pub fn delete(
    c: &mut Connection,
    online: &dyn Fn(u64) -> bool,
    name: &str,
    keep_data: bool,
    queue: bool,
    now: u64,
) -> Result<Saved> {
    let tx = c.transaction()?;
    let a = live_app(&tx, name)?;
    let node_id = a
        .node_id
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("app `{name}` has no node")))?;
    let node = nodes::get(&tx, node_id)?;
    if a.state == State::Deleting {
        return Ok(Saved {
            accepted: Accepted {
                name: name.into(),
                node: node.name,
                generation: a.generation,
                node_online: online(node_id),
            },
            node_id,
        });
    }
    let up = check_online(&node, online, queue, "delete")?;
    let seq = bump(&tx, node_id)?;
    let generation = a.generation + 1;
    tx.execute(
        "UPDATE apps SET state = 'deleting', generation = ?2, seq = ?3, keep_data = ?4, updated_at = ?5
         WHERE name = ?1",
        params![name, generation, seq, keep_data, now],
    )?;
    tx.commit()?;
    Ok(Saved {
        accepted: Accepted {
            name: name.into(),
            node: node.name,
            generation,
            node_online: up,
        },
        node_id,
    })
}

/// Saves a new shared volume, on `node` or the node the scheduler picks.
pub fn create_volume(
    c: &mut Connection,
    online: &dyn Fn(u64) -> bool,
    name: &str,
    node: Option<&str>,
    queue: bool,
    now: u64,
) -> Result<Saved> {
    let tx = c.transaction()?;
    let previous = volume(&tx, name)?;
    if previous.as_ref().is_some_and(|v| v.state != State::Deleted) {
        return Err(Error::Conflict(format!("shared volume `{name}` already exists")));
    }
    let node_id = match node {
        Some(n) => nodes::by_name(&tx, n)?.id,
        None => scheduler::pick(&tx, online, None)?,
    };
    let n = nodes::get(&tx, node_id)?;
    let up = check_online(&n, online, queue, "volume")?;
    let seq = bump(&tx, node_id)?;
    let generation = previous.as_ref().map_or(1, |v| v.generation + 1);
    tx.execute(
        "INSERT INTO volumes (name, state, node_id, generation, seq, data_digest, created_at, updated_at)
         VALUES (?1, 'active', ?2, ?3, ?4, NULL, ?5, ?5)
         ON CONFLICT (name) DO UPDATE SET state = 'active', node_id = ?2, generation = ?3, seq = ?4,
             data_digest = NULL, created_at = ?5, updated_at = ?5",
        params![name, node_id, generation, seq, now],
    )?;
    tx.commit()?;
    Ok(Saved {
        accepted: Accepted {
            name: name.into(),
            node: n.name,
            generation,
            node_online: up,
        },
        node_id,
    })
}

/// Saves new contents for a shared volume.
#[allow(clippy::too_many_arguments)]
pub fn load_volume(
    c: &mut Connection,
    bundles: &Bundles,
    online: &dyn Fn(u64) -> bool,
    name: &str,
    digest: &str,
    upload: &Path,
    size: u64,
    queue: bool,
    now: u64,
) -> Result<Saved> {
    let tx = c.transaction()?;
    let v = live_volume(&tx, name)?;
    if v.state == State::Deleting {
        return Err(Error::Conflict(format!("shared volume `{name}` is being deleted")));
    }
    let node_id = v
        .node_id
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("volume `{name}` has no node")))?;
    let n = nodes::get(&tx, node_id)?;
    let up = check_online(&n, online, queue, "load")?;
    let seq = bump(&tx, node_id)?;
    let generation = v.generation + 1;
    tx.execute(
        "INSERT OR IGNORE INTO bundles (digest, size, created_at) VALUES (?1, ?2, ?3)",
        params![digest, size, now],
    )?;
    bundles.adopt(upload, digest)?;
    tx.execute(
        "UPDATE volumes SET generation = ?2, seq = ?3, data_digest = ?4, updated_at = ?5 WHERE name = ?1",
        params![name, generation, seq, digest, now],
    )?;
    tx.commit()?;
    gc(c, bundles)?;
    Ok(Saved {
        accepted: Accepted {
            name: name.into(),
            node: n.name,
            generation,
            node_online: up,
        },
        node_id,
    })
}

/// Saves the delete of a shared volume. Refused while an app mounts it.
pub fn delete_volume(
    c: &mut Connection,
    online: &dyn Fn(u64) -> bool,
    name: &str,
    queue: bool,
    now: u64,
) -> Result<Saved> {
    let tx = c.transaction()?;
    let v = live_volume(&tx, name)?;
    let node_id = v
        .node_id
        .ok_or_else(|| Error::Internal(anyhow::anyhow!("volume `{name}` has no node")))?;
    let n = nodes::get(&tx, node_id)?;
    if v.state == State::Deleting {
        return Ok(Saved {
            accepted: Accepted {
                name: name.into(),
                node: n.name,
                generation: v.generation,
                node_online: online(node_id),
            },
            node_id,
        });
    }
    let users: Vec<String> = apps(&tx)?
        .into_iter()
        .filter(|a| a.shared.iter().any(|s| s == name))
        .map(|a| format!("`{}`", a.name))
        .collect();
    if !users.is_empty() {
        return Err(Error::Conflict(format!(
            "shared volume `{name}` is mounted by app {}",
            users.join(", ")
        )));
    }
    let up = check_online(&n, online, queue, "delete")?;
    let seq = bump(&tx, node_id)?;
    let generation = v.generation + 1;
    tx.execute(
        "UPDATE volumes SET state = 'deleting', generation = ?2, seq = ?3, updated_at = ?4 WHERE name = ?1",
        params![name, generation, seq, now],
    )?;
    tx.commit()?;
    Ok(Saved {
        accepted: Accepted {
            name: name.into(),
            node: n.name,
            generation,
            node_online: up,
        },
        node_id,
    })
}

/// The node's sequence number, and its apps and volumes that changed after `since`, in the order they changed.
pub fn desired_since(c: &Connection, node_id: u64, since: u64) -> Result<(u64, Vec<AppChange>, Vec<VolumeChange>)> {
    let seq: u64 = c.query_row("SELECT seq FROM nodes WHERE id = ?1", [node_id], |r| r.get(0))?;
    let mut st = c.prepare(&format!(
        "SELECT {APP_COLUMNS} FROM apps WHERE node_id = ?1 AND seq > ?2 AND state != 'deleted' ORDER BY seq"
    ))?;
    let apps = st
        .query_map(params![node_id, since], app_row)?
        .map(|r| {
            let a = r?;
            let want = match a.state {
                State::Deleting => AppWant::Delete { keep_data: a.keep_data },
                _ => AppWant::Deploy {
                    bundle_digest: a.bundle_digest.clone().unwrap_or_default(),
                    from: a.from_app.clone(),
                    shared: a.shared.clone(),
                },
            };
            Ok(AppChange {
                name: a.name,
                generation: a.generation,
                seq: a.seq,
                want,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut st = c.prepare(&format!(
        "SELECT {VOLUME_COLUMNS} FROM volumes WHERE node_id = ?1 AND seq > ?2 AND state != 'deleted' ORDER BY seq"
    ))?;
    let volumes = st
        .query_map(params![node_id, since], volume_row)?
        .map(|r| {
            let v = r?;
            let want = match v.state {
                State::Deleting => VolumeWant::Delete,
                _ => VolumeWant::Present {
                    data_digest: v.data_digest.clone(),
                },
            };
            Ok(VolumeChange {
                name: v.name,
                generation: v.generation,
                seq: v.seq,
                want,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((seq, apps, volumes))
}

/// Records a node's report, and finishes the deletes it says are done: their names and domains are released. A
/// delete that failed leaves the app (or volume) in place. Returns whether any delete finished or failed.
///
/// A report older than the one recorded (by its serial: it arrived late) only counts as a sign of life.
pub fn record_report(c: &mut Connection, bundles: &Bundles, node_id: u64, report: &Report, now: u64) -> Result<bool> {
    let tx = c.transaction()?;
    let recorded: Option<u64> = tx.query_row(
        "SELECT json_extract(report, '$.serial') FROM nodes WHERE id = ?1",
        [node_id],
        |r| r.get(0),
    )?;
    if recorded.is_some_and(|r| report.serial < r) {
        tx.execute("UPDATE nodes SET last_seen = ?2 WHERE id = ?1", params![node_id, now])?;
        tx.commit()?;
        return Ok(false);
    }
    tx.execute(
        "UPDATE nodes SET facts = ?2, report = ?3, last_seen = ?4 WHERE id = ?1",
        params![
            node_id,
            serde_json::to_string(&report.facts)?,
            serde_json::to_string(report)?,
            now
        ],
    )?;
    let mut settled = false;
    let deleting: Vec<AppRow> = {
        let mut st = tx.prepare(&format!(
            "SELECT {APP_COLUMNS} FROM apps WHERE node_id = ?1 AND state = 'deleting'"
        ))?;
        st.query_map([node_id], app_row)?.collect::<rusqlite::Result<_>>()?
    };
    for a in deleting {
        match report.applied.apps.get(&a.name) {
            Some(o) if o.generation >= a.generation && o.ok => {
                tx.execute(
                    "UPDATE apps SET state = 'deleted', domain = NULL, bundle_digest = NULL, updated_at = ?2
                     WHERE name = ?1",
                    params![a.name, now],
                )?;
                tracing::info!(app = a.name, "deleted");
                settled = true;
            }
            Some(o) if o.generation >= a.generation => {
                tx.execute(
                    "UPDATE apps SET state = 'active', updated_at = ?2 WHERE name = ?1",
                    params![a.name, now],
                )?;
                tracing::warn!(app = a.name, error = ?o.error, "the node couldn't delete the app; it stays");
                settled = true;
            }
            _ => {}
        }
    }
    let deleting: Vec<VolumeRow> = {
        let mut st = tx.prepare(&format!(
            "SELECT {VOLUME_COLUMNS} FROM volumes WHERE node_id = ?1 AND state = 'deleting'"
        ))?;
        st.query_map([node_id], volume_row)?.collect::<rusqlite::Result<_>>()?
    };
    for v in deleting {
        match report.applied.volumes.get(&v.name) {
            Some(o) if o.generation >= v.generation && o.ok => {
                tx.execute(
                    "UPDATE volumes SET state = 'deleted', data_digest = NULL, updated_at = ?2 WHERE name = ?1",
                    params![v.name, now],
                )?;
                settled = true;
            }
            Some(o) if o.generation >= v.generation => {
                tx.execute(
                    "UPDATE volumes SET state = 'active', updated_at = ?2 WHERE name = ?1",
                    params![v.name, now],
                )?;
                settled = true;
            }
            _ => {}
        }
    }
    // A node that has applied generations beyond what's recorded (the database was restored from an older backup):
    // the next change must come after them, or the node would take it for one it has already applied.
    for (name, o) in &report.applied.apps {
        tx.execute(
            "UPDATE apps SET generation = ?3 WHERE name = ?1 AND node_id = ?2 AND state != 'deleted' AND generation < ?3",
            params![name, node_id, o.generation],
        )?;
    }
    for (name, o) in &report.applied.volumes {
        tx.execute(
            "UPDATE volumes SET generation = ?3 WHERE name = ?1 AND node_id = ?2 AND state != 'deleted'
             AND generation < ?3",
            params![name, node_id, o.generation],
        )?;
    }
    tx.commit()?;
    if settled {
        gc(c, bundles)?;
    }
    Ok(settled)
}

/// Removes the bundles that no app or volume refers to any more.
pub fn gc(c: &Connection, bundles: &Bundles) -> Result<()> {
    let mut st = c.prepare(
        "DELETE FROM bundles WHERE digest NOT IN (
             SELECT bundle_digest FROM apps WHERE bundle_digest IS NOT NULL
             UNION SELECT data_digest FROM volumes WHERE data_digest IS NOT NULL)
         RETURNING digest",
    )?;
    let gone: Vec<String> = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for d in gone {
        bundles.remove(&d);
    }
    Ok(())
}

/// Reads the basic-auth users of apps deployed before the server kept them, from their bundles. An app whose
/// bundle can't be read keeps none recorded, and the ingress doesn't route it until it's deployed again.
pub fn backfill_auth(c: &Connection, bundles: &Bundles, limits: &kn_bundle::Limits) -> Result<()> {
    let missing: Vec<(String, String)> = {
        let mut st = c.prepare(
            "SELECT name, bundle_digest FROM apps
             WHERE auth IS NULL AND state != 'deleted' AND bundle_digest IS NOT NULL",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (name, digest) in missing {
        let checked = (|| {
            let zip = bundles
                .path(&digest)
                .ok_or_else(|| Error::Internal(anyhow::anyhow!("bad digest {digest}")))?;
            let scratch = bundles.temp_dir()?;
            crate::bundles::check_app(&zip, scratch.path(), limits)
        })();
        match checked {
            Ok(b) => {
                c.execute(
                    "UPDATE apps SET auth = ?2 WHERE name = ?1",
                    params![name, serde_json::to_string(&b.auth)?],
                )?;
            }
            Err(e) => tracing::warn!(
                app = name,
                error = %e,
                "couldn't read the app's basic-auth users from its bundle; the ingress won't route it until it's \
                 deployed again"
            ),
        }
    }
    Ok(())
}

/// Removes bundle files that have no row (left by a crash between moving one into place and committing).
pub fn sweep(c: &Connection, bundles: &Bundles) -> Result<()> {
    gc(c, bundles)?;
    for d in bundles.digests()? {
        let known: bool = c.query_row("SELECT count(*) > 0 FROM bundles WHERE digest = ?1", [&d], |r| r.get(0))?;
        if !known {
            tracing::warn!(digest = d, "removed a bundle that nothing refers to");
            bundles.remove(&d);
        }
    }
    Ok(())
}

/// What kind of thing a change is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    App,
    Volume,
}

/// Whether generation `generation` of an app or volume is done on its node. Returns it, and the node's ID.
pub fn change_status(c: &Connection, kind: Kind, name: &str, generation: u64) -> Result<(ChangeStatus, u64)> {
    let (state, node_id, current) = match kind {
        Kind::App => {
            let a = app(c, name)?.ok_or_else(|| Error::NotFound(format!("app `{name}` doesn't exist")))?;
            (a.state, a.node_id, a.generation)
        }
        Kind::Volume => {
            let v = volume(c, name)?.ok_or_else(|| Error::NotFound(format!("shared volume `{name}` doesn't exist")))?;
            (v.state, v.node_id, v.generation)
        }
    };
    if generation == 0 || generation > current {
        return Err(Error::NotFound(format!("`{name}` has no generation {generation}")));
    }
    let node_id = node_id.ok_or_else(|| Error::Internal(anyhow::anyhow!("`{name}` has no node")))?;
    let node = nodes::get(c, node_id)?;
    let applied = nodes::applied(&node);
    let outcome: Option<&Outcome> = match kind {
        Kind::App => applied.apps.get(name),
        Kind::Volume => applied.volumes.get(name),
    };
    let mut s = ChangeStatus {
        name: name.into(),
        generation,
        node: node.name,
        node_online: false,
        done: false,
        ok: false,
        error: None,
        problems: Vec::new(),
        notes: Vec::new(),
        superseded_by: None,
    };
    match outcome {
        Some(o) if o.generation == generation => {
            s.done = true;
            s.ok = o.ok;
            s.error = o.error.clone();
            s.problems = o.problems.clone();
            s.notes = o.notes.clone();
        }
        Some(o) if o.generation > generation => {
            s.done = true;
            s.superseded_by = Some(o.generation);
        }
        // The node has acknowledged a delete (and may have forgotten its outcome since).
        _ if state == State::Deleted && generation == current => {
            s.done = true;
            s.ok = true;
        }
        _ if generation < current => {
            s.done = true;
            s.superseded_by = Some(current);
        }
        _ => {}
    }
    Ok((s, node_id))
}

/// The memory placed on each node, and how many apps: (node ID, bytes, apps).
pub fn placed(c: &Connection) -> Result<Vec<(u64, u64, u64)>> {
    let mut st = c.prepare(
        "SELECT node_id, sum(memory_bytes), count(*) FROM apps
         WHERE state != 'deleted' AND node_id IS NOT NULL GROUP BY node_id",
    )?;
    let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

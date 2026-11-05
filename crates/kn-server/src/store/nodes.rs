//! Nodes: joining, their tokens, what they last reported, cordons and labels, and forgetting them.
//!
//! A forgotten node is kept as a tombstone: its row (with `forgotten_at`) and its token's (revoked), so that when it
//! comes back the server can tell it apart from a request with a wrong or unknown token, and answer that it was
//! forgotten. Its name is free for a new node; nothing of it is ever attached to one.

use std::collections::BTreeMap;

use kn_proto::agent::{NodeFacts, Report};
use kn_proto::api::TokenKind;
use rusqlite::{Connection, OptionalExtension, params};

use super::tokens;
use crate::error::{Error, Result};

/// A node as the server records it.
#[derive(Debug, Clone)]
pub struct Node {
    pub id: u64,
    pub name: String,
    /// The sequence number of its desired state.
    pub seq: u64,
    pub facts: Option<NodeFacts>,
    pub report: Option<Report>,
    pub joined_at: u64,
    pub last_seen: Option<u64>,
    pub cordoned: bool,
    /// The labels set with `node label`.
    pub labels: BTreeMap<String, String>,
    pub forgotten_at: Option<u64>,
}

const COLUMNS: &str = "id, name, seq, facts, report, joined_at, last_seen, cordoned, labels, forgotten_at";

/// A row of `nodes`, as read.
struct Raw {
    id: u64,
    name: String,
    seq: u64,
    facts: Option<String>,
    report: Option<String>,
    joined_at: u64,
    last_seen: Option<u64>,
    cordoned: bool,
    labels: String,
    forgotten_at: Option<u64>,
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Raw> {
    Ok(Raw {
        id: r.get(0)?,
        name: r.get(1)?,
        seq: r.get(2)?,
        facts: r.get(3)?,
        report: r.get(4)?,
        joined_at: r.get(5)?,
        last_seen: r.get(6)?,
        cordoned: r.get(7)?,
        labels: r.get(8)?,
        forgotten_at: r.get(9)?,
    })
}

fn node(r: Raw) -> Result<Node> {
    Ok(Node {
        id: r.id,
        name: r.name,
        seq: r.seq,
        facts: r.facts.map(|f| serde_json::from_str(&f)).transpose()?,
        report: r.report.map(|r| serde_json::from_str(&r)).transpose()?,
        joined_at: r.joined_at,
        last_seen: r.last_seen,
        cordoned: r.cordoned,
        labels: serde_json::from_str(&r.labels)?,
        forgotten_at: r.forgotten_at,
    })
}

/// Every node that isn't forgotten, by name.
pub fn list(c: &Connection) -> Result<Vec<Node>> {
    let mut st = c.prepare(&format!(
        "SELECT {COLUMNS} FROM nodes WHERE forgotten_at IS NULL ORDER BY name"
    ))?;
    let rows = st.query_map([], row)?;
    rows.map(|r| node(r?)).collect()
}

/// Every node that isn't forgotten, without its last report (`report` is `None`), for callers that only need the
/// facts: a report holds every app and volume, so parsing them all is the costly part.
pub fn list_facts(c: &Connection) -> Result<Vec<Node>> {
    let mut st = c.prepare(&format!(
        "SELECT {} FROM nodes WHERE forgotten_at IS NULL ORDER BY name",
        COLUMNS.replace("report", "NULL")
    ))?;
    let rows = st.query_map([], row)?;
    rows.map(|r| node(r?)).collect()
}

/// A node by ID, forgotten or not.
pub fn get(c: &Connection, id: u64) -> Result<Node> {
    let r = c
        .query_row(&format!("SELECT {COLUMNS} FROM nodes WHERE id = ?1"), [id], row)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("there's no node {id}")))?;
    node(r)
}

/// The node with this name that isn't forgotten.
pub fn by_name(c: &Connection, name: &str) -> Result<Node> {
    let r = c
        .query_row(
            &format!("SELECT {COLUMNS} FROM nodes WHERE name = ?1 AND forgotten_at IS NULL"),
            [name],
            row,
        )
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("there's no node `{name}`")))?;
    node(r)
}

/// Who a node token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenOwner {
    Node {
        id: u64,
        name: String,
    },
    /// The node was forgotten: its token was revoked then, and kept.
    Forgotten {
        id: u64,
        name: String,
        at: u64,
    },
}

/// The node whose token has this hash. `None` for anything else: a token never issued, another kind of token, or
/// a node token revoked for another reason (a node that joined again before its first poll).
pub fn by_token(c: &Connection, hash: &str) -> Result<Option<TokenOwner>> {
    let row: Option<(u64, String, Option<u64>, Option<u64>)> = c
        .query_row(
            "SELECT n.id, n.name, n.forgotten_at, t.revoked_at FROM tokens t JOIN nodes n ON n.id = t.node_id
             WHERE t.hash = ?1 AND t.kind = 'node'",
            [hash],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    Ok(match row {
        Some((id, name, Some(at), _)) => Some(TokenOwner::Forgotten { id, name, at }),
        Some((id, name, None, None)) => Some(TokenOwner::Node { id, name }),
        _ => None,
    })
}

/// Whether the node was forgotten, and when.
pub fn forgotten_at(c: &Connection, id: u64) -> Result<Option<u64>> {
    Ok(
        c.query_row("SELECT forgotten_at FROM nodes WHERE id = ?1", [id], |r| r.get(0))
            .optional()?
            .flatten(),
    )
}

/// Cordons a node (it gets no new nests), or lifts the cordon.
pub fn set_cordoned(c: &Connection, name: &str, cordoned: bool) -> Result<()> {
    let n = by_name(c, name)?;
    c.execute("UPDATE nodes SET cordoned = ?2 WHERE id = ?1", params![n.id, cordoned])?;
    Ok(())
}

/// Sets and removes a node's labels (those set with `node label`).
pub fn set_labels(c: &Connection, name: &str, set: &BTreeMap<String, String>, remove: &[String]) -> Result<()> {
    let n = by_name(c, name)?;
    let mut labels = n.labels;
    for k in remove {
        labels.remove(k);
    }
    labels.extend(set.iter().map(|(k, v)| (k.clone(), v.clone())));
    c.execute(
        "UPDATE nodes SET labels = ?2 WHERE id = ?1",
        params![n.id, serde_json::to_string(&labels)?],
    )?;
    Ok(())
}

/// Swaps a join token for a node: records the node and its token's hash, and uses up the join token, in one
/// transaction. A node that joined but never polled (it didn't get the answer) may join again under the same name
/// with a new join token; its old token is revoked.
pub fn join(
    c: &mut Connection,
    join_hash: &str,
    name: &str,
    facts: &NodeFacts,
    node_hash: &str,
    now: u64,
) -> Result<u64> {
    let tx = c.transaction()?;
    let join_id: u64 = tx
        .query_row(
            "SELECT id FROM tokens WHERE hash = ?1 AND kind = 'join' AND revoked_at IS NULL AND used_at IS NULL",
            [join_hash],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| Error::Unauthorized("the join token isn't valid, or has been used".into()))?;
    let existing: Option<(u64, Option<u64>)> = tx
        .query_row(
            "SELECT id, last_seen FROM nodes WHERE name = ?1 AND forgotten_at IS NULL",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let facts_json = serde_json::to_string(facts)?;
    let id = match existing {
        Some((_, Some(_))) => {
            return Err(Error::Conflict(format!("a node named `{name}` has already joined")));
        }
        Some((id, None)) => {
            tx.execute(
                "UPDATE tokens SET revoked_at = ?2 WHERE node_id = ?1 AND revoked_at IS NULL",
                params![id, now],
            )?;
            tx.execute("UPDATE nodes SET facts = ?2 WHERE id = ?1", params![id, facts_json])?;
            id
        }
        None => {
            tx.execute(
                "INSERT INTO nodes (name, facts, joined_at) VALUES (?1, ?2, ?3)",
                params![name, facts_json, now],
            )?;
            tx.last_insert_rowid() as u64
        }
    };
    tx.execute("UPDATE tokens SET used_at = ?2 WHERE id = ?1", params![join_id, now])?;
    tokens::create(&tx, TokenKind::Node, node_hash, name, &[], &[], Some(id), now)?;
    tx.commit()?;
    Ok(id)
}

/// The node's facts as a struct, for placement.
pub fn facts_of(n: &Node) -> NodeFacts {
    n.facts.clone().unwrap_or_default()
}

/// Whether the node speaks a version of the agent link with backups (see [`kn_proto::agent::BACKUPS_PROTOCOL`]).
pub fn has_backups(n: &Node) -> bool {
    n.facts
        .as_ref()
        .is_some_and(|f| f.protocol >= kn_proto::agent::BACKUPS_PROTOCOL)
}

/// Refuses a backup or restore for a node too old for them.
pub fn require_backups(n: &Node) -> Result<()> {
    if has_backups(n) {
        return Ok(());
    }
    Err(Error::Conflict(format!(
        "node `{}` runs a kiwi-nest from before backups; upgrade it first",
        n.name
    )))
}

/// The node's labels: those it reports, with those set by `node label` over them.
pub fn labels_of(n: &Node) -> BTreeMap<String, String> {
    let mut labels = n.facts.as_ref().map(|f| f.labels.clone()).unwrap_or_default();
    labels.extend(n.labels.iter().map(|(k, v)| (k.clone(), v.clone())));
    labels
}

/// The last report's applied changes, or none.
pub fn applied(n: &Node) -> kn_proto::agent::Applied {
    n.report.as_ref().map(|r| r.applied.clone()).unwrap_or_default()
}

/// What forgetting a node released, and the commands it had open (now lost).
#[derive(Debug, Clone)]
pub struct Forgetting {
    pub forgot: kn_proto::api::Forgot,
    pub node_id: u64,
    pub lost_commands: Vec<u64>,
}

/// Forgets a node, in one transaction: keeps it as a tombstone (`forgotten_at`, its token revoked but kept), and
/// releases what's placed on it: its apps' names and domains, its shared volumes, and so its nests, which are free
/// to be placed again. Their data stays on the node. Refused while the node is online, unless `force`. A dry run
/// says what it would release and changes nothing.
pub fn forget(
    c: &mut Connection,
    name: &str,
    online: &dyn Fn(u64) -> bool,
    force: bool,
    dry_run: bool,
    now: u64,
) -> Result<Forgetting> {
    let tx = c.transaction()?;
    let n = by_name(&tx, name)?;
    let up = online(n.id);
    let names = |sql: &str| -> Result<Vec<String>> {
        let mut st = tx.prepare(sql)?;
        Ok(st
            .query_map([n.id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<String>>>()?)
    };
    let apps = names("SELECT name FROM apps WHERE node_id = ?1 AND state != 'deleted' ORDER BY name")?;
    let volumes = names("SELECT name FROM volumes WHERE node_id = ?1 AND state != 'deleted' ORDER BY name")?;
    let nests = names(
        "SELECT DISTINCT nest FROM (
             SELECT nest FROM apps WHERE node_id = ?1 AND state != 'deleted' AND nest IS NOT NULL
             UNION SELECT nest FROM volumes WHERE node_id = ?1 AND state != 'deleted' AND nest IS NOT NULL)
         ORDER BY nest",
    )?;
    let forgot = kn_proto::api::Forgot {
        node: n.name.clone(),
        online: up,
        apps,
        volumes,
        nests,
        dry_run,
    };
    if up && !force && !dry_run {
        return Err(Error::Conflict(format!(
            "node `{name}` is online, so it isn't forgotten: stop it first, or pass --force (its apps then go down \
             when it next hears from the server)"
        )));
    }
    if dry_run {
        return Ok(Forgetting {
            forgot,
            node_id: n.id,
            lost_commands: Vec::new(),
        });
    }
    tx.execute("UPDATE nodes SET forgotten_at = ?2 WHERE id = ?1", params![n.id, now])?;
    tx.execute(
        "UPDATE tokens SET revoked_at = ?2 WHERE node_id = ?1 AND revoked_at IS NULL",
        params![n.id, now],
    )?;
    tx.execute(
        "UPDATE apps SET state = 'deleted', domain = NULL, bundle_digest = NULL, updated_at = ?2
         WHERE node_id = ?1 AND state != 'deleted'",
        params![n.id, now],
    )?;
    tx.execute(
        "UPDATE volumes SET state = 'deleted', data_digest = NULL, updated_at = ?2
         WHERE node_id = ?1 AND state != 'deleted'",
        params![n.id, now],
    )?;
    let lost_commands: Vec<u64> = {
        let mut st = tx.prepare(
            "UPDATE commands SET state = 'lost', finished_at = ?2 WHERE node_id = ?1 AND state IN ('pending', 'delivered')
             RETURNING id",
        )?;
        st.query_map(params![n.id, now], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    tx.commit()?;
    Ok(Forgetting {
        forgot,
        node_id: n.id,
        lost_commands,
    })
}

//! Nodes: joining, their tokens, and what they last reported.

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
}

const COLUMNS: &str = "id, name, seq, facts, report, joined_at, last_seen";

fn row(r: &rusqlite::Row) -> rusqlite::Result<(u64, String, u64, Option<String>, Option<String>, u64, Option<u64>)> {
    Ok((
        r.get(0)?,
        r.get(1)?,
        r.get(2)?,
        r.get(3)?,
        r.get(4)?,
        r.get(5)?,
        r.get(6)?,
    ))
}

fn node(
    (id, name, seq, facts, report, joined_at, last_seen): (
        u64,
        String,
        u64,
        Option<String>,
        Option<String>,
        u64,
        Option<u64>,
    ),
) -> Result<Node> {
    Ok(Node {
        id,
        name,
        seq,
        facts: facts.map(|f| serde_json::from_str(&f)).transpose()?,
        report: report.map(|r| serde_json::from_str(&r)).transpose()?,
        joined_at,
        last_seen,
    })
}

pub fn list(c: &Connection) -> Result<Vec<Node>> {
    let mut st = c.prepare(&format!("SELECT {COLUMNS} FROM nodes ORDER BY name"))?;
    let rows = st.query_map([], row)?;
    rows.map(|r| node(r?)).collect()
}

pub fn get(c: &Connection, id: u64) -> Result<Node> {
    let r = c
        .query_row(&format!("SELECT {COLUMNS} FROM nodes WHERE id = ?1"), [id], row)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("there's no node {id}")))?;
    node(r)
}

pub fn by_name(c: &Connection, name: &str) -> Result<Node> {
    let r = c
        .query_row(&format!("SELECT {COLUMNS} FROM nodes WHERE name = ?1"), [name], row)
        .optional()?
        .ok_or_else(|| Error::NotFound(format!("there's no node `{name}`")))?;
    node(r)
}

/// The node whose token has this hash.
pub fn by_token(c: &Connection, hash: &str) -> Result<Option<(u64, String)>> {
    let Some(t) = tokens::lookup(c, hash)? else {
        return Ok(None);
    };
    if t.kind != TokenKind::Node {
        return Ok(None);
    }
    let Some(id) = t.node_id else { return Ok(None) };
    Ok(c
        .query_row("SELECT id, name FROM nodes WHERE id = ?1", [id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .optional()?)
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
        .query_row("SELECT id, last_seen FROM nodes WHERE name = ?1", [name], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
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

/// The last report's applied changes, or none.
pub fn applied(n: &Node) -> kn_proto::agent::Applied {
    n.report.as_ref().map(|r| r.applied.clone()).unwrap_or_default()
}

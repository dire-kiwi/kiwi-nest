//! Tokens, stored by hash.

use kn_proto::api::{TokenInfo, TokenKind};
use rusqlite::{Connection, OptionalExtension, params};

use super::{json, rfc3339};
use crate::error::{Error, Result};

/// A token that may be used.
#[derive(Debug, Clone)]
pub struct Token {
    pub id: u64,
    pub kind: TokenKind,
    pub name: String,
    pub patterns: Vec<String>,
    pub from: Vec<String>,
    pub node_id: Option<u64>,
}

pub fn kind_from_str(s: &str) -> Result<TokenKind> {
    Ok(match s {
        "admin" => TokenKind::Admin,
        "deploy" => TokenKind::Deploy,
        "join" => TokenKind::Join,
        "node" => TokenKind::Node,
        other => return Err(Error::Internal(anyhow::anyhow!("unknown token kind {other}"))),
    })
}

/// Saves a token's hash; returns its ID.
pub fn create(
    c: &Connection,
    kind: TokenKind,
    hash: &str,
    name: &str,
    patterns: &[String],
    from: &[String],
    node_id: Option<u64>,
    now: u64,
) -> Result<u64> {
    c.execute(
        "INSERT INTO tokens (kind, hash, name, patterns, from_patterns, node_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            kind.as_str(),
            hash,
            name,
            serde_json::to_string(patterns)?,
            serde_json::to_string(from)?,
            node_id,
            now
        ],
    )?;
    Ok(c.last_insert_rowid() as u64)
}

/// The token with this hash, unless it's revoked (or a join token that was used).
pub fn lookup(c: &Connection, hash: &str) -> Result<Option<Token>> {
    let row = c
        .query_row(
            "SELECT id, kind, name, patterns, from_patterns, node_id FROM tokens
             WHERE hash = ?1 AND revoked_at IS NULL AND NOT (kind = 'join' AND used_at IS NOT NULL)",
            [hash],
            |r| {
                Ok((
                    r.get::<_, u64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<u64>>(5)?,
                ))
            },
        )
        .optional()?;
    row.map(|(id, kind, name, patterns, from, node_id)| {
        Ok(Token {
            id,
            kind: kind_from_str(&kind)?,
            name,
            patterns: json(patterns)?,
            from: json(from)?,
            node_id,
        })
    })
    .transpose()
}

/// How many admin tokens may be used.
pub fn admin_count(c: &Connection) -> Result<u64> {
    Ok(c.query_row(
        "SELECT count(*) FROM tokens WHERE kind = 'admin' AND revoked_at IS NULL",
        [],
        |r| r.get(0),
    )?)
}

/// Every token that may still be used, oldest first.
pub fn list(c: &Connection) -> Result<Vec<TokenInfo>> {
    let mut st = c.prepare(
        "SELECT t.id, t.kind, t.name, t.patterns, t.from_patterns, n.name, t.created_at, t.used_at
         FROM tokens t LEFT JOIN nodes n ON n.id = t.node_id
         WHERE t.revoked_at IS NULL AND NOT (t.kind = 'join' AND t.used_at IS NOT NULL)
         ORDER BY t.id",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, u64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, u64>(6)?,
            r.get::<_, Option<u64>>(7)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, kind, name, patterns, from, node, created_at, used_at) = row?;
        out.push(TokenInfo {
            id,
            kind: kind_from_str(&kind)?,
            name,
            patterns: json(patterns)?,
            from: json(from)?,
            node,
            created_at: rfc3339(created_at),
            used_at: used_at.map(rfc3339),
        });
    }
    Ok(out)
}

/// Revokes a token. The last admin token can't be revoked, so the cluster can't lock itself out.
pub fn revoke(c: &mut Connection, id: u64, now: u64) -> Result<()> {
    let tx = c.transaction()?;
    let kind: Option<String> = tx
        .query_row(
            "SELECT kind FROM tokens WHERE id = ?1 AND revoked_at IS NULL",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    match kind.as_deref() {
        None => return Err(Error::NotFound(format!("there's no token {id}"))),
        Some("admin") if admin_count(&tx)? <= 1 => {
            return Err(Error::Conflict("that's the last admin token, so it can't be revoked".into()));
        }
        Some(_) => {}
    }
    tx.execute("UPDATE tokens SET revoked_at = ?2 WHERE id = ?1", params![id, now])?;
    tx.commit()?;
    Ok(())
}

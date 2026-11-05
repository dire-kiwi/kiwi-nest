//! Commands for nodes, kept as history.
//!
//! A command is `pending` until a poll hands it out, then `delivered`, then `done` when its result comes back.
//! Handing out and marking delivered happen in one transaction, so a command is delivered at most once. One whose
//! caller went away before it was handed out is `cancelled`, so it never runs late; one handed out that the node's
//! next report doesn't list as running, or that was open when the server restarted, is `lost`.

use kn_proto::agent::{Action, Command};
use rusqlite::{Connection, params};

use crate::error::Result;

pub fn insert(c: &Connection, node_id: u64, action: &Action, now: u64) -> Result<u64> {
    c.execute(
        "INSERT INTO commands (node_id, action, state, created_at) VALUES (?1, ?2, 'pending', ?3)",
        params![node_id, serde_json::to_string(action)?, now],
    )?;
    Ok(c.last_insert_rowid() as u64)
}

/// For a poll from `node_id` that's still running `running`: marks the delivered commands it isn't running and
/// hasn't answered as lost, and hands out the pending ones. Returns (lost, handed out).
pub fn take(c: &mut Connection, node_id: u64, running: &[u64], now: u64) -> Result<(Vec<u64>, Vec<Command>)> {
    let tx = c.transaction()?;
    let delivered: Vec<u64> = {
        let mut st = tx.prepare("SELECT id FROM commands WHERE node_id = ?1 AND state = 'delivered'")?;
        st.query_map([node_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    let lost: Vec<u64> = delivered.into_iter().filter(|id| !running.contains(id)).collect();
    for id in &lost {
        tx.execute(
            "UPDATE commands SET state = 'lost', finished_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
    }
    let out = hand_out(&tx, node_id, now)?;
    tx.commit()?;
    Ok((lost, out))
}

/// Hands out the node's pending commands, marking them delivered.
pub fn take_pending(c: &mut Connection, node_id: u64, now: u64) -> Result<Vec<Command>> {
    let tx = c.transaction()?;
    let out = hand_out(&tx, node_id, now)?;
    tx.commit()?;
    Ok(out)
}

fn hand_out(tx: &rusqlite::Transaction, node_id: u64, now: u64) -> Result<Vec<Command>> {
    let pending: Vec<(u64, String)> = {
        let mut st = tx.prepare("SELECT id, action FROM commands WHERE node_id = ?1 AND state = 'pending' ORDER BY id")?;
        st.query_map([node_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut out = Vec::new();
    for (id, action) in pending {
        tx.execute(
            "UPDATE commands SET state = 'delivered', delivered_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        out.push(Command {
            id,
            action: serde_json::from_str(&action)?,
        });
    }
    Ok(out)
}

/// Records a delivered command's result; returns whether it was waiting for one.
pub fn finish(c: &Connection, id: u64, node_id: u64, result: &str, now: u64) -> Result<bool> {
    let n = c.execute(
        "UPDATE commands SET state = 'done', result = ?3, finished_at = ?4
         WHERE id = ?1 AND node_id = ?2 AND state = 'delivered'",
        params![id, node_id, result, now],
    )?;
    Ok(n == 1)
}

/// Cancels a command nobody's waiting for any more, if it hasn't been handed out.
pub fn cancel(c: &Connection, id: u64, now: u64) -> Result<()> {
    c.execute(
        "UPDATE commands SET state = 'cancelled', finished_at = ?2 WHERE id = ?1 AND state = 'pending'",
        params![id, now],
    )?;
    Ok(())
}

/// When the server starts: nobody's waiting for the open commands any more.
pub fn lose_open(c: &Connection, now: u64) -> Result<usize> {
    Ok(c.execute(
        "UPDATE commands SET state = CASE state WHEN 'pending' THEN 'cancelled' ELSE 'lost' END, finished_at = ?1
         WHERE state IN ('pending', 'delivered')",
        [now],
    )?)
}

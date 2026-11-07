//! Commands for nodes, kept as history.
//!
//! A command is `pending` until a poll hands it out, then `delivered`, then `done` when its result comes back.
//! Handing out and marking delivered happen in one transaction, so a command is delivered at most once. One whose
//! caller went away before it was handed out is `cancelled`, so it never runs late; one handed out that the node's
//! next report doesn't list as running, or that was open when the server restarted, is `lost`.
//!
//! Finished commands are kept for [`KEEP`] (inspects, which no history shows, for [`KEEP_INSPECTS`]), and then
//! [`prune`]d.

use kn_proto::agent::{Action, Command, CommandResult};
use kn_proto::api::CommandInfo;
use kn_proto::backup::Kind;
use rusqlite::{Connection, params};

use super::rfc3339;
use crate::error::Result;

/// How long a finished command is kept, as its app's history.
pub const KEEP: u64 = 30 * 24 * 3600;

/// How long a finished inspect is kept. They're most of the commands, and in no history.
pub const KEEP_INSPECTS: u64 = 24 * 3600;

/// What a command acts on: `app`, `volume` or `node`, and its name (none for a node's).
fn target(action: &Action) -> (&'static str, &str) {
    let kind = match action {
        Action::Inspect { .. } | Action::Logs { .. } | Action::Suspend { .. } | Action::Resume { .. } => "app",
        Action::Backup { what: Kind::App, .. } => "app",
        Action::Backup { .. } | Action::VolumeInfo { .. } | Action::DeleteSnapshot { .. } => "volume",
        // The node's own, not an app's or a volume's.
        Action::PruneImages { .. } | Action::Upgrade { .. } => "node",
    };
    (kind, action.target())
}

/// Records a command for `node_id`, asked for by `caller` (`user NAME`, `token NAME`, `schedule`).
pub fn insert(c: &Connection, node_id: u64, action: &Action, caller: &str, now: u64) -> Result<u64> {
    let (kind, name) = target(action);
    c.execute(
        "INSERT INTO commands (node_id, action, state, created_at, target_kind, target, caller, action_kind)
         VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, json_extract(?2, '$.kind'))",
        params![node_id, serde_json::to_string(action)?, now, kind, name, caller],
    )?;
    Ok(c.last_insert_rowid() as u64)
}

/// Migration 6: what earlier commands act on, from their actions.
pub fn backfill(c: &Connection) -> anyhow::Result<()> {
    let rows: Vec<(u64, String)> = {
        let mut st = c.prepare("SELECT id, action FROM commands")?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    for (id, action) in rows {
        // An action this build can't read stays without a target, out of every history.
        let Ok(action) = serde_json::from_str::<Action>(&action) else {
            continue;
        };
        let (kind, name) = target(&action);
        c.execute(
            "UPDATE commands SET target_kind = ?2, target = ?3 WHERE id = ?1",
            params![id, kind, name],
        )?;
    }
    Ok(())
}

/// An app's recent commands since `since` (when it was made, so an earlier app's of the same name aren't
/// included), newest first, but for inspecting it (which the CLI and the UI do all the time, and changes nothing).
pub fn history(c: &Connection, app: &str, since: u64, limit: u32) -> Result<Vec<CommandInfo>> {
    let mut st = c.prepare(
        "SELECT c.id, c.action, coalesce(n.name, ''), c.caller, c.state, c.result, c.created_at, c.finished_at
         FROM commands c LEFT JOIN nodes n ON n.id = c.node_id
         WHERE c.target_kind = 'app' AND c.target = ?1 AND c.created_at >= ?3
           AND c.action_kind IS NOT 'inspect'
         ORDER BY c.id DESC LIMIT ?2",
    )?;
    let rows = st.query_map(params![app, limit, since], |r| {
        Ok((
            r.get::<_, u64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, u64>(6)?,
            r.get::<_, Option<u64>>(7)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (id, action, node, caller, state, result, created, finished) = row?;
        let action: Option<Action> = serde_json::from_str(&action).ok();
        let (name, detail) = action.as_ref().map_or(("unknown".into(), String::new()), describe);
        // A streamed command (logs) has no result: it's done once its output ended.
        let (ok, error) = match result.as_deref().map(serde_json::from_str::<CommandResult>) {
            Some(Ok(CommandResult::Ok { .. })) => (Some(true), None),
            Some(Ok(CommandResult::Err { error, .. })) => (Some(false), Some(error.error)),
            Some(Err(_)) => (None, None),
            None if state == "done" => (Some(true), None),
            None => (None, None),
        };
        out.push(CommandInfo {
            id,
            action: name,
            detail,
            node,
            by: if caller.is_empty() { "unknown".into() } else { caller },
            state,
            ok,
            error,
            created_at: rfc3339(created),
            finished_at: finished.map(rfc3339),
        });
    }
    Ok(out)
}

/// A command's name and what else is worth showing about it.
fn describe(action: &Action) -> (String, String) {
    match action {
        Action::Inspect { .. } => ("inspect".into(), String::new()),
        Action::Logs { query, .. } => {
            let mut d = Vec::new();
            if let Some(t) = query.tail {
                d.push(format!("last {t} lines"));
            }
            if query.follow {
                d.push("following".into());
            }
            if let Some(s) = &query.service {
                d.push(format!("service {s}"));
            }
            ("logs".into(), d.join(", "))
        }
        Action::Suspend { .. } => ("suspend".into(), String::new()),
        Action::Resume { .. } => ("resume".into(), String::new()),
        Action::VolumeInfo { .. } => ("volume info".into(), String::new()),
        Action::DeleteSnapshot { app, .. } => ("delete snapshot".into(), format!("of {app}")),
        Action::PruneImages { grace_secs } => (
            "prune images".into(),
            grace_secs.map_or_else(String::new, |g| format!("grace {g}s")),
        ),
        Action::Upgrade { version, .. } => ("upgrade".into(), format!("to {version}")),
        Action::Backup { targets, full, .. } => {
            let mut d = Vec::new();
            if targets.is_empty() {
                d.push("every target".to_string());
            } else {
                d.push(format!("to {}", targets.join(", ")));
            }
            if *full {
                d.push("full".into());
            }
            ("backup".into(), d.join(", "))
        }
    }
}

/// For a poll from `node_id` that's still running `running`: marks the delivered commands it isn't running and
/// hasn't answered as lost, and hands out the pending ones. Returns (lost, handed out).
pub fn take(c: &mut Connection, node_id: u64, running: &[u64], now: u64) -> Result<(Vec<u64>, Vec<Command>)> {
    let tx = c.transaction()?;
    let delivered: Vec<u64> = {
        let mut st = tx.prepare("SELECT id FROM commands WHERE node_id = ?1 AND state = 'delivered'")?;
        st.query_map([node_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
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
        let mut st =
            tx.prepare("SELECT id, action FROM commands WHERE node_id = ?1 AND state = 'pending' ORDER BY id")?;
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

/// Deletes the finished commands older than [`KEEP`], and finished inspects older than [`KEEP_INSPECTS`]. Returns
/// how many it deleted.
pub fn prune(c: &Connection, now: u64) -> Result<usize> {
    Ok(c.execute(
        "DELETE FROM commands
         WHERE state IN ('done', 'cancelled', 'lost')
           AND coalesce(finished_at, created_at) < CASE WHEN action_kind IS 'inspect' THEN ?2 ELSE ?1 END",
        params![now.saturating_sub(KEEP), now.saturating_sub(KEEP_INSPECTS)],
    )?)
}

/// When the server starts: nobody's waiting for the open commands any more.
pub fn lose_open(c: &Connection, now: u64) -> Result<usize> {
    Ok(c.execute(
        "UPDATE commands SET state = CASE state WHEN 'pending' THEN 'cancelled' ELSE 'lost' END, finished_at = ?1
         WHERE state IN ('pending', 'delivered')",
        [now],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 3600;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::store::migrate(&mut c).unwrap();
        c.execute("INSERT INTO nodes (name, joined_at) VALUES ('a', 0)", [])
            .unwrap();
        c
    }

    fn inspect() -> Action {
        Action::Inspect { app: "blog".into() }
    }

    fn suspend() -> Action {
        Action::Suspend { app: "blog".into() }
    }

    /// Runs `action` to the end at `now`.
    fn run(c: &mut Connection, action: &Action, now: u64) -> u64 {
        let id = insert(c, 1, action, "user ivan", now).unwrap();
        take_pending(c, 1, now).unwrap();
        assert!(finish(c, id, 1, r#"{"kind":"ok","value":null}"#, now).unwrap());
        id
    }

    fn ids(c: &Connection) -> Vec<u64> {
        let mut st = c.prepare("SELECT id FROM commands ORDER BY id").unwrap();
        st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
    }

    #[test]
    fn finished_commands_are_pruned_once_old_and_inspects_sooner() {
        let mut c = db();
        let now = 100 * DAY;
        run(&mut c, &suspend(), now - KEEP - 1);
        let kept = run(&mut c, &suspend(), now - KEEP + 1);
        run(&mut c, &inspect(), now - KEEP_INSPECTS - 1);
        let inspect_kept = run(&mut c, &inspect(), now - KEEP_INSPECTS + 1);
        // Cancelled and lost ones go too, by when they ended.
        let cancelled = insert(&c, 1, &suspend(), "user ivan", now - KEEP - 10).unwrap();
        cancel(&c, cancelled, now - KEEP - 1).unwrap();
        insert(&c, 1, &suspend(), "user ivan", now - KEEP - 10).unwrap();
        take(&mut c, 1, &[], now - KEEP - 10).unwrap();
        take(&mut c, 1, &[], now - KEEP - 1).unwrap();
        // Open ones stay however old they are: someone may be waiting for them.
        let pending = insert(&c, 1, &suspend(), "user ivan", 0).unwrap();
        let delivered = insert(&c, 1, &inspect(), "user ivan", 0).unwrap();
        c.execute("UPDATE commands SET state = 'delivered' WHERE id = ?1", [delivered])
            .unwrap();

        assert_eq!(prune(&c, now).unwrap(), 4);
        assert_eq!(ids(&c), vec![kept, inspect_kept, pending, delivered]);
        assert_eq!(prune(&c, now).unwrap(), 0);
    }

    #[test]
    fn the_history_leaves_inspects_out_by_its_index() {
        let mut c = db();
        let s1 = run(&mut c, &suspend(), 1000);
        for t in 0..50 {
            run(&mut c, &inspect(), 1001 + t);
        }
        let r = run(&mut c, &Action::Resume { app: "blog".into() }, 2000);
        run(&mut c, &Action::Suspend { app: "other".into() }, 2001);
        let h = history(&c, "blog", 0, 10).unwrap();
        assert_eq!(h.iter().map(|c| c.id).collect::<Vec<_>>(), vec![r, s1]);
        assert_eq!(h[0].action, "resume");
        assert_eq!(h[0].by, "user ivan");
        assert_eq!(h[0].ok, Some(true));
        // Only what's since the app was made, and only `limit` of it.
        assert_eq!(history(&c, "blog", 1500, 10).unwrap().len(), 1);
        assert_eq!(history(&c, "blog", 0, 1).unwrap()[0].id, r);

        // The query reads the partial index, which has no inspects in it, rather than the table.
        let plan: Vec<String> = {
            let mut st = c
                .prepare(
                    "EXPLAIN QUERY PLAN SELECT c.id FROM commands c LEFT JOIN nodes n ON n.id = c.node_id
                     WHERE c.target_kind = 'app' AND c.target = ?1 AND c.created_at >= ?3
                       AND c.action_kind IS NOT 'inspect'
                     ORDER BY c.id DESC LIMIT ?2",
                )
                .unwrap();
            st.query_map(params!["blog", 10, 0], |r| r.get::<_, String>(3))
                .unwrap()
                .map(|r| r.unwrap())
                .collect()
        };
        assert!(plan.iter().any(|p| p.contains("commands_history")), "{plan:?}");
    }

    #[test]
    fn migration_7_records_earlier_commands_kinds() {
        let mut c = Connection::open_in_memory().unwrap();
        for m in &crate::store::MIGRATIONS[..6] {
            c.execute_batch(m.sql).unwrap();
            if let Some(then) = m.then {
                then(&c).unwrap();
            }
        }
        c.pragma_update(None, "user_version", 6).unwrap();
        c.execute("INSERT INTO nodes (name, joined_at) VALUES ('a', 0)", [])
            .unwrap();
        for action in [
            r#"{"kind":"inspect","app":"blog"}"#,
            r#"{"kind":"suspend","app":"blog"}"#,
            "not json",
        ] {
            c.execute(
                "INSERT INTO commands (node_id, action, state, created_at, finished_at, target_kind, target)
                 VALUES (1, ?1, 'done', 10, 10, 'app', 'blog')",
                [action],
            )
            .unwrap();
        }
        crate::store::migrate(&mut c).unwrap();
        let kinds: Vec<Option<String>> = {
            let mut st = c.prepare("SELECT action_kind FROM commands ORDER BY id").unwrap();
            st.query_map([], |r| r.get(0)).unwrap().map(|r| r.unwrap()).collect()
        };
        assert_eq!(kinds, vec![Some("inspect".into()), Some("suspend".into()), None]);
        // The unreadable one is listed as unknown; the inspect isn't.
        let h = history(&c, "blog", 0, 10).unwrap();
        assert_eq!(
            h.iter().map(|c| c.action.as_str()).collect::<Vec<_>>(),
            vec!["unknown", "suspend"]
        );
    }
}

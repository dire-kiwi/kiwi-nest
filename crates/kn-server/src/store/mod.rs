//! The server's SQLite database, on a thread of its own.
//!
//! Every query runs on one dedicated thread, which owns the connection: [`Store::call`] sends it a closure and awaits
//! the result. So the async tasks never block on SQLite, and the database's work is serialized: a transaction that
//! checks a name and then claims it can't race another one.
//!
//! The schema is built by numbered migrations ([`MIGRATIONS`]), recorded in `PRAGMA user_version`; each runs in a
//! transaction of its own, once, with foreign keys checked at its end (so one may rebuild a table). A database from a
//! newer kiwi-nest is refused.
//!
//! The database runs in WAL mode with `synchronous = FULL`, so a change the server has answered for survives a
//! crash or a power cut.
//!
//! - [`tokens`]: admin, deploy, join and node tokens, by hash.
//! - [`nodes`]: nodes, their facts and their last reports.
//! - [`apps`]: apps and shared volumes, the desired state of each node, and whether a change is done.
//! - [`nests`]: which nest each app and shared volume is in, and the node each nest is pinned to.
//! - [`commands`]: one-off commands for nodes.

pub mod apps;
pub mod commands;
pub mod nests;
pub mod nodes;
pub mod tokens;

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use rusqlite::Connection;
use tokio::sync::oneshot;

use crate::error::{Error, Result};

/// One step of the schema: SQL, then (optionally) code that fills in what SQL can't, in the same transaction.
pub struct Migration {
    pub sql: &'static str,
    pub then: Option<fn(&Connection) -> anyhow::Result<()>>,
}

/// The migrations, in order. Never change one that has shipped; add another.
pub const MIGRATIONS: &[Migration] = &[
    // 1: tokens, nodes, bundles, apps, shared volumes and commands.
    Migration {
        sql: MIGRATION_1,
        then: None,
    },
    // 2: nests; nodes can be cordoned, labelled and forgotten (kept as a tombstone, so their names are unique only
    // among the nodes that aren't forgotten); apps record whether they idle out.
    Migration {
        sql: MIGRATION_2,
        then: Some(nests::backfill),
    },
];

const MIGRATION_1: &str = r#"
    CREATE TABLE nodes (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL UNIQUE,
        -- Bumped by every change to the node's desired state.
        seq INTEGER NOT NULL DEFAULT 0,
        facts TEXT,
        report TEXT,
        joined_at INTEGER NOT NULL,
        last_seen INTEGER
    );
    CREATE TABLE tokens (
        id INTEGER PRIMARY KEY,
        kind TEXT NOT NULL CHECK (kind IN ('admin', 'deploy', 'join', 'node')),
        -- SHA-256 of the token, in hex. The token itself is never stored.
        hash TEXT NOT NULL UNIQUE,
        name TEXT NOT NULL DEFAULT '',
        patterns TEXT NOT NULL DEFAULT '[]',
        from_patterns TEXT NOT NULL DEFAULT '[]',
        node_id INTEGER REFERENCES nodes (id),
        created_at INTEGER NOT NULL,
        used_at INTEGER,
        revoked_at INTEGER
    );
    CREATE TABLE bundles (
        digest TEXT PRIMARY KEY,
        size INTEGER NOT NULL,
        created_at INTEGER NOT NULL
    );
    CREATE TABLE apps (
        name TEXT PRIMARY KEY,
        -- `deleting` until the node has deleted it; `deleted` rows keep the name's generations increasing.
        state TEXT NOT NULL CHECK (state IN ('active', 'deleting', 'deleted')),
        node_id INTEGER REFERENCES nodes (id),
        -- Reserved across the cluster; released (NULL) once the app is deleted.
        domain TEXT UNIQUE,
        generation INTEGER NOT NULL,
        -- The node's sequence number when this last changed.
        seq INTEGER NOT NULL,
        bundle_digest TEXT REFERENCES bundles (digest),
        from_app TEXT,
        shared TEXT NOT NULL DEFAULT '[]',
        memory_bytes INTEGER NOT NULL DEFAULT 0,
        keep_data INTEGER NOT NULL DEFAULT 0,
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX apps_by_node ON apps (node_id, seq);
    CREATE TABLE volumes (
        name TEXT PRIMARY KEY,
        state TEXT NOT NULL CHECK (state IN ('active', 'deleting', 'deleted')),
        node_id INTEGER REFERENCES nodes (id),
        generation INTEGER NOT NULL,
        seq INTEGER NOT NULL,
        data_digest TEXT REFERENCES bundles (digest),
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL
    );
    CREATE INDEX volumes_by_node ON volumes (node_id, seq);
    CREATE TABLE commands (
        id INTEGER PRIMARY KEY,
        node_id INTEGER NOT NULL REFERENCES nodes (id),
        action TEXT NOT NULL,
        state TEXT NOT NULL CHECK (state IN ('pending', 'delivered', 'done', 'cancelled', 'lost')),
        result TEXT,
        created_at INTEGER NOT NULL,
        delivered_at INTEGER,
        finished_at INTEGER
    );
    CREATE INDEX commands_by_node ON commands (node_id, state);
    "#;

const MIGRATION_2: &str = r#"
    CREATE TABLE nodes_new (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL,
        seq INTEGER NOT NULL DEFAULT 0,
        facts TEXT,
        report TEXT,
        joined_at INTEGER NOT NULL,
        last_seen INTEGER,
        -- Cordoned nodes get no new nests.
        cordoned INTEGER NOT NULL DEFAULT 0,
        -- Labels set with `node label`, as a JSON object; they win over the node's own.
        labels TEXT NOT NULL DEFAULT '{}',
        -- Set by `node forget`. The row stays, as does its token's, so the node can be told it was forgotten.
        forgotten_at INTEGER
    );
    INSERT INTO nodes_new (id, name, seq, facts, report, joined_at, last_seen)
        SELECT id, name, seq, facts, report, joined_at, last_seen FROM nodes;
    DROP TABLE nodes;
    ALTER TABLE nodes_new RENAME TO nodes;
    CREATE UNIQUE INDEX nodes_by_name ON nodes (name) WHERE forgotten_at IS NULL;
    CREATE TABLE nests (
        name TEXT PRIMARY KEY,
        -- Where it's pinned. A nest with no apps or volumes left in it is gone, and is placed again when used.
        node_id INTEGER NOT NULL REFERENCES nodes (id),
        -- Why that node was chosen.
        placement TEXT NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL
    );
    ALTER TABLE apps ADD COLUMN nest TEXT;
    -- How the app came to be in its nest, such as `through --from blog`.
    ALTER TABLE apps ADD COLUMN nest_via TEXT NOT NULL DEFAULT '';
    ALTER TABLE apps ADD COLUMN idle_timeout_secs INTEGER NOT NULL DEFAULT 0;
    ALTER TABLE volumes ADD COLUMN nest TEXT;
    CREATE INDEX apps_by_nest ON apps (nest);
    CREATE INDEX volumes_by_nest ON volumes (nest);
    "#;

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// The database, owned by its thread.
#[derive(Clone)]
pub struct Store {
    tx: std::sync::mpsc::Sender<Job>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn rfc3339(secs: u64) -> String {
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_secs(secs)).to_string()
}

impl Store {
    /// Opens (or creates) the database and brings its schema up to date.
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(10))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        migrate(&mut conn).with_context(|| format!("migrating {}", path.display()))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let (tx, rx) = std::sync::mpsc::channel::<Job>();
        std::thread::Builder::new().name("kn-store".into()).spawn(move || {
            for job in rx {
                // A panicking query fails its own call (its reply is dropped), not the store.
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut conn)));
            }
        })?;
        Ok(Store { tx })
    }

    /// Runs `f` on the database's thread.
    pub async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.tx
            .send(Box::new(move |c| {
                let _ = tx.send(f(c));
            }))
            .map_err(|_| Error::Internal(anyhow::anyhow!("the database thread stopped")))?;
        rx.await
            .map_err(|_| Error::Internal(anyhow::anyhow!("a database query failed")))?
    }
}

/// Runs the migrations the database hasn't had yet, with foreign keys off (SQLite only changes that outside a
/// transaction) and checked at the end of each.
pub fn migrate(conn: &mut Connection) -> anyhow::Result<()> {
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    let have: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    let have = usize::try_from(have).unwrap_or(0);
    if have > MIGRATIONS.len() {
        anyhow::bail!(
            "the database is at version {have}, from a newer kiwi-nest (this one knows {})",
            MIGRATIONS.len()
        );
    }
    for (i, m) in MIGRATIONS.iter().enumerate().skip(have) {
        let tx = conn.transaction()?;
        tx.execute_batch(m.sql)
            .with_context(|| format!("migration {}", i + 1))?;
        if let Some(then) = m.then {
            then(&tx).with_context(|| format!("migration {}", i + 1))?;
        }
        let broken: i64 = tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| r.get(0))?;
        if broken > 0 {
            anyhow::bail!("migration {} left {broken} broken foreign key(s)", i + 1);
        }
        tx.pragma_update(None, "user_version", (i + 1) as i64)?;
        tx.commit()?;
        tracing::info!(version = i + 1, "migrated the database");
    }
    Ok(())
}

/// Parses a JSON column, or the default if it's NULL.
pub(crate) fn json<T: serde::de::DeserializeOwned + Default>(s: Option<String>) -> Result<T> {
    match s {
        Some(s) => Ok(serde_json::from_str(&s)?),
        None => Ok(T::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_run_once_and_refuse_newer_databases() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db.sqlite");
        let mut conn = Connection::open(&path).unwrap();
        migrate(&mut conn).unwrap();
        let v: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
        assert_eq!(v as usize, MIGRATIONS.len());
        // Again: nothing to do (a second CREATE TABLE would fail).
        migrate(&mut conn).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        let err = migrate(&mut conn).unwrap_err().to_string();
        assert!(err.contains("newer kiwi-nest"), "{err}");
    }

    #[tokio::test]
    async fn calls_run_on_the_store_thread() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("db.sqlite")).unwrap();
        let name = store
            .call(|_| Ok(std::thread::current().name().map(str::to_string)))
            .await
            .unwrap();
        assert_eq!(name.as_deref(), Some("kn-store"));
        // A query that panics fails, and the store carries on.
        assert!(store.call(|_| -> Result<()> { panic!("boom") }).await.is_err());
        assert_eq!(store.call(|_| Ok(1)).await.unwrap(), 1);
    }
}

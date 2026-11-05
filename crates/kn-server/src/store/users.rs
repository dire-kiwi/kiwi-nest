//! The web UI's users and their sessions.
//!
//! A user has a name and an Argon2id hash of their password. A session is a random token in the browser's cookie,
//! stored here only as its SHA-256 (like the API's tokens). It ends after [`SESSION_IDLE`] without use, after
//! [`SESSION_MAX`] in any case, when its user logs out, and when the user's password changes or the user is
//! deleted.

use kn_proto::api::{SessionInfo, UserInfo};
use rusqlite::{Connection, OptionalExtension, params};

use super::rfc3339;
use crate::error::{Error, Result};

/// A session unused for this long has ended.
pub const SESSION_IDLE: u64 = 12 * 3600;
/// A session ends this long after it was made, used or not.
pub const SESSION_MAX: u64 = 7 * 24 * 3600;
/// A session's last use is written at most this often.
const TOUCH_EVERY: u64 = 60;

/// A user, with their password's hash.
#[derive(Debug, Clone)]
pub struct User {
    pub id: u64,
    pub name: String,
    pub password_hash: String,
}

/// Whether `name` may name a user: lowercase letters, digits, `-`, `_` and `.`, up to 64.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.'))
}

pub fn create(c: &Connection, name: &str, password_hash: &str, now: u64) -> Result<UserInfo> {
    let n = c.execute(
        "INSERT INTO users (name, password_hash, created_at, password_changed_at) VALUES (?1, ?2, ?3, ?3)
         ON CONFLICT (name) DO NOTHING",
        params![name, password_hash, now],
    )?;
    if n == 0 {
        return Err(Error::Conflict(format!("there's a user `{name}` already")));
    }
    Ok(UserInfo {
        name: name.into(),
        created_at: rfc3339(now),
        last_login_at: None,
    })
}

pub fn list(c: &Connection) -> Result<Vec<UserInfo>> {
    let mut st = c.prepare("SELECT name, created_at, last_login_at FROM users ORDER BY name")?;
    let rows = st.query_map([], |r| {
        Ok(UserInfo {
            name: r.get(0)?,
            created_at: rfc3339(r.get(1)?),
            last_login_at: r.get::<_, Option<u64>>(2)?.map(rfc3339),
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn get(c: &Connection, name: &str) -> Result<Option<User>> {
    Ok(c.query_row(
        "SELECT id, name, password_hash FROM users WHERE name = ?1",
        [name],
        |r| {
            Ok(User {
                id: r.get(0)?,
                name: r.get(1)?,
                password_hash: r.get(2)?,
            })
        },
    )
    .optional()?)
}

fn missing(name: &str) -> Error {
    Error::NotFound(format!("there's no user `{name}`"))
}

/// Changes a user's password, ending every session of theirs.
pub fn set_password(c: &mut Connection, name: &str, password_hash: &str, now: u64) -> Result<()> {
    let tx = c.transaction()?;
    let id: u64 = tx
        .query_row("SELECT id FROM users WHERE name = ?1", [name], |r| r.get(0))
        .optional()?
        .ok_or_else(|| missing(name))?;
    tx.execute(
        "UPDATE users SET password_hash = ?2, password_changed_at = ?3 WHERE id = ?1",
        params![id, password_hash, now],
    )?;
    tx.execute("DELETE FROM sessions WHERE user_id = ?1", [id])?;
    tx.commit()?;
    Ok(())
}

/// Deletes a user and their sessions.
pub fn delete(c: &mut Connection, name: &str) -> Result<()> {
    let tx = c.transaction()?;
    let id: u64 = tx
        .query_row("SELECT id FROM users WHERE name = ?1", [name], |r| r.get(0))
        .optional()?
        .ok_or_else(|| missing(name))?;
    tx.execute("DELETE FROM sessions WHERE user_id = ?1", [id])?;
    tx.execute("DELETE FROM users WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(())
}

/// Saves a new session for a user who just logged in, and drops sessions that have ended.
pub fn start_session(c: &Connection, user_id: u64, hash: &str, now: u64) -> Result<()> {
    expire(c, now)?;
    c.execute(
        "INSERT INTO sessions (hash, user_id, created_at, last_used_at) VALUES (?1, ?2, ?3, ?3)",
        params![hash, user_id, now],
    )?;
    c.execute(
        "UPDATE users SET last_login_at = ?2 WHERE id = ?1",
        params![user_id, now],
    )?;
    Ok(())
}

/// The session with this hash and its user, if it hasn't ended; notes that it was used.
pub fn session(c: &Connection, hash: &str, now: u64) -> Result<Option<(User, SessionInfo)>> {
    let row = c
        .query_row(
            "SELECT u.id, u.name, u.password_hash, s.created_at, s.last_used_at
             FROM sessions s JOIN users u ON u.id = s.user_id WHERE s.hash = ?1",
            [hash],
            |r| {
                Ok((
                    User {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        password_hash: r.get(2)?,
                    },
                    r.get::<_, u64>(3)?,
                    r.get::<_, u64>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((user, created, used)) = row else {
        return Ok(None);
    };
    if now >= created.saturating_add(SESSION_MAX) || now >= used.saturating_add(SESSION_IDLE) {
        c.execute("DELETE FROM sessions WHERE hash = ?1", [hash])?;
        return Ok(None);
    }
    let used = if now >= used + TOUCH_EVERY {
        c.execute(
            "UPDATE sessions SET last_used_at = ?2 WHERE hash = ?1",
            params![hash, now],
        )?;
        now
    } else {
        used
    };
    let info = SessionInfo {
        user: user.name.clone(),
        created_at: rfc3339(created),
        idle_expires_at: rfc3339((used + SESSION_IDLE).min(created + SESSION_MAX)),
        expires_at: rfc3339(created + SESSION_MAX),
    };
    Ok(Some((user, info)))
}

/// Ends a session (logging out).
pub fn end_session(c: &Connection, hash: &str) -> Result<()> {
    c.execute("DELETE FROM sessions WHERE hash = ?1", [hash])?;
    Ok(())
}

/// Drops the sessions that have ended.
pub fn expire(c: &Connection, now: u64) -> Result<usize> {
    Ok(c.execute(
        "DELETE FROM sessions WHERE created_at + ?1 <= ?3 OR last_used_at + ?2 <= ?3",
        params![SESSION_MAX, SESSION_IDLE, now],
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let mut c = Connection::open_in_memory().unwrap();
        crate::store::migrate(&mut c).unwrap();
        c
    }

    #[test]
    fn sessions_end_when_idle_or_old_and_with_their_user() {
        let mut c = db();
        create(&c, "ivan", "h1", 1000).unwrap();
        assert!(matches!(create(&c, "ivan", "h2", 1000), Err(Error::Conflict(_))));
        let u = get(&c, "ivan").unwrap().unwrap();
        start_session(&c, u.id, "s1", 1000).unwrap();
        assert_eq!(
            list(&c).unwrap()[0].last_login_at.as_deref(),
            Some(rfc3339(1000).as_str())
        );
        // Used within the idle time, it carries on, up to its maximum.
        let mut t = 1000;
        while t + SESSION_IDLE - 1 < 1000 + SESSION_MAX {
            t += SESSION_IDLE - 1;
            assert!(session(&c, "s1", t).unwrap().is_some(), "at {t}");
        }
        assert!(session(&c, "s1", 1000 + SESSION_MAX).unwrap().is_none());
        // Unused, it ends after the idle time.
        start_session(&c, u.id, "s2", 5000).unwrap();
        assert!(session(&c, "s2", 5000 + SESSION_IDLE).unwrap().is_none());
        // A new password ends every session.
        start_session(&c, u.id, "s3", 6000).unwrap();
        set_password(&mut c, "ivan", "h3", 6001).unwrap();
        assert!(session(&c, "s3", 6002).unwrap().is_none());
        assert_eq!(get(&c, "ivan").unwrap().unwrap().password_hash, "h3");
        // So does deleting the user.
        start_session(&c, u.id, "s4", 7000).unwrap();
        delete(&mut c, "ivan").unwrap();
        assert!(session(&c, "s4", 7001).unwrap().is_none());
        assert!(matches!(delete(&mut c, "ivan"), Err(Error::NotFound(_))));
        // Logging out ends one.
        create(&c, "kim", "h", 8000).unwrap();
        let k = get(&c, "kim").unwrap().unwrap();
        start_session(&c, k.id, "s5", 8000).unwrap();
        end_session(&c, "s5").unwrap();
        assert!(session(&c, "s5", 8001).unwrap().is_none());
    }

    #[test]
    fn user_names_are_checked() {
        assert!(valid_name("ivan"));
        assert!(valid_name("ops.team-1_x"));
        assert!(!valid_name(""));
        assert!(!valid_name("Ivan"));
        assert!(!valid_name("a b"));
        assert!(!valid_name(&"a".repeat(65)));
    }
}

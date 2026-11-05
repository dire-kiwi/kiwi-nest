//! What the registry's garbage collection needs to know: which images deployed apps use, when tags were pushed and
//! images released, the nodes' registry credentials, and whether anything may be pulling now.
//!
//! - **Uses.** Each app records the image references of its bundle (`apps.images`). Apps that are deployed or being
//!   deleted (still on their node) use them.
//! - **Releases** (`image_releases`): when an app stops using an image (redeployed with other images, deleted, or its
//!   node forgotten), the reference is stamped, so the image is kept for the grace period: a delete followed by a
//!   deploy finds it.
//! - **Tags** (`registry_tags`): when each was last pushed through the ingress, and the digest garbage collection
//!   last saw it at, with when it first saw that digest (so a tag pushed some other way is kept for the grace period
//!   too).

use std::collections::{BTreeMap, HashSet};

use kn_proto::agent::Report;
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

/// One tag the registry holds, and the digest of its manifest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Listed {
    pub repository: String,
    pub tag: String,
    pub digest: String,
}

/// The images that apps on nodes use, and the apps whose images aren't known (deployed before the server recorded
/// them, with a bundle it couldn't read).
#[derive(Debug, Clone, Default)]
pub struct Uses {
    /// (app, node, image references).
    pub apps: Vec<(String, String, Vec<String>)>,
    pub unknown: Vec<String>,
}

/// Records that `tag` of `repository` is being pushed.
pub fn record_push(c: &Connection, repository: &str, tag: &str, now: u64) -> Result<()> {
    c.execute(
        "INSERT INTO registry_tags (repository, tag, pushed_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (repository, tag) DO UPDATE SET pushed_at = excluded.pushed_at",
        params![repository, tag, now],
    )?;
    Ok(())
}

/// Stamps the images in `old` that aren't in `new` as released now.
pub fn release(c: &Connection, old: &[String], new: &[String], now: u64) -> Result<()> {
    for image in old.iter().filter(|i| !new.contains(i)) {
        c.execute(
            "INSERT INTO image_releases (reference, released_at) VALUES (?1, ?2)
             ON CONFLICT (reference) DO UPDATE SET released_at = excluded.released_at",
            params![image, now],
        )?;
    }
    Ok(())
}

/// The images an app's row records, if they're known.
pub fn images_of(c: &Connection, app: &str) -> Result<Vec<String>> {
    let images: Option<Option<String>> = c
        .query_row("SELECT images FROM apps WHERE name = ?1", [app], |r| r.get(0))
        .optional()?;
    Ok(images
        .flatten()
        .and_then(|i| serde_json::from_str(&i).ok())
        .unwrap_or_default())
}

/// Releases the images of every app of a node that's still placed on it (before it's forgotten).
pub fn release_node(c: &Connection, node_id: u64, now: u64) -> Result<()> {
    let apps: Vec<String> = {
        let mut st = c.prepare("SELECT name FROM apps WHERE node_id = ?1 AND state != 'deleted'")?;
        st.query_map([node_id], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    };
    for a in apps {
        release(c, &images_of(c, &a)?, &[], now)?;
    }
    Ok(())
}

/// The images that apps deployed on nodes (or being deleted from them) use.
pub fn uses(c: &Connection) -> Result<Uses> {
    let mut st = c.prepare(
        "SELECT a.name, n.name, a.images FROM apps a JOIN nodes n ON n.id = a.node_id
         WHERE a.state IN ('active', 'deleting') AND n.forgotten_at IS NULL ORDER BY a.name",
    )?;
    let rows = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut out = Uses::default();
    for row in rows {
        let (app, node, images) = row?;
        match images.map(|i| serde_json::from_str::<Vec<String>>(&i)) {
            Some(Ok(images)) => out.apps.push((app, node, images)),
            _ => out.unknown.push(app),
        }
    }
    Ok(out)
}

/// Every release: (reference, when).
pub fn releases(c: &Connection) -> Result<Vec<(String, u64)>> {
    let mut st = c.prepare("SELECT reference, released_at FROM image_releases")?;
    Ok(st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

/// What's known of each tag: (repository, tag) → (pushed at, digest seen, first seen at that digest).
pub type TagTimes = BTreeMap<(String, String), (Option<u64>, Option<String>, Option<u64>)>;

pub fn tag_times(c: &Connection) -> Result<TagTimes> {
    let mut st = c.prepare("SELECT repository, tag, pushed_at, digest, seen_at FROM registry_tags")?;
    let rows = st.query_map([], |r| {
        Ok((
            (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
            (r.get(2)?, r.get(3)?, r.get(4)?),
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Records the digest each listed tag is at, and when it was first seen there. Returns whether any tag moved to
/// another manifest, which leaves the old one untagged. Forgets tags no longer listed that weren't pushed within
/// `keep` seconds, and releases older than that.
pub fn see(c: &mut Connection, listed: &[Listed], keep: u64, now: u64) -> Result<bool> {
    let tx = c.transaction()?;
    let known = tag_times(&tx)?;
    let mut moved = false;
    for l in listed {
        let key = (l.repository.clone(), l.tag.clone());
        match known.get(&key) {
            Some((_, Some(d), _)) if d == &l.digest => {}
            other => {
                moved |= matches!(other, Some((_, Some(_), _)));
                tx.execute(
                    "INSERT INTO registry_tags (repository, tag, digest, seen_at) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT (repository, tag) DO UPDATE SET digest = excluded.digest, seen_at = excluded.seen_at",
                    params![l.repository, l.tag, l.digest, now],
                )?;
            }
        }
    }
    let present: HashSet<(&str, &str)> = listed.iter().map(|l| (l.repository.as_str(), l.tag.as_str())).collect();
    let cutoff = now.saturating_sub(keep);
    for ((repo, tag), (pushed, _, _)) in &known {
        if !present.contains(&(repo.as_str(), tag.as_str())) && pushed.is_none_or(|p| p < cutoff) {
            tx.execute(
                "DELETE FROM registry_tags WHERE repository = ?1 AND tag = ?2",
                params![repo, tag],
            )?;
        }
    }
    tx.execute("DELETE FROM image_releases WHERE released_at < ?1", [cutoff])?;
    tx.commit()?;
    Ok(moved)
}

/// Whether the registry's own garbage collection is owed: manifests were deleted (or untagged) since it last ran.
pub fn collect_pending(c: &Connection) -> Result<bool> {
    Ok(
        c.query_row("SELECT collect_pending FROM registry_gc WHERE id = 1", [], |r| r.get(0))
            .optional()?
            .unwrap_or(false),
    )
}

pub fn set_collect_pending(c: &Connection, pending: bool, now: u64) -> Result<()> {
    c.execute(
        "INSERT INTO registry_gc (id, collect_pending, updated_at) VALUES (1, ?1, ?2)
         ON CONFLICT (id) DO UPDATE SET collect_pending = excluded.collect_pending, updated_at = excluded.updated_at",
        params![pending, now],
    )?;
    Ok(())
}

/// Why something may be pulling from the registry now, if anything may: a deploy on its way to an online node, or
/// an app that an online node reports as deploying or starting.
pub fn busy(c: &Connection, online: &dyn Fn(u64) -> bool) -> Result<Option<String>> {
    let mut st = c.prepare("SELECT id, name, report FROM nodes WHERE forgotten_at IS NULL")?;
    let nodes: Vec<(u64, String, Option<String>)> = st
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for (id, node, report) in nodes {
        if !online(id) {
            continue;
        }
        let report: Option<Report> = report.and_then(|r| serde_json::from_str(&r).ok());
        if let Some(r) = &report
            && let Some(a) = r
                .apps
                .iter()
                .find(|a| a.starting || a.status == kn_proto::state::AppStatus::Deploying)
        {
            return Ok(Some(format!("app `{}` is starting on node `{node}`", a.name)));
        }
        let mut st = c.prepare("SELECT name, generation FROM apps WHERE node_id = ?1 AND state = 'active'")?;
        let apps: Vec<(String, u64)> = st
            .query_map([id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        for (app, generation) in apps {
            let applied = report
                .as_ref()
                .and_then(|r| r.applied.apps.get(&app))
                .map_or(0, |o| o.generation);
            if applied < generation {
                return Ok(Some(format!("app `{app}` is being deployed on node `{node}`")));
            }
        }
    }
    Ok(None)
}

/// Stores the hash of a node's registry credential, replacing the last one.
pub fn set_node_credential(c: &Connection, node_id: u64, hash: &str) -> Result<()> {
    c.execute(
        "UPDATE nodes SET registry_hash = ?2 WHERE id = ?1 AND forgotten_at IS NULL",
        params![node_id, hash],
    )?;
    Ok(())
}

/// The node whose registry credential has this hash, unless it's forgotten.
pub fn node_by_credential(c: &Connection, hash: &str) -> Result<Option<String>> {
    Ok(c.query_row(
        "SELECT name FROM nodes WHERE registry_hash = ?1 AND forgotten_at IS NULL",
        [hash],
        |r| r.get(0),
    )
    .optional()?)
}

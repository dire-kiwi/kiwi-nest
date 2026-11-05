//! Nests: apps and shared volumes that must share a node.
//!
//! Each app and shared volume names its nest (`apps.nest`, `volumes.nest`), and the `nests` table pins each nest to
//! a node, with why that node was chosen. A nest exists while it has members: an app or volume that isn't deleted
//! (one being deleted still counts, as its data is still on the node). Once the last one is gone, the row is only
//! history, and the next app or volume to name the nest places it again, possibly elsewhere.

use std::collections::{BTreeMap, HashMap};

use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

/// A nest as the server records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nest {
    pub name: String,
    pub node_id: u64,
    /// Why its node was chosen.
    pub placement: String,
    pub created_at: u64,
}

/// A nest with its members.
#[derive(Debug, Clone)]
pub struct Members {
    pub nest: Nest,
    /// Its apps, with the memory each may use.
    pub apps: Vec<(String, u64)>,
    pub volumes: Vec<String>,
}

/// Whether `name` may name a nest: what an app name may be.
pub fn valid_name(name: &str) -> bool {
    kn_bundle::manifest::is_valid_app_name(name)
}

/// The nest named `name`, if it exists (has members).
pub fn live(c: &Connection, name: &str) -> Result<Option<Nest>> {
    Ok(c.query_row(
        "SELECT name, node_id, placement, created_at FROM nests n WHERE name = ?1
             AND (EXISTS (SELECT 1 FROM apps WHERE nest = n.name AND state != 'deleted')
                  OR EXISTS (SELECT 1 FROM volumes WHERE nest = n.name AND state != 'deleted'))",
        [name],
        |r| {
            Ok(Nest {
                name: r.get(0)?,
                node_id: r.get(1)?,
                placement: r.get(2)?,
                created_at: r.get(3)?,
            })
        },
    )
    .optional()?)
}

/// Pins a new nest (or one whose members are all gone) to a node.
pub fn place(c: &Connection, name: &str, node_id: u64, placement: &str, now: u64) -> Result<()> {
    c.execute(
        "INSERT INTO nests (name, node_id, placement, created_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (name) DO UPDATE SET node_id = ?2, placement = ?3, created_at = ?4",
        params![name, node_id, placement, now],
    )?;
    Ok(())
}

/// Every nest that exists, by name, with its members.
pub fn list(c: &Connection) -> Result<Vec<Members>> {
    let mut nests: BTreeMap<String, Members> = BTreeMap::new();
    {
        let mut st = c.prepare("SELECT name, node_id, placement, created_at FROM nests ORDER BY name")?;
        let rows = st.query_map([], |r| {
            Ok(Nest {
                name: r.get(0)?,
                node_id: r.get(1)?,
                placement: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?;
        for n in rows {
            let n = n?;
            nests.insert(
                n.name.clone(),
                Members {
                    nest: n,
                    apps: Vec::new(),
                    volumes: Vec::new(),
                },
            );
        }
    }
    let mut st = c.prepare(
        "SELECT nest, name, memory_bytes FROM apps WHERE state != 'deleted' AND nest IS NOT NULL ORDER BY name",
    )?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?, r.get(2)?)))? {
        let (nest, name, mem) = r?;
        if let Some(m) = nests.get_mut(&nest) {
            m.apps.push((name, mem));
        }
    }
    let mut st =
        c.prepare("SELECT nest, name FROM volumes WHERE state != 'deleted' AND nest IS NOT NULL ORDER BY name")?;
    for r in st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get(1)?)))? {
        let (nest, name) = r?;
        if let Some(m) = nests.get_mut(&nest) {
            m.volumes.push(name);
        }
    }
    Ok(nests
        .into_values()
        .filter(|m| !m.apps.is_empty() || !m.volumes.is_empty())
        .collect())
}

/// Migration 2: puts the apps and volumes of a database from before nests into nests, as the rules would have.
/// A volume starts a nest named after it; an app mounting volumes joins the first one's, an app made `--from`
/// another joins that one's, and any other app starts its own. Every nest is pinned where its members already are
/// (the step before nests kept them together), and a name already taken on another node gets a `-2`, `-3`, ….
pub fn backfill(c: &Connection) -> anyhow::Result<()> {
    let mut pinned: HashMap<String, u64> = HashMap::new();
    let volumes: Vec<(String, u64)> = {
        let mut st = c.prepare(
            "SELECT name, node_id FROM volumes WHERE state != 'deleted' AND node_id IS NOT NULL ORDER BY name",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut volume_nest = HashMap::new();
    for (name, node) in volumes {
        let nest = claim(c, &mut pinned, &name, node)?;
        c.execute("UPDATE volumes SET nest = ?2 WHERE name = ?1", params![name, nest])?;
        volume_nest.insert(name, nest);
    }
    type App = (String, u64, Option<String>, String);
    let apps: Vec<App> = {
        let mut st = c.prepare(
            "SELECT name, node_id, from_app, shared FROM apps WHERE state != 'deleted' AND node_id IS NOT NULL
             ORDER BY name",
        )?;
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let mut app_nest: HashMap<String, (String, String)> = HashMap::new();
    let mut left = Vec::new();
    for (name, node, from, shared) in apps {
        let shared: Vec<String> = serde_json::from_str(&shared).unwrap_or_default();
        match shared.iter().find_map(|v| volume_nest.get(v)) {
            Some(nest) if pinned.get(nest) == Some(&node) => {
                app_nest.insert(name, (nest.clone(), format!("through shared volume `{}`", shared[0])));
            }
            _ => left.push((name, node, from)),
        }
    }
    // Apps made `--from` another follow it, however long the chain. Those left start nests of their own, roots
    // first (an app whose source isn't waiting too), so that the apps made from them follow on the next pass.
    loop {
        loop {
            let before = left.len();
            left.retain(|(name, node, from)| {
                let Some(f) = from else { return true };
                let Some((nest, _)) = app_nest.get(f).cloned() else {
                    return true;
                };
                if pinned.get(&nest) != Some(node) {
                    return true;
                }
                app_nest.insert(name.clone(), (nest, format!("through --from {f}")));
                false
            });
            if left.len() == before {
                break;
            }
        }
        if left.is_empty() {
            break;
        }
        let root = left
            .iter()
            .position(|(_, _, from)| from.as_ref().is_none_or(|f| !left.iter().any(|(n, ..)| n == f)))
            .unwrap_or(0);
        let (name, node, _) = left.remove(root);
        let nest = claim(c, &mut pinned, &name, node)?;
        app_nest.insert(name, (nest, "named after the app".into()));
    }
    for (name, (nest, via)) in app_nest {
        c.execute(
            "UPDATE apps SET nest = ?2, nest_via = ?3 WHERE name = ?1",
            params![name, nest, via],
        )?;
    }
    Ok(())
}

/// For [`backfill`]: the nest named `preferred` on `node`, made if needed, or `preferred-2`, … if that name is
/// pinned to another node.
fn claim(c: &Connection, pinned: &mut HashMap<String, u64>, preferred: &str, node: u64) -> anyhow::Result<String> {
    let mut name = preferred.to_string();
    let mut n = 1;
    loop {
        match pinned.get(&name) {
            Some(&on) if on == node => return Ok(name),
            Some(_) => {
                n += 1;
                name = format!("{preferred}-{n}");
            }
            None => {
                place(c, &name, node, "placed before nests existed", super::now())?;
                pinned.insert(name.clone(), node);
                return Ok(name);
            }
        }
    }
}

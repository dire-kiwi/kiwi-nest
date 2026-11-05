//! Placing a new nest on a node. (A nest that exists stays where it is: see [`crate::store::apps`].)
//!
//! 1. **Filter.** A node qualifies if it's online and not cordoned, has the architecture the bundle asks for (and
//!    one that every image the registry holds for it is built for), has every label in its `node_labels`, and allows
//!    every host path it mounts. `--node` names the node outright, but
//!    the filter still applies (apart from being online, which `--queue` may waive).
//! 2. **Score** by memory headroom: the node's memory, less a reserve, less the `mem_limit`s of its always-on apps,
//!    less a share (half, by default) of those of its apps that idle out. A service without a limit counts as
//!    256 MiB. Ties go to more free btrfs space, then to fewer apps, then to the name.
//!
//! [`place`] is pure, so its rules are tested with tables; [`candidates`] reads the nodes and what's placed on them.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use kn_proto::agent::NodeFacts;
use rusqlite::Connection;

use crate::error::Result;
use crate::store::{apps, nodes};

/// How placement counts memory.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    /// Memory kept back on every node for the system and Docker.
    pub memory_reserve: u64,
    /// The share of an idling app's `mem_limit` counted against its node: idle apps are stopped most of the time.
    pub idle_factor: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            memory_reserve: 512 << 20,
            idle_factor: 0.5,
        }
    }
}

/// What a node already has placed on it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Load {
    /// The `mem_limit`s of its always-on apps.
    pub always_on: u64,
    /// The `mem_limit`s of its apps that idle out.
    pub idle: u64,
    pub apps: u64,
}

impl Load {
    /// The load with one more app.
    pub fn with(self, memory: u64, idles: bool) -> Load {
        let mut l = self;
        if idles {
            l.idle += memory;
        } else {
            l.always_on += memory;
        }
        l.apps += 1;
        l
    }
}

/// A node that might take a nest.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: u64,
    pub name: String,
    pub online: bool,
    pub cordoned: bool,
    pub facts: NodeFacts,
    /// Its labels: those it reports, with those set by `node label` over them.
    pub labels: BTreeMap<String, String>,
    pub load: Load,
}

/// What the nest's first member needs.
#[derive(Debug, Clone, Default)]
pub struct Needs {
    pub arch: Option<String>,
    pub labels: BTreeMap<String, String>,
    pub host_paths: Vec<String>,
    /// The images in the cluster's registry, with the architectures each is built for.
    pub images: Vec<crate::registry::Platforms>,
}

/// The node chosen, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub id: u64,
    pub name: String,
    pub why: String,
}

/// A node's memory headroom with `load` on it. Negative when it's overcommitted.
pub fn headroom(memory: u64, load: Load, s: &Settings) -> i128 {
    i128::from(memory)
        - i128::from(s.memory_reserve)
        - i128::from(load.always_on)
        - (load.idle as f64 * s.idle_factor).round() as i128
}

/// A size in bytes for people, such as `1.5 GiB`, `-300 MiB`.
pub fn bytes(b: i128) -> String {
    const GIB: f64 = (1u64 << 30) as f64;
    const MIB: i128 = 1 << 20;
    if b.abs() >= 1 << 30 {
        format!("{:.1} GiB", b as f64 / GIB)
    } else {
        format!("{} MiB", b / MIB)
    }
}

/// Why the node can't take a nest that needs `needs`, if it can't. `online` says whether being offline counts.
pub fn unfit(c: &Candidate, needs: &Needs, online: bool) -> Option<String> {
    if online && !c.online {
        return Some("offline".into());
    }
    if c.cordoned {
        return Some("cordoned".into());
    }
    if let Some(a) = &needs.arch
        && &c.facts.arch != a
    {
        return Some(format!("arch `{}`, not `{a}`", c.facts.arch));
    }
    if let Some(i) = needs.images.iter().find(|i| !i.archs.contains(&c.facts.arch)) {
        return Some(format!(
            "arch `{}`, and image `{}` is only built for {}",
            c.facts.arch,
            i.image,
            i.archs.join(", ")
        ));
    }
    let missing: Vec<String> = needs
        .labels
        .iter()
        .filter(|(k, v)| c.labels.get(*k) != Some(*v))
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    if !missing.is_empty() {
        return Some(format!("no label {}", missing.join(", ")));
    }
    let disallowed: Vec<&str> = needs
        .host_paths
        .iter()
        .filter(|p| {
            !c.facts
                .allowed_host_paths
                .iter()
                .any(|a| Path::new(p.as_str()).starts_with(a))
        })
        .map(String::as_str)
        .collect();
    if !disallowed.is_empty() {
        return Some(format!(
            "host path {} not allowed (see `--allow-host-path`)",
            disallowed
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    None
}

/// Why no node qualified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoNode {
    /// Every node, with why it didn't qualify.
    pub unfit: Vec<(String, String)>,
}

impl NoNode {
    /// Says why no node can take new nest `nest`.
    pub fn describe(&self, nest: &str) -> String {
        if self.unfit.is_empty() {
            return "no node has joined the cluster yet: make a join token with `kiwi-nest token create join`".into();
        }
        format!(
            "no node can take new nest `{nest}`: {}",
            self.unfit
                .iter()
                .map(|(n, why)| format!("{n} ({why})"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    /// Whether every node that failed failed only for being offline (so `--node` with `--queue` would do).
    pub fn all_offline(&self) -> bool {
        !self.unfit.is_empty() && self.unfit.iter().all(|(_, why)| why == "offline")
    }
}

/// Picks the node for a new nest, and says why.
pub fn place(candidates: &[Candidate], needs: &Needs, s: &Settings) -> Result<Choice, NoNode> {
    let mut fit = Vec::new();
    let mut unfit_nodes = Vec::new();
    for c in candidates {
        match unfit(c, needs, true) {
            Some(why) => unfit_nodes.push((c.name.clone(), why)),
            None => fit.push(c),
        }
    }
    if fit.is_empty() {
        return Err(NoNode { unfit: unfit_nodes });
    }
    let room = |c: &Candidate| headroom(c.facts.memory_bytes, c.load, s);
    let rank = |a: &&Candidate, b: &&Candidate| -> Ordering {
        room(a)
            .cmp(&room(b))
            .then(a.facts.volumes.free_bytes.cmp(&b.facts.volumes.free_bytes))
            .then(b.load.apps.cmp(&a.load.apps))
            .then(b.name.cmp(&a.name))
    };
    fit.sort_by(|a, b| rank(b, a));
    let best = fit[0];
    let mut why = match fit.get(1) {
        None => format!("the only node that qualified ({} memory headroom)", bytes(room(best))),
        Some(next) => {
            let others = fit[1..]
                .iter()
                .map(|c| format!("{} {}", c.name, bytes(room(c))))
                .collect::<Vec<_>>()
                .join(", ");
            if room(best) != room(next) {
                format!("the most memory headroom: {} (others: {others})", bytes(room(best)))
            } else if best.facts.volumes.free_bytes != next.facts.volumes.free_bytes {
                format!(
                    "as much memory headroom as {} ({}), and more free btrfs space ({}, against {})",
                    next.name,
                    bytes(room(best)),
                    bytes(best.facts.volumes.free_bytes.into()),
                    bytes(next.facts.volumes.free_bytes.into())
                )
            } else if best.load.apps != next.load.apps {
                format!(
                    "as much memory headroom and btrfs space as {}, and fewer apps ({}, against {})",
                    next.name, best.load.apps, next.load.apps
                )
            } else {
                format!(
                    "level with {} on memory, btrfs space and apps; first by name",
                    next.name
                )
            }
        }
    };
    if !unfit_nodes.is_empty() {
        why.push_str(&format!(
            "; not eligible: {}",
            unfit_nodes
                .iter()
                .map(|(n, w)| format!("{n} ({w})"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(Choice {
        id: best.id,
        name: best.name.clone(),
        why,
    })
}

/// Every node that isn't forgotten, with what's placed on it, not counting the app `except` (being redeployed).
pub fn candidates(c: &Connection, online: &dyn Fn(u64) -> bool, except: Option<&str>) -> Result<Vec<Candidate>> {
    let loads = loads(c, except)?;
    Ok(nodes::list_facts(c)?
        .into_iter()
        .map(|n| Candidate {
            id: n.id,
            online: online(n.id),
            cordoned: n.cordoned,
            labels: nodes::labels_of(&n),
            facts: nodes::facts_of(&n),
            load: loads.get(&n.id).copied().unwrap_or_default(),
            name: n.name,
        })
        .collect())
}

/// What's placed on each node, not counting the app `except`.
pub fn loads(c: &Connection, except: Option<&str>) -> Result<HashMap<u64, Load>> {
    Ok(apps::placed(c, except)?
        .into_iter()
        .map(|(n, always_on, idle, apps)| (n, Load { always_on, idle, apps }))
        .collect())
}

#[cfg(test)]
mod tests {
    use kn_proto::agent::DiskSpace;

    use super::*;

    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    fn node(name: &str, memory: u64) -> Candidate {
        Candidate {
            id: name.bytes().map(u64::from).sum(),
            name: name.into(),
            online: true,
            cordoned: false,
            facts: NodeFacts {
                arch: "amd64".into(),
                memory_bytes: memory,
                volumes: DiskSpace {
                    total_bytes: 100 * GIB,
                    free_bytes: 50 * GIB,
                    unallocated_bytes: None,
                },
                ..Default::default()
            },
            labels: BTreeMap::new(),
            load: Load::default(),
        }
    }

    fn with(mut c: Candidate, f: impl FnOnce(&mut Candidate)) -> Candidate {
        f(&mut c);
        c
    }

    fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn headroom_counts_the_reserve_always_on_apps_and_a_share_of_idle_ones() {
        let s = Settings::default();
        let cases = [
            // (memory, always-on, idle, headroom)
            (4 * GIB, 0, 0, i128::from(4 * GIB - 512 * MIB)),
            (4 * GIB, GIB, 0, i128::from(3 * GIB - 512 * MIB)),
            (4 * GIB, 0, GIB, i128::from(4 * GIB - 512 * MIB - 512 * MIB)),
            (GIB, GIB, 0, -i128::from(512 * MIB)),
        ];
        for (memory, always_on, idle, want) in cases {
            let load = Load {
                always_on,
                idle,
                apps: 1,
            };
            assert_eq!(headroom(memory, load, &s), want, "{memory} {always_on} {idle}");
        }
        let all = Settings {
            memory_reserve: 0,
            idle_factor: 1.0,
        };
        let load = Load {
            always_on: GIB,
            idle: GIB,
            apps: 2,
        };
        assert_eq!(headroom(4 * GIB, load, &all), i128::from(2 * GIB));
        assert_eq!(
            Load::default().with(GIB, true).with(MIB, false),
            Load {
                always_on: MIB,
                idle: GIB,
                apps: 2
            }
        );
    }

    #[test]
    fn the_filter_says_why_a_node_doesnt_qualify() {
        let base = node("a", 4 * GIB);
        let needs = Needs::default();
        let cases: Vec<(&str, Candidate, Needs, bool, Option<&str>)> = vec![
            ("anything goes", base.clone(), needs.clone(), true, None),
            (
                "offline",
                with(base.clone(), |c| c.online = false),
                needs.clone(),
                true,
                Some("offline"),
            ),
            (
                "offline, but --queue",
                with(base.clone(), |c| c.online = false),
                needs.clone(),
                false,
                None,
            ),
            (
                "cordoned",
                with(base.clone(), |c| c.cordoned = true),
                needs.clone(),
                false,
                Some("cordoned"),
            ),
            (
                "the wrong arch",
                base.clone(),
                Needs {
                    arch: Some("arm64".into()),
                    ..Default::default()
                },
                true,
                Some("arch `amd64`, not `arm64`"),
            ),
            (
                "an image built for other architectures",
                base.clone(),
                Needs {
                    images: vec![crate::registry::Platforms {
                        image: "r.example.com/app:1".into(),
                        archs: vec!["arm64".into(), "riscv64".into()],
                    }],
                    ..Default::default()
                },
                true,
                Some("arch `amd64`, and image `r.example.com/app:1` is only built for arm64, riscv64"),
            ),
            (
                "an image built for this one",
                base.clone(),
                Needs {
                    images: vec![crate::registry::Platforms {
                        image: "r.example.com/app:1".into(),
                        archs: vec!["amd64".into(), "arm64".into()],
                    }],
                    ..Default::default()
                },
                true,
                None,
            ),
            (
                "the right arch",
                base.clone(),
                Needs {
                    arch: Some("amd64".into()),
                    ..Default::default()
                },
                true,
                None,
            ),
            (
                "a missing label",
                with(base.clone(), |c| c.labels = labels(&[("disk", "hdd"), ("gpu", "yes")])),
                Needs {
                    labels: labels(&[("disk", "ssd"), ("gpu", "yes")]),
                    ..Default::default()
                },
                true,
                Some("no label disk=ssd"),
            ),
            (
                "every label",
                with(base.clone(), |c| c.labels = labels(&[("disk", "ssd"), ("gpu", "yes")])),
                Needs {
                    labels: labels(&[("disk", "ssd")]),
                    ..Default::default()
                },
                true,
                None,
            ),
            (
                "a host path not allowed",
                with(base.clone(), |c| c.facts.allowed_host_paths = vec!["/srv/media".into()]),
                Needs {
                    host_paths: vec!["/srv/media/films".into(), "/etc".into()],
                    ..Default::default()
                },
                true,
                Some("host path `/etc` not allowed (see `--allow-host-path`)"),
            ),
            (
                "host paths allowed",
                with(base.clone(), |c| c.facts.allowed_host_paths = vec!["/srv".into()]),
                Needs {
                    host_paths: vec!["/srv/media".into()],
                    ..Default::default()
                },
                true,
                None,
            ),
            (
                "a path that only shares a prefix",
                with(base.clone(), |c| c.facts.allowed_host_paths = vec!["/srv/media".into()]),
                Needs {
                    host_paths: vec!["/srv/mediax".into()],
                    ..Default::default()
                },
                true,
                Some("host path `/srv/mediax` not allowed (see `--allow-host-path`)"),
            ),
        ];
        for (what, c, needs, online, want) in cases {
            assert_eq!(unfit(&c, &needs, online).as_deref(), want, "{what}");
        }
    }

    #[test]
    fn the_score_picks_the_most_headroom_then_btrfs_space_then_fewer_apps_then_the_name() {
        let s = Settings::default();
        let busy = Load {
            always_on: GIB,
            idle: 0,
            apps: 1,
        };
        type Case = (&'static str, Vec<Candidate>, &'static str, &'static str);
        let cases: Vec<Case> = vec![
            (
                "more memory",
                vec![node("a", 4 * GIB), node("b", 8 * GIB)],
                "b",
                "the most memory headroom",
            ),
            (
                "less placed on it",
                vec![
                    with(node("a", 8 * GIB), |c| {
                        c.load = Load {
                            always_on: 6 * GIB,
                            ..busy
                        }
                    }),
                    node("b", 4 * GIB),
                ],
                "b",
                "the most memory headroom",
            ),
            (
                "idle apps count for half",
                vec![
                    with(node("a", 4 * GIB), |c| {
                        c.load = Load {
                            always_on: 0,
                            idle: 2 * GIB,
                            apps: 4,
                        }
                    }),
                    with(node("b", 4 * GIB), |c| {
                        c.load = Load {
                            always_on: 1536 * MIB,
                            idle: 0,
                            apps: 1,
                        }
                    }),
                ],
                "a",
                "the most memory headroom",
            ),
            (
                "a tie on memory goes to btrfs space",
                vec![
                    node("a", 4 * GIB),
                    with(node("b", 4 * GIB), |c| c.facts.volumes.free_bytes = 60 * GIB),
                ],
                "b",
                "more free btrfs space",
            ),
            (
                "then to fewer apps",
                vec![
                    with(node("a", 4 * GIB), |c| {
                        c.load = Load {
                            always_on: 0,
                            idle: 0,
                            apps: 3,
                        }
                    }),
                    with(node("b", 4 * GIB), |c| {
                        c.load = Load {
                            always_on: 0,
                            idle: 0,
                            apps: 1,
                        }
                    }),
                ],
                "b",
                "fewer apps (1, against 3)",
            ),
            (
                "then to the name",
                vec![node("b", 4 * GIB), node("a", 4 * GIB)],
                "a",
                "first by name",
            ),
            (
                "cordoned and offline nodes don't count, however roomy",
                vec![
                    with(node("a", 64 * GIB), |c| c.cordoned = true),
                    with(node("b", 64 * GIB), |c| c.online = false),
                    node("c", 2 * GIB),
                ],
                "c",
                "the only node that qualified (1.5 GiB memory headroom); not eligible: a (cordoned), b (offline)",
            ),
            (
                "a full node is still chosen if it's the best there is",
                vec![with(node("a", GIB), |c| c.load = busy)],
                "a",
                "the only node that qualified (-512 MiB memory headroom)",
            ),
        ];
        for (what, nodes, want, why) in cases {
            let c = place(&nodes, &Needs::default(), &s).unwrap_or_else(|e| panic!("{what}: {e:?}"));
            assert_eq!(c.name, want, "{what}: {}", c.why);
            assert!(c.why.contains(why), "{what}: {}", c.why);
        }
    }

    #[test]
    fn placement_respects_the_filter() {
        let s = Settings::default();
        let amd = node("big", 64 * GIB);
        let arm = with(node("pi", 4 * GIB), |c| c.facts.arch = "arm64".into());
        let arm64 = Needs {
            arch: Some("arm64".into()),
            ..Default::default()
        };
        // An arm64 app never lands on the amd64 node, however much room it has.
        assert_eq!(place(&[amd.clone(), arm.clone()], &arm64, &s).unwrap().name, "pi");
        let e = place(std::slice::from_ref(&amd), &arm64, &s).unwrap_err();
        assert_eq!(
            e.describe("x"),
            "no node can take new nest `x`: big (arch `amd64`, not `arm64`)"
        );
        assert!(!e.all_offline());
        let ssd = Needs {
            labels: labels(&[("disk", "ssd")]),
            ..Default::default()
        };
        let labelled = with(node("small", GIB), |c| c.labels = labels(&[("disk", "ssd")]));
        assert_eq!(place(&[amd.clone(), labelled], &ssd, &s).unwrap().name, "small");
        let e = place(&[with(amd.clone(), |c| c.online = false)], &Needs::default(), &s).unwrap_err();
        assert!(e.all_offline());
        assert!(
            place(&[], &Needs::default(), &s)
                .unwrap_err()
                .describe("x")
                .contains("no node has joined")
        );
    }

    #[test]
    fn sizes_read_well() {
        assert_eq!(bytes(i128::from(3 * GIB / 2)), "1.5 GiB");
        assert_eq!(bytes(i128::from(300 * MIB)), "300 MiB");
        assert_eq!(bytes(-i128::from(300 * MIB)), "-300 MiB");
        assert_eq!(bytes(-i128::from(2 * GIB)), "-2.0 GiB");
    }
}

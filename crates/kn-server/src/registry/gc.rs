//! Deciding which of the registry's tags to delete. [`plan`] is pure, so its rules are tested with tables.
//!
//! The registry's tags are grouped by the manifest they name, in each repository, since deleting a manifest
//! deletes every tag that names it. A group is kept if any of its tags:
//!
//! - is used by an app deployed on any node (or being deleted from one, as it's still there), by tag or by the
//!   group's digest; or
//! - was pushed, released (an app that used it was redeployed with other images, or deleted) or first seen at
//!   that digest within the grace period.
//!
//! So a tag that shares its manifest with a tag that's kept is kept too. Every other group's manifest is deleted.

use std::collections::{BTreeMap, HashMap};

use kn_proto::api::GcTag;

use super::refs::ImageRef;
use crate::store::registry::Listed;

/// An app that uses an image in the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Use {
    pub app: String,
    pub node: String,
    pub image: ImageRef,
}

/// Something that happened to a tag (or a digest) recently: it was pushed, released or first seen, at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub what: &'static str,
    pub at: u64,
}

/// What the plan is made from.
#[derive(Debug, Clone, Default)]
pub struct Inputs {
    pub tags: Vec<Listed>,
    pub uses: Vec<Use>,
    /// The latest event of each (repository, tag or digest).
    pub events: HashMap<(String, String), Event>,
    pub now: u64,
    pub grace: u64,
}

/// What to delete and what to keep.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    /// The manifests to delete: (repository, digest).
    pub delete: Vec<(String, String)>,
    /// The tags that go with them.
    pub deleted: Vec<GcTag>,
    pub kept: Vec<GcTag>,
}

/// A time in the past, for people: `just now`, `5m ago`, `3h ago`, `2d ago`.
pub fn ago(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", secs / 60),
        3600..172_800 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

impl Inputs {
    /// Adds an event for a key, keeping the latest.
    pub fn event(&mut self, repository: &str, reference: &str, what: &'static str, at: u64) {
        let e = self
            .events
            .entry((repository.to_string(), reference.to_string()))
            .or_insert(Event { what, at });
        if at > e.at {
            *e = Event { what, at };
        }
    }

    /// Why `reference` (a tag or digest) of `repository` is kept on its own account, if it is.
    fn reason(&self, repository: &str, reference: &str, by_digest: bool) -> Option<String> {
        let used: Vec<&Use> = self
            .uses
            .iter()
            .filter(|u| {
                u.image.repository == repository
                    && if by_digest {
                        u.image.digest.as_deref() == Some(reference)
                    } else {
                        u.image.tag.as_deref() == Some(reference) && u.image.digest.is_none()
                    }
            })
            .collect();
        if let Some(u) = used.first() {
            let more = match used.len() {
                1 => String::new(),
                n => format!(" (and {} more)", n - 1),
            };
            let how = if by_digest { ", by digest" } else { "" };
            return Some(format!("used by app {} on node {}{more}{how}", u.app, u.node));
        }
        let e = self.events.get(&(repository.to_string(), reference.to_string()))?;
        (e.at + self.grace > self.now).then(|| format!("{} {}", e.what, ago(self.now.saturating_sub(e.at))))
    }
}

pub fn plan(inputs: &Inputs) -> Plan {
    let mut groups: BTreeMap<(&str, &str), Vec<&Listed>> = BTreeMap::new();
    for t in &inputs.tags {
        groups
            .entry((t.repository.as_str(), t.digest.as_str()))
            .or_default()
            .push(t);
    }
    let mut plan = Plan::default();
    for ((repo, digest), tags) in groups {
        let own: Vec<Option<String>> = tags.iter().map(|t| inputs.reason(repo, &t.tag, false)).collect();
        let by_digest = inputs.reason(repo, digest, true);
        let first_kept = tags.iter().zip(&own).find_map(|(t, r)| r.as_ref().map(|r| (t, r)));
        let group_reason = match (&by_digest, first_kept) {
            (Some(r), _) => Some(r.clone()),
            (None, Some((t, r))) => Some(format!("shares its manifest with {repo}:{}, which is {r}", t.tag)),
            (None, None) => None,
        };
        let tag = |t: &Listed, why: String| GcTag {
            repository: t.repository.clone(),
            tag: t.tag.clone(),
            digest: t.digest.clone(),
            why,
        };
        match group_reason {
            Some(shared) => {
                for (t, r) in tags.iter().zip(own) {
                    plan.kept.push(tag(t, r.unwrap_or_else(|| shared.clone())));
                }
            }
            None => {
                plan.delete.push((repo.to_string(), digest.to_string()));
                for t in tags {
                    plan.deleted.push(tag(t, String::new()));
                }
            }
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: u64 = 3600;
    const NOW: u64 = 1_000_000;

    fn d(n: u8) -> String {
        format!("sha256:{}", format!("{n:02x}").repeat(32))
    }

    fn listed(repo: &str, tag: &str, digest: u8) -> Listed {
        Listed {
            repository: repo.into(),
            tag: tag.into(),
            digest: d(digest),
        }
    }

    fn used(app: &str, node: &str, image: &str) -> Use {
        Use {
            app: app.into(),
            node: node.into(),
            image: ImageRef::parse(image).unwrap(),
        }
    }

    fn inputs(tags: Vec<Listed>, uses: Vec<Use>) -> Inputs {
        Inputs {
            tags,
            uses,
            events: HashMap::new(),
            now: NOW,
            grace: 24 * HOUR,
        }
    }

    fn deleted(p: &Plan) -> Vec<String> {
        p.deleted
            .iter()
            .map(|t| format!("{}:{}", t.repository, t.tag))
            .collect()
    }

    fn why(p: &Plan, repo: &str, tag: &str) -> String {
        p.kept
            .iter()
            .find(|t| t.repository == repo && t.tag == tag)
            .map(|t| t.why.clone())
            .unwrap_or_else(|| panic!("{repo}:{tag} should be kept: {p:?}"))
    }

    #[test]
    fn keeps_tags_used_on_any_node() {
        let p = plan(&inputs(
            vec![listed("blog", "1", 1), listed("blog", "2", 2), listed("blog", "3", 3)],
            vec![
                used("blog", "a", "r.example.com/blog:1"),
                used("blog-pr4", "b", "r.example.com/blog:2"),
            ],
        ));
        assert_eq!(deleted(&p), vec!["blog:3"]);
        assert_eq!(p.delete, vec![("blog".to_string(), d(3))]);
        assert_eq!(why(&p, "blog", "1"), "used by app blog on node a");
        // Used only on node b: kept all the same.
        assert_eq!(why(&p, "blog", "2"), "used by app blog-pr4 on node b");
    }

    #[test]
    fn keeps_tags_sharing_a_manifest_with_one_kept() {
        let p = plan(&inputs(
            vec![
                listed("blog", "1", 1),
                listed("blog", "latest", 1),
                listed("blog", "old", 2),
                listed("blog", "older", 2),
                // The same digest in another repository is another manifest.
                listed("other", "x", 1),
            ],
            vec![used("blog", "b", "r.example.com/blog:1")],
        ));
        assert_eq!(
            why(&p, "blog", "latest"),
            "shares its manifest with blog:1, which is used by app blog on node b"
        );
        let mut gone = deleted(&p);
        gone.sort();
        assert_eq!(gone, vec!["blog:old", "blog:older", "other:x"]);
        assert_eq!(p.delete.len(), 2, "one delete per manifest: {:?}", p.delete);
    }

    #[test]
    fn keeps_what_apps_use_by_digest() {
        let p = plan(&inputs(
            vec![listed("blog", "1", 1), listed("blog", "2", 2)],
            vec![used("blog", "a", &format!("r.example.com/blog@{}", d(1)))],
        ));
        assert_eq!(deleted(&p), vec!["blog:2"]);
        assert_eq!(why(&p, "blog", "1"), "used by app blog on node a, by digest");
        // `name:tag@digest` uses the digest; the tag doesn't count on its own.
        let p = plan(&inputs(
            vec![listed("blog", "1", 1), listed("blog", "2", 2)],
            vec![used("blog", "a", &format!("r.example.com/blog:2@{}", d(1)))],
        ));
        assert_eq!(deleted(&p), vec!["blog:2"]);
    }

    #[test]
    fn keeps_tags_pushed_released_or_first_seen_within_the_grace_period() {
        let mut i = inputs(
            vec![
                listed("app", "pushed", 1),
                listed("app", "released", 2),
                listed("app", "seen", 3),
                listed("app", "stale", 4),
                listed("app", "by-digest", 5),
            ],
            vec![],
        );
        i.event("app", "pushed", "pushed", NOW - HOUR);
        i.event("app", "released", "pushed", NOW - 100 * HOUR);
        i.event("app", "released", "released", NOW - 2 * HOUR);
        i.event("app", "seen", "first seen", NOW - 30);
        i.event("app", "stale", "pushed", NOW - 25 * HOUR);
        i.event("app", &d(5), "released", NOW - 23 * HOUR);
        let p = plan(&i);
        assert_eq!(deleted(&p), vec!["app:stale"]);
        assert_eq!(why(&p, "app", "pushed"), "pushed 1h ago");
        assert_eq!(why(&p, "app", "released"), "released 2h ago");
        assert_eq!(why(&p, "app", "seen"), "first seen just now");
        assert_eq!(why(&p, "app", "by-digest"), "released 23h ago");
        // With no grace period, only what's used is kept.
        i.grace = 0;
        assert_eq!(plan(&i).deleted.len(), 5);
    }

    #[test]
    fn times_read_for_people() {
        assert_eq!(ago(0), "just now");
        assert_eq!(ago(59), "just now");
        assert_eq!(ago(61), "1m ago");
        assert_eq!(ago(3 * HOUR), "3h ago");
        assert_eq!(ago(47 * HOUR), "47h ago");
        assert_eq!(ago(72 * HOUR), "3d ago");
    }
}

//! Backup keys, chains and retention.
//!
//! Every backup is one object, and its key says what it is, so a target's listing is all it takes to work out its
//! chains, and a key alone is enough to restore from:
//!
//! ```text
//! apps/APP/STAMP.full.send.zst            a full `send` backup of APP's volumes
//! apps/APP/STAMP.after-PARENT.send.zst    an incremental one, against the backup taken at PARENT
//! apps/APP/STAMP.tar.zst                  a tar archive
//! volumes/NAME/…                          the same for shared volumes
//! server/STAMP.tar.zst                    the server's own state
//! ```
//!
//! `STAMP` is when the backup's snapshot was taken, in UTC (`20261106T030000Z`), so keys sort in time order. A
//! target's `prefix` goes before all of it.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;

use kn_proto::backup::{BackupInfo, Format, Keep, Kind};

/// When a backup's snapshot was taken, in Unix seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Stamp(pub u64);

/// Days since 1970-01-01 to (year, month, day), in the proleptic Gregorian calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// (year, month, day) to days since 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

impl Stamp {
    pub fn now() -> Stamp {
        Stamp(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
        )
    }

    /// Days since the epoch.
    pub fn day(self) -> i64 {
        (self.0 / 86_400) as i64
    }

    /// Weeks since the epoch, starting on Mondays.
    pub fn week(self) -> i64 {
        // 1970-01-01 was a Thursday.
        (self.day() + 3).div_euclid(7)
    }

    /// Months since the epoch.
    pub fn month(self) -> i64 {
        let (y, m, _) = civil_from_days(self.day());
        y * 12 + i64::from(m)
    }

    /// RFC 3339, such as `2026-11-06T03:00:00Z`.
    pub fn rfc3339(self) -> String {
        humantime::format_rfc3339_seconds(std::time::UNIX_EPOCH + std::time::Duration::from_secs(self.0)).to_string()
    }

    pub fn parse(s: &str) -> Option<Stamp> {
        let b = s.as_bytes();
        if b.len() != 16 || b[8] != b'T' || b[15] != b'Z' {
            return None;
        }
        let digits = |r: std::ops::Range<usize>| -> Option<u32> {
            let t = &s[r];
            t.bytes().all(|c| c.is_ascii_digit()).then(|| t.parse().ok()).flatten()
        };
        let (y, mo, d) = (digits(0..4)?, digits(4..6)?, digits(6..8)?);
        let (h, mi, se) = (digits(9..11)?, digits(11..13)?, digits(13..15)?);
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || se > 59 || y < 1970 {
            return None;
        }
        let days = days_from_civil(i64::from(y), mo, d);
        if civil_from_days(days) != (i64::from(y), mo, d) {
            return None;
        }
        Some(Stamp(
            days as u64 * 86_400 + u64::from(h) * 3600 + u64::from(mi) * 60 + u64::from(se),
        ))
    }
}

impl fmt::Display for Stamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (y, m, d) = civil_from_days(self.day());
        let s = self.0 % 86_400;
        write!(f, "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z", s / 3600, s / 60 % 60, s % 60)
    }
}

/// Whether `name` can be an app's or a volume's name in a key: letters, digits, `.`, `_` and `-`, starting with a
/// letter or digit, at most 63 bytes. (App and volume names are narrower still.)
pub fn valid_name(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

/// What a backup is, from its key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub kind: Kind,
    /// The app or volume; empty for the server.
    pub name: String,
    pub stamp: Stamp,
    /// For an incremental `send` backup, when the backup it follows was taken.
    pub parent: Option<Stamp>,
    pub format: Format,
}

fn kind_dir(kind: Kind) -> &'static str {
    match kind {
        Kind::App => "apps",
        Kind::Volume => "volumes",
        Kind::Server => "server",
    }
}

/// The key prefix of everything backed up of one app, volume or the server: `apps/blog/`.
pub fn scope(kind: Kind, name: &str) -> String {
    match kind {
        Kind::Server => "server/".into(),
        _ => format!("{}/{name}/", kind_dir(kind)),
    }
}

impl Key {
    pub fn new(kind: Kind, name: &str, stamp: Stamp, parent: Option<Stamp>, format: Format) -> Key {
        Key {
            kind,
            name: if kind == Kind::Server {
                String::new()
            } else {
                name.into()
            },
            stamp,
            parent: if format == Format::Send { parent } else { None },
            format,
        }
    }

    /// Parses a key, without any target prefix. Refuses anything kiwi-nest wouldn't have made.
    pub fn parse(key: &str) -> Option<Key> {
        let format = if key.ends_with(Format::Send.suffix()) {
            Format::Send
        } else if key.ends_with(Format::Tar.suffix()) {
            Format::Tar
        } else {
            return None;
        };
        let rest = &key[..key.len() - format.suffix().len()];
        let (kind, name, file) = match rest.split('/').collect::<Vec<_>>()[..] {
            ["server", file] => (Kind::Server, "", file),
            ["apps", name, file] => (Kind::App, name, file),
            ["volumes", name, file] => (Kind::Volume, name, file),
            _ => return None,
        };
        if kind != Kind::Server && !valid_name(name) {
            return None;
        }
        let (stamp, parent) = match (format, file.split_once('.')) {
            (Format::Tar, None) => (Stamp::parse(file)?, None),
            (Format::Send, Some((s, "full"))) => (Stamp::parse(s)?, None),
            (Format::Send, Some((s, p))) => {
                let parent = Stamp::parse(p.strip_prefix("after-")?)?;
                let stamp = Stamp::parse(s)?;
                if parent >= stamp {
                    return None;
                }
                (stamp, Some(parent))
            }
            _ => return None,
        };
        if kind == Kind::Server && format != Format::Tar {
            return None;
        }
        Some(Key {
            kind,
            name: name.into(),
            stamp,
            parent,
            format,
        })
    }

    /// Whether it's a full backup (a tar archive always is).
    pub fn is_full(&self) -> bool {
        self.parent.is_none()
    }

    pub fn info(&self, target: &str, size: Option<u64>) -> BackupInfo {
        BackupInfo {
            target: target.into(),
            key: self.to_string(),
            kind: self.kind,
            name: self.name.clone(),
            format: self.format,
            taken_at: self.stamp.rfc3339(),
            parent: self.parent.map(Stamp::rfc3339),
            size,
        }
    }
}

impl fmt::Display for Key {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let part = match (self.format, self.parent) {
            (Format::Tar, _) => String::new(),
            (Format::Send, None) => ".full".into(),
            (Format::Send, Some(p)) => format!(".after-{p}"),
        };
        write!(
            f,
            "{}{}{part}{}",
            scope(self.kind, &self.name),
            self.stamp,
            self.format.suffix()
        )
    }
}

/// One app's, volume's or the server's `send` backups in a target, by stamp. (Two keys with one stamp can't both
/// be kiwi-nest's; the first is kept.)
fn by_stamp(keys: &[Key]) -> BTreeMap<Stamp, &Key> {
    let mut m = BTreeMap::new();
    for k in keys.iter().filter(|k| k.format == Format::Send) {
        m.entry(k.stamp).or_insert(k);
    }
    m
}

/// The chain that restoring `key` needs, full backup first, from a listing of its app's or volume's backups.
pub fn chain(listing: &[Key], key: &Key) -> Result<Vec<Key>, String> {
    if key.format == Format::Tar {
        return Ok(vec![key.clone()]);
    }
    let all = by_stamp(listing);
    let mut out = vec![key.clone()];
    let mut at = key.parent;
    while let Some(p) = at {
        let Some(k) = all.get(&p) else {
            return Err(format!(
                "`{key}` needs the backup taken at {p} ({}), which isn't in the target any more",
                p.rfc3339()
            ));
        };
        out.push((*k).clone());
        at = k.parent;
        if out.len() > 10_000 {
            return Err("the chain of backups doesn't end".into());
        }
    }
    out.reverse();
    Ok(out)
}

/// Whether the backup at `stamp` and every backup it needs are in the listing; with the stamp of its chain's full
/// backup.
fn chain_start(all: &BTreeMap<Stamp, &Key>, stamp: Stamp) -> Option<Stamp> {
    let mut at = all.get(&stamp)?;
    for _ in 0..10_000 {
        match at.parent {
            None => return Some(at.stamp),
            Some(p) => at = all.get(&p)?,
        }
    }
    None
}

/// The parent for the next `send` backup to a target: the newest backup that's both kept on the node (`local`) and
/// in the target with its whole chain, unless that chain's full backup is older than `full_every` seconds at `now`.
/// `None` means a full backup.
pub fn choose_parent(listing: &[Key], local: &BTreeSet<Stamp>, now: Stamp, full_every: u64) -> Option<Stamp> {
    let all = by_stamp(listing);
    let newest = local.iter().rev().find(|s| all.contains_key(s))?;
    let start = chain_start(&all, *newest)?;
    (now.0.saturating_sub(start.0) < full_every).then_some(*newest)
}

/// Which of one app's, volume's or the server's backups in a target retention deletes: everything `keep` doesn't
/// keep, except what a kept backup needs (its chain). The newest is always kept. With `keep` empty, nothing.
pub fn prune(listing: &[Key], keep: &Keep) -> Vec<Key> {
    if keep.everything() || listing.is_empty() {
        return Vec::new();
    }
    let mut sorted: Vec<&Key> = listing.iter().collect();
    sorted.sort_by_key(|k| std::cmp::Reverse(k.stamp));
    sorted.dedup_by(|a, b| a == b);
    let mut kept: BTreeSet<Stamp> = BTreeSet::new();
    kept.insert(sorted[0].stamp);
    for k in sorted.iter().take(keep.last.unwrap_or(0) as usize) {
        kept.insert(k.stamp);
    }
    let mut buckets = |n: Option<u32>, bucket: fn(Stamp) -> i64| {
        let Some(n) = n else { return };
        let mut seen: Vec<i64> = Vec::new();
        for k in &sorted {
            let b = bucket(k.stamp);
            if seen.contains(&b) {
                continue;
            }
            if seen.len() >= n as usize {
                break;
            }
            seen.push(b);
            kept.insert(k.stamp);
        }
    };
    buckets(keep.daily, Stamp::day);
    buckets(keep.weekly, Stamp::week);
    buckets(keep.monthly, Stamp::month);
    // What a kept incremental backup needs, all the way to its full backup.
    let sends: HashMap<Stamp, &Key> = sorted
        .iter()
        .filter(|k| k.format == Format::Send)
        .map(|k| (k.stamp, *k))
        .collect();
    let mut todo: Vec<Stamp> = kept.iter().copied().collect();
    while let Some(s) = todo.pop() {
        for k in sorted.iter().filter(|k| k.stamp == s) {
            if let Some(p) = k.parent
                && sends.contains_key(&p)
                && kept.insert(p)
            {
                todo.push(p);
            }
        }
    }
    listing.iter().filter(|k| !kept.contains(&k.stamp)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Stamp {
        Stamp::parse(s).unwrap()
    }

    #[test]
    fn stamps_round_trip() {
        for s in [
            "19700101T000000Z",
            "20261106T030405Z",
            "20240229T235959Z",
            "21000301T120000Z",
        ] {
            assert_eq!(at(s).to_string(), s);
        }
        assert_eq!(at("19700102T000001Z").0, 86_401);
        assert_eq!(at("20261106T030405Z").rfc3339(), "2026-11-06T03:04:05Z");
        for bad in [
            "20230229T000000Z",
            "20261306T000000Z",
            "20261106T240000Z",
            "2026-11-06",
            "20261106T030405",
        ] {
            assert!(Stamp::parse(bad).is_none(), "{bad}");
        }
        // Weeks start on Monday: 2026-11-08 is a Sunday, 2026-11-09 a Monday.
        assert_eq!(at("20261108T230000Z").week(), at("20261102T000000Z").week());
        assert_ne!(at("20261108T230000Z").week(), at("20261109T000000Z").week());
        assert_eq!(at("20261130T000000Z").month() + 1, at("20261201T000000Z").month());
    }

    #[test]
    fn keys_round_trip_and_odd_ones_are_refused() {
        for k in [
            "apps/blog/20261106T030000Z.full.send.zst",
            "apps/blog/20261107T030000Z.after-20261106T030000Z.send.zst",
            "volumes/corpus.v2/20261106T030000Z.tar.zst",
            "server/20261106T030000Z.tar.zst",
        ] {
            assert_eq!(Key::parse(k).unwrap().to_string(), k);
        }
        let k = Key::parse("apps/blog/20261107T030000Z.after-20261106T030000Z.send.zst").unwrap();
        assert_eq!(
            (k.kind, k.name.as_str(), k.parent),
            (Kind::App, "blog", Some(at("20261106T030000Z")))
        );
        for bad in [
            "apps/blog/20261106T030000Z.send.zst",
            "apps/blog/20261106T030000Z.full.tar.zst",
            "apps/../20261106T030000Z.tar.zst",
            "apps/.x/20261106T030000Z.tar.zst",
            "apps/blog/x/20261106T030000Z.tar.zst",
            "apps/blog/20261106T030000Z.after-20261107T030000Z.send.zst",
            "server/20261106T030000Z.full.send.zst",
            "other/blog/20261106T030000Z.tar.zst",
            "apps/blog/20261106T030000Z.tar",
        ] {
            assert!(Key::parse(bad).is_none(), "{bad}");
        }
    }

    fn send(s: &str, parent: Option<&str>) -> Key {
        Key::new(Kind::App, "blog", at(s), parent.map(at), Format::Send)
    }

    #[test]
    fn chains_are_followed_to_their_full_backup() {
        let listing = vec![
            send("20261101T000000Z", None),
            send("20261102T000000Z", Some("20261101T000000Z")),
            send("20261103T000000Z", Some("20261102T000000Z")),
            send("20261105T000000Z", Some("20261104T000000Z")),
        ];
        let c = chain(&listing, &listing[2]).unwrap();
        assert_eq!(c, listing[..3]);
        let e = chain(&listing, &listing[3]).unwrap_err();
        assert!(e.contains("isn't in the target"), "{e}");
    }

    #[test]
    fn the_parent_is_the_newest_backup_on_both_sides_with_its_chain() {
        let listing = vec![
            send("20261101T000000Z", None),
            send("20261102T000000Z", Some("20261101T000000Z")),
            send("20261103T000000Z", Some("20261102T000000Z")),
        ];
        let local: BTreeSet<Stamp> = [at("20261102T000000Z"), at("20261103T000000Z"), at("20261104T000000Z")].into();
        let now = at("20261104T000000Z");
        let week = 7 * 86_400;
        // 4th isn't in the target; 3rd is, with its chain.
        assert_eq!(choose_parent(&listing, &local, now, week), Some(at("20261103T000000Z")));
        // A chain whose full backup is too old starts again.
        assert_eq!(choose_parent(&listing, &local, at("20261109T000000Z"), week), None);
        // Nothing in common, or a broken chain: full.
        assert_eq!(
            choose_parent(&listing, &[at("20261104T000000Z")].into(), now, week),
            None
        );
        let broken = vec![listing[1].clone(), listing[2].clone()];
        assert_eq!(choose_parent(&broken, &local, now, week), None);
    }

    #[test]
    fn retention_keeps_what_newer_backups_need() {
        // Daily backups for 20 days: a full one every 7 days, incremental ones between.
        let mut listing = Vec::new();
        let mut parent = None;
        for d in 1..=20u32 {
            let s = format!("202611{d:02}T030000Z");
            let p = if (d - 1) % 7 == 0 { None } else { parent.clone() };
            listing.push(send(&s, p.as_deref()));
            parent = Some(s);
        }
        let gone = prune(
            &listing,
            &Keep {
                last: Some(2),
                ..Keep::default()
            },
        );
        let gone: BTreeSet<Stamp> = gone.iter().map(|k| k.stamp).collect();
        // The last two (19th and 20th) need the chain from the 15th's full backup.
        for d in 15..=20 {
            assert!(!gone.contains(&at(&format!("202611{d:02}T030000Z"))), "day {d}");
        }
        assert_eq!(gone.len(), 14);
        // Nothing set: nothing deleted.
        assert!(prune(&listing, &Keep::default()).is_empty());
        // The newest is always kept.
        let tars: Vec<Key> = (1..=3)
            .map(|d| {
                Key::new(
                    Kind::Server,
                    "",
                    at(&format!("202611{d:02}T030000Z")),
                    None,
                    Format::Tar,
                )
            })
            .collect();
        let gone = prune(
            &tars,
            &Keep {
                last: Some(0),
                ..Keep::default()
            },
        );
        assert_eq!(gone, tars[..2]);
    }

    #[test]
    fn retention_by_day_week_and_month() {
        // Two tar backups a day, through November and December.
        let mut listing = Vec::new();
        for day in 0..61u64 {
            for h in [3u64, 15] {
                let s = Stamp(at("20261101T000000Z").0 + day * 86_400 + h * 3600);
                listing.push(Key::new(Kind::Volume, "corpus", s, None, Format::Tar));
            }
        }
        let keep = Keep {
            last: None,
            daily: Some(3),
            weekly: Some(2),
            monthly: Some(2),
        };
        let gone = prune(&listing, &keep);
        let kept: BTreeSet<String> = listing
            .iter()
            .filter(|k| !gone.contains(k))
            .map(|k| k.stamp.to_string())
            .collect();
        let want: BTreeSet<String> = [
            // The newest of each of the last three days (also this week's and this month's).
            "20261231T150000Z",
            "20261230T150000Z",
            "20261229T150000Z",
            // Last week's newest (Sunday 27 December).
            "20261227T150000Z",
            // November's newest.
            "20261130T150000Z",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(kept, want);
    }
}

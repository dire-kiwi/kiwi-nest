//! Release versions, and comparing them: `MAJOR.MINOR.PATCH`, optionally with a pre-release (`0.2.0-rc.1`) and
//! build metadata (`+…`, ignored), as in semantic versioning. A pre-release comes before its release, and
//! pre-releases compare identifier by identifier, numbers as numbers.
//!
//! Versions decide two things: the server refuses a CLI older than itself (it sends its version in its
//! `User-Agent`, [`cli_version`]), and the server upgrades nodes older than itself.

use std::cmp::Ordering;
use std::fmt;

/// A parsed release version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// The pre-release identifiers, such as `["rc", "1"]`; empty for a release.
    pub pre: Vec<String>,
}

impl Version {
    /// Parses `1.2.3`, `1.2.3-rc.1` or `v1.2.3`; build metadata (`+…`) is dropped.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').unwrap_or(s);
        let s = s.split_once('+').map_or(s, |(v, _)| v);
        let (core, pre) = match s.split_once('-') {
            Some((c, p)) => (c, Some(p)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let mut num = || -> Option<u64> {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse().ok()
        };
        let (major, minor, patch) = (num()?, num()?, num()?);
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            None => Vec::new(),
            Some(p) => {
                let ids: Vec<String> = p.split('.').map(str::to_string).collect();
                if ids
                    .iter()
                    .any(|i| i.is_empty() || !i.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'))
                {
                    return None;
                }
                ids
            }
        };
        Some(Version {
            major,
            minor,
            patch,
            pre,
        })
    }

    /// This build's version.
    pub fn current() -> Version {
        Version::parse(crate::VERSION).expect("the workspace's version is a valid version")
    }
}

fn cmp_ident(a: &str, b: &str) -> Ordering {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y),
        // Numbers come before words.
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => a.cmp(b),
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(&other.pre) {
                        let o = cmp_ident(a, b);
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if !self.pre.is_empty() {
            write!(f, "-{}", self.pre.join("."))?;
        }
        Ok(())
    }
}

/// The `User-Agent` product the CLI sends: `kiwi-nest/VERSION`.
pub const CLI_PRODUCT: &str = "kiwi-nest/";

/// The CLI's version, from a request's `User-Agent` (`kiwi-nest/VERSION`, maybe followed by more); `None` for any
/// other client (a browser, curl, Docker).
pub fn cli_version(user_agent: &str) -> Option<Version> {
    let rest = user_agent.trim().strip_prefix(CLI_PRODUCT)?;
    Version::parse(rest.split_whitespace().next()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("{s} parses"))
    }

    #[test]
    fn parses_and_prints() {
        assert_eq!(v("1.2.3").to_string(), "1.2.3");
        assert_eq!(v("v0.10.0").to_string(), "0.10.0");
        assert_eq!(v("1.2.3-rc.1+build.5").to_string(), "1.2.3-rc.1");
        for bad in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "1.x.3",
            "1.2.3-",
            "1.2.3-a..b",
            "-1.2.3",
            "1.2.3-ä",
        ] {
            assert!(Version::parse(bad).is_none(), "{bad}");
        }
        Version::current();
    }

    #[test]
    fn orders_like_releases() {
        let order = [
            "0.1.0",
            "0.1.1",
            "0.2.0-alpha",
            "0.2.0-alpha.1",
            "0.2.0-alpha.beta",
            "0.2.0-beta.2",
            "0.2.0-beta.11",
            "0.2.0-rc.1",
            "0.2.0",
            "0.10.0",
            "1.0.0",
        ];
        for w in order.windows(2) {
            assert!(v(w[0]) < v(w[1]), "{} < {}", w[0], w[1]);
        }
        assert_eq!(v("1.0.0+a").cmp(&v("1.0.0+b")), Ordering::Equal);
    }

    #[test]
    fn finds_the_clis_version() {
        assert_eq!(cli_version("kiwi-nest/0.3.1"), Some(v("0.3.1")));
        assert_eq!(cli_version("kiwi-nest/0.3.1 (linux)"), Some(v("0.3.1")));
        assert_eq!(cli_version("kiwi-nest-node/0.3.1"), None);
        assert_eq!(cli_version("curl/8.0"), None);
        assert_eq!(cli_version("kiwi-nest/garbage"), None);
    }
}

//! Image references, as bundles write them (`registry.example.com/blog-deps:3`), and the registry's request paths.

/// An image reference: `[HOST/]REPOSITORY[:TAG][@DIGEST]`, with `latest` as the tag when it names neither.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageRef {
    /// The registry's host, lowercased, with its port if it has one; `None` for Docker Hub's short names.
    pub host: Option<String>,
    pub repository: String,
    pub tag: Option<String>,
    pub digest: Option<String>,
}

impl ImageRef {
    /// Parses a reference, or `None` if it isn't one.
    pub fn parse(s: &str) -> Option<ImageRef> {
        let s = s.trim();
        let (name, digest) = match s.split_once('@') {
            Some((n, d)) if valid_digest(d) => (n, Some(d.to_string())),
            Some(_) => return None,
            None => (s, None),
        };
        let (host, path) = match name.split_once('/') {
            Some((h, rest)) if h.contains(['.', ':']) || h == "localhost" => (Some(h.to_ascii_lowercase()), rest),
            _ => (None, name),
        };
        // A tag is after the last `:` of the last component.
        let last = path.rfind('/').map_or(0, |i| i + 1);
        let (repository, tag) = match path[last..].find(':') {
            Some(i) => (&path[..last + i], Some(path[last + i + 1..].to_string())),
            None => (path, None),
        };
        if !valid_repository(repository) || tag.as_deref().is_some_and(|t| !valid_tag(t)) {
            return None;
        }
        let tag = match (&tag, &digest) {
            (None, None) => Some("latest".to_string()),
            _ => tag,
        };
        Some(ImageRef {
            host,
            repository: repository.to_string(),
            tag,
            digest,
        })
    }

    /// Whether it names an image in the registry at `host`.
    pub fn is_on(&self, host: &str) -> bool {
        self.host.as_deref().is_some_and(|h| same_host(h, host))
    }

    /// What to ask the registry for: the digest if it has one, else the tag.
    pub fn reference(&self) -> &str {
        self.digest.as_deref().or(self.tag.as_deref()).unwrap_or("latest")
    }
}

/// Whether two registry hosts are the same, a default HTTPS port aside.
fn same_host(a: &str, b: &str) -> bool {
    let strip = |h: &str| h.strip_suffix(":443").unwrap_or(h).to_ascii_lowercase();
    strip(a) == strip(b)
}

/// A repository name: lowercase components of letters and digits, joined by `.`, `_`, `__` or runs of `-`, separated
/// by `/`.
pub fn valid_repository(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= 255
        && r.split('/').all(|c| {
            let b = c.as_bytes();
            !b.is_empty()
                && b.iter()
                    .all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || matches!(x, b'.' | b'_' | b'-'))
                && b[0].is_ascii_alphanumeric()
                && b[b.len() - 1].is_ascii_alphanumeric()
                && !c.contains("..")
                && !c.contains("___")
        })
}

/// A tag: up to 128 letters, digits, `_`, `.` and `-`, not starting with `.` or `-`.
pub fn valid_tag(t: &str) -> bool {
    let b = t.as_bytes();
    !b.is_empty()
        && b.len() <= 128
        && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
        && b.iter()
            .all(|x| x.is_ascii_alphanumeric() || matches!(x, b'_' | b'.' | b'-'))
}

/// A content digest, such as `sha256:` and 64 hex digits.
pub fn valid_digest(d: &str) -> bool {
    match d.split_once(':') {
        Some(("sha256", h)) => h.len() == 64 && h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        Some(("sha512", h)) => h.len() == 128 && h.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)),
        _ => false,
    }
}

/// What a request to the registry's API is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `/v2/`, which clients ask first to find out how to log in.
    Base,
    /// `/v2/_catalog`.
    Catalog,
    /// `/v2/REPOSITORY/manifests/REFERENCE`.
    Manifest { repository: String, reference: String },
    /// Anything else under a repository: blobs, uploads, tags, referrers.
    Repository(String),
    /// A path the registry doesn't serve.
    Other,
}

/// Works out what a request's path (its query aside) is for.
pub fn target(uri: &str) -> Target {
    let path = uri.split(['?', '#']).next().unwrap_or("");
    let Some(rest) = path.strip_prefix("/v2/") else {
        return if path == "/v2" { Target::Base } else { Target::Other };
    };
    if rest.is_empty() {
        return Target::Base;
    }
    if rest == "_catalog" {
        return Target::Catalog;
    }
    let parts: Vec<&str> = rest.split('/').collect();
    let repo = |n: usize| -> Option<String> {
        let r = parts[..parts.len().checked_sub(n)?].join("/");
        valid_repository(&r).then_some(r)
    };
    let n = parts.len();
    let found = if n >= 3 && parts[n - 2] == "manifests" {
        repo(2).map(|repository| Target::Manifest {
            repository,
            reference: parts[n - 1].to_string(),
        })
    } else if n >= 4 && parts[n - 3] == "blobs" && parts[n - 2] == "uploads" {
        repo(3).map(Target::Repository)
    } else if n >= 3 && matches!(parts[n - 2], "blobs" | "tags" | "referrers") {
        repo(2).map(Target::Repository)
    } else {
        None
    };
    found.unwrap_or(Target::Other)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(host: Option<&str>, repo: &str, tag: Option<&str>, digest: Option<&str>) -> ImageRef {
        ImageRef {
            host: host.map(str::to_string),
            repository: repo.into(),
            tag: tag.map(str::to_string),
            digest: digest.map(str::to_string),
        }
    }

    #[test]
    fn parses_references() {
        let d = format!("sha256:{}", "ab".repeat(32));
        assert_eq!(
            ImageRef::parse("registry.example.com/blog-deps:3"),
            Some(r(Some("registry.example.com"), "blog-deps", Some("3"), None))
        );
        assert_eq!(
            ImageRef::parse("localhost:7443/team/app"),
            Some(r(Some("localhost:7443"), "team/app", Some("latest"), None))
        );
        assert_eq!(
            ImageRef::parse(&format!("Registry.Example.com/a@{d}")),
            Some(r(Some("registry.example.com"), "a", None, Some(&d)))
        );
        assert_eq!(
            ImageRef::parse(&format!("registry.example.com/a:1@{d}")),
            Some(r(Some("registry.example.com"), "a", Some("1"), Some(&d)))
        );
        assert_eq!(
            ImageRef::parse("python:3.13-alpine"),
            Some(r(None, "python", Some("3.13-alpine"), None))
        );
        assert_eq!(
            ImageRef::parse("library/python"),
            Some(r(None, "library/python", Some("latest"), None))
        );
        assert_eq!(ImageRef::parse("Upper/case"), None);
        assert_eq!(ImageRef::parse("a@sha256:short"), None);
        assert_eq!(ImageRef::parse("a:-bad"), None);
        assert!(
            ImageRef::parse("registry.example.com:443/a:1")
                .unwrap()
                .is_on("registry.example.com")
        );
        assert!(
            !ImageRef::parse("other.example.com/a:1")
                .unwrap()
                .is_on("registry.example.com")
        );
        assert!(!ImageRef::parse("a:1").unwrap().is_on("registry.example.com"));
        assert_eq!(ImageRef::parse(&format!("h.io/a:1@{d}")).unwrap().reference(), d);
    }

    #[test]
    fn works_out_request_targets() {
        assert_eq!(target("/v2/"), Target::Base);
        assert_eq!(target("/v2"), Target::Base);
        assert_eq!(target("/v2/_catalog?n=10"), Target::Catalog);
        assert_eq!(
            target("/v2/team/app/manifests/v1"),
            Target::Manifest {
                repository: "team/app".into(),
                reference: "v1".into()
            }
        );
        // A component may be called `manifests`.
        assert_eq!(
            target("/v2/a/manifests/manifests/v1"),
            Target::Manifest {
                repository: "a/manifests".into(),
                reference: "v1".into()
            }
        );
        assert_eq!(target("/v2/app/blobs/uploads/"), Target::Repository("app".into()));
        assert_eq!(
            target("/v2/app/blobs/uploads/abc-123?_state=x"),
            Target::Repository("app".into())
        );
        assert_eq!(target("/v2/app/blobs/sha256:00"), Target::Repository("app".into()));
        assert_eq!(target("/v2/app/tags/list"), Target::Repository("app".into()));
        assert_eq!(target("/v2/Bad/tags/list"), Target::Other);
        assert_eq!(target("/v1/whatever"), Target::Other);
        assert_eq!(target("/v2/app"), Target::Other);
    }
}

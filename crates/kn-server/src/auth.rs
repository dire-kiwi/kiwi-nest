//! Tokens: making them, hashing them, and what each kind may do.
//!
//! A token is a kind prefix and 32 random bytes in hex (`kna_…` admin, `knd_…` deploy, `knj_…` join, `knn_…`
//! node). Only its SHA-256 is stored; with that much randomness a plain hash is as good as a slow one, and it can be
//! looked up directly.
//!
//! - **Admin** tokens may do anything.
//! - **Deploy** tokens may deploy, delete, inspect, suspend, resume and read the logs of apps whose names match one
//!   of their patterns, where `*` matches any run of characters (`blog-pr*`). `--from` may name an app that matches
//!   a pattern or one of the token's `from` patterns, so a token for `blog-pr*` with `from: [blog]` can make
//!   previews of `blog` without being able to change it. They can't create, load or delete volumes, nor mount one
//!   read-write (`ro` and `snapshot` mounts can't change it), and they can't touch nodes or tokens.
//! - **Join** tokens are used once, by a node, to get its node token.
//! - **Node** tokens are a node's own, for the agent link only. `node forget` revokes one but keeps it, so the node
//!   is told it was forgotten (`410 Gone`) rather than that its token is wrong (`401`), which a token that was
//!   never issued gets.
//!
//! The web UI's users log in with a password, and get a session cookie ([`crate::ui`]); a request with one is an
//! admin's, as long as it passes [`crate::ui::check_cookie_request`] (from the server's own origin, and JSON if it
//! changes anything). A request with a token is judged by its token alone, cookie or not.
//!
//! Each node also has an ingress token (`kni_…`), given to it when it joins, which the ingress adds to every request
//! it passes to the node's gateway; see [`crate::ingress`].
//!
//! And a registry credential (`knr_…`, stored as a hash), which may only pull from the cluster's registry, where
//! admin and deploy tokens may also push; see [`crate::registry`].

use axum::extract::FromRequestParts;
use axum::http::header;
use axum::http::request::Parts;
use kn_proto::api::{TokenKind, WhoAmI};
use sha2::{Digest, Sha256};

use crate::Server;
use crate::error::{Error, Result};
use crate::store::nodes::TokenOwner;

/// Makes a new token of a kind.
pub fn generate(kind: TokenKind) -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the system's random number generator");
    let prefix = match kind {
        TokenKind::Admin => "kna",
        TokenKind::Deploy => "knd",
        TokenKind::Join => "knj",
        TokenKind::Node => "knn",
    };
    format!("{prefix}_{}", hex::encode(bytes))
}

/// Makes a node's ingress token (`kni_…`), which the ingress adds to its requests to the node's gateway. Unlike
/// the other tokens, it's stored as it is, since the ingress's configuration needs it.
pub fn generate_ingress() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("the system's random number generator");
    format!("kni_{}", hex::encode(bytes))
}

/// What's stored for a token.
pub fn hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

/// Whether a pattern is acceptable: what an app name may hold, plus `*`.
pub fn valid_pattern(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= 64
        && p.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'*')
}

/// Whether `name` matches `pattern`, where `*` matches any run of characters (including none).
pub fn matches(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.as_bytes(), name.as_bytes());
    // Greedy matching with backtracking to the last `*`.
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some((pi, ni));
            pi += 1;
        } else if pi < p.len() && p[pi] == n[ni] {
            pi += 1;
            ni += 1;
        } else if let Some((sp, sn)) = star {
            pi = sp + 1;
            ni = sn + 1;
            star = Some((sp, sn + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == b'*')
}

/// Whoever made a request to the CLI API: an admin or deploy token, or a web UI user's session (an admin).
#[derive(Debug, Clone)]
pub struct Caller {
    /// The token's ID; 0 for a session.
    pub id: u64,
    pub kind: TokenKind,
    pub name: String,
    pub patterns: Vec<String>,
    pub from: Vec<String>,
    /// The web UI's user, for a session.
    pub user: Option<String>,
}

impl Caller {
    /// Who this is, as a command's history shows it: `user NAME` or `token NAME` (`admin token 3` for one
    /// without a name).
    pub fn by(&self) -> String {
        match (&self.user, self.name.as_str()) {
            (Some(u), _) => format!("user {u}"),
            (None, "") => format!("{} token {}", self.kind, self.id),
            (None, n) => format!("token {n}"),
        }
    }

    pub fn is_admin(&self) -> bool {
        self.kind == TokenKind::Admin
    }

    /// Refuses unless the caller is an admin.
    pub fn admin(&self) -> Result<()> {
        if self.is_admin() {
            Ok(())
        } else {
            Err(Error::Forbidden("this needs an admin token".into()))
        }
    }

    /// Whether the caller may deploy, delete and manage the app `name`.
    pub fn may_manage(&self, name: &str) -> bool {
        self.is_admin() || self.patterns.iter().any(|p| matches(p, name))
    }

    /// Refuses unless the caller may manage `name`.
    pub fn manage(&self, name: &str) -> Result<()> {
        if self.may_manage(name) {
            Ok(())
        } else {
            Err(Error::Forbidden(format!(
                "this token can't manage app `{name}`: it's limited to {}",
                self.patterns.join(", ")
            )))
        }
    }

    /// Refuses unless the caller may name `from` as `--from`.
    pub fn may_use_from(&self, from: &str) -> Result<()> {
        if self.may_manage(from) || self.from.iter().any(|p| matches(p, from)) {
            Ok(())
        } else {
            Err(Error::Forbidden(format!(
                "this token can't make an app `--from` `{from}`"
            )))
        }
    }

    pub fn whoami(&self) -> WhoAmI {
        WhoAmI {
            kind: self.kind,
            name: self.name.clone(),
            user: self.user.clone(),
            patterns: self.patterns.clone(),
            from: self.from.clone(),
        }
    }
}

/// The bearer token of a request.
pub fn bearer(parts: &Parts) -> Result<&str> {
    let value = parts
        .headers
        .get(header::AUTHORIZATION)
        .ok_or_else(|| Error::Unauthorized("this needs a token: `Authorization: Bearer TOKEN`".into()))?;
    value
        .to_str()
        .ok()
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .ok_or_else(|| Error::Unauthorized("the Authorization header must be `Bearer TOKEN`".into()))
}

impl FromRequestParts<Server> for Caller {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, server: &Server) -> Result<Self> {
        if !parts.headers.contains_key(header::AUTHORIZATION)
            && let Some(session) = crate::ui::session(parts, server).await?
        {
            return Ok(Caller {
                id: 0,
                kind: TokenKind::Admin,
                name: String::new(),
                patterns: Vec::new(),
                from: Vec::new(),
                user: Some(session.user.name),
            });
        }
        let h = hash(bearer(parts)?);
        let token = server.store.call(move |c| crate::store::tokens::lookup(c, &h)).await?;
        match token {
            Some(t) if matches!(t.kind, TokenKind::Admin | TokenKind::Deploy) => Ok(Caller {
                id: t.id,
                kind: t.kind,
                name: t.name,
                patterns: t.patterns,
                from: t.from,
                user: None,
            }),
            Some(_) => Err(Error::Forbidden("this token is for nodes, not the CLI".into())),
            None => Err(Error::Unauthorized("the token isn't valid".into())),
        }
    }
}

/// A node making a request on the agent link, by its node token.
#[derive(Debug, Clone)]
pub struct NodeCaller {
    pub node_id: u64,
    pub name: String,
}

impl FromRequestParts<Server> for NodeCaller {
    type Rejection = Error;

    async fn from_request_parts(parts: &mut Parts, server: &Server) -> Result<Self> {
        let h = hash(bearer(parts)?);
        let found = server.store.call(move |c| crate::store::nodes::by_token(c, &h)).await?;
        match found {
            Some(TokenOwner::Node { id, name }) => Ok(NodeCaller { node_id: id, name }),
            Some(TokenOwner::Forgotten { id, name, at }) => Err(Error::Forgotten { node_id: id, name, at }),
            None => Err(Error::Unauthorized("the node token isn't valid".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_like_globs() {
        assert!(matches("blog-pr*", "blog-pr12"));
        assert!(matches("blog-pr*", "blog-pr"));
        assert!(!matches("blog-pr*", "blog"));
        assert!(!matches("blog-pr*", "xblog-pr1"));
        assert!(matches("blog", "blog"));
        assert!(!matches("blog", "blog-pr1"));
        assert!(matches("*", "anything"));
        assert!(matches("*-pr*", "shop-pr4"));
        assert!(!matches("*-pr*", "shop"));
        assert!(matches("a*b*c", "a-x-b-y-c"));
        assert!(!matches("a*b*c", "a-x-c-y-b"));
        assert!(matches("a**", "a"));
    }

    #[test]
    fn patterns_are_checked() {
        assert!(valid_pattern("blog-pr*"));
        assert!(valid_pattern("*"));
        assert!(!valid_pattern(""));
        assert!(!valid_pattern("Blog"));
        assert!(!valid_pattern("blog/pr"));
        assert!(!valid_pattern("blog?"));
    }

    #[test]
    fn tokens_are_random_and_hashed() {
        let a = generate(TokenKind::Deploy);
        let b = generate(TokenKind::Deploy);
        assert_ne!(a, b);
        assert!(a.starts_with("knd_") && a.len() == 4 + 64);
        assert_eq!(hash(&a), hash(&a));
        assert_ne!(hash(&a), a);
        assert_eq!(hash(&a).len(), 64);
    }

    #[test]
    fn deploy_tokens_are_limited_to_their_patterns() {
        let c = Caller {
            id: 1,
            kind: TokenKind::Deploy,
            name: "ci".into(),
            patterns: vec!["blog-pr*".into()],
            from: vec!["blog".into()],
            user: None,
        };
        assert!(c.manage("blog-pr3").is_ok());
        assert!(c.manage("blog").is_err());
        assert!(c.admin().is_err());
        assert!(c.may_use_from("blog").is_ok());
        assert!(c.may_use_from("blog-pr1").is_ok());
        assert!(c.may_use_from("shop").is_err());
    }
}

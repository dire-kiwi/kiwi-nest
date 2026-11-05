//! The registry's HTTP API, as the server uses it directly on loopback (where the registry needs no credentials):
//! listing repositories and tags, finding a tag's digest, deleting manifests, and reading an image's platforms.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::{StatusCode, header};
use serde::Deserialize;
use serde_json::Value;

/// Every kind of manifest the registry may hold: image indexes and lists, and single images.
pub const MANIFEST_TYPES: &str = "application/vnd.oci.image.index.v1+json, \
                                  application/vnd.docker.distribution.manifest.list.v2+json, \
                                  application/vnd.oci.image.manifest.v1+json, \
                                  application/vnd.docker.distribution.manifest.v2+json";

/// How many repositories or tags to ask for at once.
const PAGE: usize = 1000;

pub struct Client {
    http: reqwest::Client,
    base: String,
}

#[derive(Deserialize)]
struct Catalog {
    #[serde(default)]
    repositories: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Tags {
    #[serde(default)]
    tags: Option<Vec<String>>,
}

/// The `next` link of a paginated answer, if there's one, as a path and query.
fn next_link(resp: &reqwest::Response) -> Option<String> {
    let link = resp.headers().get(header::LINK)?.to_str().ok()?;
    let (target, rel) = link.split_once(';')?;
    if !rel.contains("rel=\"next\"") {
        return None;
    }
    let target = target.trim().trim_start_matches('<').trim_end_matches('>');
    // An absolute URL's path and query, or the path itself.
    Some(target[target.find("/v2/")?..].to_string())
}

impl Client {
    pub fn new(addr: SocketAddr) -> anyhow::Result<Client> {
        Client::with_base(format!("http://{addr}"))
    }

    pub fn with_base(base: String) -> anyhow::Result<Client> {
        let http = reqwest::Client::builder()
            .user_agent(format!("kiwi-nest-server/{}", kn_proto::VERSION))
            .no_proxy()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Client { http, base })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Whether the registry answers.
    pub async fn ping(&self) -> bool {
        matches!(self.http.get(self.url("/v2/")).send().await, Ok(r) if r.status().is_success())
    }

    async fn paged<T: for<'de> Deserialize<'de>>(&self, first: String, mut each: impl FnMut(T)) -> anyhow::Result<()> {
        let mut path = Some(first);
        let mut pages = 0;
        while let Some(p) = path.take() {
            let resp = self.http.get(self.url(&p)).send().await?;
            if resp.status() == StatusCode::NOT_FOUND {
                return Ok(());
            }
            if !resp.status().is_success() {
                bail!("the registry answered {} for {p}", resp.status());
            }
            path = next_link(&resp);
            each(resp.json().await.with_context(|| format!("reading {p}"))?);
            pages += 1;
            if pages > 10_000 {
                bail!("the registry's answer for {p} doesn't end");
            }
        }
        Ok(())
    }

    /// Every repository.
    pub async fn repositories(&self) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        self.paged(format!("/v2/_catalog?n={PAGE}"), |c: Catalog| {
            out.extend(c.repositories.unwrap_or_default())
        })
        .await?;
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// A repository's tags.
    pub async fn tags(&self, repository: &str) -> anyhow::Result<Vec<String>> {
        let mut out = Vec::new();
        self.paged(format!("/v2/{repository}/tags/list?n={PAGE}"), |t: Tags| {
            out.extend(t.tags.unwrap_or_default())
        })
        .await?;
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// The digest of the manifest a tag (or digest) names, or `None` if there's none.
    pub async fn digest(&self, repository: &str, reference: &str) -> anyhow::Result<Option<String>> {
        let resp = self
            .http
            .head(self.url(&format!("/v2/{repository}/manifests/{reference}")))
            .header(header::ACCEPT, MANIFEST_TYPES)
            .send()
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            bail!("the registry answered {} for {repository}:{reference}", resp.status());
        }
        let d = resp
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .with_context(|| format!("the registry gave no digest for {repository}:{reference}"))?;
        Ok(Some(d))
    }

    /// Deletes a manifest, and with it every tag that names it.
    pub async fn delete(&self, repository: &str, digest: &str) -> anyhow::Result<()> {
        let resp = self
            .http
            .delete(self.url(&format!("/v2/{repository}/manifests/{digest}")))
            .send()
            .await?;
        match resp.status() {
            s if s.is_success() || s == StatusCode::NOT_FOUND => Ok(()),
            s => bail!(
                "the registry answered {s} to deleting {repository}@{digest}: {}",
                resp.text().await.unwrap_or_default().trim()
            ),
        }
    }

    async fn get_json(&self, path: &str, accept: &str) -> anyhow::Result<Option<Value>> {
        let resp = self
            .http
            .get(self.url(path))
            .header(header::ACCEPT, accept)
            .send()
            .await?;
        if resp.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !resp.status().is_success() {
            bail!("the registry answered {} for {path}", resp.status());
        }
        Ok(Some(resp.json().await.with_context(|| format!("reading {path}"))?))
    }

    /// The Linux architectures an image is built for (`amd64`, `arm64`, …), or `None` if the registry doesn't
    /// hold it.
    pub async fn platforms(&self, repository: &str, reference: &str) -> anyhow::Result<Option<Vec<String>>> {
        let Some(m) = self
            .get_json(&format!("/v2/{repository}/manifests/{reference}"), MANIFEST_TYPES)
            .await?
        else {
            return Ok(None);
        };
        if let Some(list) = m["manifests"].as_array() {
            return Ok(Some(index_platforms(list)));
        }
        let Some(config) = m["config"]["digest"].as_str() else {
            bail!("{repository}:{reference} is a manifest without a config");
        };
        let Some(c) = self
            .get_json(&format!("/v2/{repository}/blobs/{config}"), "application/json, */*")
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(config_platform(&c).into_iter().collect()))
    }
}

/// The architectures of an image index's Linux images (attestations, listed as `unknown`, aside).
pub fn index_platforms(list: &[Value]) -> Vec<String> {
    let mut out: Vec<String> = list
        .iter()
        .filter_map(|m| {
            let p = &m["platform"];
            let os = p["os"].as_str().unwrap_or("linux");
            let arch = p["architecture"].as_str()?;
            (os == "linux" && arch != "unknown").then(|| arch.to_string())
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The architecture in an image's config, if it's a Linux one.
pub fn config_platform(c: &Value) -> Option<String> {
    let os = c["os"].as_str().unwrap_or("linux");
    let arch = c["architecture"].as_str()?;
    (os == "linux").then(|| arch.to_string())
}

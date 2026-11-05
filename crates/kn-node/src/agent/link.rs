//! The HTTP side of the agent link: the requests a node makes to the server.

use std::path::Path;
use std::time::Duration;

use futures_util::StreamExt;
use kn_proto::agent::{
    CommandResult, FORGOTTEN, ForgottenBody, IngressToken, JoinRequest, JoinResponse, PollRequest, PollResponse,
};
use kn_proto::api::ErrorBody;
use tokio::io::AsyncWriteExt;

/// Why a request to the server failed.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// The server couldn't be reached, or the connection broke.
    #[error("couldn't reach the server: {0}")]
    Network(#[from] reqwest::Error),
    /// The server answered with an error.
    #[error("the server answered {status}: {message}")]
    Status { status: u16, message: String },
    /// The server says this node was forgotten: `410 Gone` with a [`ForgottenBody`] whose error is [`FORGOTTEN`].
    /// Nothing else, not a network error nor a `401`, is ever taken for this.
    #[error("the server forgot node `{}` (node {})", .0.name, .0.node_id)]
    Forgotten(ForgottenBody),
    #[error("{0}")]
    Other(String),
}

impl LinkError {
    pub fn is_not_found(&self) -> bool {
        matches!(self, LinkError::Status { status: 404, .. })
    }
}

/// Installs ring as the process's TLS provider, once.
pub fn install_tls() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// A client for the server's agent routes.
#[derive(Clone)]
pub struct Link {
    http: reqwest::Client,
    /// The server's URL with the API prefix.
    base: String,
    token: String,
}

/// A client for the server, trusting `ca_certs` (PEM) as well as the system's roots.
fn client(ca_certs: Option<&[u8]>) -> Result<reqwest::Client, LinkError> {
    install_tls();
    let mut b = reqwest::Client::builder()
        .user_agent(format!("kiwi-nest-node/{}", kn_proto::VERSION))
        .connect_timeout(Duration::from_secs(10))
        .tcp_keepalive(Duration::from_secs(30));
    if let Some(pem) = ca_certs {
        let certs = reqwest::Certificate::from_pem_bundle(pem)?;
        if certs.is_empty() {
            return Err(LinkError::Other("the CA certificate file has no certificates".into()));
        }
        b = b.tls_certs_merge(certs);
    }
    Ok(b.build()?)
}

fn base(server: &str) -> Result<String, LinkError> {
    let s = server.trim_end_matches('/');
    if !s.starts_with("http://") && !s.starts_with("https://") {
        return Err(LinkError::Other(format!(
            "the server must be an http:// or https:// URL, not {s}"
        )));
    }
    Ok(format!("{s}{}", kn_proto::API_PREFIX))
}

async fn check(resp: reqwest::Response) -> Result<reqwest::Response, LinkError> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status().as_u16();
    let text = resp.text().await.unwrap_or_default();
    if status == 410
        && let Ok(body) = serde_json::from_str::<ForgottenBody>(&text)
        && body.error == FORGOTTEN
    {
        return Err(LinkError::Forgotten(body));
    }
    let message = match serde_json::from_str::<ErrorBody>(&text) {
        Ok(b) => b.error,
        Err(_) => text.trim().to_string(),
    };
    Err(LinkError::Status { status, message })
}

impl Link {
    pub fn new(server: &str, token: &str, ca_certs: Option<&[u8]>) -> Result<Link, LinkError> {
        Ok(Link {
            http: client(ca_certs)?,
            base: base(server)?,
            token: token.to_string(),
        })
    }

    /// Swaps a join token for a node token.
    pub async fn join(server: &str, ca_certs: Option<&[u8]>, req: &JoinRequest) -> Result<JoinResponse, LinkError> {
        let resp = client(ca_certs)?
            .post(format!("{}/agent/join", base(server)?))
            .json(req)
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }

    /// The node's ingress token, for a node that joined before the server gave them out.
    pub async fn ingress_token(&self) -> Result<String, LinkError> {
        let resp = self
            .http
            .get(format!("{}/agent/ingress-token", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        Ok(check(resp).await?.json::<IngressToken>().await?.ingress_token)
    }

    /// A new registry credential for this node, replacing its last one; `None` if the cluster has no registry.
    pub async fn registry_login(&self) -> Result<Option<kn_proto::agent::RegistryLogin>, LinkError> {
        let resp = self
            .http
            .post(format!("{}/agent/registry", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        Ok(check(resp)
            .await?
            .json::<kn_proto::agent::RegistryCredential>()
            .await?
            .registry)
    }

    pub async fn poll(&self, req: &PollRequest, wait: Duration) -> Result<PollResponse, LinkError> {
        let resp = self
            .http
            .post(format!("{}/agent/poll", self.base))
            .bearer_auth(&self.token)
            .timeout(wait + Duration::from_secs(30))
            .json(req)
            .send()
            .await?;
        Ok(check(resp).await?.json().await?)
    }

    /// Sends a report on its own, beside the open poll.
    pub async fn report(&self, report: &kn_proto::agent::Report) -> Result<(), LinkError> {
        let resp = self
            .http
            .post(format!("{}/agent/report", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(30))
            .json(report)
            .send()
            .await?;
        check(resp).await?;
        Ok(())
    }

    /// Downloads a bundle into `dest`, refusing more than `limit` bytes.
    pub async fn bundle(&self, digest: &str, dest: &Path, limit: u64) -> Result<(), LinkError> {
        let resp = self
            .http
            .get(format!("{}/agent/bundles/{digest}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await?;
        let resp = check(resp).await?;
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| LinkError::Other(format!("writing {}: {e}", dest.display())))?;
        let mut stream = resp.bytes_stream();
        let mut n = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            n += chunk.len() as u64;
            if n > limit {
                return Err(LinkError::Other(format!(
                    "bundle {digest} is larger than {limit} bytes"
                )));
            }
            file.write_all(&chunk)
                .await
                .map_err(|e| LinkError::Other(format!("writing {}: {e}", dest.display())))?;
        }
        file.flush()
            .await
            .map_err(|e| LinkError::Other(format!("writing {}: {e}", dest.display())))?;
        Ok(())
    }

    pub async fn result(&self, id: u64, result: &CommandResult) -> Result<(), LinkError> {
        let resp = self
            .http
            .post(format!("{}/agent/commands/{id}/result", self.base))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(60))
            .json(result)
            .send()
            .await?;
        check(resp).await?;
        Ok(())
    }

    /// Streams a command's output to the server; returns once the server has stopped taking it (the output ended,
    /// or nobody is reading it any more).
    pub async fn output(&self, id: u64, body: reqwest::Body) -> Result<(), LinkError> {
        let resp = self
            .http
            .post(format!("{}/agent/commands/{id}/output", self.base))
            .bearer_auth(&self.token)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .body(body)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::GONE {
            return Ok(());
        }
        check(resp).await?;
        Ok(())
    }
}

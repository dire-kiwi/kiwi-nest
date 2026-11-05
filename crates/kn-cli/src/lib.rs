//! The kiwi-nest CLI's commands.
//!
//! For now they talk to a standalone node's loopback API (`--node-api`); clusters come with the server.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use futures_util::StreamExt;
use kn_proto::api::{AppInfo, AppSummary, DeployDirectory, DeployQuery, ErrorBody, LogsQuery};

/// The standalone node API's default address.
pub const DEFAULT_NODE_API: &str = "http://127.0.0.1:7380";

/// Options for every client command.
#[derive(Debug, Clone, Args)]
pub struct GlobalArgs {
    /// The standalone node API to talk to.
    #[arg(long, global = true, env = "KIWI_NEST_NODE_API", default_value = DEFAULT_NODE_API, value_name = "URL")]
    pub node_api: String,
}

#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Deploy a bundle (a directory or a ZIP) as NAME, replacing the app if it exists.
    Deploy {
        name: String,
        /// The bundle: a directory with compose.yaml at its root, or a ZIP of one.
        path: PathBuf,
        /// Refuse if the app already exists.
        #[arg(long)]
        create_only: bool,
        /// Have the node read the directory itself, rather than uploading it (the node must be on this machine).
        #[arg(long)]
        node_reads_path: bool,
    },
    /// List the apps.
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show an app's details and containers.
    Inspect {
        name: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show an app's logs.
    Logs {
        name: String,
        /// Lines to show from the end of each container's log.
        #[arg(long, short = 'n')]
        tail: Option<u32>,
        /// Keep streaming new lines.
        #[arg(long, short)]
        follow: bool,
        /// Prefix each line with its timestamp.
        #[arg(long, short)]
        timestamps: bool,
        /// Only this service.
        #[arg(long, short)]
        service: Option<String>,
    },
    /// Delete an app: its containers, its bundle and its volumes.
    Delete { name: String },
}

/// A client for the node API.
pub struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    pub fn new(base: &str) -> anyhow::Result<Self> {
        let base = base.trim_end_matches('/').to_string();
        if !base.starts_with("http://") && !base.starts_with("https://") {
            bail!("the node API must be an http:// URL, not {base}");
        }
        let http = reqwest::Client::builder()
            .user_agent(format!("kiwi-nest/{}", kn_proto::VERSION))
            .build()?;
        Ok(Client { http, base })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}{path}", self.base, kn_proto::API_PREFIX)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> anyhow::Result<reqwest::Response> {
        let resp = req
            .send()
            .await
            .with_context(|| format!("couldn't reach the node API at {}", self.base))?;
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        match serde_json::from_str::<ErrorBody>(&text) {
            Ok(body) => {
                let mut msg = body.error;
                for p in body.problems {
                    msg.push_str("\n  - ");
                    msg.push_str(&p);
                }
                bail!(msg)
            }
            Err(_) => bail!("the node API answered {status}: {}", text.trim()),
        }
    }

    pub async fn deploy_zip(&self, name: &str, zip: Vec<u8>, create_only: bool) -> anyhow::Result<AppInfo> {
        let req = self
            .http
            .put(self.url(&format!("/apps/{name}")))
            .query(&DeployQuery { create_only })
            .header(reqwest::header::CONTENT_TYPE, "application/zip")
            .body(zip);
        Ok(self.send(req).await?.json().await?)
    }

    pub async fn deploy_directory(&self, name: &str, dir: &Path, create_only: bool) -> anyhow::Result<AppInfo> {
        let directory = dir.to_str().context("the path isn't UTF-8")?.to_string();
        let req = self
            .http
            .put(self.url(&format!("/apps/{name}")))
            .query(&DeployQuery { create_only })
            .json(&DeployDirectory { directory });
        Ok(self.send(req).await?.json().await?)
    }

    pub async fn list(&self) -> anyhow::Result<Vec<AppSummary>> {
        Ok(self.send(self.http.get(self.url("/apps"))).await?.json().await?)
    }

    pub async fn inspect(&self, name: &str) -> anyhow::Result<AppInfo> {
        Ok(self
            .send(self.http.get(self.url(&format!("/apps/{name}"))))
            .await?
            .json()
            .await?)
    }

    pub async fn logs(&self, name: &str, q: &LogsQuery) -> anyhow::Result<reqwest::Response> {
        self.send(self.http.get(self.url(&format!("/apps/{name}/logs"))).query(q))
            .await
    }

    pub async fn delete(&self, name: &str) -> anyhow::Result<()> {
        self.send(self.http.delete(self.url(&format!("/apps/{name}")))).await?;
        Ok(())
    }
}

/// Runs a client command.
pub async fn run(global: &GlobalArgs, cmd: Command) -> anyhow::Result<()> {
    let client = Client::new(&global.node_api)?;
    match cmd {
        Command::Deploy {
            name,
            path,
            create_only,
            node_reads_path,
        } => {
            let info = if node_reads_path {
                let dir = std::path::absolute(&path)?;
                client.deploy_directory(&name, &dir, create_only).await?
            } else {
                let zip = read_bundle(path).await?;
                client.deploy_zip(&name, zip, create_only).await?
            };
            println!(
                "deployed {} (generation {}): {} on http://127.0.0.1:{} ({}:{})",
                info.name, info.generation, info.domain, info.port, info.service, info.container_port
            );
        }
        Command::List { json } => {
            let apps = client.list().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&apps)?);
            } else {
                print_table(
                    &["NAME", "STATUS", "CONTAINERS", "GEN", "PORT", "DOMAIN", "UPDATED"],
                    apps.iter()
                        .map(|a| {
                            vec![
                                a.name.clone(),
                                a.status.to_string(),
                                a.containers.clone(),
                                a.generation.to_string(),
                                a.port.to_string(),
                                a.domain.clone(),
                                a.updated_at.clone(),
                            ]
                        })
                        .collect(),
                );
            }
        }
        Command::Inspect { name, json } => {
            let info = client.inspect(&name).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&info)?);
            } else {
                print_info(&info);
            }
        }
        Command::Logs {
            name,
            tail,
            follow,
            timestamps,
            service,
        } => {
            let resp = client
                .logs(
                    &name,
                    &LogsQuery {
                        tail,
                        follow,
                        timestamps,
                        service,
                    },
                )
                .await?;
            let mut stream = resp.bytes_stream();
            let mut out = std::io::stdout().lock();
            while let Some(chunk) = stream.next().await {
                out.write_all(&chunk?)?;
                out.flush()?;
            }
        }
        Command::Delete { name } => {
            client.delete(&name).await?;
            println!("deleted {name}");
        }
    }
    Ok(())
}

/// A bundle as a ZIP: a ZIP file as it is, or a directory packed (refusing what a node would refuse).
async fn read_bundle(path: PathBuf) -> anyhow::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let meta = std::fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        if meta.is_dir() {
            kn_bundle::dir::pack(&path, &kn_bundle::Limits::default())
                .with_context(|| format!("packing {}", path.display()))
        } else {
            std::fs::read(&path).with_context(|| format!("reading {}", path.display()))
        }
    })
    .await?
}

fn print_table(headers: &[&str], rows: Vec<Vec<String>>) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in &rows {
        for (w, cell) in widths.iter_mut().zip(row) {
            *w = (*w).max(cell.chars().count());
        }
    }
    let line = |cells: Vec<&str>| {
        let s: Vec<String> = cells.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect();
        println!("{}", s.join("  ").trim_end());
    };
    line(headers.to_vec());
    for row in &rows {
        line(row.iter().map(String::as_str).collect());
    }
}

fn print_info(i: &AppInfo) {
    let idle = if i.idle_timeout_secs == 0 {
        "always on".to_string()
    } else {
        format!("{}s", i.idle_timeout_secs)
    };
    println!("name:        {}", i.name);
    println!("status:      {} (generation {})", i.status, i.generation);
    if let Some(e) = &i.error {
        println!("error:       {e}");
    }
    println!("domain:      {}", i.domain);
    println!(
        "routed to:   {}:{} on http://127.0.0.1:{}",
        i.service, i.container_port, i.port
    );
    println!("idle:        {idle}");
    println!("project:     {}", i.project);
    println!("bundle:      {}", i.bundle_digest);
    println!(
        "volumes:     {}",
        if i.volumes.is_empty() {
            "none".to_string()
        } else {
            i.volumes.join(", ")
        }
    );
    println!("created:     {}", i.created_at);
    println!("updated:     {}", i.updated_at);
    println!();
    print_table(
        &["SERVICE", "CONTAINER", "STATE", "HEALTH", "STATUS"],
        i.containers
            .iter()
            .map(|c| {
                vec![
                    c.service.clone(),
                    c.name.clone(),
                    c.state.clone(),
                    c.health.clone(),
                    c.status.clone(),
                ]
            })
            .collect(),
    );
}

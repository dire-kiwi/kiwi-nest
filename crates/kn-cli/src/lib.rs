//! The kiwi-nest CLI's commands.
//!
//! For now they talk to a standalone node's loopback API (`--node-api`); clusters come with the server.
//!
//! `volume …` manages shared volumes; `deploy --from` and `delete --keep-data` decide what happens to an app's.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use futures_util::StreamExt;
use kn_proto::api::{AppInfo, AppSummary, DeleteQuery, DeployDirectory, DeployQuery, ErrorBody, LogsQuery, VolumeInfo};

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
        /// On the app's first deploy, start each named volume it shares with APP as an instant snapshot of APP's.
        #[arg(long, value_name = "APP")]
        from: Option<String>,
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
    Delete {
        name: String,
        /// Keep the app's volumes (and its snapshots of shared volumes), so a later deploy of NAME gets them back.
        #[arg(long)]
        keep_data: bool,
    },
    /// Shared volumes, which apps mount through `x-kiwi-nest.shared`.
    #[command(subcommand)]
    Volume(VolumeCommand),
}

#[derive(Debug, Clone, Subcommand)]
pub enum VolumeCommand {
    /// Create an empty shared volume.
    Create { name: String },
    /// List the shared volumes.
    List {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show a shared volume: the apps that mount it, and its snapshots.
    Inspect {
        name: String,
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Replace a shared volume's contents with a directory (or a ZIP of one) of plain files.
    ///
    /// Refused while an app mounts it `ro` or `rw`. Apps' snapshots keep what they had.
    Load {
        name: String,
        path: PathBuf,
        /// Have the node read the directory itself, rather than uploading it (the node must be on this machine).
        #[arg(long)]
        node_reads_path: bool,
    },
    /// Delete a shared volume. Refused while an app mounts it or any snapshot of it exists.
    Delete { name: String },
    /// Delete APP's snapshot of a shared volume. Refused while APP is deployed and mounts it.
    DeleteSnapshot { name: String, app: String },
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

    pub async fn deploy_zip(&self, name: &str, zip: Vec<u8>, q: &DeployQuery) -> anyhow::Result<AppInfo> {
        let req = self
            .http
            .put(self.url(&format!("/apps/{name}")))
            .query(q)
            .header(reqwest::header::CONTENT_TYPE, "application/zip")
            .body(zip);
        Ok(self.send(req).await?.json().await?)
    }

    pub async fn deploy_directory(&self, name: &str, dir: &Path, q: &DeployQuery) -> anyhow::Result<AppInfo> {
        let req = self
            .http
            .put(self.url(&format!("/apps/{name}")))
            .query(q)
            .json(&directory(dir)?);
        Ok(self.send(req).await?.json().await?)
    }

    pub async fn volumes(&self) -> anyhow::Result<Vec<VolumeInfo>> {
        Ok(self.send(self.http.get(self.url("/volumes"))).await?.json().await?)
    }

    pub async fn volume(&self, name: &str) -> anyhow::Result<VolumeInfo> {
        Ok(self
            .send(self.http.get(self.url(&format!("/volumes/{name}"))))
            .await?
            .json()
            .await?)
    }

    pub async fn volume_create(&self, name: &str) -> anyhow::Result<VolumeInfo> {
        Ok(self
            .send(self.http.put(self.url(&format!("/volumes/{name}"))))
            .await?
            .json()
            .await?)
    }

    pub async fn volume_load_zip(&self, name: &str, zip: Vec<u8>) -> anyhow::Result<VolumeInfo> {
        let req = self
            .http
            .put(self.url(&format!("/volumes/{name}/data")))
            .header(reqwest::header::CONTENT_TYPE, "application/zip")
            .body(zip);
        Ok(self.send(req).await?.json().await?)
    }

    pub async fn volume_load_directory(&self, name: &str, dir: &Path) -> anyhow::Result<VolumeInfo> {
        let req = self
            .http
            .put(self.url(&format!("/volumes/{name}/data")))
            .json(&directory(dir)?);
        Ok(self.send(req).await?.json().await?)
    }

    pub async fn volume_delete(&self, name: &str) -> anyhow::Result<()> {
        self.send(self.http.delete(self.url(&format!("/volumes/{name}"))))
            .await?;
        Ok(())
    }

    pub async fn volume_delete_snapshot(&self, name: &str, app: &str) -> anyhow::Result<()> {
        self.send(self.http.delete(self.url(&format!("/volumes/{name}/snapshots/{app}"))))
            .await?;
        Ok(())
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

    pub async fn delete(&self, name: &str, keep_data: bool) -> anyhow::Result<()> {
        self.send(
            self.http
                .delete(self.url(&format!("/apps/{name}")))
                .query(&DeleteQuery { keep_data }),
        )
        .await?;
        Ok(())
    }
}

fn directory(dir: &Path) -> anyhow::Result<DeployDirectory> {
    Ok(DeployDirectory {
        directory: dir.to_str().context("the path isn't UTF-8")?.to_string(),
    })
}

/// Runs a client command.
pub async fn run(global: &GlobalArgs, cmd: Command) -> anyhow::Result<()> {
    let client = Client::new(&global.node_api)?;
    match cmd {
        Command::Deploy {
            name,
            path,
            create_only,
            from,
            node_reads_path,
        } => {
            let q = DeployQuery { create_only, from };
            let info = if node_reads_path {
                let dir = std::path::absolute(&path)?;
                client.deploy_directory(&name, &dir, &q).await?
            } else {
                let zip = read_bundle(path, kn_bundle::Limits::default()).await?;
                client.deploy_zip(&name, zip, &q).await?
            };
            println!(
                "deployed {} (generation {}): {} on http://127.0.0.1:{} ({}:{})",
                info.name, info.generation, info.domain, info.port, info.service, info.container_port
            );
            for note in &info.notes {
                println!("  {note}");
            }
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
        Command::Delete { name, keep_data } => {
            client.delete(&name, keep_data).await?;
            if keep_data {
                println!("deleted {name}, keeping its volumes");
            } else {
                println!("deleted {name}");
            }
        }
        Command::Volume(cmd) => volume(&client, cmd).await?,
    }
    Ok(())
}

async fn volume(client: &Client, cmd: VolumeCommand) -> anyhow::Result<()> {
    match cmd {
        VolumeCommand::Create { name } => {
            client.volume_create(&name).await?;
            println!("created shared volume {name}");
        }
        VolumeCommand::List { json } => {
            let volumes = client.volumes().await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&volumes)?);
            } else {
                print_table(
                    &["NAME", "MOUNTED BY", "SNAPSHOTS", "LOADED", "PROBLEM"],
                    volumes
                        .iter()
                        .map(|v| {
                            vec![
                                v.name.clone(),
                                mounts(v),
                                v.snapshots.join(","),
                                v.loaded_at.clone().unwrap_or_default(),
                                v.problem.clone().unwrap_or_default(),
                            ]
                        })
                        .collect(),
                );
            }
        }
        VolumeCommand::Inspect { name, json } => {
            let v = client.volume(&name).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else {
                println!("name:        {}", v.name);
                println!("created:     {}", v.created_at);
                println!("loaded:      {}", v.loaded_at.as_deref().unwrap_or("never"));
                println!("mounted by:  {}", or_none(mounts(&v)));
                println!("snapshots:   {}", or_none(v.snapshots.join(", ")));
                if let Some(p) = &v.problem {
                    println!("problem:     {p}");
                }
            }
        }
        VolumeCommand::Load {
            name,
            path,
            node_reads_path,
        } => {
            if node_reads_path {
                let dir = std::path::absolute(&path)?;
                client.volume_load_directory(&name, &dir).await?;
            } else {
                let zip = read_bundle(path, kn_bundle::Limits::for_volumes()).await?;
                client.volume_load_zip(&name, zip).await?;
            }
            println!("loaded shared volume {name}");
        }
        VolumeCommand::Delete { name } => {
            client.volume_delete(&name).await?;
            println!("deleted shared volume {name}");
        }
        VolumeCommand::DeleteSnapshot { name, app } => {
            client.volume_delete_snapshot(&name, &app).await?;
            println!("deleted {app}'s snapshot of {name}");
        }
    }
    Ok(())
}

fn mounts(v: &VolumeInfo) -> String {
    v.mounts
        .iter()
        .map(|m| format!("{} ({})", m.app, m.mode))
        .collect::<Vec<_>>()
        .join(", ")
}

fn or_none(s: String) -> String {
    if s.is_empty() { "none".into() } else { s }
}

/// A bundle as a ZIP: a ZIP file as it is, or a directory packed (refusing what a node would refuse).
async fn read_bundle(path: PathBuf, limits: kn_bundle::Limits) -> anyhow::Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let meta = std::fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?;
        if meta.is_dir() {
            kn_bundle::dir::pack(&path, &limits).with_context(|| format!("packing {}", path.display()))
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
    println!("volumes:     {}", or_none(i.volumes.join(", ")));
    println!(
        "shared:      {}",
        or_none(
            i.shared
                .iter()
                .map(|m| format!("{} at {} ({})", m.name, m.target, m.mode))
                .collect::<Vec<_>>()
                .join(", ")
        )
    );
    if let Some(from) = &i.from {
        println!("from:        {from}");
    }
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

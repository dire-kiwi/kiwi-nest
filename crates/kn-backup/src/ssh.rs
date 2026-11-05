//! A directory on another machine, through OpenSSH's `ssh` and a POSIX shell there.
//!
//! Each operation is one `ssh HOST COMMAND`, with every path single-quoted. An upload is `cat` into a `.partial-`
//! file beside its final name; once the data has all been sent, a second command checks the file's size, syncs it
//! and renames it into place. An upload that fails is killed (never just ended, which would look like the end of the
//! data), so a partial file is never renamed.

use std::process::Stdio;

use anyhow::{Context, bail};
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use kn_proto::backup::SshConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

use crate::store::{Body, ByteStream, Object, PARTIAL_PREFIX, Store, check_key, next};

pub struct SshStore {
    /// The `ssh` program.
    program: String,
    cfg: SshConfig,
    /// The key and known hosts, when they came as contents (from the server), written to private files here; `ssh`
    /// is given those.
    _files: Option<tempfile::TempDir>,
    identity: Option<String>,
    known_hosts: Option<String>,
}

/// Writes `contents` to `dir/name`, readable only by this user (as `ssh` insists for a key).
fn private_file(dir: &std::path::Path, name: &str, contents: &str) -> anyhow::Result<String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = dir.join(name);
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("writing {}", path.display()))?;
    f.write_all(contents.as_bytes())?;
    if !contents.ends_with('\n') {
        f.write_all(b"\n")?;
    }
    Ok(path.to_string_lossy().into_owned())
}

/// Quotes a string for a POSIX shell.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

impl SshStore {
    pub fn new(cfg: SshConfig) -> anyhow::Result<SshStore> {
        SshStore::with_program("ssh", cfg)
    }

    /// With another program in `ssh`'s place (tests run one that runs the command locally).
    pub fn with_program(program: &str, cfg: SshConfig) -> anyhow::Result<SshStore> {
        if !cfg.path.starts_with('/') || cfg.path.contains('\n') {
            bail!("an ssh target's path must be absolute, not {:?}", cfg.path);
        }
        let host_ok = !cfg.host.is_empty()
            && !cfg.host.starts_with('-')
            && cfg
                .host
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-' | b'_' | b'@' | b':' | b'[' | b']'));
        if !host_ok {
            bail!("an ssh target's host must be HOST or USER@HOST, not {:?}", cfg.host);
        }
        let (mut identity, mut known_hosts) = (cfg.identity.clone(), cfg.known_hosts.clone());
        let files = if cfg.identity_data.is_some() || cfg.known_hosts_data.is_some() {
            // A private directory (mode 0700), removed with the store.
            let dir = tempfile::Builder::new()
                .prefix("kiwi-nest-ssh-")
                .tempdir()
                .context("making a directory for the ssh key")?;
            if let Some(k) = &cfg.identity_data {
                identity = Some(private_file(dir.path(), "identity", k)?);
            }
            if let Some(k) = &cfg.known_hosts_data {
                known_hosts = Some(private_file(dir.path(), "known_hosts", k)?);
            }
            Some(dir)
        } else {
            None
        };
        Ok(SshStore {
            program: program.into(),
            cfg,
            _files: files,
            identity,
            known_hosts,
        })
    }

    fn command(&self, remote: &str) -> Command {
        let mut c = Command::new(&self.program);
        c.args(["-o", "BatchMode=yes", "-o", "ServerAliveInterval=30"]);
        if let Some(p) = self.cfg.port {
            c.args(["-p", &p.to_string()]);
        }
        if let Some(i) = &self.identity {
            c.args(["-i", i, "-o", "IdentitiesOnly=yes"]);
        }
        if let Some(k) = &self.known_hosts {
            c.args(["-o", &format!("UserKnownHostsFile={k}")]);
        }
        c.arg("--").arg(&self.cfg.host).arg(remote);
        c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        c.kill_on_drop(true);
        c
    }

    /// Runs a command there, and returns its output; fails with its stderr if it fails.
    async fn run(&self, remote: &str) -> anyhow::Result<Vec<u8>> {
        let out = self
            .command(remote)
            .output()
            .await
            .with_context(|| format!("running {}", self.program))?;
        if !out.status.success() {
            bail!(
                "ssh {}: {} ({})",
                self.cfg.host,
                String::from_utf8_lossy(&out.stderr).trim(),
                out.status
            );
        }
        Ok(out.stdout)
    }

    fn path(&self, key: &str) -> anyhow::Result<String> {
        check_key(key)?;
        Ok(format!("{}/{key}", self.cfg.path.trim_end_matches('/')))
    }
}

/// Parses `wc -c` output: `SIZE ./PATH` lines, and `SIZE total` lines between batches, which are skipped.
fn parse_listing(out: &str, prefix: &str) -> Vec<Object> {
    let mut objects: Vec<Object> = out
        .lines()
        .filter_map(|l| {
            let l = l.trim_start();
            let (size, path) = l.split_once(' ')?;
            let key = path.trim_start().strip_prefix("./")?;
            let hidden = key.split('/').any(|c| c.starts_with('.'));
            (!hidden && key.starts_with(prefix)).then(|| Object {
                key: key.to_string(),
                size: size.parse().ok(),
            })
        })
        .collect();
    objects.sort_by(|a, b| a.key.cmp(&b.key));
    objects
}

impl Store for SshStore {
    fn describe(&self) -> String {
        format!("{}:{}", self.cfg.host, self.cfg.path)
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<Object>>> {
        Box::pin(async move {
            let root = quote(&self.cfg.path);
            let out = self
                .run(&format!(
                    "cd {root} 2>/dev/null || exit 0; find . -type f ! -name '{PARTIAL_PREFIX}*' -exec wc -c {{}} +"
                ))
                .await?;
            Ok(parse_listing(&String::from_utf8_lossy(&out), prefix))
        })
    }

    fn put<'a>(&'a self, key: &'a str, mut body: Body) -> BoxFuture<'a, anyhow::Result<u64>> {
        Box::pin(async move {
            let dest = self.path(key)?;
            let (dir, file) = dest.rsplit_once('/').context("a key has a directory")?;
            let partial = format!("{dir}/{PARTIAL_PREFIX}{file}");
            let mut child = self
                .command(&format!("mkdir -p {} && cat > {}", quote(dir), quote(&partial)))
                .stdin(Stdio::piped())
                .spawn()
                .with_context(|| format!("running {}", self.program))?;
            let mut stdin = child.stdin.take().context("ssh's stdin")?;
            let fed = async {
                let mut n = 0u64;
                while let Some(b) = next(&mut body).await? {
                    stdin.write_all(&b).await.context("sending to ssh")?;
                    n += b.len() as u64;
                }
                stdin.flush().await?;
                anyhow::Ok(n)
            }
            .await;
            let n = match fed {
                Ok(n) => n,
                Err(e) => {
                    // Killed, not ended: the other side must never see a clean end of the data.
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    let _ = self.run(&format!("rm -f {}", quote(&partial))).await;
                    return Err(e);
                }
            };
            drop(stdin);
            let out = child.wait_with_output().await?;
            if !out.status.success() {
                bail!(
                    "uploading to ssh {}: {} ({})",
                    self.cfg.host,
                    String::from_utf8_lossy(&out.stderr).trim(),
                    out.status
                );
            }
            let (p, d) = (quote(&partial), quote(&dest));
            self.run(&format!(
                "n=$(wc -c < {p}) && [ $n -eq {n} ] || {{ echo \"the upload has $n bytes, not {n}\" >&2; exit 1; }}; \
                 sync {p} 2>/dev/null || sync; mv -f {p} {d} || exit 1; sync {} 2>/dev/null; exit 0",
                quote(dir)
            ))
            .await
            .context("finishing the upload")?;
            Ok(n)
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, anyhow::Result<ByteStream>> {
        Box::pin(async move {
            let path = self.path(key)?;
            let mut child = self
                .command(&format!("cat {}", quote(&path)))
                .spawn()
                .with_context(|| format!("running {}", self.program))?;
            let stdout = child.stdout.take().context("ssh's stdout")?;
            let mut stderr = child.stderr.take().context("ssh's stderr")?;
            let host = self.cfg.host.clone();
            // The data, then an error if `cat` (or ssh) failed: a short read is never mistaken for the whole.
            let data = tokio_util::io::ReaderStream::with_capacity(stdout, 1 << 20);
            let tail = futures_util::stream::once(async move {
                let mut err = String::new();
                let _ = stderr.read_to_string(&mut err).await;
                match child.wait().await {
                    Ok(s) if s.success() => None,
                    Ok(s) => Some(Err(std::io::Error::other(format!("ssh {host}: {} ({s})", err.trim())))),
                    Err(e) => Some(Err(e)),
                }
            })
            .filter_map(|x| async move { x });
            Ok(Box::pin(data.chain(tail)) as ByteStream)
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let path = self.path(key)?;
            self.run(&format!("rm -f {}", quote(&path))).await?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_for_the_shell() {
        assert_eq!(quote("a b"), "'a b'");
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert!(
            SshStore::new(SshConfig {
                host: "-oProxyCommand=x".into(),
                port: None,
                path: "/x".into(),
                identity: None,
                known_hosts: None,
                identity_data: None,
                known_hosts_data: None,
            })
            .is_err()
        );
    }

    #[test]
    fn a_key_sent_as_contents_is_given_to_ssh_as_a_private_file() {
        use std::os::unix::fs::PermissionsExt;
        let cfg = SshConfig {
            host: "backup@nas".into(),
            port: None,
            path: "/srv/kn".into(),
            identity: None,
            known_hosts: None,
            identity_data: Some("-----BEGIN OPENSSH PRIVATE KEY-----\nkey".into()),
            known_hosts_data: Some("nas ssh-ed25519 AAAA\n".into()),
        };
        let store = SshStore::new(cfg).unwrap();
        let (id, kh) = (store.identity.clone().unwrap(), store.known_hosts.clone().unwrap());
        assert!(std::fs::read_to_string(&id).unwrap().ends_with("key\n"));
        assert_eq!(std::fs::read_to_string(&kh).unwrap(), "nas ssh-ed25519 AAAA\n");
        for f in [&id, &kh] {
            assert_eq!(std::fs::metadata(f).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let args: Vec<String> = store
            .command("true")
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|w| w[0] == "-i" && w[1] == id), "{args:?}");
        assert!(args.contains(&format!("UserKnownHostsFile={kh}")), "{args:?}");
        drop(store);
        assert!(!std::path::Path::new(&id).exists());
    }

    #[test]
    fn parses_listings() {
        let out = "  11 ./apps/blog/a.tar.zst\n 3 ./apps/.hidden/x\n0 ./volumes/c/b.tar.zst\n14 total\n";
        let all = parse_listing(out, "");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].size, Some(11));
        assert_eq!(parse_listing(out, "volumes/").len(), 1);
    }

    /// A stand-in for `ssh`: ignores the options and the host, and runs the command with `sh` here.
    fn fake_ssh(dir: &std::path::Path) -> String {
        let path = dir.join("fake-ssh");
        std::fs::write(
            &path,
            "#!/bin/sh\nwhile [ \"$1\" != -- ]; do shift; done\nshift 2\nexec sh -c \"$1\"\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[tokio::test]
    async fn an_ssh_store_conforms() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().join("it's backups");
        let store = SshStore::with_program(
            &fake_ssh(d.path()),
            SshConfig {
                host: "backup@nas".into(),
                port: Some(2222),
                path: root.to_string_lossy().into_owned(),
                identity: Some("/etc/key".into()),
                known_hosts: None,
                identity_data: None,
                known_hosts_data: None,
            },
        )
        .unwrap();
        crate::store::tests::conforms(&store).await;
        // A missing object is an error, not an empty one.
        let s = store.get("apps/blog/20201106T030000Z.tar.zst").await.unwrap();
        let items: Vec<_> = s.collect().await;
        assert!(items.iter().any(|r| r.is_err()), "{items:?}");
        let left: Vec<_> = std::fs::read_dir(root.join("apps/blog"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }
}

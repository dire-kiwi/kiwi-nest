//! A configured target: its store, its format and schedule, and backups through zstd.
//!
//! [`Target::upload`] runs a producer (a `btrfs send`, a tar writer) on a blocking thread, compresses what it
//! writes with zstd, and streams that to the store; [`Target::download`] does the reverse for a consumer (a receive,
//! a tar extractor). A producer that fails makes the upload fail, and the store then leaves nothing.

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use bytes::Bytes;
use futures_util::StreamExt;
use kn_proto::backup::{BackupInfo, Format, Kind, TargetConfig, TargetsFile};
use tokio::sync::mpsc;

use crate::key::{self, Key, Stamp};
use crate::s3::S3Store;
use crate::ssh::SshStore;
use crate::store::{Chunk, DirStore, Store};

/// The zstd level backups are compressed at.
const LEVEL: i32 = 3;

/// The default age at which a `send` chain starts again with a full backup.
pub const DEFAULT_FULL_EVERY: Duration = Duration::from_secs(7 * 86_400);

fn duration(what: &str, s: &str) -> anyhow::Result<Duration> {
    let d =
        humantime::parse_duration(s).with_context(|| format!("bad {what} {s:?}: use a duration such as 24h or 7d"))?;
    if d < Duration::from_secs(60) {
        bail!("{what} must be at least a minute, not {s:?}");
    }
    Ok(d)
}

/// What a producer gets to write a backup to, and a consumer to read one from.
pub type Producer = Box<dyn FnOnce(&mut dyn Write) -> anyhow::Result<()> + Send>;

pub struct Target {
    cfg: TargetConfig,
    store: Box<dyn Store>,
    every: Option<Duration>,
    full_every: Duration,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("name", &self.cfg.name)
            .field("store", &self.store.describe())
            .finish()
    }
}

/// One backup a listing found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub key: Key,
    pub size: Option<u64>,
}

impl Target {
    /// Checks a target's configuration and opens its store (which doesn't reach it yet).
    pub fn open(cfg: TargetConfig) -> anyhow::Result<Target> {
        let store = Self::store_for(&cfg)?;
        Self::with_store(cfg, store)
    }

    /// A target with a given store (tests use stand-ins).
    pub fn with_store(cfg: TargetConfig, store: Box<dyn Store>) -> anyhow::Result<Target> {
        if !key::valid_name(&cfg.name) {
            bail!(
                "a backup target's name must be letters, digits, `.`, `_` and `-`, not {:?}",
                cfg.name
            );
        }
        let ok_prefix = cfg.prefix.is_empty()
            || (cfg.prefix.ends_with('/') && crate::store::check_key(cfg.prefix.trim_end_matches('/')).is_ok());
        if !ok_prefix {
            bail!(
                "target `{}`: a prefix must be plain path components ending with `/`, such as `home/`, not {:?}",
                cfg.name,
                cfg.prefix
            );
        }
        let every = cfg.every.as_deref().map(|e| duration("`every`", e)).transpose()?;
        let full_every = match &cfg.full_every {
            Some(f) => duration("`full_every`", f)?,
            None => DEFAULT_FULL_EVERY,
        };
        Ok(Target {
            cfg,
            store,
            every,
            full_every,
        })
    }

    fn store_for(cfg: &TargetConfig) -> anyhow::Result<Box<dyn Store>> {
        let name = &cfg.name;
        Ok(match (&cfg.s3, &cfg.ssh, &cfg.dir) {
            (Some(s3), None, None) => Box::new(S3Store::new(s3).with_context(|| format!("target `{name}`"))?),
            (None, Some(ssh), None) => {
                Box::new(SshStore::new(ssh.clone()).with_context(|| format!("target `{name}`"))?)
            }
            (None, None, Some(dir)) => {
                Box::new(DirStore::new(PathBuf::from(&dir.path)).with_context(|| format!("target `{name}`"))?)
            }
            _ => bail!("backup target `{name}` needs exactly one of `s3`, `ssh` and `dir`"),
        })
    }

    pub fn name(&self) -> &str {
        &self.cfg.name
    }

    pub fn config(&self) -> &TargetConfig {
        &self.cfg
    }

    pub fn format(&self) -> Format {
        self.cfg.format
    }

    /// How often scheduled backups go here, if they do.
    pub fn every(&self) -> Option<Duration> {
        self.every
    }

    pub fn full_every(&self) -> Duration {
        self.full_every
    }

    pub fn describe(&self) -> String {
        format!("`{}` ({})", self.cfg.name, self.store.describe())
    }

    fn full(&self, key: &Key) -> String {
        format!("{}{key}", self.cfg.prefix)
    }

    /// The backups here, of one app, volume or the server (`scope`), or of everything. Objects whose keys
    /// kiwi-nest wouldn't make are ignored.
    pub async fn list(&self, scope: Option<(Kind, &str)>) -> anyhow::Result<Vec<Listed>> {
        let within = scope.map(|(k, n)| key::scope(k, n)).unwrap_or_default();
        let prefix = format!("{}{within}", self.cfg.prefix);
        let objects = self
            .store
            .list(&prefix)
            .await
            .with_context(|| format!("listing target {}", self.describe()))?;
        let mut out: Vec<Listed> = objects
            .into_iter()
            .filter_map(|o| {
                let rel = o.key.strip_prefix(&self.cfg.prefix)?;
                Some(Listed {
                    key: Key::parse(rel)?,
                    size: o.size,
                })
            })
            .collect();
        out.sort_by(|a, b| (a.key.kind, &a.key.name, a.key.stamp).cmp(&(b.key.kind, &b.key.name, b.key.stamp)));
        Ok(out)
    }

    /// The backups of one app, volume or the server, as `backup list` shows them.
    pub async fn infos(&self, scope: Option<(Kind, &str)>) -> anyhow::Result<Vec<BackupInfo>> {
        Ok(self
            .list(scope)
            .await?
            .into_iter()
            .map(|l| l.key.info(&self.cfg.name, l.size))
            .collect())
    }

    /// A stamp for a new backup of `scope`: now, or just after the newest backup it has here, whichever is later,
    /// so keys never collide and always sort in order.
    pub fn next_stamp(listing: &[Listed], also_after: Option<Stamp>) -> Stamp {
        let newest = listing.iter().map(|l| l.key.stamp).chain(also_after).max();
        let now = Stamp::now();
        match newest {
            Some(n) if n >= now => Stamp(n.0 + 1),
            _ => now,
        }
    }

    /// The parent for a new `send` backup here, given the snapshots kept on the node; `None` for a full one.
    pub fn parent_for(&self, listing: &[Listed], local: &BTreeSet<Stamp>, now: Stamp) -> Option<Stamp> {
        let keys: Vec<Key> = listing.iter().map(|l| l.key.clone()).collect();
        key::choose_parent(&keys, local, now, self.full_every.as_secs())
    }

    /// Uploads a backup, compressing what `produce` writes; returns its size. Fails, leaving nothing, if `produce` or
    /// the upload does.
    pub async fn upload(&self, key: &Key, produce: Producer) -> anyhow::Result<u64> {
        let (tx, rx) = mpsc::channel::<Chunk>(4);
        let producer = tokio::task::spawn_blocking(move || {
            let mut w = ChannelWriter::new(tx.clone());
            let made = (|| -> anyhow::Result<()> {
                let mut enc = zstd::stream::write::Encoder::new(&mut w, LEVEL)?;
                produce(&mut enc)?;
                enc.finish()?;
                w.flush()?;
                Ok(())
            })();
            let last = match &made {
                Ok(()) => Chunk::End,
                Err(e) => Chunk::Failed(format!("{e:#}")),
            };
            let _ = tx.blocking_send(last);
            made
        });
        let full = self.full(key);
        let put = self.store.put(&full, rx).await;
        let produced = producer.await;
        match (put, produced) {
            (Ok(n), Ok(Ok(()))) => Ok(n),
            // The producer's own error says more than the upload's echo of it.
            (_, Ok(Err(e))) => Err(e.context(format!("backing up to {}", self.describe()))),
            (Err(e), _) => Err(e.context(format!("uploading {key} to {}", self.describe()))),
            (Ok(_), Err(e)) => Err(anyhow::anyhow!("the backup's writer failed: {e}")),
        }
    }

    /// Downloads a backup and hands it, decompressed, to `consume` on a blocking thread.
    pub async fn download<T: Send + 'static>(
        &self,
        key: &Key,
        consume: impl FnOnce(&mut dyn Read) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let full = self.full(key);
        let mut stream = self
            .store
            .get(&full)
            .await
            .with_context(|| format!("downloading {key} from {}", self.describe()))?;
        let (tx, rx) = mpsc::channel::<io::Result<Bytes>>(4);
        let pump = tokio::spawn(async move {
            while let Some(item) = stream.next().await {
                let failed = item.is_err();
                if tx.send(item).await.is_err() || failed {
                    break;
                }
            }
        });
        let consumer = tokio::task::spawn_blocking(move || -> anyhow::Result<T> {
            let mut r = ChannelReader::new(rx);
            let mut dec = zstd::stream::read::Decoder::new(&mut r)?;
            let out = consume(&mut dec)?;
            // Whatever the consumer left unread must still be readable: a download cut short is an error.
            io::copy(&mut dec, &mut io::sink())?;
            Ok(out)
        });
        let r = consumer.await?;
        pump.abort();
        r.with_context(|| format!("restoring {key} from {}", self.describe()))
    }

    pub async fn delete(&self, key: &Key) -> anyhow::Result<()> {
        self.store.delete(&self.full(key)).await
    }

    /// Applies retention to one app's, volume's or the server's backups here; returns what it deleted.
    pub async fn prune(&self, kind: Kind, name: &str) -> anyhow::Result<Vec<Key>> {
        if self.cfg.keep.everything() {
            return Ok(Vec::new());
        }
        let listing: Vec<Key> = self
            .list(Some((kind, name)))
            .await?
            .into_iter()
            .map(|l| l.key)
            .collect();
        let gone = key::prune(&listing, &self.cfg.keep);
        for k in &gone {
            self.delete(k)
                .await
                .with_context(|| format!("deleting {k} from {}", self.describe()))?;
            tracing::info!(target = self.cfg.name, key = %k, "deleted an old backup");
        }
        Ok(gone)
    }
}

/// Reads a targets file (YAML or JSON) and checks every target in it.
pub fn load(path: &Path) -> anyhow::Result<Vec<Target>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let file: TargetsFile =
        serde_yaml_ng::from_str(&text).with_context(|| format!("reading the backup targets in {}", path.display()))?;
    open_all(file.targets).with_context(|| format!("the backup targets in {}", path.display()))
}

/// Opens targets, refusing two of one name.
pub fn open_all(cfgs: Vec<TargetConfig>) -> anyhow::Result<Vec<Target>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for c in cfgs {
        if !seen.insert(c.name.clone()) {
            bail!("there are two backup targets named `{}`", c.name);
        }
        out.push(Target::open(c)?);
    }
    Ok(out)
}

/// A [`Write`] that sends what's written over a channel, in pieces of about a megabyte.
pub struct ChannelWriter {
    tx: mpsc::Sender<Chunk>,
    buf: Vec<u8>,
}

const PIECE: usize = 1 << 20;

impl ChannelWriter {
    pub fn new(tx: mpsc::Sender<Chunk>) -> Self {
        ChannelWriter {
            tx,
            buf: Vec::with_capacity(PIECE),
        }
    }

    fn send(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let piece = Bytes::from(std::mem::replace(&mut self.buf, Vec::with_capacity(PIECE)));
        self.tx
            .blocking_send(Chunk::Data(piece))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the upload stopped"))
    }
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        // An upload that stopped stops its producer at once, however well its data compresses.
        if self.tx.is_closed() {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "the upload stopped"));
        }
        self.buf.extend_from_slice(data);
        if self.buf.len() >= PIECE {
            self.send()?;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.send()
    }
}

/// A [`Read`] over a channel of byte pieces.
pub struct ChannelReader {
    rx: mpsc::Receiver<io::Result<Bytes>>,
    cur: Bytes,
}

impl ChannelReader {
    pub fn new(rx: mpsc::Receiver<io::Result<Bytes>>) -> Self {
        ChannelReader { rx, cur: Bytes::new() }
    }
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        while self.cur.is_empty() {
            match self.rx.blocking_recv() {
                Some(Ok(b)) => self.cur = b,
                Some(Err(e)) => return Err(e),
                None => return Ok(0),
            }
        }
        let n = out.len().min(self.cur.len());
        out[..n].copy_from_slice(&self.cur.split_to(n));
        Ok(n)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use kn_proto::backup::{DirConfig, Keep};

    use super::*;

    pub(crate) fn dir_target(dir: &Path, name: &str, format: Format) -> Target {
        Target::open(TargetConfig {
            name: name.into(),
            s3: None,
            ssh: None,
            dir: Some(DirConfig {
                path: dir.to_string_lossy().into_owned(),
            }),
            prefix: "home/".into(),
            format,
            every: Some("1h".into()),
            full_every: None,
            keep: Keep {
                last: Some(2),
                ..Keep::default()
            },
        })
        .unwrap()
    }

    #[tokio::test]
    async fn uploads_compress_and_failures_leave_nothing() {
        let d = tempfile::tempdir().unwrap();
        let t = dir_target(d.path(), "local", Format::Tar);
        let k = Key::new(Kind::App, "blog", Stamp(1_800_000_000), None, Format::Tar);
        let data: Vec<u8> = (0..3_000_000u32).map(|i| (i % 7) as u8).collect();
        let d2 = data.clone();
        let size = t.upload(&k, Box::new(move |w| Ok(w.write_all(&d2)?))).await.unwrap();
        assert!(size < 100_000, "{size}");
        assert!(d.path().join("home").join(k.to_string()).exists());
        let back = t
            .download(&k, |r| {
                let mut v = Vec::new();
                r.read_to_end(&mut v)?;
                Ok(v)
            })
            .await
            .unwrap();
        assert_eq!(back, data);

        // A producer failing part way through leaves nothing.
        let k2 = Key::new(Kind::App, "blog", Stamp(1_800_000_100), None, Format::Tar);
        let e = t
            .upload(
                &k2,
                Box::new(|w| {
                    w.write_all(&vec![1u8; 5_000_000])?;
                    anyhow::bail!("the snapshot vanished")
                }),
            )
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("the snapshot vanished"), "{e:#}");
        let listed = t.list(Some((Kind::App, "blog"))).await.unwrap();
        assert_eq!(listed.iter().map(|l| &l.key).collect::<Vec<_>>(), [&k]);

        // So does one that panics.
        let k3 = Key::new(Kind::App, "blog", Stamp(1_800_000_200), None, Format::Tar);
        assert!(t.upload(&k3, Box::new(|_| panic!("boom"))).await.is_err());
        assert_eq!(t.list(None).await.unwrap().len(), 1);

        // A corrupt download is an error, not short data.
        std::fs::write(d.path().join("home").join(k.to_string()), b"not zstd").unwrap();
        assert!(t.download(&k, |r| Ok(r.read(&mut [0; 10])?)).await.is_err());
    }

    #[tokio::test]
    async fn an_upload_cut_off_part_way_leaves_nothing_listed() {
        let d = tempfile::tempdir().unwrap();
        let t = std::sync::Arc::new(dir_target(d.path(), "local", Format::Tar));
        let k = Key::new(Kind::App, "blog", Stamp(1_800_000_000), None, Format::Tar);
        // A slow producer, and the upload dropped part way, as when the node stops.
        let (t2, k2) = (t.clone(), k.clone());
        let upload = tokio::spawn(async move {
            t2.upload(
                &k2,
                Box::new(|w| {
                    for _ in 0..1000 {
                        w.write_all(&[3u8; 1 << 20])?;
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Ok(())
                }),
            )
            .await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        upload.abort();
        assert!(upload.await.is_err());
        assert!(t.list(None).await.unwrap().is_empty());
        // The same backup made again is complete.
        t.upload(&k, Box::new(|w| Ok(w.write_all(b"whole")?))).await.unwrap();
        assert_eq!(t.list(None).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn stamps_never_collide_and_retention_runs_per_name() {
        let d = tempfile::tempdir().unwrap();
        let t = dir_target(d.path(), "local", Format::Tar);
        let far = Stamp(Stamp::now().0 + 1000);
        let listing = vec![Listed {
            key: Key::new(Kind::App, "blog", far, None, Format::Tar),
            size: None,
        }];
        assert_eq!(Target::next_stamp(&listing, None), Stamp(far.0 + 1));
        assert!(Target::next_stamp(&[], None) >= Stamp::now());
        for s in 1..=4 {
            let k = Key::new(Kind::App, "blog", Stamp(s * 86_400), None, Format::Tar);
            t.upload(&k, Box::new(|_| Ok(()))).await.unwrap();
        }
        let k = Key::new(Kind::Volume, "corpus", Stamp(86_400), None, Format::Tar);
        t.upload(&k, Box::new(|_| Ok(()))).await.unwrap();
        let gone = t.prune(Kind::App, "blog").await.unwrap();
        assert_eq!(gone.len(), 2);
        assert_eq!(t.list(Some((Kind::App, "blog"))).await.unwrap().len(), 2);
        assert_eq!(t.list(Some((Kind::Volume, "corpus"))).await.unwrap().len(), 1);
    }

    #[test]
    fn configurations_are_checked() {
        let ok = TargetConfig {
            name: "nas".into(),
            s3: None,
            ssh: None,
            dir: Some(DirConfig { path: "/srv/b".into() }),
            prefix: String::new(),
            format: Format::Send,
            every: None,
            full_every: None,
            keep: Keep::default(),
        };
        Target::open(ok.clone()).unwrap();
        for bad in [
            TargetConfig {
                dir: None,
                ..ok.clone()
            },
            TargetConfig {
                name: "../x".into(),
                ..ok.clone()
            },
            TargetConfig {
                prefix: "home".into(),
                ..ok.clone()
            },
            TargetConfig {
                prefix: "../".into(),
                ..ok.clone()
            },
            TargetConfig {
                every: Some("often".into()),
                ..ok.clone()
            },
            TargetConfig {
                every: Some("1s".into()),
                ..ok.clone()
            },
            TargetConfig {
                dir: Some(DirConfig { path: "rel".into() }),
                ..ok.clone()
            },
        ] {
            assert!(Target::open(bad.clone()).is_err(), "{bad:?}");
        }
        assert!(open_all(vec![ok.clone(), ok]).is_err());
    }
}

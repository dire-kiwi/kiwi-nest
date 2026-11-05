//! Where backups are kept: an object store with four operations, and its local-directory form.
//!
//! An upload is fed as [`Chunk`]s and ends with [`Chunk::End`]. Only then may a store make the object visible; a
//! [`Chunk::Failed`], or the feed stopping without an end, means the upload failed, and the store must leave nothing
//! that looks like a complete backup (an S3 multipart upload is aborted; a file is written under a `.partial-` name
//! and only renamed into place at the end).

use std::io;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;

use anyhow::{Context, bail};
use bytes::Bytes;
use futures_util::Stream;
use futures_util::future::BoxFuture;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// The name partial uploads have before they're complete, beside their final name.
pub const PARTIAL_PREFIX: &str = ".partial-";

/// A piece of an upload.
#[derive(Debug)]
pub enum Chunk {
    Data(Bytes),
    /// Everything has been sent: the object may be made visible.
    End,
    /// The data couldn't be made: the upload must be abandoned.
    Failed(String),
}

/// What an upload reads from.
pub type Body = mpsc::Receiver<Chunk>;

/// What a download gives.
pub type ByteStream = Pin<Box<dyn Stream<Item = io::Result<Bytes>> + Send>>;

/// The next piece of data of an upload; `None` at its end. Fails if the feed failed or stopped short.
pub async fn next(body: &mut Body) -> anyhow::Result<Option<Bytes>> {
    match body.recv().await {
        Some(Chunk::Data(b)) => Ok(Some(b)),
        Some(Chunk::End) => Ok(None),
        Some(Chunk::Failed(e)) => bail!("{e}"),
        None => bail!("the backup's data stopped before its end"),
    }
}

/// An object in a store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    pub key: String,
    pub size: Option<u64>,
}

/// An object store. Keys are relative paths with `/` between components.
pub trait Store: Send + Sync {
    /// For messages, such as `s3://bucket` or `backup@nas:/srv/kn`.
    fn describe(&self) -> String;
    /// Every complete object whose key starts with `prefix`.
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<Object>>>;
    /// Uploads an object, replacing any of that key; returns its size.
    fn put<'a>(&'a self, key: &'a str, body: Body) -> BoxFuture<'a, anyhow::Result<u64>>;
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, anyhow::Result<ByteStream>>;
    /// Deleting an object that isn't there is fine.
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, anyhow::Result<()>>;
}

/// Checks a key is a relative path of plain components, none of them hidden.
pub fn check_key(key: &str) -> anyhow::Result<()> {
    let p = Path::new(key);
    let ok = !key.is_empty()
        && !key.contains("//")
        && !key.ends_with('/')
        && p.components()
            .all(|c| matches!(c, Component::Normal(n) if !n.to_string_lossy().starts_with('.')));
    if !ok {
        bail!("`{key}` isn't a key kiwi-nest makes");
    }
    Ok(())
}

/// A directory on this machine.
pub struct DirStore {
    root: PathBuf,
}

impl DirStore {
    pub fn new(root: PathBuf) -> anyhow::Result<DirStore> {
        if !root.is_absolute() {
            bail!("a backup directory must be absolute, not {}", root.display());
        }
        Ok(DirStore { root })
    }

    fn path(&self, key: &str) -> anyhow::Result<PathBuf> {
        check_key(key)?;
        Ok(self.root.join(key))
    }
}

fn walk(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<Object>) -> io::Result<()> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(io::Error::other)?
            .to_string_lossy()
            .into_owned();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            // Only descend where the prefix can still match.
            let as_dir = format!("{rel}/");
            if as_dir.starts_with(prefix) || prefix.starts_with(&as_dir) {
                walk(root, &path, prefix, out)?;
            }
        } else if ft.is_file() && rel.starts_with(prefix) {
            out.push(Object {
                key: rel,
                size: Some(entry.metadata()?.len()),
            });
        }
    }
    Ok(())
}

impl Store for DirStore {
    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<Object>>> {
        Box::pin(async move {
            let (root, prefix) = (self.root.clone(), prefix.to_string());
            tokio::task::spawn_blocking(move || {
                let mut out = Vec::new();
                walk(&root, &root, &prefix, &mut out).with_context(|| format!("listing {}", root.display()))?;
                out.sort_by(|a, b| a.key.cmp(&b.key));
                Ok(out)
            })
            .await?
        })
    }

    fn put<'a>(&'a self, key: &'a str, mut body: Body) -> BoxFuture<'a, anyhow::Result<u64>> {
        Box::pin(async move {
            let dest = self.path(key)?;
            let dir = dest.parent().context("a key has a parent")?.to_path_buf();
            let file = dest
                .file_name()
                .context("a key has a name")?
                .to_string_lossy()
                .into_owned();
            let partial = dir.join(format!("{PARTIAL_PREFIX}{file}"));
            tokio::fs::create_dir_all(&dir)
                .await
                .with_context(|| format!("creating {}", dir.display()))?;
            let written = async {
                let mut f = tokio::fs::File::create(&partial).await?;
                let mut n = 0u64;
                while let Some(b) = next(&mut body).await? {
                    f.write_all(&b).await?;
                    n += b.len() as u64;
                }
                f.flush().await?;
                f.sync_all().await?;
                drop(f);
                tokio::fs::rename(&partial, &dest).await?;
                tokio::fs::File::open(&dir).await?.sync_all().await?;
                anyhow::Ok(n)
            }
            .await;
            if written.is_err() {
                let _ = tokio::fs::remove_file(&partial).await;
            }
            written.with_context(|| format!("writing {}", dest.display()))
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, anyhow::Result<ByteStream>> {
        Box::pin(async move {
            let path = self.path(key)?;
            let f = tokio::fs::File::open(&path)
                .await
                .with_context(|| format!("opening {}", path.display()))?;
            Ok(Box::pin(tokio_util::io::ReaderStream::with_capacity(f, 1 << 20)) as ByteStream)
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let path = self.path(key)?;
            match tokio::fs::remove_file(&path).await {
                Err(e) if e.kind() != io::ErrorKind::NotFound => {
                    Err(e).with_context(|| format!("removing {}", path.display()))
                }
                _ => Ok(()),
            }
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use futures_util::StreamExt;

    use super::*;

    pub(crate) async fn feed(chunks: Vec<Chunk>) -> Body {
        let (tx, rx) = mpsc::channel(chunks.len() + 1);
        for c in chunks {
            tx.send(c).await.unwrap();
        }
        rx
    }

    pub(crate) async fn read_all(s: ByteStream) -> Vec<u8> {
        let parts: Vec<Bytes> = s.map(|r| r.unwrap()).collect().await;
        parts.concat()
    }

    /// What every store must do; run against each.
    pub(crate) async fn conforms(store: &dyn Store) {
        let key = "apps/blog/20261106T030000Z.tar.zst";
        let body = feed(vec![
            Chunk::Data(Bytes::from_static(b"hello ")),
            Chunk::Data(Bytes::from_static(b"world")),
            Chunk::End,
        ])
        .await;
        assert_eq!(store.put(key, body).await.unwrap(), 11);
        assert_eq!(read_all(store.get(key).await.unwrap()).await, b"hello world");

        // A failed upload, or one cut short, leaves nothing, and doesn't replace what's there.
        let failed = "apps/blog/20261107T030000Z.tar.zst";
        let body = feed(vec![
            Chunk::Data(Bytes::from_static(b"half")),
            Chunk::Failed("the snapshot couldn't be read".into()),
        ])
        .await;
        let e = store.put(failed, body).await.unwrap_err();
        assert!(format!("{e:#}").contains("couldn't be read"), "{e:#}");
        let body = feed(vec![Chunk::Data(Bytes::from_static(b"half"))]).await;
        assert!(store.put(failed, body).await.is_err());
        let body = feed(vec![Chunk::Data(Bytes::from_static(b"x")), Chunk::Failed("no".into())]).await;
        assert!(store.put(key, body).await.is_err());
        assert_eq!(read_all(store.get(key).await.unwrap()).await, b"hello world");

        let other = "volumes/corpus/20261106T030000Z.tar.zst";
        store.put(other, feed(vec![Chunk::End]).await).await.unwrap();
        let all = store.list("").await.unwrap();
        let keys: Vec<&str> = all.iter().map(|o| o.key.as_str()).collect();
        assert_eq!(keys, [key, other], "{all:?}");
        let apps = store.list("apps/").await.unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].size, Some(11));
        assert!(store.list("apps/blo/").await.unwrap().is_empty());
        store.delete(key).await.unwrap();
        store.delete(key).await.unwrap();
        assert_eq!(store.list("apps/").await.unwrap(), []);
        store.delete(other).await.unwrap();
        assert_eq!(store.list("").await.unwrap(), []);
    }

    #[tokio::test]
    async fn a_directory_store_conforms() {
        let d = tempfile::tempdir().unwrap();
        let store = DirStore::new(d.path().join("backups")).unwrap();
        conforms(&store).await;
        // No partial files are left behind.
        let left: Vec<_> = std::fs::read_dir(d.path().join("backups/apps/blog"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
        assert!(store.path("../x").is_err() && store.path("/x").is_err() && store.path("a/.partial-x").is_err());
    }
}

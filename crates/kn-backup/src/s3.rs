//! An S3-compatible bucket (S3, R2, MinIO, …), with requests signed by `rusty-s3` and sent with `reqwest`.
//!
//! An upload that fits in one part is a single `PUT`. A larger one is a multipart upload, completed only once the
//! data has all been sent, and aborted if anything fails, so it never leaves an object behind. (Parts of an upload
//! cut off by a crash stay in the bucket, invisible, until a lifecycle rule aborts them; give the bucket one.)

use std::time::Duration;

use anyhow::{Context, bail};
use bytes::{Bytes, BytesMut};
use futures_util::StreamExt;
use futures_util::future::BoxFuture;
use kn_proto::backup::S3Config;
use rusty_s3::actions::{
    AbortMultipartUpload, CompleteMultipartUpload, CompleteMultipartUploadResponse, CreateMultipartUpload,
    DeleteObject, GetObject, ListObjectsV2, PutObject, UploadPart,
};
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

use crate::store::{Body, ByteStream, Object, Store, check_key, next};

/// How long each signed request stays valid.
const SIGNED_FOR: Duration = Duration::from_secs(3600);

/// The size of the first parts of a multipart upload. Parts grow as the upload does, since S3 allows 10,000.
const FIRST_PART: usize = 16 << 20;
const MAX_PART: usize = 512 << 20;

fn part_size(parts_done: usize) -> usize {
    (FIRST_PART << (parts_done / 1000).min(5)).min(MAX_PART)
}

pub struct S3Store {
    bucket: Bucket,
    credentials: Credentials,
    http: reqwest::Client,
    /// The size parts start at (smaller in tests).
    first_part: usize,
}

impl S3Store {
    pub fn new(cfg: &S3Config) -> anyhow::Result<S3Store> {
        let endpoint: url::Url = cfg
            .endpoint
            .parse()
            .with_context(|| format!("the S3 endpoint {:?} isn't a URL", cfg.endpoint))?;
        let style = if cfg.virtual_host {
            UrlStyle::VirtualHost
        } else {
            UrlStyle::Path
        };
        let bucket = Bucket::new(endpoint, style, cfg.bucket.clone(), cfg.region.clone())
            .map_err(|e| anyhow::anyhow!("the S3 bucket {:?} at {:?}: {e:?}", cfg.bucket, cfg.endpoint))?;
        let secret = match (&cfg.secret_key, &cfg.secret_key_file) {
            (Some(s), None) => s.clone(),
            (None, Some(f)) => std::fs::read_to_string(f)
                .with_context(|| format!("reading the S3 secret key from {f}"))?
                .trim()
                .to_string(),
            _ => bail!("an S3 target needs one of `secret_key` and `secret_key_file`"),
        };
        let _ = rustls_provider();
        let http = reqwest::Client::builder()
            .user_agent(format!("kiwi-nest/{}", kn_proto::VERSION))
            .connect_timeout(Duration::from_secs(30))
            .read_timeout(Duration::from_secs(300))
            .build()?;
        Ok(S3Store {
            bucket,
            credentials: Credentials::new(cfg.access_key.clone(), secret),
            http,
            first_part: FIRST_PART,
        })
    }

    #[cfg(test)]
    pub fn with_first_part(mut self, size: usize) -> Self {
        self.first_part = size;
        self
    }

    fn part_size(&self, done: usize) -> usize {
        if self.first_part == FIRST_PART {
            part_size(done)
        } else {
            self.first_part
        }
    }

    async fn check(&self, what: &str, resp: reqwest::Response) -> anyhow::Result<reqwest::Response> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        let code = between(&body, "<Code>", "</Code>").unwrap_or_default();
        let message = between(&body, "<Message>", "</Message>").unwrap_or_default();
        bail!(
            "S3 {what} in bucket {}: {status}{}{}",
            self.bucket.name(),
            if code.is_empty() {
                String::new()
            } else {
                format!(" {code}")
            },
            if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            }
        )
    }

    async fn put_part(&self, key: &str, number: u16, upload_id: &str, data: Bytes) -> anyhow::Result<String> {
        let mut attempt = 0;
        loop {
            let url = UploadPart::new(&self.bucket, Some(&self.credentials), key, number, upload_id).sign(SIGNED_FOR);
            let r = async {
                let resp = self.http.put(url).body(data.clone()).send().await?;
                let resp = self.check(&format!("uploading part {number} of {key}"), resp).await?;
                let etag = resp
                    .headers()
                    .get(reqwest::header::ETAG)
                    .and_then(|v| v.to_str().ok())
                    .context("S3 answered an uploaded part without its ETag")?;
                anyhow::Ok(etag.to_string())
            }
            .await;
            match r {
                Ok(etag) => return Ok(etag),
                Err(e) if attempt >= 2 => return Err(e),
                Err(e) => {
                    attempt += 1;
                    tracing::warn!(key, part = number, error = %e, "uploading a part failed; trying again");
                    tokio::time::sleep(Duration::from_secs(2 * attempt)).await;
                }
            }
        }
    }

    async fn multipart(&self, key: &str, first: Bytes, body: &mut Body) -> anyhow::Result<u64> {
        let url = CreateMultipartUpload::new(&self.bucket, Some(&self.credentials), key).sign(SIGNED_FOR);
        let resp = self.http.post(url).send().await?;
        let text = self
            .check(&format!("starting to upload {key}"), resp)
            .await?
            .text()
            .await?;
        let created = CreateMultipartUpload::parse_response(&text)
            .map_err(|e| anyhow::anyhow!("S3's answer to starting an upload: {e}"))?;
        let upload_id = created.upload_id().to_string();
        let uploaded = self.parts(key, &upload_id, first, body).await;
        match uploaded {
            Ok(n) => Ok(n),
            Err(e) => {
                let url =
                    AbortMultipartUpload::new(&self.bucket, Some(&self.credentials), key, &upload_id).sign(SIGNED_FOR);
                match self.http.delete(url).send().await {
                    Ok(r) if r.status().is_success() => tracing::info!(key, "aborted the failed upload"),
                    Ok(r) => tracing::warn!(key, status = %r.status(), "couldn't abort the failed upload"),
                    Err(err) => tracing::warn!(key, error = %err, "couldn't abort the failed upload"),
                }
                Err(e)
            }
        }
    }

    async fn parts(&self, key: &str, upload_id: &str, first: Bytes, body: &mut Body) -> anyhow::Result<u64> {
        let mut etags = Vec::new();
        let mut total = first.len() as u64;
        etags.push(self.put_part(key, 1, upload_id, first).await?);
        let mut buf = BytesMut::new();
        let mut ended = false;
        while !ended {
            let want = self.part_size(etags.len());
            while buf.len() < want {
                match next(body).await? {
                    Some(b) => buf.extend_from_slice(&b),
                    None => {
                        ended = true;
                        break;
                    }
                }
            }
            if buf.is_empty() {
                break;
            }
            if etags.len() >= 10_000 {
                bail!("the backup is too large for one S3 object");
            }
            let part = buf.split().freeze();
            total += part.len() as u64;
            let number = etags.len() as u16 + 1;
            etags.push(self.put_part(key, number, upload_id, part).await?);
        }
        let action = CompleteMultipartUpload::new(
            &self.bucket,
            Some(&self.credentials),
            key,
            upload_id,
            etags.iter().map(String::as_str),
        );
        let url = action.sign(SIGNED_FOR);
        let resp = self.http.post(url).body(action.body()).send().await?;
        let text = self
            .check(&format!("finishing the upload of {key}"), resp)
            .await?
            .text()
            .await?;
        // S3 can answer 200 and still fail, with the error in the body.
        CompleteMultipartUploadResponse::parse(&text)
            .map_err(|e| anyhow::anyhow!("S3 didn't finish the upload of {key}: {e}: {}", text.trim()))?;
        Ok(total)
    }
}

/// The text between two markers, for S3's XML errors.
fn between<'a>(s: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let a = s.find(start)? + start.len();
    let b = s[a..].find(end)? + a;
    Some(&s[a..b])
}

/// The process's TLS provider (ring), installed once.
fn rustls_provider() -> bool {
    rustls::crypto::ring::default_provider().install_default().is_ok()
}

impl Store for S3Store {
    fn describe(&self) -> String {
        format!("s3 bucket {} at {}", self.bucket.name(), self.bucket.base_url())
    }

    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<Object>>> {
        Box::pin(async move {
            let mut out = Vec::new();
            let mut token: Option<String> = None;
            loop {
                let mut action = ListObjectsV2::new(&self.bucket, Some(&self.credentials));
                if !prefix.is_empty() {
                    action.with_prefix(prefix);
                }
                if let Some(t) = &token {
                    action.with_continuation_token(t.clone());
                }
                let url = action.sign(SIGNED_FOR);
                let resp = self.http.get(url).send().await?;
                let text = self.check("listing", resp).await?.text().await?;
                let page = ListObjectsV2::parse_response(&text).map_err(|e| anyhow::anyhow!("S3's listing: {e}"))?;
                out.extend(page.contents.into_iter().map(|c| Object {
                    key: c.key,
                    size: c.size,
                }));
                match page.next_continuation_token {
                    Some(t) => token = Some(t),
                    None => break,
                }
            }
            out.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(out)
        })
    }

    fn put<'a>(&'a self, key: &'a str, mut body: Body) -> BoxFuture<'a, anyhow::Result<u64>> {
        Box::pin(async move {
            check_key(key)?;
            // The first part: if the data ends within it, one PUT does.
            let mut buf = BytesMut::new();
            let mut ended = false;
            while buf.len() < self.part_size(0) {
                match next(&mut body).await? {
                    Some(b) => buf.extend_from_slice(&b),
                    None => {
                        ended = true;
                        break;
                    }
                }
            }
            let first = buf.freeze();
            if !ended {
                return self.multipart(key, first, &mut body).await;
            }
            let n = first.len() as u64;
            let url = PutObject::new(&self.bucket, Some(&self.credentials), key).sign(SIGNED_FOR);
            let resp = self.http.put(url).body(first).send().await?;
            self.check(&format!("uploading {key}"), resp).await?;
            Ok(n)
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, anyhow::Result<ByteStream>> {
        Box::pin(async move {
            check_key(key)?;
            let url = GetObject::new(&self.bucket, Some(&self.credentials), key).sign(SIGNED_FOR);
            let resp = self.http.get(url).send().await?;
            let resp = self.check(&format!("downloading {key}"), resp).await?;
            let stream = resp.bytes_stream().map(|r| r.map_err(std::io::Error::other));
            Ok(Box::pin(stream) as ByteStream)
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            check_key(key)?;
            let url = DeleteObject::new(&self.bucket, Some(&self.credentials), key).sign(SIGNED_FOR);
            let resp = self.http.delete(url).send().await?;
            self.check(&format!("deleting {key}"), resp).await?;
            Ok(())
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::store::Chunk;

    #[test]
    fn parts_grow() {
        assert_eq!(part_size(0), 16 << 20);
        assert_eq!(part_size(999), 16 << 20);
        assert_eq!(part_size(1000), 32 << 20);
        assert_eq!(part_size(9999), 512 << 20);
        assert!((0..10_000).map(part_size).sum::<usize>() > 1 << 40);
    }

    /// `KIWI_NEST_TEST_S3`: `ENDPOINT BUCKET ACCESS_KEY SECRET_KEY`, such as a MinIO's, for the tests against a
    /// real S3-compatible server.
    pub(crate) fn real() -> Option<S3Config> {
        let Ok(v) = std::env::var("KIWI_NEST_TEST_S3") else {
            eprintln!("skipped: set KIWI_NEST_TEST_S3 to 'ENDPOINT BUCKET ACCESS SECRET' to test against S3");
            return None;
        };
        let parts: Vec<&str> = v.split_whitespace().collect();
        let [endpoint, bucket, access, secret] = parts[..] else {
            panic!("KIWI_NEST_TEST_S3 should be 'ENDPOINT BUCKET ACCESS SECRET'");
        };
        Some(S3Config {
            endpoint: endpoint.into(),
            bucket: bucket.into(),
            region: "us-east-1".into(),
            access_key: access.into(),
            secret_key: Some(secret.into()),
            secret_key_file: None,
            virtual_host: false,
        })
    }

    #[tokio::test]
    async fn a_real_s3_store_conforms_and_aborts_failed_multipart_uploads() {
        let Some(cfg) = real() else { return };
        let prefix = format!("kn-test-{}/", std::process::id());
        let store = S3Store::new(&cfg).unwrap().with_first_part(5 << 20);
        let scoped = Scoped(&store, &prefix);
        crate::store::tests::conforms(&scoped).await;

        // A multipart upload (three parts) that works…
        let big = Bytes::from(vec![7u8; 12 << 20]);
        let key = format!("{prefix}apps/big/20261106T030000Z.tar.zst");
        let body = crate::store::tests::feed(vec![Chunk::Data(big.clone()), Chunk::End]).await;
        assert_eq!(store.put(&key, body).await.unwrap(), 12 << 20);
        let got = crate::store::tests::read_all(store.get(&key).await.unwrap()).await;
        assert_eq!(got.len(), 12 << 20);
        // …and one that fails after its first part: nothing is left.
        let failed = format!("{prefix}apps/big/20261107T030000Z.tar.zst");
        let body = crate::store::tests::feed(vec![Chunk::Data(big), Chunk::Failed("cut off".into())]).await;
        assert!(store.put(&failed, body).await.is_err());
        let left = store.list(&format!("{prefix}apps/big/")).await.unwrap();
        assert_eq!(left.len(), 1, "{left:?}");
        store.delete(&key).await.unwrap();
    }

    /// A store with its keys under a prefix, so tests share a bucket.
    pub(crate) struct Scoped<'a>(pub &'a S3Store, pub &'a str);

    impl Store for Scoped<'_> {
        fn describe(&self) -> String {
            self.0.describe()
        }
        fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, anyhow::Result<Vec<Object>>> {
            Box::pin(async move {
                let all = self.0.list(&format!("{}{prefix}", self.1)).await?;
                Ok(all
                    .into_iter()
                    .map(|o| Object {
                        key: o.key[self.1.len()..].to_string(),
                        size: o.size,
                    })
                    .collect())
            })
        }
        fn put<'a>(&'a self, key: &'a str, body: Body) -> BoxFuture<'a, anyhow::Result<u64>> {
            Box::pin(async move { self.0.put(&format!("{}{key}", self.1), body).await })
        }
        fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, anyhow::Result<ByteStream>> {
            Box::pin(async move { self.0.get(&format!("{}{key}", self.1)).await })
        }
        fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, anyhow::Result<()>> {
            Box::pin(async move { self.0.delete(&format!("{}{key}", self.1)).await })
        }
    }
}

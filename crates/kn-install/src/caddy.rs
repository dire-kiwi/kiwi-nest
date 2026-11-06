//! The ingress's Caddy: a pinned release from Caddy's GitHub releases, checked against the SHA-512 recorded here
//! (and so not only against the checksums file beside it), unpacked to [`crate::systemd::CADDY_BIN`].
//!
//! A distribution's `caddy` package isn't used, since it starts a Caddy of its own on port 80.

use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, bail};
use sha2::{Digest, Sha512};

/// The Caddy release `server init` installs.
pub const VERSION: &str = "2.11.9";

/// The SHA-512 of each Linux tarball of [`VERSION`], from its `caddy_VERSION_checksums.txt`.
const TARBALLS: [(&str, &str); 2] = [
    (
        "amd64",
        "0e85544340290a16ce4bfb76fe7d65e9de1bf3a6e67f5ccfff30b30d27ea4c73e078077b7bf3e58ae60b6b12e44327705287059c775bca5782e8ee85ea7edd75",
    ),
    (
        "arm64",
        "9e5e76d7888542924de2c25353ae87c5f77e2a22c5b1198633cae13688922282e05383d142599205c88d8d80eee5e967d291214d7abce4835329a191b20dd6e0",
    ),
];

fn arch() -> anyhow::Result<&'static str> {
    match std::env::consts::ARCH {
        "x86_64" => Ok("amd64"),
        "aarch64" => Ok("arm64"),
        a => bail!("there's no pinned Caddy for {a}: install one and pass --caddy"),
    }
}

/// Downloads the pinned Caddy, checks it, and installs it at `dest`.
pub async fn download(http: &reqwest::Client, dest: &Path) -> anyhow::Result<()> {
    let arch = arch()?;
    let want = TARBALLS.iter().find(|(a, _)| *a == arch).expect("a pinned tarball").1;
    let name = format!("caddy_{VERSION}_linux_{arch}.tar.gz");
    let url = format!("https://github.com/caddyserver/caddy/releases/download/v{VERSION}/{name}");
    let resp = http
        .get(&url)
        .send()
        .await
        .with_context(|| format!("downloading {url}"))?;
    if !resp.status().is_success() {
        bail!("downloading {url}: {}", resp.status());
    }
    let bytes = resp.bytes().await.with_context(|| format!("downloading {url}"))?;
    let got = hex::encode(Sha512::digest(&bytes));
    if got != want {
        bail!("{name} doesn't match the SHA-512 kiwi-nest pins for it: got {got}");
    }
    let binary = unpack(&bytes).with_context(|| format!("unpacking {name}"))?;
    install_bytes(&binary, dest)
}

/// The `caddy` file in a release tarball.
fn unpack(tarball: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tarball));
    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() && entry.path()?.as_os_str() == "caddy" {
            let mut out = Vec::new();
            entry.read_to_end(&mut out)?;
            return Ok(out);
        }
    }
    bail!("there's no caddy in it")
}

/// Installs a Caddy binary that's already on the machine (`--caddy PATH`).
pub fn install_file(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if std::fs::canonicalize(src).ok() == std::fs::canonicalize(dest).ok() && dest.exists() {
        return Ok(());
    }
    let bytes = std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;
    install_bytes(&bytes, dest)
}

fn install_bytes(bytes: &[u8], dest: &Path) -> anyhow::Result<()> {
    crate::layout::write_atomic(dest, bytes, 0o755)?;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// The installed Caddy's version, which also checks it runs.
pub fn version(bin: &Path) -> anyhow::Result<String> {
    let out = Command::new(bin)
        .arg("version")
        .output()
        .with_context(|| format!("running {} version", bin.display()))?;
    if !out.status.success() {
        bail!(
            "{} version failed: {}",
            bin.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unpacks_caddy_from_a_tarball() {
        let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (name, body) in [("LICENSE", &b"license"[..]), ("caddy", b"#!/bin/sh\n")] {
            let mut h = tar::Header::new_gnu();
            h.set_size(body.len() as u64);
            h.set_mode(0o755);
            h.set_cksum();
            tar.append_data(&mut h, name, body).unwrap();
        }
        let gz = tar.into_inner().unwrap().finish().unwrap();
        assert_eq!(unpack(&gz).unwrap(), b"#!/bin/sh\n");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("caddy/caddy");
        install_bytes(&unpack(&gz).unwrap(), &dest).unwrap();
        assert_eq!(std::fs::metadata(&dest).unwrap().permissions().mode() & 0o777, 0o755);
    }
}

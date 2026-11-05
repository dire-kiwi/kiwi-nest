//! `SHA256SUMS`: lines of `HEX  NAME` (or `HEX *NAME`, as `sha256sum --binary` writes), as a release publishes.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, bail};
use sha2::{Digest, Sha256};

/// Parses a `SHA256SUMS` file into file name → lowercase hex digest.
pub fn parse(text: &str) -> anyhow::Result<BTreeMap<String, String>> {
    let mut sums = BTreeMap::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (hex, name) = line
            .split_once(char::is_whitespace)
            .with_context(|| format!("line {} of SHA256SUMS isn't `DIGEST  NAME`", i + 1))?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("line {} of SHA256SUMS has no SHA-256 digest", i + 1);
        }
        if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
            bail!("line {} of SHA256SUMS names {name:?}, which isn't a file name", i + 1);
        }
        if sums.insert(name.to_string(), hex.to_ascii_lowercase()).is_some() {
            bail!("SHA256SUMS lists {name} twice");
        }
    }
    Ok(sums)
}

/// The SHA-256 of a file, in lowercase hex, and its size.
pub fn sha256_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        size += n as u64;
    }
    Ok((hex::encode(h.finalize()), size))
}

/// Checks a file against its expected SHA-256.
pub fn verify_file(path: &Path, expected: &str) -> anyhow::Result<u64> {
    let (got, size) = sha256_file(path).with_context(|| format!("reading {}", path.display()))?;
    if !got.eq_ignore_ascii_case(expected) {
        bail!(
            "{} doesn't match its checksum: its SHA-256 is {got}, and SHA256SUMS says {expected}",
            path.display()
        );
    }
    Ok(size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_styles_and_refuses_nonsense() {
        let a = "a".repeat(64);
        let b = "B".repeat(64);
        let sums = parse(&format!("{a}  kiwi-nest-0.2.0-x86_64-unknown-linux-musl\n{b} *SHA\n\n")).unwrap();
        assert_eq!(sums["kiwi-nest-0.2.0-x86_64-unknown-linux-musl"], a);
        assert_eq!(sums["SHA"], "b".repeat(64));
        for bad in [
            "abc  file".to_string(),
            format!("{a}"),
            format!("{a}  ../etc/passwd"),
            format!("{a}  dir/file"),
            format!("{a}  x\n{a}  x"),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn checks_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"hello").unwrap();
        let (sum, size) = sha256_file(&p).unwrap();
        assert_eq!(size, 5);
        assert_eq!(sum, "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        assert_eq!(verify_file(&p, &sum.to_uppercase()).unwrap(), 5);
        let err = verify_file(&p, &"0".repeat(64)).unwrap_err().to_string();
        assert!(err.contains("doesn't match"), "{err}");
    }
}

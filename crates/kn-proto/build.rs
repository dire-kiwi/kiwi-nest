//! Records the git commit the binary is built from, for `kiwi-nest version`.
//!
//! `KIWI_NEST_COMMIT` overrides it, for builds outside a git checkout.

use std::path::Path;
use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn watch_git_path(path: &str) {
    if let Some(p) = git(&["rev-parse", "--path-format=absolute", "--git-path", path])
        && Path::new(&p).exists()
    {
        println!("cargo:rerun-if-changed={p}");
    }
}

fn main() {
    println!("cargo:rerun-if-env-changed=KIWI_NEST_COMMIT");
    let commit = match std::env::var("KIWI_NEST_COMMIT") {
        Ok(c) if !c.is_empty() => c,
        _ => {
            watch_git_path("HEAD");
            watch_git_path("packed-refs");
            if let Some(r) = git(&["symbolic-ref", "-q", "HEAD"]) {
                watch_git_path(&r);
            }
            git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".to_string())
        }
    };
    println!("cargo:rustc-env=KN_GIT_COMMIT={commit}");
}

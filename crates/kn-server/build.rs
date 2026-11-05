//! Chooses the web UI to embed: `web/dist` once it's been built (`make web`), otherwise a page saying it wasn't, so
//! the server builds (and its tests run) without Node.

use std::path::Path;

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let web = manifest.join("../../web");
    let dist = web.join("dist");
    let dir = if dist.join("index.html").is_file() {
        // Rebuilt when the UI is.
        println!("cargo:rerun-if-changed={}", dist.display());
        dist
    } else {
        // Until it's built: `web` changes when `dist` appears in it.
        println!("cargo:rerun-if-changed={}", web.display());
        manifest.join("ui-missing")
    };
    let dir = dir.canonicalize().expect("the UI's directory");
    println!("cargo:rustc-env=KN_UI_DIR={}", dir.display());
    println!("cargo:rerun-if-changed=build.rs");
}

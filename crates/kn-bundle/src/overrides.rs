//! Changing a bundle's `x-kiwi-nest` when it's deployed (`kiwi-nest deploy --domain …`), so a pull request's
//! preview can be deployed from production's bundle: its own domain, idling out, and no scheduled backups.
//!
//! The CLI rewrites `compose.yaml` before it packs the bundle, so the server and the node see an ordinary bundle
//! and check it as they check any other. The file is written again from its parsed form, with merge keys (`<<`)
//! applied and anchors expanded, which Compose reads the same way; comments are lost.

use std::path::Path;

use serde_yaml_ng::Value;

use crate::manifest::{is_valid_domain, parse_duration};
use crate::{BundleError, COMPOSE_FILE, Limits};

/// What to change in `x-kiwi-nest`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overrides {
    pub domain: Option<String>,
    pub idle_timeout: Option<String>,
    pub backups: Option<bool>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self == &Overrides::default()
    }
}

/// `compose.yaml` with `x-kiwi-nest` changed.
pub fn apply(yaml: &str, o: &Overrides) -> Result<String, String> {
    if let Some(d) = &o.domain
        && !is_valid_domain(d)
    {
        return Err(format!(
            "`{d}` isn't a valid domain: use lowercase letters, digits, `-` and `.`"
        ));
    }
    if let Some(t) = &o.idle_timeout
        && parse_duration(t).is_none()
    {
        return Err(format!("`{t}` isn't a duration, such as 10m or 0"));
    }
    let mut doc: Value = serde_yaml_ng::from_str(yaml).map_err(|e| format!("compose.yaml isn't valid YAML: {e}"))?;
    doc.apply_merge()
        .map_err(|e| format!("compose.yaml has a bad merge key (`<<`): {e}"))?;
    let Some(m) = doc.get_mut("x-kiwi-nest").and_then(Value::as_mapping_mut) else {
        return Err("compose.yaml has no `x-kiwi-nest` mapping to change".into());
    };
    if let Some(d) = &o.domain {
        m.insert("domain".into(), Value::String(d.clone()));
    }
    if let Some(t) = &o.idle_timeout {
        m.insert("idle_timeout".into(), Value::String(t.clone()));
    }
    if let Some(b) = o.backups {
        m.insert("backups".into(), Value::Bool(b));
    }
    let body = serde_yaml_ng::to_string(&doc).map_err(|e| format!("writing compose.yaml: {e}"))?;
    Ok(format!("# x-kiwi-nest changed by `kiwi-nest deploy`.\n{body}"))
}

/// Packs a bundle (a directory, or a ZIP) into a ZIP with `x-kiwi-nest` changed, refusing what a node would refuse.
pub fn pack(src: &Path, o: &Overrides, limits: &Limits) -> Result<Vec<u8>, BundleError> {
    let tmp = tempfile::tempdir().map_err(|e| BundleError::io("making a temporary directory", e))?;
    let root = tmp.path().join("bundle");
    std::fs::create_dir(&root).map_err(|e| BundleError::io(root.display(), e))?;
    let meta = std::fs::metadata(src).map_err(|e| BundleError::io(src.display(), e))?;
    if meta.is_dir() {
        crate::dir::copy(src, &root, limits)?;
    } else {
        let f = std::fs::File::open(src).map_err(|e| BundleError::io(src.display(), e))?;
        crate::zip::extract(f, &root, limits)?;
    }
    let path = root.join(COMPOSE_FILE);
    let yaml = std::fs::read_to_string(&path).map_err(|e| BundleError::io(COMPOSE_FILE, e))?;
    let changed = apply(&yaml, o).map_err(BundleError::Overrides)?;
    std::fs::write(&path, changed).map_err(|e| BundleError::io(COMPOSE_FILE, e))?;
    crate::dir::pack(&root, limits)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BUNDLE: &str = "\
x-kiwi-nest:
  domain: blog.example.com
  service: web
  port: 8080
x-common: &common
  mem_limit: 128m
services:
  web:
    <<: *common
    image: nginx
    command: [\"sh\", \"-c\", \"echo $$HOME\"]
";

    #[test]
    fn changes_only_what_it_is_told() {
        let o = Overrides {
            domain: Some("blog-pr12.example.com".into()),
            idle_timeout: Some("10m".into()),
            backups: Some(false),
        };
        let out = apply(BUNDLE, &o).unwrap();
        let v: Value = serde_yaml_ng::from_str(&out).unwrap();
        let m = &v["x-kiwi-nest"];
        assert_eq!(m["domain"].as_str(), Some("blog-pr12.example.com"));
        assert_eq!(m["idle_timeout"].as_str(), Some("10m"));
        assert_eq!(m["backups"].as_bool(), Some(false));
        assert_eq!(m["port"].as_u64(), Some(8080));
        // Merge keys are applied, and `$$` stays as it was.
        assert_eq!(v["services"]["web"]["mem_limit"].as_str(), Some("128m"));
        assert!(v["services"]["web"].get("<<").is_none());
        assert_eq!(v["services"]["web"]["command"][2].as_str(), Some("echo $$HOME"));

        let only = apply(
            BUNDLE,
            &Overrides {
                domain: Some("x.example.com".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let v: Value = serde_yaml_ng::from_str(&only).unwrap();
        assert!(v["x-kiwi-nest"].get("idle_timeout").is_none());
    }

    #[test]
    fn refuses_bad_values_and_bundles_without_a_manifest() {
        let bad = |o: Overrides| apply(BUNDLE, &o).unwrap_err();
        assert!(
            bad(Overrides {
                domain: Some("Bad_Domain".into()),
                ..Default::default()
            })
            .contains("valid domain")
        );
        assert!(
            bad(Overrides {
                idle_timeout: Some("soon".into()),
                ..Default::default()
            })
            .contains("duration")
        );
        let err = apply("services: {}\n", &Overrides::default()).unwrap_err();
        assert!(err.contains("no `x-kiwi-nest`"), "{err}");
    }

    #[test]
    fn packs_a_directory_or_a_zip_with_the_change() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("b");
        std::fs::create_dir_all(dir.join("site")).unwrap();
        std::fs::write(dir.join(COMPOSE_FILE), BUNDLE).unwrap();
        std::fs::write(dir.join("site/index.html"), "hi").unwrap();
        let o = Overrides {
            domain: Some("blog-pr1.example.com".into()),
            ..Default::default()
        };
        let limits = Limits::default();
        let zip = pack(&dir, &o, &limits).unwrap();
        let zip_path = tmp.path().join("b.zip");
        std::fs::write(&zip_path, &zip).unwrap();
        for packed in [zip, pack(&zip_path, &o, &limits).unwrap()] {
            let out = tmp.path().join(format!("out-{}", packed.len()));
            std::fs::create_dir(&out).unwrap();
            let index = crate::zip::extract(std::io::Cursor::new(packed), &out, &limits).unwrap();
            assert!(index.is_file("site/index.html"));
            let yaml = std::fs::read_to_string(out.join(COMPOSE_FILE)).unwrap();
            assert!(yaml.contains("blog-pr1.example.com"), "{yaml}");
            let _ = std::fs::remove_dir_all(&out);
        }
    }
}

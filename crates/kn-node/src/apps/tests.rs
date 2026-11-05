//! Tests of apps and shared volumes, with a fake `docker` and the fake subvolume backend.

use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::volumes::fake::Fake;

/// A stand-in for `docker` that logs its arguments to `calls` and succeeds, unless `fail-down`, `fail-up`,
/// `fail-pause` or `fail-unpause` exists beside it. `ps` reports a running container while `running` exists.
const FAKE_DOCKER: &str = r#"#!/bin/sh
dir=$(dirname "$0")
echo "$*" >> "$dir/calls"
case " $* " in
  *" down "*) if [ -e "$dir/fail-down" ]; then echo "down failed" >&2; exit 1; fi ;;
  *" up "*) if [ -e "$dir/fail-up" ]; then echo "up failed" >&2; exit 1; fi ;;
  *" unpause "*) if [ -e "$dir/fail-unpause" ]; then echo "unpause failed" >&2; exit 1; fi ;;
  *" pause "*) if [ -e "$dir/fail-pause" ]; then echo "pause failed" >&2; exit 1; fi ;;
  *" ps "*) if [ -e "$dir/running" ]; then echo '{"ID":"x","Name":"c","Service":"db","State":"running"}'; fi ;;
esac
exit 0
"#;

struct Env {
    dir: tempfile::TempDir,
    cfg: NodeConfig,
    fake: Arc<Fake>,
    apps: Apps,
}

/// A bundle with one service mounting `volumes` (each at `/VOL`), and `shared` as `x-kiwi-nest.shared`.
fn compose(domain: &str, volumes: &[&str], shared: &str) -> String {
    let mounts: Vec<String> = volumes.iter().map(|v| format!("\"{v}:/{v}\"")).collect();
    let declared: String = volumes.iter().map(|v| format!("  {v}: {{}}\n")).collect();
    let shared = if shared.is_empty() {
        String::new()
    } else {
        format!(", shared: {shared}")
    };
    format!(
        "x-kiwi-nest: {{ domain: {domain}, service: web, port: 80{shared} }}\n\
         services:\n  web:\n    image: x\n    volumes: [\"./site:/www\"{}]\n\
         volumes:\n{declared}",
        mounts.iter().map(|m| format!(", {m}")).collect::<String>()
    )
}

impl Env {
    async fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let docker = root.join("docker");
        std::fs::write(&docker, FAKE_DOCKER).unwrap();
        std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut cfg = NodeConfig::new(root.join("state"));
        cfg.docker = docker;
        let fake = Arc::new(Fake::default());
        let apps = Apps::open_with(cfg.clone(), fake.clone()).await.unwrap();
        Env { dir, cfg, fake, apps }
    }

    /// Opens the node again, as after a restart.
    async fn restart(&mut self) {
        self.apps = Apps::open_with(self.cfg.clone(), self.fake.clone()).await.unwrap();
    }

    fn path(&self, p: &str) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap().join(p)
    }

    fn vols(&self) -> &Volumes {
        self.apps.volumes()
    }

    fn flag(&self, name: &str, on: bool) {
        if on {
            std::fs::write(self.path(name), "").unwrap();
        } else {
            let _ = std::fs::remove_file(self.path(name));
        }
    }

    /// The `docker` calls made since the last time, each as its words after `--file …` (such as `pause`).
    fn calls(&self) -> Vec<String> {
        let text = std::fs::read_to_string(self.path("calls")).unwrap_or_default();
        let _ = std::fs::remove_file(self.path("calls"));
        text.lines()
            .map(|l| {
                let words: Vec<&str> = l.split_whitespace().collect();
                let start = words
                    .iter()
                    .position(|w| *w == "--file")
                    .map(|i| i + 2)
                    .or_else(|| words.iter().position(|w| *w == "--project-name").map(|i| i + 2))
                    .unwrap_or(0);
                format!("{} {}", project_of(&words), words[start..].join(" "))
            })
            .collect()
    }

    async fn deploy_yaml(&self, name: &str, yaml: &str, from: Option<&str>) -> Result<AppInfo> {
        let b = self.path(&format!("bundle-{name}"));
        std::fs::create_dir_all(b.join("site")).unwrap();
        std::fs::write(b.join("site/index.html"), "page").unwrap();
        std::fs::write(b.join("compose.yaml"), yaml).unwrap();
        let staged = self.apps.stage_dir(b).await?;
        let opts = DeployOptions {
            create_only: false,
            from: from.map(str::to_string),
            restore: None,
            generation: None,
        };
        self.apps.deploy(name, staged, opts).await
    }

    async fn deploy(&self, name: &str, volumes: &[&str], from: Option<&str>) -> Result<AppInfo> {
        self.deploy_yaml(name, &compose(&format!("{name}.localhost"), volumes, ""), from)
            .await
    }

    fn generation_files(&self, app: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.path(&format!("state/apps/{app}")))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn volume(&self, app: &str, vol: &str) -> PathBuf {
        self.vols().app_volume(app, vol)
    }

    async fn record(&self, app: &str) -> AppRecord {
        self.apps.inner.state.lock().await.apps[app].clone()
    }

    fn generated(&self, app: &str, generation: u64) -> String {
        std::fs::read_to_string(self.path(&format!("state/apps/{app}/compose-{generation}.yaml"))).unwrap()
    }
}

fn project_of(words: &[&str]) -> String {
    words
        .iter()
        .position(|w| *w == "--project-name")
        .map(|i| words[i + 1].trim_start_matches("kn-").to_string())
        .unwrap_or_default()
}

fn has(calls: &[String], call: &str) -> bool {
    calls.iter().any(|c| c == call)
}

#[tokio::test]
async fn a_redeploy_that_fails_before_stopping_keeps_the_running_generation() {
    let env = Env::new().await;
    assert_eq!(env.deploy("app", &["data"], None).await.unwrap().generation, 1);
    std::fs::write(env.volume("app", "data").join("db"), "data").unwrap();

    // `down` of generation 1 fails: its containers may still be running from its bundle, so that stays.
    env.flag("fail-down", true);
    let e = env.deploy("app", &["data"], None).await.unwrap_err();
    assert!(
        matches!(&e, AppsError::DeployFailed { message, .. } if message.contains("down failed")),
        "{e}"
    );
    assert_eq!(env.generation_files("app"), ["bundle-1", "compose-1.yaml"]);
    let info = env.apps.inspect("app").await.unwrap();
    assert_eq!(info.generation, 1);
    assert_eq!(info.status, AppStatus::Running);
    assert!(
        info.error
            .as_deref()
            .unwrap()
            .contains("generation 1 is still in place"),
        "{:?}",
        info.error
    );

    // `up` of the new generation fails after generation 1 was stopped: the new one is the app now.
    env.flag("fail-down", false);
    env.flag("fail-up", true);
    env.deploy("app", &["data"], None).await.unwrap_err();
    assert_eq!(env.generation_files("app"), ["bundle-2", "compose-2.yaml"]);
    let info = env.apps.inspect("app").await.unwrap();
    assert_eq!((info.generation, info.status), (2, AppStatus::Failed));

    // And a working deploy after that, with the data still there.
    env.flag("fail-up", false);
    let info = env.deploy("app", &["data"], None).await.unwrap();
    assert_eq!(
        (info.generation, info.status, info.error),
        (3, AppStatus::Running, None)
    );
    assert_eq!(env.generation_files("app"), ["bundle-3", "compose-3.yaml"]);
    assert_eq!(
        std::fs::read_to_string(env.volume("app", "data").join("db")).unwrap(),
        "data"
    );
}

#[tokio::test]
async fn a_first_deploy_that_fails_is_recorded() {
    let env = Env::new().await;
    env.flag("fail-up", true);
    env.deploy("app", &["data"], None).await.unwrap_err();
    let info = env.apps.inspect("app").await.unwrap();
    assert_eq!((info.generation, info.status), (1, AppStatus::Failed));
    assert!(info.error.unwrap().contains("up failed"));
    env.apps.delete("app", false).await.unwrap();
    assert!(matches!(env.apps.inspect("app").await, Err(AppsError::NotFound(_))));
    assert!(!env.vols().app_dir("app").exists());
}

#[tokio::test]
async fn app_volumes_are_subvolumes_checked_before_every_start() {
    let env = Env::new().await;
    env.deploy("app", &["data", "db"], None).await.unwrap();
    let r = env.record("app").await;
    let data = env.volume("app", "data");
    assert_eq!(env.vols().check(&data, None).unwrap(), r.volume_ids["data"]);
    assert_eq!(r.volume_ids.len(), 2);
    let generated = env.generated("app", 1);
    assert!(
        generated.contains(&format!("source: {}", data.display())),
        "{generated}"
    );

    // A volume swapped for a symlink (even to the right subvolume) is refused, before the running app is stopped.
    let aside = env.volume("app", "aside");
    std::fs::rename(&data, &aside).unwrap();
    std::os::unix::fs::symlink(&aside, &data).unwrap();
    env.calls();
    let e = env.deploy("app", &["data", "db"], None).await.unwrap_err().to_string();
    assert!(e.contains("volume `data`"), "{e}");
    assert!(e.contains("is a symlink"), "{e}");
    let calls = env.calls();
    assert!(
        !calls.iter().any(|c| c.contains("down") || c.contains("up")),
        "{calls:?}"
    );
    assert_eq!(env.apps.inspect("app").await.unwrap().generation, 1);

    // Another subvolume in its place is refused too.
    std::fs::remove_file(&data).unwrap();
    env.vols().create_in_place(&data).unwrap();
    let e = env.deploy("app", &["data", "db"], None).await.unwrap_err().to_string();
    assert!(e.contains("not subvolume"), "{e}");

    // So is a missing one: it's never quietly replaced by an empty volume.
    env.vols().remove(&data).unwrap();
    let e = env.deploy("app", &["data", "db"], None).await.unwrap_err().to_string();
    assert!(e.contains("volume `data`") && e.contains("doesn't exist"), "{e}");
    assert!(!data.exists());

    // Put back, it starts; and a volume added later is created.
    std::fs::rename(&aside, &data).unwrap();
    let info = env.deploy("app", &["data", "db", "cache"], None).await.unwrap();
    assert_eq!(info.notes, ["created volume `cache`"]);
    assert!(env.vols().check(&env.volume("app", "cache"), None).is_ok());
}

#[tokio::test]
async fn from_snapshots_the_shared_volumes_at_one_instant() {
    let env = Env::new().await;
    env.deploy("prod", &["data", "db", "logs"], None).await.unwrap();
    std::fs::write(env.volume("prod", "data").join("f"), "prod data").unwrap();
    std::fs::write(env.volume("prod", "db").join("f"), "prod db").unwrap();
    env.flag("running", true);
    env.calls();

    let info = env
        .deploy("preview", &["data", "db", "cache"], Some("prod"))
        .await
        .unwrap();
    assert_eq!(info.from.as_deref(), Some("prod"));
    assert_eq!(info.notes, ["snapshotted volume `data`, `db` from `prod`"]);
    // prod was paused around the snapshots, and unpaused after.
    let calls: Vec<String> = env.calls().into_iter().filter(|c| c.starts_with("prod")).collect();
    assert_eq!(calls, ["prod ps --all --format json", "prod pause", "prod unpause"]);
    let snaps = env.fake.snapshots.lock().unwrap().clone();
    assert_eq!(snaps.len(), 2, "{snaps:?}");
    assert!(!env.record("prod").await.paused);

    // The preview sees prod's data, an empty volume prod doesn't have, and the two diverge.
    assert_eq!(
        std::fs::read_to_string(env.volume("preview", "data").join("f")).unwrap(),
        "prod data"
    );
    assert_eq!(
        std::fs::read_to_string(env.volume("preview", "db").join("f")).unwrap(),
        "prod db"
    );
    assert_eq!(std::fs::read_dir(env.volume("preview", "cache")).unwrap().count(), 0);
    std::fs::write(env.volume("preview", "data").join("f"), "preview data").unwrap();
    assert_eq!(
        std::fs::read_to_string(env.volume("prod", "data").join("f")).unwrap(),
        "prod data"
    );

    // Only the first deploy snapshots.
    let info = env
        .deploy("preview", &["data", "db", "cache"], Some("prod"))
        .await
        .unwrap();
    assert!(
        info.notes[0].contains("--from only applies to an app's first deploy"),
        "{:?}",
        info.notes
    );
    assert_eq!(env.fake.snapshots.lock().unwrap().len(), 2);
    assert_eq!(
        std::fs::read_to_string(env.volume("preview", "data").join("f")).unwrap(),
        "preview data"
    );

    // A stopped app isn't paused.
    env.flag("running", false);
    env.calls();
    env.deploy("preview2", &["data"], Some("prod")).await.unwrap();
    assert!(!env.calls().iter().any(|c| c.contains("pause")));

    // Refusals: itself, a missing app.
    let e = env
        .deploy("preview3", &["data"], Some("preview3"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("--from` itself"), "{e}");
    let e = env
        .deploy("preview3", &["data"], Some("nothing"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("app `nothing` (given as --from) doesn't exist"), "{e}");
}

#[tokio::test]
async fn a_snapshot_that_fails_part_way_leaves_nothing_and_unpauses() {
    let env = Env::new().await;
    env.deploy("prod", &["a", "b"], None).await.unwrap();
    std::fs::write(env.volume("prod", "a").join("f"), "x").unwrap();
    env.flag("running", true);
    env.fake.fail_snapshots.lock().unwrap().insert("b".into());
    env.calls();

    let e = env
        .deploy("preview", &["a", "b"], Some("prod"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("injected failure"), "{e}");
    let calls = env.calls();
    assert!(has(&calls, "prod pause") && has(&calls, "prod unpause"), "{calls:?}");
    // Not half a set of volumes: none.
    assert!(!env.vols().app_dir("preview").exists());
    assert!(!Volumes::staging_for(&env.vols().app_dir("preview")).exists());
    assert_eq!(env.apps.inspect("preview").await.unwrap().status, AppStatus::Failed);
    assert!(!env.record("prod").await.paused);

    // Trying again makes the whole set.
    env.fake.fail_snapshots.lock().unwrap().clear();
    let info = env.deploy("preview", &["a", "b"], Some("prod")).await.unwrap();
    assert_eq!(info.notes, ["snapshotted volume `a`, `b` from `prod`"]);
    assert_eq!(
        std::fs::read_to_string(env.volume("preview", "a").join("f")).unwrap(),
        "x"
    );
}

#[tokio::test]
async fn a_pause_that_fails_still_unpauses() {
    let env = Env::new().await;
    env.deploy("prod", &["data"], None).await.unwrap();
    env.flag("running", true);
    env.flag("fail-pause", true);
    env.calls();
    let e = env
        .deploy("preview", &["data"], Some("prod"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("pause failed"), "{e}");
    let calls = env.calls();
    assert!(has(&calls, "prod unpause"), "{calls:?}");
    assert!(env.fake.snapshots.lock().unwrap().is_empty());
    assert!(!env.vols().app_dir("preview").exists());
    assert!(!env.record("prod").await.paused);
}

#[tokio::test]
async fn an_unpause_that_fails_is_recorded_and_retried_after_a_restart() {
    let mut env = Env::new().await;
    env.deploy("prod", &["data"], None).await.unwrap();
    env.flag("running", true);
    env.flag("fail-unpause", true);
    let e = env
        .deploy("preview", &["data"], Some("prod"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("may still be paused"), "{e}");
    let prod = env.record("prod").await;
    assert!(prod.paused);
    assert!(prod.error.unwrap().contains("unpausing failed"));
    let calls = env.calls();
    assert_eq!(calls.iter().filter(|c| *c == "prod unpause").count(), 3, "{calls:?}");

    env.flag("fail-unpause", false);
    env.restart().await;
    assert!(has(&env.calls(), "prod unpause"));
    assert!(!env.record("prod").await.paused);
}

#[tokio::test]
async fn a_restart_sweeps_half_made_volumes_and_adopts_finished_ones() {
    let mut env = Env::new().await;
    // A first deploy cut off while making its volumes: only the staging directory exists.
    let staged = env.vols().stage_dir(&env.vols().app_dir("cut")).unwrap();
    env.vols().create(&staged.join("data")).unwrap();
    // One cut off just after moving its volumes into place, before recording them.
    std::fs::create_dir_all(env.vols().app_dir("moved")).unwrap();
    let id = env.vols().create(&env.volume("moved", "data")).unwrap();
    std::fs::write(env.volume("moved", "data").join("f"), "kept").unwrap();
    env.restart().await;
    assert!(!staged.exists());

    let info = env.deploy("moved", &["data"], Some("cut")).await;
    // `cut` isn't an app, but `moved`'s volumes are in place, so this isn't its first deploy.
    let info = info.unwrap();
    assert_eq!(info.notes[1], "kept volume `data` from before");
    assert_eq!(env.record("moved").await.volume_ids["data"], id);
    assert_eq!(
        std::fs::read_to_string(env.volume("moved", "data").join("f")).unwrap(),
        "kept"
    );
}

#[tokio::test]
async fn delete_removes_the_data_unless_kept() {
    let env = Env::new().await;
    env.deploy("app", &["data"], None).await.unwrap();
    std::fs::write(env.volume("app", "data").join("f"), "x").unwrap();
    env.apps.delete("app", true).await.unwrap();
    assert!(matches!(env.apps.inspect("app").await, Err(AppsError::NotFound(_))));
    assert!(env.volume("app", "data").join("f").exists());

    // A redeploy of the same name gets it back.
    let info = env.deploy("app", &["data"], None).await.unwrap();
    assert_eq!(info.notes, ["kept volume `data` from before"]);
    assert_eq!(
        std::fs::read_to_string(env.volume("app", "data").join("f")).unwrap(),
        "x"
    );

    env.apps.delete("app", false).await.unwrap();
    assert!(!env.vols().app_dir("app").exists());
    assert!(!env.path("state/apps/app").exists());
}

#[tokio::test]
async fn shared_volumes_in_each_mode() {
    let env = Env::new().await;
    let e = env
        .deploy_yaml(
            "reader",
            &compose("r.localhost", &[], "{ corpus: { target: /c } }"),
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("shared volume `corpus`, which doesn't exist"), "{e}");

    let info = env.apps.volume_create("corpus").await.unwrap();
    assert!(info.mounts.is_empty() && info.snapshots.is_empty() && info.problem.is_none());
    let e = env.apps.volume_create("corpus").await.unwrap_err().to_string();
    assert!(e.contains("already exists"), "{e}");
    let src = env.path("corpus-v1");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("sub/words"), "v1").unwrap();
    let info = env
        .apps
        .volume_load("corpus", LoadSource::Dir(src.clone()))
        .await
        .unwrap();
    assert!(info.loaded_at.is_some());
    let data = env.vols().shared_data("corpus");
    assert_eq!(std::fs::read_to_string(data.join("sub/words")).unwrap(), "v1");

    // `ro` and `rw` mount the volume itself.
    env.deploy_yaml(
        "reader",
        &compose("r.localhost", &[], "{ corpus: { target: /c } }"),
        None,
    )
    .await
    .unwrap();
    let g = env.generated("reader", 1);
    assert!(g.contains(&format!("source: {}", data.display())), "{g}");
    assert!(g.contains("read_only: true"), "{g}");
    env.deploy_yaml(
        "writer",
        &compose("w.localhost", &[], "{ corpus: { target: /c, mode: rw } }"),
        None,
    )
    .await
    .unwrap();
    let info = env.apps.volume_info("corpus").await.unwrap();
    assert_eq!(
        info.mounts,
        [mount("reader", SharedMode::Ro), mount("writer", SharedMode::Rw)]
    );

    // `snapshot` gives the app its own copy, made on its first deploy and kept after.
    let yaml = compose("s.localhost", &[], "{ corpus: { target: /c, mode: snapshot } }");
    let info = env.deploy_yaml("snap", &yaml, None).await.unwrap();
    assert_eq!(info.notes, ["snapshotted shared volume `corpus`"]);
    let snap = env.vols().snapshot_path("corpus", "snap");
    assert_eq!(std::fs::read_to_string(snap.join("sub/words")).unwrap(), "v1");
    assert!(
        env.generated("snap", 1)
            .contains(&format!("source: {}", snap.display()))
    );
    std::fs::write(snap.join("mine"), "x").unwrap();
    assert!(!data.join("mine").exists());
    let info = env.deploy_yaml("snap", &yaml, None).await.unwrap();
    assert!(info.notes.is_empty(), "{:?}", info.notes);
    assert!(snap.join("mine").exists());

    // Loading is refused while an app mounts it `ro` or `rw`, but not `snapshot`.
    std::fs::write(src.join("sub/words"), "v2").unwrap();
    let e = env
        .apps
        .volume_load("corpus", LoadSource::Dir(src.clone()))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("while app `reader`, `writer` mounts it"), "{e}");
    env.apps.delete("reader", false).await.unwrap();
    env.apps.delete("writer", false).await.unwrap();
    env.apps
        .volume_load("corpus", LoadSource::Dir(src.clone()))
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(data.join("sub/words")).unwrap(), "v2");
    assert_eq!(std::fs::read_to_string(snap.join("sub/words")).unwrap(), "v1");
    assert!(!env.vols().shared_dir("corpus").join(".load").exists());

    // Deleting: refused while mounted, while a snapshot exists (even a kept one), and a snapshot while its app
    // is deployed and mounts it.
    let e = env.apps.volume_delete("corpus").await.unwrap_err().to_string();
    assert!(e.contains("while app `snap` mounts it"), "{e}");
    let e = env
        .apps
        .volume_delete_snapshot("corpus", "snap")
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("while the app is deployed and mounts it"), "{e}");
    env.apps.delete("snap", true).await.unwrap();
    assert!(snap.exists(), "kept with --keep-data");
    let e = env.apps.volume_delete("corpus").await.unwrap_err().to_string();
    assert!(e.contains("while snapshots of it exist, for app `snap`"), "{e}");
    assert_eq!(env.apps.volume_info("corpus").await.unwrap().snapshots, ["snap"]);

    // A redeploy gets the kept snapshot back.
    let info = env.deploy_yaml("snap", &yaml, None).await.unwrap();
    assert_eq!(info.notes, ["kept the snapshot of shared volume `corpus` from before"]);
    assert!(snap.join("mine").exists());
    // Deleting the app deletes its snapshot.
    env.apps.delete("snap", false).await.unwrap();
    assert!(!snap.exists());
    let e = env
        .apps
        .volume_delete_snapshot("corpus", "snap")
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("has no snapshot"), "{e}");

    // A snapshot whose app is gone can be deleted, and then the volume.
    env.deploy_yaml("snap", &yaml, None).await.unwrap();
    env.apps.delete("snap", true).await.unwrap();
    env.apps.volume_delete_snapshot("corpus", "snap").await.unwrap();
    env.apps.volume_delete("corpus").await.unwrap();
    assert!(!env.vols().shared_dir("corpus").exists());
    assert!(env.apps.volume_list().await.unwrap().is_empty());
    let e = env.apps.volume_delete("corpus").await.unwrap_err().to_string();
    assert!(e.contains("doesn't exist"), "{e}");
}

fn mount(app: &str, mode: SharedMode) -> kn_proto::api::VolumeMount {
    kn_proto::api::VolumeMount { app: app.into(), mode }
}

#[tokio::test]
async fn a_shared_mount_is_checked_before_every_start() {
    let env = Env::new().await;
    env.apps.volume_create("corpus").await.unwrap();
    let yaml = compose("r.localhost", &[], "{ corpus: { target: /c } }");
    env.deploy_yaml("reader", &yaml, None).await.unwrap();
    let data = env.vols().shared_data("corpus");
    let aside = env.vols().shared_dir("corpus").join("aside");
    std::fs::rename(&data, &aside).unwrap();
    std::os::unix::fs::symlink(&aside, &data).unwrap();
    let e = env.deploy_yaml("reader", &yaml, None).await.unwrap_err().to_string();
    assert!(
        e.contains("shared volume `corpus`") && e.contains("is a symlink"),
        "{e}"
    );
    let info = env.apps.volume_info("corpus").await.unwrap();
    assert!(info.problem.unwrap().contains("is a symlink"));
}

#[tokio::test]
async fn a_load_cut_off_by_a_restart_is_finished_or_undone() {
    let mut env = Env::new().await;
    env.apps.volume_create("corpus").await.unwrap();
    let dir = env.vols().shared_dir("corpus");
    let data = env.vols().shared_data("corpus");
    std::fs::write(data.join("v"), "old").unwrap();

    // Cut off before the swap: the old contents stay, and the new ones are removed.
    let new = env.vols().create(&dir.join(LOAD)).unwrap();
    std::fs::write(dir.join(LOAD).join("v"), "new").unwrap();
    set_loading(&env, new).await;
    env.restart().await;
    assert_eq!(std::fs::read_to_string(data.join("v")).unwrap(), "old");
    assert!(!dir.join(LOAD).exists());
    assert!(env.apps.volume_info("corpus").await.unwrap().problem.is_none());

    // Cut off after the swap: the new contents are kept, and the old ones removed.
    let new = env.vols().create(&dir.join(LOAD)).unwrap();
    std::fs::write(dir.join(LOAD).join("v"), "new").unwrap();
    set_loading(&env, new).await;
    env.vols().exchange(&dir.join(LOAD), &data).unwrap();
    env.restart().await;
    assert_eq!(std::fs::read_to_string(data.join("v")).unwrap(), "new");
    assert!(!dir.join(LOAD).exists());
    let rec = env.apps.inner.state.lock().await.shared["corpus"].clone();
    assert_eq!((rec.id, rec.loading), (new, None));
}

const LOAD: &str = crate::volumes::LOAD_NAME;

async fn set_loading(env: &Env, id: u64) {
    let mut state = env.apps.inner.state.lock().await;
    state.shared.get_mut("corpus").unwrap().loading = Some(id);
    env.apps.inner.persist(&state).await.unwrap();
}

#[tokio::test]
async fn a_shared_volume_moved_into_place_but_not_recorded_is_adopted() {
    let mut env = Env::new().await;
    let staged = env.vols().stage_dir(&env.vols().shared_dir("maps")).unwrap();
    std::fs::create_dir(staged.join("snaps")).unwrap();
    env.vols().create(&staged.join("data")).unwrap();
    env.vols().commit(&env.vols().shared_dir("maps")).unwrap();
    env.restart().await;
    let info = env.apps.volume_info("maps").await.unwrap();
    assert!(info.problem.is_none());
}

#[tokio::test]
async fn from_an_app_in_its_own_first_deploy_is_refused() {
    let env = Env::new().await;
    env.flag("fail-up", true);
    env.deploy("prod", &["data"], None).await.unwrap_err();
    env.flag("fail-up", false);
    // As if `prod`'s first deploy were still running (and so might be waiting on another app's snapshots).
    {
        let mut state = env.apps.inner.state.lock().await;
        state.apps.get_mut("prod").unwrap().status = AppStatus::Deploying;
    }
    env.vols().remove(&env.vols().app_dir("prod")).unwrap();
    let e = env
        .deploy("preview", &["data"], Some("prod"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("is being deployed for the first time"), "{e}");
    // Once its volumes are in place (a redeploy), waiting on it is fine.
    std::fs::create_dir_all(env.vols().app_dir("prod")).unwrap();
    let e = env
        .deploy("preview", &["data"], Some("prod"))
        .await
        .unwrap_err()
        .to_string();
    assert!(!e.contains("first time") && e.contains("`prod`'s volume `data`"), "{e}");
}

#[tokio::test]
async fn a_missing_volumes_directory_is_refused_not_made_again() {
    let env = Env::new().await;
    env.deploy("source", &["data"], None).await.unwrap();
    env.deploy("prod", &["data"], None).await.unwrap();
    std::fs::write(env.volume("prod", "data").join("f"), "x").unwrap();
    let id = env.record("prod").await.volume_ids["data"];
    // The whole directory goes, as if removed or its filesystem weren't mounted.
    env.vols().remove(&env.vols().app_dir("prod")).unwrap();
    env.calls();
    for from in [None, Some("source")] {
        let e = env.deploy("prod", &["data"], from).await.unwrap_err().to_string();
        assert!(
            e.contains("refusing to start `prod`") && e.contains("volumes directory") && e.contains("is missing"),
            "{e}"
        );
        assert!(!env.vols().app_dir("prod").exists(), "nothing is made in its place");
        let calls = env.calls();
        assert!(
            !calls.iter().any(|c| c.contains("down") || c.contains("up")),
            "{calls:?}"
        );
        let info = env.apps.inspect("prod").await.unwrap();
        assert_eq!((info.generation, info.status), (1, AppStatus::Running));
        assert_eq!(env.record("prod").await.volume_ids["data"], id);
    }
    assert!(env.fake.snapshots.lock().unwrap().is_empty());
    // Deleting the app is the way to start again.
    env.apps.delete("prod", false).await.unwrap();
    env.deploy("prod", &["data"], None).await.unwrap();
}

/// Deploys `name` with one service running `image`, on the fake runtime.
async fn deploy_image(apps: &Apps, root: &Path, name: &str, image: &str) {
    let b = root.join(format!("bundle-{name}-{}", image.replace(['/', ':'], "-")));
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(
        b.join("compose.yaml"),
        format!("x-kiwi-nest: {{ domain: {name}.localhost, service: web, port: 80 }}\nservices:\n  web:\n    image: {image}\n"),
    )
    .unwrap();
    let staged = apps.stage_dir(b).await.unwrap();
    apps.deploy(name, staged, DeployOptions::default()).await.unwrap();
}

fn kept(r: &PruneReport, image: &str) -> String {
    r.kept
        .iter()
        .find(|k| k.image == image)
        .map(|k| k.why.clone())
        .unwrap_or_else(|| panic!("{image} should be kept: {r:?}"))
}

#[tokio::test]
async fn pruning_removes_images_apps_stopped_using_once_unused_after_the_grace_period() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut cfg = NodeConfig::new(root.join("state"));
    cfg.prune_interval = Duration::ZERO;
    let rt = crate::testing::Fake::new();
    let volumes = Arc::new(Fake::default());
    let apps = Apps::open_with_runtime(cfg.clone(), volumes.clone(), rt.clone())
        .await
        .unwrap();
    let (one, two) = ("r.test/blog:1", "r.test/blog:2");
    let hour = Duration::from_secs(3600);

    deploy_image(&apps, &root, "blog", one).await;
    assert!(rt.has_image(one));
    // Nothing was released: nothing to do, and images apps never used aren't touched.
    assert_eq!(apps.prune_images(Duration::ZERO).await.unwrap(), PruneReport::default());

    // A redeploy with another image releases the first, which is kept for the grace period.
    deploy_image(&apps, &root, "blog", two).await;
    let r = apps.prune_images(hour).await.unwrap();
    assert!(r.removed.is_empty(), "{r:?}");
    assert!(kept(&r, one).contains("within the grace period"), "{r:?}");
    assert!(rt.has_image(one));

    // Used again by another app, it stays whatever the grace period.
    deploy_image(&apps, &root, "shop", one).await;
    let r = apps.prune_images(Duration::ZERO).await.unwrap();
    assert_eq!(kept(&r, one), "used by app shop");

    // Deleted: released again, and kept across a restart.
    apps.delete("shop", false).await.unwrap();
    drop(apps);
    let apps = Apps::open_with_runtime(cfg.clone(), volumes.clone(), rt.clone())
        .await
        .unwrap();
    assert!(kept(&apps.prune_images(hour).await.unwrap(), one).contains("within the grace period"));
    let r = apps.prune_images(Duration::ZERO).await.unwrap();
    assert_eq!(r.removed, vec![one.to_string()]);
    assert!(!rt.has_image(one));
    assert!(rt.has_image(two), "blog still uses it");
    // Once removed, it's forgotten.
    assert_eq!(apps.prune_images(Duration::ZERO).await.unwrap(), PruneReport::default());

    // A delete then a deploy within the grace period finds the image still there.
    apps.delete("blog", false).await.unwrap();
    assert!(apps.prune_images(hour).await.unwrap().removed.is_empty());
    assert!(rt.has_image(two));
    deploy_image(&apps, &root, "blog", two).await;
    assert_eq!(
        kept(&apps.prune_images(Duration::ZERO).await.unwrap(), two),
        "used by app blog"
    );
}

/// A local directory target, in `format`, keeping the last `keep` backups.
fn dir_target(dir: &Path, name: &str, format: kn_proto::backup::Format, keep: u32) -> kn_backup::Target {
    kn_backup::Target::open(kn_proto::backup::TargetConfig {
        name: name.into(),
        s3: None,
        ssh: None,
        dir: Some(kn_proto::backup::DirConfig {
            path: dir.to_string_lossy().into_owned(),
        }),
        prefix: String::new(),
        format,
        every: None,
        full_every: None,
        keep: kn_proto::backup::Keep {
            last: Some(keep),
            ..Default::default()
        },
    })
    .unwrap()
}

impl Env {
    fn use_targets(&self, targets: Vec<kn_backup::Target>) {
        self.apps.set_targets(Arc::new(StaticTargets::new(targets, "tests")));
    }

    async fn deploy_restored(&self, name: &str, volumes: &[&str], shared: &str, restore: &str) -> Result<AppInfo> {
        let b = self.path(&format!("bundle-{name}"));
        std::fs::create_dir_all(b.join("site")).unwrap();
        std::fs::write(b.join("site/index.html"), "page").unwrap();
        std::fs::write(
            b.join("compose.yaml"),
            compose(&format!("{name}.localhost"), volumes, shared),
        )
        .unwrap();
        let staged = self.apps.stage_dir(b).await?;
        let restore = self
            .apps
            .resolve_restore(kn_proto::backup::Kind::App, restore, None)
            .await?;
        let opts = DeployOptions {
            restore: Some(restore),
            ..DeployOptions::default()
        };
        self.apps.deploy(name, staged, opts).await
    }

    /// The snapshot sets kept for an app's backups.
    fn sets(&self, app: &str) -> Vec<String> {
        Volumes::names(&self.vols().backups_dir(kn_proto::backup::Kind::App, app)).unwrap()
    }
}

#[tokio::test]
async fn tar_backups_restore_into_a_new_app() {
    use kn_proto::backup::{Format, Kind};
    let env = Env::new().await;
    let store = env.path("backups");
    env.use_targets(vec![dir_target(&store, "local", Format::Tar, 2)]);
    env.deploy("blog", &["data", "logs"], None).await.unwrap();
    std::fs::write(env.volume("blog", "data").join("db"), "one").unwrap();
    std::fs::write(env.volume("blog", "logs").join("today"), "x").unwrap();
    env.flag("running", true);
    env.calls();

    let first = env.apps.backup(Kind::App, "blog", &[], false).await.unwrap();
    assert!(first.failed.is_empty(), "{first:?}");
    assert_eq!(first.made.len(), 1);
    let k1 = first.made[0].key.clone();
    assert!(k1.starts_with("apps/blog/") && k1.ends_with(".tar.zst"), "{k1}");
    // Paused only around the snapshots, and unpaused after.
    let calls = env.calls();
    let paused = calls.iter().position(|c| c == "blog pause").expect("paused");
    let unpaused = calls.iter().position(|c| c == "blog unpause").expect("unpaused");
    assert!(paused < unpaused, "{calls:?}");
    assert!(!env.record("blog").await.paused);
    // A tar backup needs no snapshot kept on the node.
    assert!(env.sets("blog").is_empty());

    std::fs::write(env.volume("blog", "data").join("db"), "two").unwrap();
    let second = env.apps.backup(Kind::App, "blog", &[], false).await.unwrap();
    let k2 = second.made[0].key.clone();
    assert!(k2 > k1, "{k2} after {k1}");

    // Restored into a new app: the second state. A volume the bundle doesn't declare isn't restored.
    let info = env.deploy_restored("copy", &["data", "cache"], "", &k2).await.unwrap();
    assert!(
        info.notes.iter().any(|n| n.contains("restored `data`")),
        "{:?}",
        info.notes
    );
    assert!(
        info.notes.iter().any(|n| n.contains("volume `logs`")),
        "{:?}",
        info.notes
    );
    assert_eq!(
        std::fs::read_to_string(env.volume("copy", "data").join("db")).unwrap(),
        "two"
    );
    assert!(std::fs::read_dir(env.volume("copy", "cache")).unwrap().next().is_none());
    assert_eq!(env.record("copy").await.restore.unwrap().key, k2);
    let ids = env.record("copy").await.volume_ids;
    assert_eq!(ids.len(), 2);
    // A redeploy keeps its data, whatever --restore says.
    std::fs::write(env.volume("copy", "data").join("db"), "mine").unwrap();
    let info = env.deploy_restored("copy", &["data", "cache"], "", &k1).await.unwrap();
    assert!(
        info.notes
            .iter()
            .any(|n| n.contains("only applies to an app's first deploy"))
    );
    assert_eq!(
        std::fs::read_to_string(env.volume("copy", "data").join("db")).unwrap(),
        "mine"
    );

    // Retention keeps the last two.
    std::thread::sleep(Duration::from_millis(1100));
    let third = env.apps.backup(Kind::App, "blog", &[], false).await.unwrap();
    assert!(
        third.notes.iter().any(|n| n.contains("retention removed 1")),
        "{third:?}"
    );
    let left = env.apps.backup_list(&Default::default()).await.unwrap();
    assert_eq!(left.len(), 2);
    assert!(!left.iter().any(|b| b.key == k1));

    // A key that isn't there, or of the wrong kind, is refused before anything is made.
    let e = env
        .deploy_restored("other", &["data"], "", &k1)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("no backup target has"), "{e}");
    let e = env
        .deploy_restored("other", &["data"], "", "volumes/x/20261106T030000Z.tar.zst")
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("is a backup of volume"), "{e}");
    assert!(!Volumes::exists(&env.vols().app_dir("other")).unwrap());
}

#[tokio::test]
async fn a_failed_backup_leaves_nothing() {
    use kn_proto::backup::{Format, Kind};
    let env = Env::new().await;
    // A target that can't be written: its directory is a file.
    let blocked = env.path("blocked");
    std::fs::write(&blocked, "").unwrap();
    let good = env.path("good");
    env.use_targets(vec![
        dir_target(&blocked, "blocked", Format::Tar, 5),
        dir_target(&good, "good", Format::Tar, 5),
    ]);
    env.deploy("blog", &["data"], None).await.unwrap();
    let r = env.apps.backup(Kind::App, "blog", &[], false).await.unwrap();
    assert_eq!(r.made.len(), 1);
    assert_eq!(r.failed.len(), 1);
    assert_eq!(r.failed[0].target, "blocked");
    assert!(env.sets("blog").is_empty());
    // Only that target, by name; an unknown one is refused.
    let r = env
        .apps
        .backup(Kind::App, "blog", &["good".into()], false)
        .await
        .unwrap();
    assert!(r.failed.is_empty() && r.made.len() == 1);
    let e = env
        .apps
        .backup(Kind::App, "blog", &["nope".into()], false)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no backup target `nope`"), "{e}");
    // A target the node can't use (here, one whose key file only the server had) fails on its own.
    struct Mixed(Arc<Targets>);
    impl TargetSource for Mixed {
        fn targets(&self) -> futures_util::future::BoxFuture<'_, anyhow::Result<Arc<Targets>>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }
    let mut broken = kn_proto::backup::TargetConfig {
        name: "r2".into(),
        ..dir_target(&good, "x", Format::Tar, 5).config().clone()
    };
    broken.dir = None;
    broken.s3 = Some(kn_proto::backup::S3Config {
        endpoint: "https://r2.example.com".into(),
        bucket: "b".into(),
        region: "auto".into(),
        access_key: "a".into(),
        secret_key: None,
        secret_key_file: Some("/only/on/the/server".into()),
        virtual_host: false,
    });
    let good_cfg = dir_target(&good, "good", Format::Tar, 5).config().clone();
    env.apps
        .set_targets(Arc::new(Mixed(Arc::new(Targets::open(vec![broken, good_cfg])))));
    let r = env.apps.backup(Kind::App, "blog", &[], false).await.unwrap();
    assert_eq!(r.made.len(), 1, "{r:?}");
    assert_eq!(r.failed.len(), 1);
    assert!(
        r.failed[0].target == "r2" && r.failed[0].error.contains("/only/on/the/server"),
        "{r:?}"
    );
    let r = env.apps.backup(Kind::App, "blog", &["r2".into()], false).await.unwrap();
    assert!(r.made.is_empty() && r.failed.len() == 1, "{r:?}");
    let e = env
        .apps
        .resolve_restore(Kind::App, "apps/blog/20261106T030000Z.tar.zst", Some("r2"))
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("backup target `r2` can't be used"), "{e}");
    // Without targets at all.
    env.use_targets(vec![]);
    let e = env.apps.backup(Kind::App, "blog", &[], false).await.unwrap_err();
    assert!(e.to_string().contains("no backup targets"), "{e}");
    // A `send` target needs root (for btrfs send).
    if !kn_btrfs::can_send() {
        env.use_targets(vec![dir_target(&good, "send", Format::Send, 5)]);
        let e = env.apps.backup(Kind::App, "blog", &[], false).await.unwrap_err();
        assert!(e.to_string().contains("needs the node to run as root"), "{e}");
    }
}

#[tokio::test]
async fn shared_volumes_and_an_apps_snapshots_of_them_are_backed_up() {
    use kn_proto::backup::{Format, Kind};
    let env = Env::new().await;
    env.use_targets(vec![dir_target(&env.path("backups"), "local", Format::Tar, 5)]);
    env.apps.volume_create("corpus").await.unwrap();
    std::fs::write(env.vols().shared_data("corpus").join("words"), "shared").unwrap();
    env.deploy_yaml(
        "blog",
        &compose(
            "blog.localhost",
            &["data"],
            "{ corpus: { target: /c, mode: snapshot } }",
        ),
        None,
    )
    .await
    .unwrap();
    // The app writes into its own snapshot.
    std::fs::write(env.vols().snapshot_path("corpus", "blog").join("words"), "the app's").unwrap();

    let vb = env.apps.backup(Kind::Volume, "corpus", &[], false).await.unwrap();
    let vkey = vb.made[0].key.clone();
    assert!(vkey.starts_with("volumes/corpus/"), "{vkey}");
    let ab = env.apps.backup(Kind::App, "blog", &[], false).await.unwrap();
    assert!(ab.notes.iter().any(|n| n.contains("`@corpus`")), "{ab:?}");
    let akey = ab.made[0].key.clone();

    // A volume restored under another name.
    let restore = env.apps.resolve_restore(Kind::Volume, &vkey, None).await.unwrap();
    let info = env.apps.volume_create_from("corpus2", Some(&restore)).await.unwrap();
    assert!(info.problem.is_none(), "{info:?}");
    assert_eq!(
        std::fs::read_to_string(env.vols().shared_data("corpus2").join("words")).unwrap(),
        "shared"
    );
    // An app restored with its own snapshot, not a new one of the shared volume.
    env.deploy_restored("blog2", &["data"], "{ corpus: { target: /c, mode: snapshot } }", &akey)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(env.vols().snapshot_path("corpus", "blog2").join("words")).unwrap(),
        "the app's"
    );
    assert!(env.record("blog2").await.shared["corpus"].snapshot_id.is_some());
    // A bundle that mounts the volume `ro` doesn't get the snapshot.
    let info = env
        .deploy_restored("blog3", &["data"], "{ corpus: { target: /c } }", &akey)
        .await
        .unwrap();
    assert!(
        info.notes
            .iter()
            .any(|n| n.contains("snapshot of shared volume `corpus`")),
        "{:?}",
        info.notes
    );
    assert!(!Volumes::exists(&env.vols().snapshot_path("corpus", "blog3")).unwrap());
    let staged = Volumes::staging_for(&env.vols().snapshot_path("corpus", "blog3"));
    assert!(!Volumes::exists(&staged).unwrap());

    let q = kn_proto::backup::ListQuery {
        kind: Some(Kind::Volume),
        ..Default::default()
    };
    assert_eq!(env.apps.backup_list(&q).await.unwrap().len(), 1);
}

#[tokio::test]
async fn send_backups_continue_chains_from_what_both_sides_have() {
    use kn_proto::backup::{Format, Kind};
    let Ok(btrfs) = std::env::var("KIWI_NEST_TEST_BTRFS_DIR") else {
        assert!(
            std::env::var_os("KIWI_NEST_REQUIRE_ROOT").is_none(),
            "needs KIWI_NEST_TEST_BTRFS_DIR, as root, and KIWI_NEST_REQUIRE_ROOT is set"
        );
        eprintln!("skipped: needs KIWI_NEST_TEST_BTRFS_DIR, as root");
        return;
    };
    if !kn_btrfs::can_send() {
        assert!(
            std::env::var_os("KIWI_NEST_REQUIRE_ROOT").is_none(),
            "btrfs send needs root, and KIWI_NEST_REQUIRE_ROOT is set"
        );
        eprintln!("skipped: btrfs send needs root");
        return;
    }
    let dir = tempfile::tempdir_in(std::fs::canonicalize(btrfs).unwrap()).unwrap();
    let root = dir.path().to_path_buf();
    // Whatever happens, its subvolumes (read-only snapshots too) go with it.
    struct Cleanup(PathBuf);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = kn_btrfs::remove_tree(&self.0);
        }
    }
    let _cleanup = Cleanup(root.clone());
    let docker = root.join("docker");
    std::fs::write(&docker, FAKE_DOCKER).unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut cfg = NodeConfig::new(root.join("state"));
    cfg.docker = docker;
    let apps = Apps::open_with(cfg, Arc::new(crate::volumes::Btrfs)).await.unwrap();
    let (a, b) = (root.join("a"), root.join("b"));
    apps.set_targets(Arc::new(StaticTargets::new(
        vec![
            dir_target(&a, "a", Format::Send, 10),
            dir_target(&b, "b", Format::Send, 10),
        ],
        "tests",
    )));
    let deploy = |name: &'static str, restore: Option<String>| {
        let (apps, bundle) = (apps.clone(), root.join(format!("bundle-{name}")));
        std::fs::create_dir_all(bundle.join("site")).unwrap();
        std::fs::write(
            bundle.join("compose.yaml"),
            compose(&format!("{name}.localhost"), &["data"], ""),
        )
        .unwrap();
        async move {
            let staged = apps.stage_dir(bundle).await.unwrap();
            let restore = match restore {
                Some(k) => Some(apps.resolve_restore(Kind::App, &k, Some("a")).await.unwrap()),
                None => None,
            };
            apps.deploy(
                name,
                staged,
                DeployOptions {
                    restore,
                    ..Default::default()
                },
            )
            .await
            .unwrap()
        }
    };
    deploy("blog", None).await;
    let data = apps.volumes().app_volume("blog", "data");
    std::fs::write(data.join("db"), vec![1u8; 200_000]).unwrap();
    let sets = || Volumes::names(&apps.volumes().backups_dir(Kind::App, "blog")).unwrap();

    // Both targets: full backups, and one set kept as their parent.
    let r = apps.backup(Kind::App, "blog", &[], false).await.unwrap();
    assert_eq!(r.made.len(), 2, "{r:?}");
    assert!(r.made.iter().all(|m| m.parent.is_none()));
    let first = sets();
    assert_eq!(first.len(), 1);

    // Only `a`: incremental against the kept set. `b` still needs the first set, so both are kept.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(data.join("new"), "two").unwrap();
    let r = apps.backup(Kind::App, "blog", &["a".into()], false).await.unwrap();
    let incr = &r.made[0];
    assert!(incr.parent.is_some() && incr.key.contains(".after-"), "{incr:?}");
    assert_eq!(sets().len(), 2);

    // `b` continues its own chain from the first set; then only the newest set is anyone's parent.
    std::thread::sleep(Duration::from_millis(1100));
    let r = apps.backup(Kind::App, "blog", &["b".into()], false).await.unwrap();
    assert!(r.made[0].key.contains(&format!(".after-{}", first[0])), "{r:?}");
    std::thread::sleep(Duration::from_millis(1100));
    let r = apps.backup(Kind::App, "blog", &[], false).await.unwrap();
    assert!(r.made.iter().all(|m| m.parent.is_some()), "{r:?}");
    assert_eq!(sets().len(), 1);

    // --full starts again; a chain whose parent set is gone from the node does too.
    std::thread::sleep(Duration::from_millis(1100));
    let r = apps.backup(Kind::App, "blog", &["a".into()], true).await.unwrap();
    assert!(r.made[0].parent.is_none());
    for s in sets() {
        apps.volumes()
            .remove(&apps.volumes().backups_dir(Kind::App, "blog").join(s))
            .unwrap();
    }
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(data.join("new"), "three").unwrap();
    let r = apps.backup(Kind::App, "blog", &["a".into()], false).await.unwrap();
    assert!(r.made[0].parent.is_none(), "{r:?}");

    // An incremental restore on the newest chain of `a`.
    std::thread::sleep(Duration::from_millis(1100));
    std::fs::write(data.join("new"), "four").unwrap();
    let r = apps.backup(Kind::App, "blog", &["a".into()], false).await.unwrap();
    assert!(r.made[0].parent.is_some());
    deploy("copy", Some(r.made[0].key.clone())).await;
    let copy = apps.volumes().app_volume("copy", "data");
    assert_eq!(std::fs::read_to_string(copy.join("new")).unwrap(), "four");
    assert_eq!(std::fs::read(copy.join("db")).unwrap().len(), 200_000);
    assert!(!kn_btrfs::inspect(&copy).unwrap().read_only);

    // Deleting the app sets its snapshots aside, and the next sweep destroys them.
    apps.delete("blog", false).await.unwrap();
    assert!(sets().is_empty());
    apps.volumes().sweep(&[]).unwrap();
    let backups = apps.volumes().root().join("backups/apps");
    assert_eq!(Volumes::names(&backups).unwrap(), Vec::<String>::new());
    assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 0);
    apps.delete("copy", false).await.unwrap();
}

#[tokio::test]
async fn a_restored_snapshot_survives_a_failed_deploy_and_a_restart() {
    use kn_proto::backup::{Format, Kind};
    let mut env = Env::new().await;
    env.use_targets(vec![dir_target(&env.path("backups"), "local", Format::Tar, 5)]);
    for v in ["aaa", "corpus"] {
        env.apps.volume_create(v).await.unwrap();
    }
    std::fs::write(env.vols().shared_data("corpus").join("words"), "shared").unwrap();
    let shared = "{ corpus: { target: /c, mode: snapshot } }";
    env.deploy_yaml("blog", &compose("blog.localhost", &["data"], shared), None)
        .await
        .unwrap();
    std::fs::write(env.vols().snapshot_path("corpus", "blog").join("words"), "the app's").unwrap();
    let key = env.apps.backup(Kind::App, "blog", &[], false).await.unwrap().made[0]
        .key
        .clone();

    // The restored app also mounts `aaa` (before `corpus`, by name) in snapshot mode, and snapshotting it fails:
    // the restored volumes are in place, the restored snapshot of `corpus` is staged, and the deploy fails.
    let both = "{ aaa: { target: /a, mode: snapshot }, corpus: { target: /c, mode: snapshot } }";
    env.fake.fail_snapshots.lock().unwrap().insert(".new-copy".into());
    assert!(env.deploy_restored("copy", &["data"], both, &key).await.is_err());
    assert_eq!(
        env.record("copy").await.restored_snapshots,
        BTreeSet::from(["corpus".to_string()])
    );
    env.fake.fail_snapshots.lock().unwrap().clear();

    // A restart keeps it; the next deploy moves it into place, rather than snapshotting `corpus` afresh.
    env.restart().await;
    env.use_targets(vec![dir_target(&env.path("backups"), "local", Format::Tar, 5)]);
    let staged = Volumes::staging_for(&env.vols().snapshot_path("corpus", "copy"));
    assert!(Volumes::exists(&staged).unwrap());
    env.deploy_yaml("copy", &compose("copy.localhost", &["data"], both), None)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(env.vols().snapshot_path("corpus", "copy").join("words")).unwrap(),
        "the app's"
    );
    assert!(env.record("copy").await.restored_snapshots.is_empty());

    // One that went missing is refused, never made again from the shared volume.
    env.fake.fail_snapshots.lock().unwrap().insert(".new-copy2".into());
    assert!(env.deploy_restored("copy2", &["data"], both, &key).await.is_err());
    env.fake.fail_snapshots.lock().unwrap().clear();
    env.vols()
        .remove(&Volumes::staging_for(&env.vols().snapshot_path("corpus", "copy2")))
        .unwrap();
    let e = env
        .deploy_yaml("copy2", &compose("copy2.localhost", &["data"], both), None)
        .await
        .unwrap_err()
        .to_string();
    assert!(e.contains("restored from the backup is missing"), "{e}");
}

#[tokio::test]
async fn deleting_a_volume_sets_its_backup_snapshots_aside() {
    use kn_proto::backup::Kind;
    let mut env = Env::new().await;
    env.apps.volume_create("corpus").await.unwrap();
    // A snapshot set kept as a send parent.
    let set = env.vols().backups_dir(Kind::Volume, "corpus").join("20261106T030000Z");
    std::fs::create_dir_all(&set).unwrap();
    env.apps.volume_delete("corpus").await.unwrap();
    assert!(!Volumes::exists(&env.vols().backups_dir(Kind::Volume, "corpus")).unwrap());
    let parent = env.vols().root().join("backups/volumes");
    let gone: Vec<_> = std::fs::read_dir(&parent)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(gone.len(), 1);
    assert!(gone[0].to_string_lossy().starts_with(".gone-corpus-"), "{gone:?}");
    // The next start destroys them.
    env.restart().await;
    assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0);
}

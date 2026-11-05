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

//! Tests of the Compose checker and the generated file.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_yaml_ng::Value;

use crate::compose::Mount;
use crate::generate::{GenerateError, Params, generate};
use crate::{CheckOptions, CheckedBundle, FileIndex, check};

fn files() -> FileIndex {
    let mut f = FileIndex::new();
    f.add_file("compose.yaml", 1, false, [0; 32]).unwrap();
    f.add_file("site/index.html", 1, false, [0; 32]).unwrap();
    f.add_file("app.env", 1, false, [0; 32]).unwrap();
    f
}

/// A valid two-service bundle; `{web}` and `{top}` add lines to the web service and the top level.
fn bundle(web: &str, top: &str) -> String {
    let base = [
        "image: example/web:1",
        "volumes: [\"./site:/app:ro\", \"data:/var/lib/blog\"]",
    ];
    let overridden = |line: &str| web.lines().any(|l| l.split(':').next() == line.split(':').next());
    let web: String = base
        .into_iter()
        .filter(|l| !overridden(l))
        .chain(web.lines())
        .map(|l| format!("    {l}\n"))
        .collect();
    format!(
        r#"x-kiwi-nest: {{ domain: blog.example.com, service: web, port: 8080 }}
services:
  web:
{web}  db:
    image: postgres:17
    volumes: ["db:/var/lib/postgresql/data"]
volumes:
  data: {{}}
  db:
{top}"#
    )
}

fn check_with(yaml: &str, options: &CheckOptions) -> Result<CheckedBundle, String> {
    check(yaml, &files(), options).map_err(|p| p.0.iter().map(|p| p.to_string()).collect::<Vec<_>>().join("\n"))
}

fn ok(yaml: &str) -> CheckedBundle {
    check_with(yaml, &CheckOptions::default()).unwrap_or_else(|e| panic!("should pass, but: {e}\n{yaml}"))
}

fn refused(yaml: &str) -> String {
    match check_with(yaml, &CheckOptions::default()) {
        Ok(_) => panic!("should be refused:\n{yaml}"),
        Err(e) => e,
    }
}

/// Asserts that adding `web` to the web service is refused with a message containing `expect`.
fn refuses_web(web: &str, expect: &str) {
    let e = refused(&bundle(web, ""));
    assert!(e.contains(expect), "{web}\nexpected {expect:?} in:\n{e}");
}

fn refuses_top(top: &str, expect: &str) {
    let e = refused(&bundle("", top));
    assert!(e.contains(expect), "{top}\nexpected {expect:?} in:\n{e}");
}

#[test]
fn accepts_the_base_bundle() {
    let b = ok(&bundle("", ""));
    assert_eq!(b.manifest.service, "web");
    assert_eq!(b.services.len(), 2);
    assert_eq!(b.volumes, vec!["data", "db"]);
    let web = b.service("web").unwrap();
    assert_eq!(
        web.mounts,
        vec![
            Mount::Bundle {
                path: "site".into(),
                target: "/app".into(),
                selinux: None
            },
            Mount::Volume {
                name: "data".into(),
                target: "/var/lib/blog".into(),
                read_only: false,
                selinux: None
            },
        ]
    );
}

#[test]
fn accepts_common_settings() {
    ok(&bundle(
        r#"command: ["serve", "--port", "8080"]
environment: { MODE: production, PRICE: "$$5" }
env_file: [app.env, { path: missing.env, required: false }]
healthcheck: { test: ["CMD-SHELL", "wget -qO- http://localhost:8080/ || exit 1"], interval: 5s }
restart: always
depends_on: { db: { condition: service_healthy } }
mem_limit: 256m
cpus: 0.5
user: "1000:1000"
labels: { com.example.team: blog }
logging: { driver: json-file, options: { max-size: 10m } }
networks: [default, back]
tmpfs: [/tmp]
read_only: true
init: true
expose: ["8080"]
x-notes: anything
"#,
        "networks:\n  back: { internal: true }\nx-common: &common { image: x }\nname: ignored\n",
    ));
}

#[test]
fn accepts_merge_keys() {
    let b = ok(r#"x-kiwi-nest: { domain: a.example.com, service: web, port: 80 }
x-base: &base
  image: example/web:1
  mem_limit: 128m
services:
  web:
    <<: *base
    command: serve
"#);
    assert_eq!(b.services[0].image, "example/web:1");
    assert!(b.services[0].config.contains_key("mem_limit"));
}

#[test]
fn accepts_long_mounts() {
    let b = ok(&bundle(
        r#"volumes:
  - { type: bind, source: ./site, target: /site }
  - { type: volume, source: data, target: /data, read_only: true, volume: { nocopy: true } }
  - { type: tmpfs, target: /cache, tmpfs: { size: 10m } }
  - { type: bind, source: ., target: /bundle, bind: { selinux: z } }
"#,
        "",
    ));
    let web = b.service("web").unwrap();
    assert!(matches!(&web.mounts[0], Mount::Bundle { path, .. } if path == "site"));
    assert!(matches!(&web.mounts[1], Mount::Volume { read_only: true, .. }));
    assert!(matches!(&web.mounts[2], Mount::Tmpfs { .. }));
    assert!(matches!(&web.mounts[3], Mount::Bundle { path, selinux: Some(_), .. } if path.is_empty()));
}

#[test]
fn refuses_builds() {
    refuses_web("build: .", "services.web.build: isn't supported");
    refuses_web("pull_policy: build", "services.web.pull_policy");
}

#[test]
fn refuses_a_service_without_an_image() {
    let e = refused("x-kiwi-nest: { domain: a.b, service: web, port: 80 }\nservices:\n  web: { command: x }\n");
    assert!(e.contains("services.web: needs an `image`"), "{e}");
    refuses_web("image: ''", "services.web.image");
}

#[test]
fn refuses_privileged_mode_and_its_relatives() {
    refuses_web("privileged: true", "services.web.privileged: isn't allowed");
    refuses_web("cap_add: [SYS_ADMIN]", "services.web.cap_add");
    refuses_web("devices: [/dev/sda]", "services.web.devices");
    refuses_web("security_opt: [seccomp=unconfined]", "services.web.security_opt");
    refuses_web("sysctls: { net.core.somaxconn: 1024 }", "services.web.sysctls");
    refuses_web("gpus: all", "services.web.gpus");
    // privileged: false is harmless.
    ok(&bundle("privileged: false", ""));
}

#[test]
fn refuses_host_namespaces() {
    for key in ["network_mode", "pid", "ipc", "uts", "userns_mode", "cgroup"] {
        refuses_web(&format!("{key}: host"), &format!("services.web.{key}: may only be"));
    }
    refuses_web("ipc: container:other", "services.web.ipc");
    refuses_web("pid: service:web", "services.web.pid");
    refuses_web("pid: service:nope", "services.web.pid");
    // Joining another service of the app is fine, though not for the routed service's network.
    ok(&bundle("pid: service:db\nipc: shareable", ""));
    refuses_web("network_mode: service:db", "isn't allowed on the routed service");
}

#[test]
fn refuses_published_ports() {
    refuses_web("ports: [\"8080:8080\"]", "services.web.ports: aren't allowed");
    refuses_web("ports: [\"127.0.0.1:8080:8080\"]", "services.web.ports");
}

#[test]
fn refuses_container_names() {
    refuses_web("container_name: blog", "services.web.container_name");
}

#[test]
fn refuses_unsupported_service_keys() {
    refuses_web("deploy: { replicas: 2 }", "services.web.deploy");
    refuses_web("extends: { file: other.yaml, service: x }", "services.web.extends");
    refuses_web("secrets: [token]", "services.web.secrets");
    refuses_web("configs: [conf]", "services.web.configs");
    refuses_web("profiles: [debug]", "services.web.profiles");
    refuses_web("scale: 2", "services.web.scale");
    refuses_web("runtime: nvidia", "services.web.runtime: isn't supported");
    refuses_web("external_links: [other]", "services.web.external_links");
    refuses_web("cgroup_parent: /x", "services.web.cgroup_parent");
}

#[test]
fn refuses_bad_references() {
    refuses_web("depends_on: [cache]", "`cache` isn't another service");
    refuses_web("depends_on: [web]", "`web` isn't another service");
    refuses_web("links: [cache]", "services.web.links");
    refuses_web("volumes_from: [\"container:other\"]", "services.web.volumes_from");
    refuses_web("networks: [front]", "network `front` isn't declared");
    refuses_web(
        "networks: { default: { ipv4_address: 10.0.0.5 } }",
        "may only set `aliases`",
    );
    refuses_web("restart: sometimes", "services.web.restart");
    ok(&bundle(
        "volumes_from: [\"db:ro\"]\nlinks: [\"db:database\"]\nrestart: on-failure:3",
        "",
    ));
}

#[test]
fn refuses_reserved_labels_and_other_log_drivers() {
    refuses_web("labels: { com.docker.compose.project: other }", "is reserved");
    refuses_web("labels: [\"kiwi-nest.app=other\"]", "is reserved");
    refuses_web("logging: { driver: syslog }", "services.web.logging");
}

#[test]
fn refuses_anonymous_volumes() {
    refuses_web("volumes: [/data]", "anonymous volumes aren't supported");
    refuses_web("volumes: [{ type: volume, target: /data }]", "anonymous volumes");
}

#[test]
fn refuses_undeclared_volumes() {
    refuses_web("volumes: [\"cache:/cache\"]", "volume `cache` isn't declared");
    refuses_web("volumes: [\"site:/cache\"]", "volume `site` isn't declared");
}

#[test]
fn refuses_bad_bundle_paths() {
    refuses_web("volumes: [\"./missing:/x\"]", "`./missing` isn't in the bundle");
    refuses_web("volumes: [\"../outside:/x\"]", "goes outside the bundle");
    refuses_web("volumes: [\"./site/../../x:/x\"]", "goes outside the bundle");
    refuses_web("volumes: [\"./site:/app:rw\"]", "always mounted read-only");
    refuses_web(
        "volumes: [{ type: bind, source: ./site, target: /x, read_only: false }]",
        "always mounted read-only",
    );
    refuses_web("volumes: [\"~/secrets:/x\"]", "home-directory paths");
    refuses_web("volumes: [\"./site:app\"]", "the target must be an absolute path");
}

#[test]
fn refuses_host_paths_unless_allowed() {
    refuses_web(
        "volumes: [\"/var/run/docker.sock:/var/run/docker.sock\"]",
        "which this node doesn't allow",
    );
    refuses_web("volumes: [\"/:/host\"]", "which this node doesn't allow");
    let opts = CheckOptions {
        allowed_host_paths: vec![PathBuf::from("/srv/media")],
    };
    let yaml = |src: &str| bundle(&format!("volumes: [\"{src}:/media:ro\"]"), "");
    let b = check_with(&yaml("/srv/media/films"), &opts).unwrap();
    assert!(
        matches!(&b.services[0].mounts[0], Mount::Host { path, read_only: true, .. } if path == "/srv/media/films")
    );
    check_with(&yaml("/srv/media"), &opts).unwrap();
    assert!(
        check_with(&yaml("/srv/mediax"), &opts)
            .unwrap_err()
            .contains("doesn't allow")
    );
    assert!(
        check_with(&yaml("/srv/media/../../etc"), &opts)
            .unwrap_err()
            .contains("mustn't contain `..`")
    );
}

#[test]
fn refuses_unsupported_mounts() {
    refuses_web(
        "volumes: [{ type: npipe, source: x, target: /x }]",
        "mount type `npipe` isn't supported",
    );
    refuses_web(
        "volumes: [{ type: volume, source: data, target: /x, volume: { subpath: a } }]",
        "`volume.subpath`",
    );
    refuses_web(
        "volumes: [{ type: bind, source: ./site, target: /x, bind: { propagation: rshared } }]",
        "`bind.propagation`",
    );
    refuses_web("volumes: [{ type: bind, source: data, target: /x }]", "must be a path");
    refuses_web(
        "volumes: [{ type: volume, source: ./site, target: /x }]",
        "must be a named volume",
    );
    refuses_web(
        "volumes: [\"data:/x:nocopy,shared\"]",
        "mount option `nocopy` isn't supported",
    );
    refuses_web("volumes: [\"a:b:c:d\"]", "isn't a valid mount");
    refuses_web("volumes: [{ source: data, target: /x }]", "needs a `type`");
}

#[test]
fn refuses_env_files_outside_the_bundle() {
    refuses_web("env_file: missing.env", "`missing.env` isn't a file in the bundle");
    refuses_web("env_file: /etc/environment", "must be a path in the bundle");
    refuses_web("env_file: ../x.env", "goes outside the bundle");
    refuses_web("env_file: site", "isn't a file in the bundle");
}

#[test]
fn refuses_foreign_volumes_and_networks() {
    for settings in [
        "{ external: true }",
        "{ name: shared-data }",
        "{ driver: local }",
        "{ driver_opts: { type: nfs } }",
    ] {
        let yaml = bundle("", "").replace("  data: {}", &format!("  data: {settings}"));
        let e = refused(&yaml);
        assert!(e.contains("volumes.data: must be `{}`"), "{settings}: {e}");
    }
    for settings in [
        "{ external: true }",
        "{ name: bridge }",
        "{ driver: overlay }",
        "{ ipam: {} }",
    ] {
        refuses_top(&format!("networks:\n  back: {settings}\n"), "networks.back.");
    }
    let e = refused(&bundle("", "").replace("  data: {}", "  data: {}\n  '../x': {}"));
    assert!(e.contains("isn't a valid volume name"), "{e}");
}

#[test]
fn refuses_unsupported_top_level_keys() {
    refuses_top("include: [other.yaml]\n", "include: isn't supported");
    refuses_top("secrets: { token: { file: ./token } }\n", "secrets: aren't supported");
    refuses_top("configs: {}\n", "configs: aren't supported");
    refuses_top("services2: {}\n", "services2: isn't a supported top-level key");
}

#[test]
fn refuses_interpolation() {
    refuses_web(
        "environment: { TOKEN: \"${TOKEN}\" }",
        "services.web.environment.TOKEN: uses variable interpolation",
    );
    refuses_web(
        "command: \"echo $HOME\"",
        "services.web.command: uses variable interpolation",
    );
    refuses_web(
        "image: \"example/web:${TAG:-1}\"",
        "services.web.image: uses variable interpolation",
    );
    // $$ is a literal $.
    ok(&bundle("command: \"echo $$HOME\"", ""));
}

#[test]
fn refuses_yaml_tags() {
    refuses_web("command: !reset []", "uses a YAML tag");
}

#[test]
fn refuses_broken_files() {
    assert!(refused("services: [").contains("isn't valid YAML"));
    assert!(refused("- a\n- b\n").contains("must be a mapping"));
    assert!(
        refused("services: {}\nx-kiwi-nest: { domain: a.b, service: web, port: 80 }\n")
            .contains("at least one service")
    );
    assert!(refused("a: 1\na: 2\n").contains("isn't valid YAML"));
}

#[test]
fn refuses_bad_manifests() {
    let yaml = bundle("", "").replace(
        "x-kiwi-nest: { domain: blog.example.com, service: web, port: 8080 }\n",
        "",
    );
    assert!(refused(&yaml).contains("x-kiwi-nest: is required"));
    let yaml = bundle("", "").replace("service: web", "service: api");
    assert!(refused(&yaml).contains("x-kiwi-nest.service: names `api`, which isn't a service"));
    let yaml = bundle("", "").replace("port: 8080", "port: 0");
    assert!(refused(&yaml).contains("x-kiwi-nest.port"));
}

#[test]
fn reports_every_problem() {
    let e = refused(&bundle("ports: [\"80:80\"]\nprivileged: true\nbuild: .", ""));
    assert_eq!(e.lines().count(), 3, "{e}");
}

// The generated file.

struct Gen {
    volume_dirs: BTreeMap<String, PathBuf>,
    shared_dirs: BTreeMap<String, PathBuf>,
    labels: BTreeMap<String, String>,
}

impl Gen {
    fn new() -> Self {
        let volume_dirs = [("data", "/vol/apps/blog/data"), ("db", "/vol/apps/blog/db")]
            .into_iter()
            .map(|(k, v)| (k.to_string(), PathBuf::from(v)))
            .collect();
        let labels = [("kiwi-nest.app".to_string(), "blog".to_string())]
            .into_iter()
            .collect();
        let shared_dirs = [
            ("corpus", "/vol/shared/corpus/data"),
            ("maps", "/vol/shared/maps/snaps/blog"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), PathBuf::from(v)))
        .collect();
        Gen {
            volume_dirs,
            shared_dirs,
            labels,
        }
    }

    fn params<'a>(&'a self, bundle_dir: &'a Path) -> Params<'a> {
        Params {
            project: "kn-blog",
            bundle_dir,
            volume_dirs: &self.volume_dirs,
            shared_dirs: &self.shared_dirs,
            host_port: 31234,
            labels: &self.labels,
        }
    }

    fn run(&self, yaml: &str) -> Value {
        let b = ok(yaml);
        let text = generate(&b, &self.params(Path::new("/state/apps/blog/bundle-1"))).unwrap();
        assert!(text.starts_with("# Written by kiwi-nest"));
        serde_yaml_ng::from_str(&text).unwrap()
    }
}

fn get<'a>(v: &'a Value, path: &str) -> &'a Value {
    path.split('.').fold(v, |v, k| match k.parse::<usize>() {
        Ok(i) => &v[i],
        Err(_) => &v[k],
    })
}

#[test]
fn generated_file_names_the_project() {
    let out = Gen::new().run(&bundle("", ""));
    assert_eq!(get(&out, "name"), "kn-blog");
    assert!(out.get("volumes").is_none(), "no Docker volumes at all");
}

#[test]
fn only_the_routed_service_gets_a_loopback_port() {
    let out = Gen::new().run(&bundle("", ""));
    let ports = get(&out, "services.web.ports").as_sequence().unwrap();
    assert_eq!(ports.len(), 1);
    assert_eq!(get(&ports[0], "host_ip"), "127.0.0.1");
    assert_eq!(get(&ports[0], "published"), "31234");
    assert_eq!(get(&ports[0], "target"), 8080);
    assert!(get(&out, "services.db").get("ports").is_none());
}

#[test]
fn restart_always_becomes_unless_stopped() {
    let out = Gen::new().run(&bundle("restart: always", ""));
    assert_eq!(get(&out, "services.web.restart"), "unless-stopped");
    let out = Gen::new().run(&bundle("restart: on-failure:3", ""));
    assert_eq!(get(&out, "services.web.restart"), "on-failure:3");
    let out = Gen::new().run(&bundle("", ""));
    assert!(get(&out, "services.web").get("restart").is_none());
}

#[test]
fn named_volumes_become_bind_mounts_of_the_apps_directories() {
    let out = Gen::new().run(&bundle("", ""));
    let site = get(&out, "services.web.volumes.0");
    assert_eq!(get(site, "type"), "bind");
    assert_eq!(get(site, "source"), "/state/apps/blog/bundle-1/site");
    assert_eq!(get(site, "read_only").as_bool(), Some(true));
    assert_eq!(get(site, "bind.create_host_path").as_bool(), Some(false));
    let data = get(&out, "services.web.volumes.1");
    assert_eq!(get(data, "type"), "bind");
    assert_eq!(get(data, "source"), "/vol/apps/blog/data");
    assert_eq!(get(data, "target"), "/var/lib/blog");
    assert_eq!(get(data, "read_only").as_bool(), Some(false));
    assert_eq!(get(data, "bind.create_host_path").as_bool(), Some(false));
    assert_eq!(get(&out, "services.db.volumes.0.source"), "/vol/apps/blog/db");
}

/// The base bundle, with `x-kiwi-nest.shared` set to `shared`.
fn with_shared(shared: &str, web: &str) -> String {
    bundle(web, "").replacen("port: 8080 }", &format!("port: 8080, shared: {shared} }}"), 1)
}

#[test]
fn shared_volumes_become_bind_mounts_by_mode() {
    let out = Gen::new().run(&with_shared(
        "{ corpus: { target: /corpus }, maps: { target: /maps, mode: snapshot, services: [web] } }",
        "",
    ));
    // `ro` by default, into every service.
    for svc in ["web", "db"] {
        let vols = get(&out, &format!("services.{svc}.volumes")).as_sequence().unwrap();
        let corpus = vols.iter().find(|v| v["target"] == "/corpus").unwrap();
        assert_eq!(get(corpus, "source"), "/vol/shared/corpus/data");
        assert_eq!(get(corpus, "read_only").as_bool(), Some(true));
        assert_eq!(get(corpus, "bind.create_host_path").as_bool(), Some(false));
    }
    // A snapshot is the app's own, so it's writable; and it's only in the services named.
    let web = get(&out, "services.web.volumes").as_sequence().unwrap();
    let maps = web.iter().find(|v| v["target"] == "/maps").unwrap();
    assert_eq!(get(maps, "source"), "/vol/shared/maps/snaps/blog");
    assert_eq!(get(maps, "read_only").as_bool(), Some(false));
    let db = get(&out, "services.db.volumes").as_sequence().unwrap();
    assert!(db.iter().all(|v| v["target"] != "/maps"));

    let out = Gen::new().run(&with_shared("{ corpus: { target: /corpus, mode: rw } }", ""));
    assert_eq!(get(&out, "services.web.volumes.2.read_only").as_bool(), Some(false));

    // A service without mounts of its own still gets them.
    let yaml = "x-kiwi-nest: { domain: a.example.com, service: web, port: 80, shared: { corpus: { target: /c } } }\n\
                services:\n  web:\n    image: x\n";
    let out = Gen::new().run(yaml);
    assert_eq!(get(&out, "services.web.volumes.0.target"), "/c");

    let mut g = Gen::new();
    g.shared_dirs.clear();
    assert!(matches!(
        generate(&ok(yaml), &g.params(Path::new("/b"))),
        Err(GenerateError::MissingShared(_))
    ));
}

#[test]
fn refuses_shared_volumes_on_taken_paths_or_missing_services() {
    let e = refused(&with_shared("{ corpus: { target: /var/lib/blog } }", ""));
    assert!(
        e.contains(
            "x-kiwi-nest.shared.corpus: mounts at `/var/lib/blog`, where service `web` already mounts something"
        ),
        "{e}"
    );
    // Only the services it's mounted into count.
    ok(&with_shared(
        "{ corpus: { target: /var/lib/blog, services: [db] } }",
        "",
    ));
    let e = refused(&with_shared("{ a: { target: /x }, b: { target: /x, mode: rw } }", ""));
    assert!(
        e.contains("x-kiwi-nest.shared.b: mounts at `/x` in service `web`, like shared volume `a`"),
        "{e}"
    );
    ok(&with_shared(
        "{ a: { target: /x, services: [web] }, b: { target: /x, services: [db] } }",
        "",
    ));
    let e = refused(&with_shared("{ corpus: { target: /c, services: [worker] } }", ""));
    assert!(
        e.contains("x-kiwi-nest.shared.corpus: names service `worker`, which doesn't exist"),
        "{e}"
    );
}

#[test]
fn tmpfs_and_whole_bundle_mounts() {
    let out = Gen::new().run(&bundle(
        "volumes: [{ type: tmpfs, target: /cache, tmpfs: { size: 10m } }, \".:/bundle\"]",
        "",
    ));
    assert_eq!(get(&out, "services.web.volumes.0.type"), "tmpfs");
    assert_eq!(get(&out, "services.web.volumes.0.tmpfs.size"), "10m");
    assert_eq!(get(&out, "services.web.volumes.1.source"), "/state/apps/blog/bundle-1");
}

#[test]
fn env_files_and_labels() {
    let out = Gen::new().run(&bundle("env_file: ./app.env\nlabels: [\"team=blog\"]", ""));
    assert_eq!(
        get(&out, "services.web.env_file.0"),
        "/state/apps/blog/bundle-1/app.env"
    );
    assert_eq!(get(&out, "services.web.labels.team"), "blog");
    assert_eq!(get(&out, "services.web.labels")["kiwi-nest.app"], "blog");
    assert_eq!(get(&out, "services.db.labels")["kiwi-nest.app"], "blog");
}

#[test]
fn passes_settings_through() {
    let out = Gen::new().run(&bundle(
        "command: [\"sh\", \"-c\", \"echo $$PATH\"]\nmem_limit: 256m\nnetworks: [back]\nx-ignored: 1",
        "networks:\n  back: { internal: true, x-note: 1 }\n",
    ));
    assert_eq!(get(&out, "services.web.command.2"), "echo $$PATH");
    assert_eq!(get(&out, "services.web.mem_limit"), "256m");
    assert_eq!(get(&out, "services.web.image"), "example/web:1");
    assert!(get(&out, "services.web").get("x-ignored").is_none());
    assert_eq!(get(&out, "networks.back.internal").as_bool(), Some(true));
    assert!(get(&out, "networks.back").get("x-note").is_none());
}

#[test]
fn refuses_unusable_paths() {
    let g = Gen::new();
    let b = ok(&bundle("", ""));
    assert!(matches!(
        generate(&b, &g.params(Path::new("relative"))),
        Err(GenerateError::NotAbsolute(_))
    ));
    assert!(matches!(
        generate(&b, &g.params(Path::new("/a$b"))),
        Err(GenerateError::BadPath(_))
    ));
    let mut g2 = Gen::new();
    g2.volume_dirs.remove("db");
    assert!(matches!(
        generate(&b, &g2.params(Path::new("/b"))),
        Err(GenerateError::MissingVolume(_))
    ));
}

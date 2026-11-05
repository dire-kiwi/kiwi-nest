//! The generated Caddy configuration in each TLS mode, the routes read from the store, and syncing with a fake
//! Caddy admin API.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};

use super::*;

const HASH: &str = "$2y$05$abcdefghijklmnopqrstuuG3dNw1n3eO1n2Cx4M3jW7sXkEw0LZ6u";

fn route(domain: &str, upstream: &str, auth: bool) -> Route {
    Route {
        domain: domain.into(),
        upstream: upstream.parse().unwrap(),
        token: format!("kni_{domain}"),
        auth: if auth {
            vec![BasicAuth {
                user: "preview".into(),
                hash: HASH.into(),
            }]
        } else {
            vec![]
        },
    }
}

fn api() -> SocketAddr {
    "127.0.0.1:7400".parse().unwrap()
}

fn cfg(tls: Tls) -> IngressConfig {
    let mut c = IngressConfig::new("http://127.0.0.1:2019", tls);
    c.api_domain = Some("kn.example.com".into());
    c
}

fn generate(c: &IngressConfig, routes: &[Route]) -> Value {
    let listen = c.validate().unwrap();
    caddy_config(c, &listen, api(), routes)
}

fn server(config: &Value) -> &Value {
    &config["apps"]["http"]["servers"]["kiwi-nest"]
}

/// The handlers of the route for `host`.
fn handlers<'a>(config: &'a Value, host: &str) -> &'a Vec<Value> {
    server(config)["routes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["match"][0]["host"][0] == host)
        .unwrap_or_else(|| panic!("no route for {host}"))["handle"]
        .as_array()
        .unwrap()
}

#[test]
fn routes_go_to_each_node_with_its_token_and_unknown_hosts_get_404() {
    let c = cfg(Tls::Acme { email: None, ca: None });
    let config = generate(
        &c,
        &[
            route("blog.example.com", "10.0.0.5:7382", false),
            route("blog-pr1.example.com", "10.0.0.6:7382", true),
        ],
    );
    let routes = server(&config)["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 4, "the API, two apps, and the catch-all");

    // The server's API comes first, to the server's listener, and never passes an ingress token on.
    assert_eq!(routes[0]["match"][0]["host"][0], "kn.example.com");
    let h = handlers(&config, "kn.example.com");
    assert_eq!(h.len(), 1);
    assert_eq!(h[0]["handler"], "reverse_proxy");
    assert_eq!(h[0]["upstreams"][0]["dial"], "127.0.0.1:7400");
    assert_eq!(h[0]["headers"]["request"]["delete"][0], INGRESS_HEADER);
    assert_eq!(h[0]["flush_interval"], -1);

    // An app without auth: straight to its node's gateway, with that node's token, streams flushed and WebSockets
    // kept open across reloads.
    let h = handlers(&config, "blog.example.com");
    assert_eq!(h.len(), 1);
    assert_eq!(h[0]["handler"], "reverse_proxy");
    assert_eq!(h[0]["upstreams"][0]["dial"], "10.0.0.5:7382");
    assert_eq!(
        h[0]["headers"]["request"]["set"][INGRESS_HEADER],
        json!(["kni_blog.example.com"])
    );
    assert_eq!(h[0]["flush_interval"], -1);
    assert_eq!(h[0]["stream_close_delay"], STREAM_CLOSE_DELAY);

    // An app with auth: Caddy checks it first, with bcrypt and its cache.
    let h = handlers(&config, "blog-pr1.example.com");
    assert_eq!(h.len(), 2);
    assert_eq!(h[0]["handler"], "authentication");
    let basic = &h[0]["providers"]["http_basic"];
    assert_eq!(basic["accounts"], json!([{ "username": "preview", "password": HASH }]));
    assert_eq!(basic["hash"]["algorithm"], "bcrypt");
    assert!(basic["hash_cache"].is_object());
    assert_eq!(h[1]["handler"], "reverse_proxy");
    assert_eq!(h[1]["upstreams"][0]["dial"], "10.0.0.6:7382");
    assert_eq!(
        h[1]["headers"]["request"]["set"][INGRESS_HEADER],
        json!(["kni_blog-pr1.example.com"])
    );
    for r in &routes[..3] {
        assert_eq!(r["terminal"], true);
    }

    // Anything else: 404, from Caddy.
    let last = routes.last().unwrap();
    assert!(last.get("match").is_none());
    assert_eq!(last["handle"][0]["handler"], "static_response");
    assert_eq!(last["handle"][0]["status_code"], 404);

    // The admin API stays where it is, and Caddy doesn't save the configuration (it holds the tokens).
    assert_eq!(config["admin"]["listen"], "127.0.0.1:2019");
    assert_eq!(config["admin"]["config"]["persist"], false);
}

#[test]
fn an_app_on_the_api_domain_isnt_routed() {
    let c = cfg(Tls::Internal);
    let config = generate(&c, &[route("kn.example.com", "10.0.0.5:7382", false)]);
    let routes = server(&config)["routes"].as_array().unwrap();
    assert_eq!(routes.len(), 2);
    assert_eq!(
        handlers(&config, "kn.example.com")[0]["upstreams"][0]["dial"],
        "127.0.0.1:7400"
    );
    // Without an API domain, there's no route for the API.
    let mut c = cfg(Tls::Internal);
    c.api_domain = None;
    let config = generate(&c, &[route("blog.example.com", "10.0.0.5:7382", false)]);
    assert_eq!(server(&config)["routes"].as_array().unwrap().len(), 2);
}

#[test]
fn acme_mode() {
    // Caddy's defaults: HTTPS on 443, certificates from its default ACME CAs, HTTP redirected.
    let config = generate(&cfg(Tls::Acme { email: None, ca: None }), &[]);
    assert_eq!(server(&config)["listen"], json!([":443"]));
    assert_eq!(server(&config)["tls_connection_policies"], json!([{}]));
    assert!(server(&config).get("automatic_https").is_none());
    assert_eq!(config["apps"]["http"]["http_port"], 80);
    assert_eq!(config["apps"]["http"]["https_port"], 443);
    assert!(config["apps"].get("tls").is_none());
    assert!(config["apps"].get("pki").is_none());

    // An email and a CA (such as a staging one) go to one ACME issuer.
    let mut c = cfg(Tls::Acme {
        email: Some("ops@example.com".into()),
        ca: Some("https://acme-staging-v02.api.letsencrypt.org/directory".into()),
    });
    c.http_port = 8080;
    c.https_port = 8443;
    let config = generate(&c, &[]);
    assert_eq!(server(&config)["listen"], json!([":8443"]));
    assert_eq!(config["apps"]["http"]["http_port"], 8080);
    assert_eq!(config["apps"]["http"]["https_port"], 8443);
    assert_eq!(
        config["apps"]["tls"]["automation"]["policies"],
        json!([{ "issuers": [{
            "module": "acme",
            "email": "ops@example.com",
            "ca": "https://acme-staging-v02.api.letsencrypt.org/directory",
        }] }])
    );
}

#[test]
fn internal_ca_mode() {
    let config = generate(
        &cfg(Tls::Internal),
        &[route("blog.example.com", "10.0.0.5:7382", false)],
    );
    assert_eq!(server(&config)["listen"], json!([":443"]));
    assert_eq!(server(&config)["tls_connection_policies"], json!([{}]));
    assert_eq!(
        config["apps"]["tls"]["automation"]["policies"],
        json!([{ "issuers": [{ "module": "internal" }] }])
    );
    assert_eq!(
        config["apps"]["pki"]["certificate_authorities"]["local"]["install_trust"],
        false
    );
}

#[test]
fn certificates_folder_mode() {
    let config = generate(&cfg(Tls::Folder("/etc/kiwi-nest/certs".into())), &[]);
    assert_eq!(server(&config)["listen"], json!([":443"]));
    assert_eq!(server(&config)["tls_connection_policies"], json!([{}]));
    // Never a CA: only the folder's certificates. HTTP still redirects.
    assert_eq!(
        server(&config)["automatic_https"],
        json!({ "disable_certificates": true })
    );
    assert_eq!(
        config["apps"]["tls"],
        json!({ "certificates": { "load_folders": ["/etc/kiwi-nest/certs"] } })
    );
    assert!(config["apps"].get("pki").is_none());
}

#[test]
fn plain_http_mode() {
    let mut c = cfg(Tls::Http);
    c.http_port = 8080;
    c.trusted_proxies = vec!["10.0.0.0/8".into(), "192.168.1.1".into()];
    let config = generate(&c, &[route("blog.example.com", "10.0.0.5:7382", true)]);
    assert_eq!(server(&config)["listen"], json!([":8080"]));
    assert_eq!(server(&config)["automatic_https"], json!({ "disable": true }));
    assert!(server(&config).get("tls_connection_policies").is_none());
    assert_eq!(config["apps"]["http"]["http_port"], 8080);
    assert!(config["apps"]["http"].get("https_port").is_none());
    assert!(config["apps"].get("tls").is_none());
    assert_eq!(
        server(&config)["trusted_proxies"],
        json!({ "source": "static", "ranges": ["10.0.0.0/8", "192.168.1.1"] })
    );
    // Auth is checked the same way.
    assert_eq!(handlers(&config, "blog.example.com")[0]["handler"], "authentication");
}

#[test]
fn the_configuration_is_checked() {
    let ok = cfg(Tls::Http);
    assert_eq!(ok.validate().unwrap(), "127.0.0.1:2019");
    assert_eq!(
        IngressConfig::new("http://localhost:2019/", Tls::Http)
            .validate()
            .unwrap(),
        "localhost:2019"
    );
    for admin in [
        "https://127.0.0.1:2019",
        "127.0.0.1:2019",
        "http://127.0.0.1",
        "http://127.0.0.1:0",
        "http://127.0.0.1:2019/x",
        "http://:2019",
    ] {
        assert!(IngressConfig::new(admin, Tls::Http).validate().is_err(), "{admin}");
    }
    let bad = |f: &dyn Fn(&mut IngressConfig)| {
        let mut c = cfg(Tls::Http);
        f(&mut c);
        c.validate().is_err()
    };
    assert!(bad(&|c| c.api_domain = Some("KN.example.com".into())));
    assert!(bad(&|c| c.api_domain = Some("{env.HOME}".into())));
    assert!(bad(&|c| c.https_port = c.http_port));
    assert!(bad(&|c| c.trusted_proxies = vec!["10.0.0.0/33".into()]));
    assert!(bad(&|c| c.trusted_proxies = vec!["private_ranges".into()]));
    assert!(bad(&|c| c.tls = Tls::Folder("certs".into())));
    assert!(bad(&|c| c.tls = Tls::Acme {
        email: Some("{file./etc/passwd}@x".into()),
        ca: None
    }));
    assert!(bad(&|c| c.tls = Tls::Acme {
        email: None,
        ca: Some("letsencrypt".into())
    }));
    assert!(!bad(&|c| c.trusted_proxies = vec!["fd00::/8".into(), "10.1.2.3".into()]));
}

#[test]
fn unspecified_addresses_are_reached_on_loopback() {
    assert_eq!(
        reachable("0.0.0.0:7400".parse().unwrap()),
        "127.0.0.1:7400".parse().unwrap()
    );
    assert_eq!(reachable("[::]:7400".parse().unwrap()), "[::1]:7400".parse().unwrap());
    assert_eq!(
        reachable("10.0.0.1:7400".parse().unwrap()),
        "10.0.0.1:7400".parse().unwrap()
    );
}

#[test]
fn routes_come_from_active_apps_on_nodes_the_ingress_can_reach() {
    let mut c = Connection::open_in_memory().unwrap();
    crate::store::migrate(&mut c).unwrap();
    let node = |id: u64, name: &str, ingress: &str, token: Option<&str>| {
        c.execute(
            "INSERT INTO nodes (id, name, facts, joined_at, ingress_token) VALUES (?1, ?2, ?3, 0, ?4)",
            rusqlite::params![id, name, json!({ "ingress": ingress }).to_string(), token],
        )
        .unwrap();
    };
    node(1, "a", "10.0.0.5:7382", Some("kni_a"));
    node(2, "b", "", Some("kni_b"));
    node(3, "c", "0.0.0.0:7382", Some("kni_c"));
    node(4, "d", "10.0.0.8:7382", None);
    let app = |name: &str, state: &str, node: u64, domain: Option<&str>, auth: Option<&str>| {
        c.execute(
            "INSERT INTO apps (name, state, node_id, domain, generation, seq, created_at, updated_at, auth)
             VALUES (?1, ?2, ?3, ?4, 1, 1, 0, 0, ?5)",
            rusqlite::params![name, state, node, domain, auth],
        )
        .unwrap();
    };
    let auth = json!([{ "user": "preview", "hash": HASH }]).to_string();
    app("blog", "active", 1, Some("blog.test"), Some("[]"));
    app("blog-pr1", "active", 1, Some("blog-pr1.test"), Some(&auth));
    app("old", "active", 1, Some("old.test"), None);
    app("going", "deleting", 1, Some("going.test"), Some("[]"));
    app("gone", "deleted", 1, None, Some("[]"));
    app("shop", "active", 2, Some("shop.test"), Some("[]"));
    app("notes", "active", 3, Some("notes.test"), Some("[]"));
    app("maps", "active", 4, Some("maps.test"), Some("[]"));
    let (routes, warnings) = super::routes(&c).unwrap();
    assert_eq!(
        routes,
        vec![
            Route {
                domain: "blog-pr1.test".into(),
                upstream: "10.0.0.5:7382".parse().unwrap(),
                token: "kni_a".into(),
                auth: vec![BasicAuth {
                    user: "preview".into(),
                    hash: HASH.into()
                }],
            },
            Route {
                domain: "blog.test".into(),
                upstream: "10.0.0.5:7382".parse().unwrap(),
                token: "kni_a".into(),
                auth: vec![],
            },
        ]
    );
    let warnings = warnings.join("\n");
    // An app whose users aren't known isn't routed without them.
    assert!(
        warnings.contains("app `old` isn't routed: its basic-auth users aren't known"),
        "{warnings}"
    );
    assert!(warnings.contains("node `b` has no ingress address"), "{warnings}");
    assert!(
        warnings.contains("node `c` reported a bad ingress address"),
        "{warnings}"
    );
    assert!(warnings.contains("node `d` has no ingress token"), "{warnings}");
    assert!(!warnings.contains("going") && !warnings.contains("gone"), "{warnings}");
}

/// A fake of Caddy's admin API: `/config/`, `/load` and `/pki/ca/local`, recording each load.
#[derive(Clone, Default)]
pub(crate) struct FakeCaddy {
    pub config: Arc<Mutex<Value>>,
    /// Each load's body, and whether it was forced (`Cache-Control: must-revalidate`).
    pub loads: Arc<Mutex<Vec<(Value, bool)>>>,
}

impl FakeCaddy {
    /// Serves the fake on a loopback port; returns it and its URL.
    pub async fn start() -> (FakeCaddy, String) {
        let fake = FakeCaddy::default();
        *fake.config.lock().unwrap() = Value::Null;
        let app = axum::Router::new()
            .route(
                "/config/",
                get(|State(f): State<FakeCaddy>| async move { axum::Json(f.config.lock().unwrap().clone()) }),
            )
            .route(
                "/load",
                post(
                    |State(f): State<FakeCaddy>, headers: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                        let forced = headers
                            .get("cache-control")
                            .is_some_and(|v| v.as_bytes() == b"must-revalidate");
                        f.loads.lock().unwrap().push((body.clone(), forced));
                        *f.config.lock().unwrap() = body;
                        StatusCode::OK
                    },
                ),
            )
            .route(
                "/pki/ca/local",
                get(|| async {
                    axum::Json(json!({
                        "id": "local",
                        "root_certificate": "-----BEGIN CERTIFICATE-----\nroot\n-----END CERTIFICATE-----\n",
                    }))
                }),
            )
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (fake, url)
    }

    pub fn loads(&self) -> usize {
        self.loads.lock().unwrap().len()
    }

    /// Like a restarted Caddy: no configuration.
    pub fn restart(&self) {
        *self.config.lock().unwrap() = Value::Null;
    }

    pub fn current(&self) -> Value {
        self.config.lock().unwrap().clone()
    }

    /// Waits until `f` holds of the configuration.
    pub async fn until(&self, what: &str, f: impl Fn(&Value) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while !f(&self.current()) {
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting until {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Starts a server configured with `ingress`, and its sync loop.
async fn server_with(ingress: IngressConfig) -> (tempfile::TempDir, Server, CancellationToken) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = crate::ServerConfig::new(dir.path().join("server"));
    cfg.ingress = Some(ingress);
    let (server, _) = Server::open(cfg).await.unwrap();
    let stop = CancellationToken::new();
    let (i, s, st) = (server.ingress.clone().unwrap(), server.clone(), stop.clone());
    tokio::spawn(async move { i.run(s, api(), st).await });
    (dir, server, stop)
}

#[tokio::test]
async fn caddy_is_configured_at_start_after_changes_and_again_after_it_restarts() {
    let (fake, url) = FakeCaddy::start().await;
    let mut c = IngressConfig::new(url, Tls::Internal);
    c.sync_interval = Duration::from_millis(200);
    let (dir, server, stop) = server_with(c).await;
    fake.until("the first load", |c| !c.is_null()).await;
    assert_eq!(fake.loads(), 1);
    assert!(
        fake.current()["admin"]["listen"]
            .as_str()
            .unwrap()
            .starts_with("127.0.0.1:")
    );
    // The internal CA's root is saved for nodes and the CLI.
    let ca = dir.path().join("server/ingress-ca.crt");
    for _ in 0..100 {
        if ca.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(std::fs::read_to_string(&ca).unwrap().contains("BEGIN CERTIFICATE"));

    // Nothing changed: Caddy isn't reloaded, however often it's checked.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(fake.loads(), 1);

    // A change is loaded at once.
    server
        .store
        .call(|c| {
            c.execute(
                "INSERT INTO nodes (id, name, facts, joined_at, ingress_token)
                 VALUES (1, 'a', '{\"ingress\":\"10.0.0.5:7382\"}', 0, 'kni_a')",
                [],
            )?;
            c.execute(
                "INSERT INTO apps (name, state, node_id, domain, generation, seq, created_at, updated_at, auth)
                 VALUES ('blog', 'active', 1, 'blog.test', 1, 1, 0, 0, '[]')",
                [],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    server.ingress_changed();
    let has_blog = |c: &Value| c.to_string().contains("blog.test");
    tokio::time::timeout(Duration::from_millis(150), fake.until("blog is routed", has_blog))
        .await
        .expect("a change is loaded without waiting for the interval");
    assert_eq!(fake.loads(), 2);

    // A restarted Caddy has nothing; it gets the configuration back.
    fake.restart();
    fake.until("the configuration is back", has_blog).await;
    assert_eq!(fake.loads(), 3);
    stop.cancel();
}

#[tokio::test]
async fn a_changed_certificates_folder_forces_a_reload() {
    let (fake, url) = FakeCaddy::start().await;
    let certs = tempfile::tempdir().unwrap();
    std::fs::write(certs.path().join("wildcard.pem"), "one").unwrap();
    let mut c = IngressConfig::new(url, Tls::Folder(certs.path().to_path_buf()));
    c.sync_interval = Duration::from_millis(100);
    let (_dir, _server, stop) = server_with(c).await;
    fake.until("the first load", |c| !c.is_null()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(fake.loads(), 1);
    std::fs::write(certs.path().join("wildcard.pem"), "renewed").unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while fake.loads() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the renewed certificate should be loaded"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let loads = fake.loads.lock().unwrap().clone();
    assert!(!loads[0].1);
    assert!(
        loads[1].1,
        "the same configuration is loaded again with must-revalidate"
    );
    assert_eq!(loads[0].0, loads[1].0);
    stop.cancel();
}

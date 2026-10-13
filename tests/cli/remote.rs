//! Offline end-to-end pairing, exclusive routing, stdio MCP and revocation.
#[cfg(windows)]
#[path = "../support/windows.rs"]
mod windows;
use search::cli::auth::{self, Credential, Devices, Host, Remotes};
use search::client::{self as client_api, Client, Remote};
use std::{path::PathBuf, sync::Arc};

/// Own an isolated HTTPS host, its offline settings-defined engine, and shutdown cleanup.
struct Fixture {
    root: PathBuf,
    host: Arc<Host>,
    url: String,
    handle: axum_server::Handle<std::net::SocketAddr>,
    task: tokio::task::JoinHandle<()>,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_window(std::time::Duration::from_secs(900)).await
    }
    async fn with_window(window: std::time::Duration) -> Self {
        Self::with_settings(window, 10).await
    }
    /// Start a host with a chosen service default to test HTTP limit resolution.
    async fn with_settings(window: std::time::Duration, max_results: usize) -> Self {
        Self::with_config(
            window,
            search::core::Config {
                search: search::core::config::SearchSettings {
                    max_results,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await
    }
    /// Start the real paired host with chosen machine defaults and an offline engine.
    async fn with_config(window: std::time::Duration, mut config: search::core::Config) -> Self {
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("search-remote-{}", uuid::Uuid::new_v4()));
        let host = Arc::new(Host::open(&root, "localhost", window).unwrap());
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        config.home = root.clone();
        config.address = address.to_string();
        if config.engines.use_engines == ["mwmbl"] {
            config.engines = search::core::config::EngineSettings {
                use_engines: vec!["fixture".into()],
                config: [("fixture".into(), fixture_engine(&root))].into(),
                ..Default::default()
            };
        }
        let engine = Arc::new(search::core::Search::open(config).unwrap());
        let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
            host.cert.as_bytes().to_vec(),
            host.key.as_bytes().to_vec(),
        )
        .await
        .unwrap();
        let handle = axum_server::Handle::new();
        let server_handle = handle.clone();
        let router = search::cli::http::router(
            engine,
            host.clone(),
            &format!("localhost:{}", address.port()),
        );
        let task = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)
                .unwrap()
                .handle(server_handle)
                .serve(router.into_make_service())
                .await
                .unwrap();
        });
        Self {
            root,
            host,
            url: format!("https://localhost:{}", address.port()),
            handle,
            task,
        }
    }
    fn profile(&self) -> Remote {
        Remote {
            url: self.url.clone(),
            cert: self.host.cert.clone(),
            fingerprint: self.host.fingerprint.clone(),
            secret: String::new(),
            device: String::new(),
        }
    }
    async fn pair(&self) -> Remote {
        let mut profile = self.profile();
        let response = client_api::client(&profile)
            .unwrap()
            .post(format!("{}/pair", self.url))
            .json(&serde_json::json!({"code": self.host.code, "name": "laptop"}))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());
        let credential: Credential = response.json().await.unwrap();
        profile.secret = credential.secret;
        profile.device = credential.device;
        profile
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.handle.shutdown();
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Concurrent MCP initialization reserves capacity before creating transports;
/// a successful deletion releases the same slot for the next session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_transport_capacity_is_reserved_before_initialization() {
    let mut config = search::core::Config::default();
    config.research.session_entries = 1;
    let fixture = Fixture::with_config(std::time::Duration::from_secs(60), config).await;
    let profile = fixture.pair().await;
    let client = client_api::client(&profile).unwrap();
    let initialize = || {
        client.post(format!("{}/mcp", fixture.url))
        .bearer_auth(&profile.secret).header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"capacity","version":"1"}
        }}))
    };
    let (first, second) = tokio::join!(initialize().send(), initialize().send());
    let first = first.unwrap();
    let second = second.unwrap();
    let (accepted, refused) = if first.status().is_success() {
        (first, second)
    } else {
        (second, first)
    };
    assert!(accepted.status().is_success());
    assert_eq!(refused.status(), 503);
    let session = accepted
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let deleted = client
        .delete(format!("{}/mcp", fixture.url))
        .bearer_auth(&profile.secret)
        .header("mcp-session-id", &session)
        .header("Accept", "application/json, text/event-stream")
        .send()
        .await
        .unwrap();
    assert!(deleted.status().is_success());
    assert!(initialize().send().await.unwrap().status().is_success());
}

/// Paired responses honor local bounds without retrying or switching targets.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_response_limit_is_configurable_and_fails_without_fallback() {
    let fixture = Fixture::new().await;
    let profile = fixture.pair().await;
    let tiny = search::core::config::RemoteSettings {
        max_response_bytes: 1,
        ..Default::default()
    };
    let client = Client::remote_with_settings(profile.clone(), &tiny).unwrap();
    assert!(client.engines().await.unwrap_err().contains("too large"));
    let client = Client::remote(profile).unwrap();
    assert_eq!(client.engines().await.unwrap(), vec!["fixture"]);
}

/// Pairing pin checks precede code transmission and wrong TLS identities fail handshakes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_trust_reuse_and_revoke_over_https() {
    let fixture = Fixture::new().await;
    let mut wrong = fixture.profile();
    wrong.fingerprint = "00".repeat(32);
    assert!(client_api::client(&wrong).is_err());
    let other = Fixture::new().await;
    let mut wrong_host = other.profile();
    wrong_host.url = fixture.url.clone();
    assert!(client_api::client(&wrong_host)
        .unwrap()
        .post(format!("{}/pair", fixture.url))
        .json(&serde_json::json!({"code": fixture.host.code, "name": "wrong"}))
        .send()
        .await
        .is_err());
    // A failed pin/handshake has not consumed the host code.
    let profile = fixture.pair().await;
    let client = client_api::client(&profile).unwrap();
    assert_eq!(
        client
            .post(format!("{}/pair", fixture.url))
            .json(&serde_json::json!({"code": fixture.host.code, "name": "reuse"}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let execution = Client::remote(profile.clone()).unwrap();
    assert!(execution.engines().await.is_ok());
    let mcp = client.post(format!("{}/mcp", fixture.url)).bearer_auth(&profile.secret)
        .header("Accept", "application/json, text/event-stream")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"paired-http","version":"1"}}})).send().await.unwrap();
    assert!(mcp.status().is_success());
    let host_settings = fixture.root.join("host-settings.json");
    std::fs::write(&host_settings, serde_json::json!({}).to_string()).unwrap();
    let binary = env!("CARGO_BIN_EXE_search");
    let devices = local_command(binary, &fixture.root)
        .args(["devices", "-config", host_settings.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(devices.status.success());
    assert!(String::from_utf8_lossy(&devices.stdout).contains(&profile.device));
    assert!(!String::from_utf8_lossy(&devices.stdout).contains(&profile.secret));
    assert!(local_command(binary, &fixture.root)
        .args([
            "revoke",
            &profile.device,
            "-config",
            host_settings.to_str().unwrap()
        ])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(
        client
            .post(format!("{}/mcp", fixture.url))
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let error = execution.engines().await.unwrap_err();
    assert!(error.contains("401") && error.contains("no local fallback"));
}

/// CLI and the actual stdio MCP process read the same selection and remote engine results.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_and_stdio_route_all_operations_and_fail_explicitly() {
    use std::io::{BufRead, BufReader, Write};
    let fixture = Fixture::new().await;
    let client_root = fixture.root.join("client");
    std::fs::create_dir_all(&client_root).unwrap();
    #[cfg(windows)]
    windows::secure(&client_root);
    let config_path = client_root.join("settings.json");
    std::fs::write(
        &config_path,
        serde_json::json!({"engines":{"enabled":false}}).to_string(),
    )
    .unwrap();
    let config = Some(config_path.to_string_lossy().into_owned());
    let path = auth::dir(&client_root).unwrap();
    let binary = env!("CARGO_BIN_EXE_search");
    let certificate = fixture.host.path.join("host.pem");
    let mut pairing = local_command(binary, &client_root)
        .args([
            "remote",
            "pair",
            "home",
            &fixture.url,
            certificate.to_str().unwrap(),
            &fixture.host.fingerprint,
            "-config",
            config.as_deref().unwrap(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    writeln!(pairing.stdin.take().unwrap(), "{}", fixture.host.code).unwrap();
    let paired = pairing.wait_with_output().unwrap();
    assert!(
        paired.status.success(),
        "{}",
        String::from_utf8_lossy(&paired.stderr)
    );
    let remotes: Remotes = auth::read(&path.join("remotes.json")).unwrap();
    assert!(remotes.selected.is_none());
    assert!(!String::from_utf8_lossy(&paired.stdout).contains(&remotes.remotes["home"].secret));
    assert!(local_command(binary, &client_root)
        .args([
            "remote",
            "use",
            "home",
            "-config",
            config.as_deref().unwrap()
        ])
        .output()
        .unwrap()
        .status
        .success());
    let remotes: Remotes = auth::read(&path.join("remotes.json")).unwrap();
    let client =
        Client::remote(remotes.remotes[remotes.selected.as_ref().unwrap()].clone()).unwrap();
    let response = client
        .search(search::core::Query {
            text: "needle".into(),
            limit: 10,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(response.results.iter().any(|r| r.title == "Remote needle"));
    let urls = ["http://127.0.0.1/first", "http://127.0.0.1/second"].map(String::from);
    let pages = client.fetch(&urls).await.unwrap();
    assert_eq!(
        pages.iter().map(|p| &p.url).collect::<Vec<_>>(),
        [&urls[0], &urls[1]]
    );
    assert!(pages.iter().all(|page| page.error.is_some()));
    let output = local_command(binary, &client_root)
        .args(["needle", "-json", "-config", config.as_deref().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Remote needle"));
    // Speak JSON-RPC to the real harness-facing stdio transport.
    let mut child = local_command(binary, &client_root)
        .args(["stdio", "-config", config.as_deref().unwrap()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"offline-test","version":"1"}}})).unwrap();
    let mut line = String::new();
    stdout.read_line(&mut line).unwrap();
    assert!(line.contains("protocolVersion"));
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"web_search","arguments":{"queries":["needle"]}}})).unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert!(line.contains("Remote needle"), "{line}");
    auth::update(
        &fixture.host.path.join("devices.json"),
        |devices: &mut Devices| {
            devices.clear();
            Ok(())
        },
    )
    .unwrap();
    writeln!(stdin, "{}", serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"web_fetch","arguments":{"urls":["https://example.invalid"]}}})).unwrap();
    line.clear();
    stdout.read_line(&mut line).unwrap();
    assert!(line.contains("no local fallback"), "{line}");
    drop(stdin);
    child.kill().unwrap();
    child.wait().unwrap();
    let output = local_command(binary, &client_root)
        .args(["needle", "-json", "-config", config.as_deref().unwrap()])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no local fallback"));
    // Explicitly selecting local execution works without any authentication.
    assert!(local_command(binary, &client_root)
        .args(["remote", "off", "-config", config.as_deref().unwrap()])
        .output()
        .unwrap()
        .status
        .success());
    let local = local_command(binary, &client_root)
        .args(["needle", "-json", "-config", config.as_deref().unwrap()])
        .output()
        .unwrap();
    assert!(local.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&local.stdout).unwrap()["results"],
        serde_json::json!([])
    );
}

/// Define trusted host code that returns a unique result without network access.
fn fixture_engine(root: &std::path::Path) -> serde_json::Value {
    std::fs::write(
        root.join("program.py"),
        r#"import json, sys
request = json.load(sys.stdin)
assert request['version'] == 1
if request['query'] == 'bounds':
    json.dump({'results':[{'url':'https://example.invalid/'+str(i),'title':str(i),'snippet':'bounded'} for i in range(request['limit'])]}, sys.stdout)
    sys.exit(0)
assert request['query'] == 'needle'
json.dump({'results':[{'url':'https://example.invalid/needle','title':'Remote needle','snippet':'needle from the remote engine'}]}, sys.stdout)
"#,
    )
    .unwrap();
    serde_json::json!({
        "type": "command",
        "command": if cfg!(windows) { "python.exe" } else { "python3" },
        "version": 1,
        "args": ["program.py"],
        "cwd": root
    })
}

/// Keep each CLI subprocess's home isolated without mutating the test runner environment.
fn local_command(binary: &str, home: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(binary);
    command
        .env("SEARCH_HOME", home)
        .env_remove("CONFIG")
        .env_remove("ADDRESS");
    command
}

/// Even a redirect to a trusted host must never relay a device credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credential_clients_refuse_redirects() {
    let fixture = Fixture::new().await;
    let mut profile = fixture.pair().await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    profile.url = format!(
        "https://localhost:{}",
        listener.local_addr().unwrap().port()
    );
    let target = format!("{}/v1/status", fixture.url);
    let router = axum::Router::new().route(
        "/v1/status",
        axum::routing::get(move || async move { axum::response::Redirect::temporary(&target) }),
    );
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        fixture.host.cert.as_bytes().to_vec(),
        fixture.host.key.as_bytes().to_vec(),
    )
    .await
    .unwrap();
    let handle = axum_server::Handle::new();
    let server_handle = handle.clone();
    let task = tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, tls)
            .unwrap()
            .handle(server_handle)
            .serve(router.into_make_service())
            .await
            .unwrap();
    });
    let error = Client::remote(profile)
        .unwrap()
        .engines()
        .await
        .unwrap_err();
    assert!(error.contains("307"), "{error}");
    handle.shutdown();
    task.await.unwrap();
}

/// An expired code is refused through the HTTPS endpoint, without enrolling a device.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_pairing_code_is_refused_over_https() {
    let fixture = Fixture::with_window(std::time::Duration::ZERO).await;
    let response = client_api::client(&fixture.profile())
        .unwrap()
        .post(format!("{}/pair", fixture.url))
        .json(&serde_json::json!({"code": fixture.host.code, "name": "expired"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    assert!(
        auth::read::<Devices>(&fixture.host.path.join("devices.json"))
            .unwrap()
            .is_empty()
    );
}

/// A local command renews a live HTTPS host, retaining existing devices; each code admits one device.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_code_renews_a_running_host_with_persisted_attempts() {
    let fixture = Fixture::new().await;
    let existing = fixture.pair().await;
    let settings = fixture.root.join("host-settings.json");
    std::fs::write(&settings, serde_json::json!({}).to_string()).unwrap();
    let renew = || {
        let output = local_command(env!("CARGO_BIN_EXE_search"), &fixture.root)
            .args(["pair-code", "-config", settings.to_str().unwrap()])
            .output()
            .unwrap();
        assert!(output.status.success());
        let code = String::from_utf8(output.stdout).unwrap().trim().to_owned();
        assert_eq!(code.len(), 64);
        let stored = std::fs::read_to_string(fixture.host.path.join("pairing.json")).unwrap();
        assert!(!stored.contains(&code));
        code
    };
    let client = client_api::client(&fixture.profile()).unwrap();
    let pair = |code: String| {
        client
            .post(format!("{}/pair", fixture.url))
            .json(&serde_json::json!({"code": code, "name": "another"}))
            .send()
    };
    let exhausted = renew();
    // The old startup code cannot be used, even though the server still holds its initial display.
    assert_eq!(pair(fixture.host.code.clone()).await.unwrap().status(), 403);
    for _ in 0..19 {
        assert_eq!(pair("wrong".into()).await.unwrap().status(), 403);
    }
    let state: serde_json::Value = auth::read(&fixture.host.path.join("pairing.json")).unwrap();
    assert_eq!(state["attempts"], 20);
    assert_eq!(pair(exhausted).await.unwrap().status(), 403);
    let superseded = renew();
    let code = renew();
    assert_eq!(pair(superseded).await.unwrap().status(), 403);
    let (first, second) = tokio::join!(pair(code.clone()), pair(code.clone()));
    let statuses = [first.unwrap().status(), second.unwrap().status()];
    assert_eq!(statuses.iter().filter(|s| s.is_success()).count(), 1);
    assert_eq!(statuses.iter().filter(|s| **s == 403).count(), 1);
    assert_eq!(pair(code).await.unwrap().status(), 403);
    assert!(Client::remote(existing).unwrap().engines().await.is_ok());
}

/// The host enforces the shared input bounds itself rather than trusting paired clients.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn host_refuses_out_of_bounds_input() {
    let fixture = Fixture::new().await;
    let profile = fixture.pair().await;
    let client = client_api::client(&profile).unwrap();
    let search = client
        .get(format!("{}/v1/search", fixture.url))
        .query(&[("q", "x".repeat(513))])
        .bearer_auth(&profile.secret)
        .send()
        .await
        .unwrap();
    assert_eq!(search.status(), 400);
    let fetch = client
        .post(format!("{}/v1/fetch", fixture.url))
        .json(&serde_json::json!({"urls": vec!["https://example.com"; 11]}))
        .bearer_auth(&profile.secret)
        .send()
        .await
        .unwrap();
    assert_eq!(fetch.status(), 400);
}

/// The host exposes research on the same paired API, with one bounded packet
/// per request and the shared input bounds enforced server-side.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_research_returns_one_bounded_packet() {
    let fixture = Fixture::new().await;
    let profile = fixture.pair().await;
    let client = client_api::client(&profile).unwrap();
    let response = client
        .post(format!("{}/v1/research", fixture.url))
        .bearer_auth(&profile.secret)
        .json(&serde_json::json!({"queries": ["needle"], "options": {"sources": 2}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let packet: serde_json::Value = response.json().await.unwrap();
    assert_eq!(packet["queries"][0], "needle");
    assert_eq!(packet["coverage"]["candidates"], 1);
    let bad = client
        .post(format!("{}/v1/research", fixture.url))
        .bearer_auth(&profile.secret)
        .json(&serde_json::json!({"queries": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), 400);
}

/// A duplicate host cannot supersede the running host's admission state before bind succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_serve_preserves_pairing_state() {
    let fixture = Fixture::new().await;
    let path = fixture.host.path.join("pairing.json");
    let before = std::fs::read(&path).unwrap();
    let settings = fixture.root.join("duplicate-settings.json");
    std::fs::write(&settings, "{}").unwrap();
    let address = fixture.url.replace("https://localhost:", "127.0.0.1:");
    let output = local_command(env!("CARGO_BIN_EXE_search"), &fixture.root)
        .args([
            "serve",
            "-config",
            settings.to_str().unwrap(),
            "-address",
            &address,
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bind:"));
    assert_eq!(std::fs::read(path).unwrap(), before);
    fixture.pair().await;
}

/// `GET /v1/search` resolves omitted/zero limits to the host default, while an
/// explicit limit is capped at the shared 50-result ceiling before the host
/// maximum applies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_search_defaults_and_zero_are_bounded() {
    for configured in [17, 80] {
        let fixture = Fixture::with_settings(std::time::Duration::from_secs(900), configured).await;
        let profile = fixture.pair().await;
        let client = client_api::client(&profile).unwrap();
        for suffix in ["", "&limit=0", "&limit=3", "&limit=51"] {
            let value: serde_json::Value = client
                .get(format!("{}/v1/search?q=bounds{suffix}", fixture.url))
                .bearer_auth(&profile.secret)
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let expected = match suffix {
                "&limit=3" => 3,
                "&limit=51" => configured.min(50),
                _ => configured,
            };
            assert_eq!(
                value["results"].as_array().unwrap().len(),
                expected,
                "configured {configured}, suffix {suffix}"
            );
        }
    }
}

/// Missing trust is local only beneath safe ancestors, and inspecting it creates no storage.
#[cfg(unix)]
#[test]
fn missing_trust_selection_checks_replacement_boundary() {
    use std::os::unix::fs::PermissionsExt;
    let root = std::env::temp_dir()
        .canonicalize()
        .unwrap()
        .join(format!("search-selection-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let settings = root.join("shared-settings.json");
    std::fs::write(&settings, r#"{"engines":{"enabled":false}}"#).unwrap();
    std::fs::set_permissions(&settings, std::fs::Permissions::from_mode(0o444)).unwrap();
    let home = root.join("home");
    let run = || {
        local_command(env!("CARGO_BIN_EXE_search"), &home)
            .args(["needle", "-json", "-config", settings.to_str().unwrap()])
            .output()
            .unwrap()
    };
    assert!(run().status.success());
    assert!(!home.exists());
    std::fs::create_dir(&home).unwrap();
    for mode in [0o775, 0o777] {
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(mode)).unwrap();
        let output = run();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("protected from foreign writers"));
        assert!(!home.join("trust").exists());
    }
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(run().status.success());
    assert!(!home.join("trust").exists());
    std::os::unix::fs::symlink(home.join("missing"), home.join("trust")).unwrap();
    assert!(!run().status.success());
    std::fs::remove_file(home.join("trust")).unwrap();
    std::fs::remove_dir(&home).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(!run().status.success());
    assert!(!home.exists());
    std::fs::remove_dir_all(root).unwrap();
}

/// Own public-content fixtures on loopback; only explicit test settings allow their addresses.
struct PagesFixture {
    url: String,
    blocked_hits: Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}
impl PagesFixture {
    /// Serve two distinct articles, a forbidden link, and deterministic robots policy.
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let article = format!("<html><head><title>Research root</title></head><body><article><h1>Research root</h1><p>{}</p><p><a href='/docs/child'>Child article</a> <a href='/docs/blocked'>Forbidden</a><img src='/image.png'></p></article></body></html>", "Evidence about rust ownership from a useful source. ".repeat(40));
        let child = format!("<html><head><title>Research child</title></head><body><article><h1>Research child</h1><p>{}</p></article></body></html>", "Independent evidence about rust ownership and borrowing. ".repeat(40));
        let blocked_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let blocked_counter = blocked_hits.clone();
        let app = axum::Router::new()
            .route(
                "/robots.txt",
                axum::routing::get(|| async { "User-agent: *\nDisallow: /docs/blocked\n" }),
            )
            .route(
                "/docs/",
                axum::routing::get(move || {
                    let article = article.clone();
                    async move { axum::response::Html(article) }
                }),
            )
            .route(
                "/docs/child",
                axum::routing::get(move || {
                    let child = child.clone();
                    async move { axum::response::Html(child) }
                }),
            )
            .route(
                "/docs/blocked",
                axum::routing::get(move || {
                    let counter = blocked_counter.clone();
                    async move {
                        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        "forbidden article"
                    }
                }),
            );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url,
            task,
            blocked_hits,
        }
    }
    /// Configure a version-two engine that echoes every requested filter and finds both articles.
    fn settings(&self) -> serde_json::Value {
        let root = format!("{}/docs/", self.url);
        let child = format!("{}/docs/child", self.url);
        let filter_json = serde_json::json!({"freshness":"week","start_date":"2024-02-29","end_date":"2026-10-13","include_domains":["example.com"],"exclude_domains":["other.com"],"language":"en","region":"GB","category":"science","safe":false,"exact_phrase":"rust ownership"}).to_string();
        let program = format!("import json,sys; r=json.load(sys.stdin); assert r['query']!='filtered research' or r['filters']==json.loads({filter_json:?}); assert r['version']==2; json.dump({{'results':[{{'url':{root:?},'title':json.dumps(r['filters'],sort_keys=True),'snippet':'rust ownership'}},{{'url':{child:?},'title':'Independent evidence','snippet':'rust ownership'}}]}},sys.stdout)");
        serde_json::json!({
            "engines":{"use":["machine"],"config":{"machine":{"type":"command","version":2,"command":if cfg!(windows) {"python.exe"} else {"python3"},"args":["-c",program]}}},
            "fetch":{"allow_private_networks":true},
            "research":{"read_characters":48,"sources":2,"source_characters":80,"total_characters":140,"max_per_host":2},
            "crawl":{"max_pages":3,"max_depth":2,"delay":1}
        })
    }
    /// Decode the same settings used by CLI subprocesses and paired hosts.
    fn config(&self) -> search::core::Config {
        serde_json::from_value(self.settings()).unwrap()
    }
}
impl Drop for PagesFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Local and paired reads use host windows, expose continuation, and opt in to full text and links.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_reads_resolve_host_defaults_and_preserve_explicit_full_and_links() {
    use search::client::ReadRequest;
    let pages = PagesFixture::new().await;
    let config = pages.config();
    let local = Client::Local(Arc::new(
        search::core::Search::open(config.clone()).unwrap(),
    ));
    let fixture = Fixture::with_config(std::time::Duration::from_secs(900), config).await;
    let remote = Client::remote(fixture.pair().await).unwrap();
    for client in [local, remote] {
        let request = ReadRequest {
            urls: vec![format!("{}/docs/", pages.url)],
            ..Default::default()
        };
        let first = client.read(request.clone()).await.unwrap().remove(0);
        assert_eq!(first.page.text.chars().count(), 48);
        assert_eq!(first.next_offset, Some(48));
        assert!(first.page.links.is_empty());
        let custom = client
            .read(ReadRequest {
                max_characters: Some(100),
                ..request.clone()
            })
            .await
            .unwrap()
            .remove(0);
        assert_eq!(custom.page.text.chars().count(), 100);
        assert_eq!(custom.next_offset, Some(100));
        let second = client
            .read(ReadRequest {
                offset: 48,
                ..request.clone()
            })
            .await
            .unwrap()
            .remove(0);
        let full = client
            .read(ReadRequest {
                full: true,
                links: true,
                ..request.clone()
            })
            .await
            .unwrap()
            .remove(0);
        assert!(full.page.text.chars().count() > 1000);
        assert!(full.next_offset.is_none());
        assert_eq!(
            first.page.text + &second.page.text,
            full.page.text.chars().take(96).collect::<String>()
        );
        assert!(full
            .page
            .links
            .contains(&format!("{}/docs/child", pages.url)));
        assert!(full
            .page
            .links
            .iter()
            .all(|link| !link.contains("image.png")));
        let focused = client
            .read(ReadRequest {
                query: Some("ownership".into()),
                ..request.clone()
            })
            .await
            .unwrap()
            .remove(0);
        assert!(
            focused
                .passages
                .unwrap()
                .iter()
                .map(|p| p.text.chars().count())
                .sum::<usize>()
                <= 48
        );
        assert!(client
            .read(ReadRequest {
                full: true,
                max_characters: Some(4),
                ..request
            })
            .await
            .is_err());
    }
}

/// POST search and research carry identical filters; host research defaults stay authoritative.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn machine_filters_and_successful_research_match_locally_and_remotely() {
    use search::client::ResearchRequest;
    let pages = PagesFixture::new().await;
    let config = pages.config();
    let local = Client::Local(Arc::new(
        search::core::Search::open(config.clone()).unwrap(),
    ));
    let fixture = Fixture::with_config(std::time::Duration::from_secs(900), config).await;
    let profile = fixture.pair().await;
    let remote = Client::remote(profile.clone()).unwrap();
    let filters: search::core::QueryFilters = serde_json::from_value(serde_json::json!({"freshness":"week","start_date":"2024-02-29","end_date":"2026-10-13","include_domains":["example.com"],"exclude_domains":["other.com"],"language":"en","region":"GB","category":"science","safe":false,"exact_phrase":"rust ownership"})).unwrap();
    for client in [local, remote] {
        let answer = client
            .search(search::core::Query {
                text: "rust ownership".into(),
                filters: filters.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
        let echoed: serde_json::Value = serde_json::from_str(&answer.results[0].title).unwrap();
        assert_eq!(echoed, serde_json::to_value(&filters).unwrap());
        let packet = client
            .research(ResearchRequest {
                queries: vec!["filtered research".into()],
                options: search::core::ResearchOptions {
                    filters: filters.clone(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(packet.coverage.read, 2);
        assert!(packet.characters > 0 && packet.characters <= 140);
        assert!(packet.sources.iter().all(|source| source.error.is_none()
            && source
                .content_hash
                .as_ref()
                .is_some_and(|hash| hash.len() == 64)));
        assert!(packet.synthesis.is_none());
        let session_request = ResearchRequest {
            queries: vec!["rust ownership".into()],
            options: search::core::ResearchOptions {
                session: Some("disabled-processing-session".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(client
            .research(ResearchRequest {
                synthesize: true,
                ..session_request.clone()
            })
            .await
            .is_err());
        assert_eq!(
            client
                .research(session_request)
                .await
                .unwrap()
                .coverage
                .read,
            2
        );
        assert!(client
            .search(search::core::Query {
                text: "q".into(),
                filters: search::core::QueryFilters {
                    start_date: Some("2025-02-29".into()),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await
            .is_err());
        assert!(client
            .research(ResearchRequest {
                queries: vec!["rust".into()],
                synthesize: true,
                ..Default::default()
            })
            .await
            .is_err());
        assert!(client
            .research(ResearchRequest {
                queries: vec!["rust".into()],
                rerank: true,
                ..Default::default()
            })
            .await
            .is_err());
    }
    let response = client_api::client(&profile)
        .unwrap()
        .get(format!("{}/v1/search", fixture.url))
        .bearer_auth(&profile.secret)
        .query(&[("q", "rust ownership")])
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
}

/// The same bounded crawler executes behind both routes and never follows robots-forbidden links.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn useful_crawl_and_map_match_local_and_paired_hosts() {
    let pages = PagesFixture::new().await;
    let config = pages.config();
    let local = Client::Local(Arc::new(
        search::core::Search::open(config.clone()).unwrap(),
    ));
    let fixture = Fixture::with_config(std::time::Duration::from_secs(900), config).await;
    let remote = Client::remote(fixture.pair().await).unwrap();
    for client in [local, remote] {
        let request = search::core::CrawlRequest {
            url: format!("{}/docs/", pages.url),
            options: search::core::CrawlOptions {
                path_prefix: Some("/docs/".into()),
                ..Default::default()
            },
        };
        let crawl = client.crawl(request.clone()).await.unwrap();
        assert_eq!(crawl.pages.len(), 2, "{crawl:?}");
        assert_eq!(
            pages
                .blocked_hits
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(crawl
            .pages
            .iter()
            .all(|page| !page.text.is_empty() && page.error.is_none()));
        assert!(crawl.urls.contains(&format!("{}/docs/child", pages.url)));
        assert!(!crawl.urls.contains(&format!("{}/docs/blocked", pages.url)));
        let map = client.map(request).await.unwrap();
        assert!(map.pages.is_empty());
        assert_eq!(map.urls, crawl.urls);
        assert!(!map.timed_out);
        assert!(client
            .map(search::core::CrawlRequest {
                url: "file:///tmp/x".into(),
                options: Default::default()
            })
            .await
            .is_err());
    }
}

/// Collection text has both per-page and total windows on local/remote machine
/// surfaces, with recoverable offsets and explicit full-page opt-in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn crawl_text_windows_do_not_hide_omitted_page_content() {
    let pages = PagesFixture::new().await;
    let config = pages.config();
    let local = Client::Local(Arc::new(
        search::core::Search::open(config.clone()).unwrap(),
    ));
    let fixture = Fixture::with_config(std::time::Duration::from_secs(900), config).await;
    let remote = Client::remote(fixture.pair().await).unwrap();
    for client in [local, remote] {
        let mut request = search::core::CrawlRequest {
            url: format!("{}/docs/", pages.url),
            options: Default::default(),
        };
        request.options.max_characters = 100;
        request.options.total_characters = 1;
        let report = client.crawl(request.clone()).await.unwrap();
        assert_eq!(report.pages.len(), 2);
        assert_eq!(
            report
                .pages
                .iter()
                .map(|page| page.text.chars().count())
                .sum::<usize>(),
            1
        );
        assert_eq!(report.windows[0].returned_characters, 1);
        assert_eq!(report.windows[0].next_offset, Some(1));
        assert_eq!(report.windows[1].returned_characters, 0);
        assert_eq!(report.windows[1].next_offset, Some(0));
        assert!(report
            .windows
            .iter()
            .all(|window| window.available_characters > 100));
        assert!(report.pages.iter().all(|page| page.truncated.is_none()));
        request.options.max_characters = 0;
        request.options.total_characters = 0;
        request.options.full = true;
        let full = client.crawl(request).await.unwrap();
        assert!(full.windows.is_empty());
        assert!(full
            .pages
            .iter()
            .all(|page| page.text.chars().count() > 100));
    }
}

/// Authenticated device identity namespaces user session tokens and owns MCP transport sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn research_sessions_and_mcp_sessions_do_not_leak_between_devices() {
    use search::client::ResearchRequest;
    let pages = PagesFixture::new().await;
    let fixture = Fixture::with_config(std::time::Duration::from_secs(900), pages.config()).await;
    let first = fixture.pair().await;
    let code = auth::pair_code(&fixture.host.path, std::time::Duration::from_secs(900)).unwrap();
    let credential = fixture.host.pair(&code, "second-device").unwrap();
    let second = Remote {
        secret: credential.secret,
        device: credential.device,
        ..fixture.profile()
    };
    let request = ResearchRequest {
        queries: vec!["rust ownership".into()],
        options: search::core::ResearchOptions {
            session: Some("shared-user-token".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let first_client = Client::remote(first.clone()).unwrap();
    assert_eq!(
        first_client
            .research(request.clone())
            .await
            .unwrap()
            .coverage
            .read,
        2
    );
    let repeated = first_client.research(request.clone()).await.unwrap();
    assert_eq!(repeated.coverage.read, 0);
    assert_eq!(repeated.coverage.previously_seen, 2);
    assert_eq!(
        Client::remote(second.clone())
            .unwrap()
            .research(request)
            .await
            .unwrap()
            .coverage
            .read,
        2
    );
    let http = client_api::client(&first).unwrap();
    let initialize = http.post(format!("{}/mcp", fixture.url)).bearer_auth(&first.secret).header("Accept","application/json, text/event-stream").json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"owner","version":"1"}}})).send().await.unwrap();
    assert!(initialize.status().is_success());
    let session = initialize
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap();
    let stolen = http
        .post(format!("{}/mcp", fixture.url))
        .bearer_auth(&second.secret)
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", session)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert_eq!(stolen.status(), 403);
    let owned = http
        .post(format!("{}/mcp", fixture.url))
        .bearer_auth(&first.secret)
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", session)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .send()
        .await
        .unwrap();
    assert!(owned.status().is_success());
}

/// CLI flags reach useful local execution, including read alias, filters, research and collection mapping.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_machine_flags_execute_successfully_against_live_fixtures() {
    let pages = PagesFixture::new().await;
    let root = std::env::temp_dir().join(format!("search-cli-machine-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let settings = root.join("settings.json");
    std::fs::write(&settings, pages.settings().to_string()).unwrap();
    let binary = env!("CARGO_BIN_EXE_search");
    let url = format!("{}/docs/", pages.url);
    let execute = |args: &[&str]| {
        let output = local_command(binary, &root)
            .args(args)
            .args(["-config", settings.to_str().unwrap(), "-json"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let read = execute(&["read", &url]);
    assert_eq!(read[0]["text"].as_str().unwrap().chars().count(), 48);
    assert_eq!(read[0]["next_offset"], 48);
    let full = execute(&["fetch", &url, "-full", "-links"]);
    assert!(full[0]["text"].as_str().unwrap().chars().count() > 1000);
    assert!(full[0]["links"]
        .as_array()
        .unwrap()
        .contains(&serde_json::json!(format!("{}/docs/child", pages.url))));
    let found = execute(&[
        "rust ownership",
        "-language",
        "en",
        "-safe",
        "false",
        "-start-date",
        "2024-02-29",
        "-exact-phrase",
        "rust ownership",
    ]);
    let forwarded: serde_json::Value =
        serde_json::from_str(found["results"][0]["title"].as_str().unwrap()).unwrap();
    assert_eq!(forwarded["language"], "en");
    assert_eq!(forwarded["safe"], false);
    assert_eq!(forwarded["start_date"], "2024-02-29");
    let packet = execute(&[
        "research",
        "rust ownership",
        "-sources",
        "1",
        "-max-attempts",
        "2",
        "-max-per-host",
        "1",
        "-total-chars",
        "40",
        "-timeout",
        "10000",
        "-session",
        "cli-evidence",
    ]);
    assert_eq!(packet["coverage"]["read"], 1);
    assert!(packet["characters"].as_u64().unwrap() <= 40);
    let mapped = execute(&[
        "map",
        &url,
        "-max-pages",
        "2",
        "-max-depth",
        "1",
        "-max-bytes",
        "100000",
        "-timeout",
        "10000",
        "-path-prefix",
        "/docs/",
    ]);
    assert_eq!(mapped["urls"].as_array().unwrap().len(), 2);
    assert!(mapped["pages"].as_array().unwrap().is_empty());
    let disabled = local_command(binary, &root)
        .args([
            "research",
            "rust",
            "-synthesize",
            "-config",
            settings.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!disabled.status.success());
    assert!(String::from_utf8_lossy(&disabled.stderr).contains("not configured"));
    std::fs::remove_dir_all(root).unwrap();
}

/// Decode the tool response from either MCP JSON or a completed SSE message.
async fn call_mcp(
    http: &reqwest::Client,
    origin: &str,
    profile: &Remote,
    session: &str,
    name: &str,
    arguments: serde_json::Value,
) -> serde_json::Value {
    let response = http.post(format!("{origin}/mcp")).bearer_auth(&profile.secret)
        .header("Accept", "application/json, text/event-stream").header("mcp-session-id", session)
        .json(&serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
        .send().await.unwrap();
    assert!(response.status().is_success());
    let body = response.text().await.unwrap();
    let data = body
        .lines()
        .filter_map(|line| line.strip_prefix("data:").map(str::trim))
        .find(|data| !data.is_empty())
        .unwrap_or(&body);
    let value: serde_json::Value = serde_json::from_str(data).unwrap();
    assert!(value.get("error").is_none(), "{value}");
    value["result"]["structuredContent"].clone()
}

/// MCP tools execute useful reads/research/inventory with the same device scope as paired REST.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_machine_tools_share_read_defaults_research_scope_and_crawl_policy() {
    let pages = PagesFixture::new().await;
    let fixture = Fixture::with_config(std::time::Duration::from_secs(900), pages.config()).await;
    let profile = fixture.pair().await;
    let http = client_api::client(&profile).unwrap();
    let initialized = http.post(format!("{}/mcp",fixture.url)).bearer_auth(&profile.secret)
        .header("Accept","application/json, text/event-stream")
        .json(&serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"machine-test","version":"1"}}})).send().await.unwrap();
    let session = initialized
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let notified = http
        .post(format!("{}/mcp", fixture.url))
        .bearer_auth(&profile.secret)
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session)
        .json(&serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await
        .unwrap();
    assert!(notified.status().is_success());
    let url = format!("{}/docs/", pages.url);
    let read = call_mcp(
        &http,
        &fixture.url,
        &profile,
        &session,
        "web_fetch",
        serde_json::json!({"urls":[url]}),
    )
    .await;
    assert_eq!(
        read["pages"][0]["text"].as_str().unwrap().chars().count(),
        48
    );
    assert_eq!(read["pages"][0]["next_offset"], 48);
    let full = call_mcp(
        &http,
        &fixture.url,
        &profile,
        &session,
        "web_fetch",
        serde_json::json!({"urls":[url],"full":true,"links":true}),
    )
    .await;
    assert!(full["pages"][0]["text"].as_str().unwrap().chars().count() > 1000);
    assert!(!full["pages"][0]["links"].as_array().unwrap().is_empty());
    let search = call_mcp(
        &http,
        &fixture.url,
        &profile,
        &session,
        "web_search",
        serde_json::json!({"queries":["rust ownership"],"filters":{"language":"en","safe":false}}),
    )
    .await;
    let forwarded: serde_json::Value = serde_json::from_str(
        search["queries"][0]["results"][0]["title"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(forwarded["language"], "en");
    assert_eq!(forwarded["safe"], false);
    let evidence = call_mcp(&http,&fixture.url,&profile,&session,"web_research",serde_json::json!({"queries":["rust ownership"],"options":{"session":"mcp-rest-session","sources":2,"total_characters":120}})).await;
    assert_eq!(evidence["coverage"]["read"], 2);
    assert!(evidence["characters"].as_u64().unwrap() <= 120);
    let repeated = Client::remote(profile.clone())
        .unwrap()
        .research(search::client::ResearchRequest {
            queries: vec!["rust ownership".into()],
            options: search::core::ResearchOptions {
                session: Some("mcp-rest-session".into()),
                ..Default::default()
            },
            ..Default::default()
        })
        .await
        .unwrap();
    assert_eq!(repeated.coverage.read, 0);
    assert_eq!(repeated.coverage.previously_seen, 2);
    let crawl = call_mcp(
        &http,
        &fixture.url,
        &profile,
        &session,
        "web_crawl",
        serde_json::json!({"url":url,"path_prefix":"/docs/"}),
    )
    .await;
    assert_eq!(crawl["pages"].as_array().unwrap().len(), 2);
    let map = call_mcp(
        &http,
        &fixture.url,
        &profile,
        &session,
        "web_map",
        serde_json::json!({"url":url,"path_prefix":"/docs/"}),
    )
    .await;
    assert!(map["pages"].as_array().unwrap().is_empty());
    assert_eq!(map["urls"], crawl["urls"]);
    assert_eq!(
        pages
            .blocked_hits
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
}

/// Configured processing stays opt-in and produces identical audited local/host results.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn research_processing_is_opt_in_and_runs_on_the_execution_host() {
    let pages = PagesFixture::new().await;
    let mut settings = pages.settings();
    let command = |program: &str| serde_json::json!({"command":if cfg!(windows) {"python.exe"} else {"python3"},"args":["-c",program]});
    settings["processing"] = serde_json::json!({
        "reranker":command("import json,sys; r=json.load(sys.stdin); assert r['operation']=='rerank'; json.dump({'urls':[s['url'] for s in reversed(r['packet']['sources'])]},sys.stdout)"),
        "synthesizer":command("import json,sys; r=json.load(sys.stdin); assert r['operation']=='synthesize'; json.dump({'answer':'Grounded fixture answer','citations':[r['packet']['sources'][0]['url']]},sys.stdout)")
    });
    let config: search::core::Config = serde_json::from_value(settings).unwrap();
    let local = Client::Local(Arc::new(
        search::core::Search::open(config.clone()).unwrap(),
    ));
    let fixture = Fixture::with_config(std::time::Duration::from_secs(900), config).await;
    let remote = Client::remote(fixture.pair().await).unwrap();
    for client in [local, remote] {
        let request = search::client::ResearchRequest {
            queries: vec!["rust ownership".into()],
            ..Default::default()
        };
        let original = client.research(request.clone()).await.unwrap();
        assert!(original.synthesis.is_none());
        assert_eq!(original.sources[0].url, format!("{}/docs/", pages.url));
        let processed = client
            .research(search::client::ResearchRequest {
                synthesize: true,
                rerank: true,
                ..request
            })
            .await
            .unwrap();
        assert_eq!(
            processed.sources[0].url,
            format!("{}/docs/child", pages.url)
        );
        assert_eq!(processed.characters, original.characters);
        assert_eq!(processed.coverage.read, 2);
        assert!(processed.processing_errors.is_empty());
        let synthesis = processed.synthesis.unwrap();
        assert!(synthesis.contains("Grounded fixture answer"));
        assert!(synthesis.contains(&format!("{}/docs/child", pages.url)));
    }
}

/// Character-window continuation must not hide an independently truncated download.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_read_reports_both_window_continuation_and_byte_truncation() {
    let pages = PagesFixture::new().await;
    let root = std::env::temp_dir().join(format!("search-cli-cut-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let settings = root.join("settings.json");
    let mut config = pages.settings();
    config["fetch"]["max_response_bytes"] = serde_json::json!(1024);
    std::fs::write(&settings, config.to_string()).unwrap();
    let output = local_command(env!("CARGO_BIN_EXE_search"), &root)
        .args([
            "read",
            &format!("{}/docs/", pages.url),
            "-max-chars",
            "20",
            "-config",
            settings.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("continues; read on with -offset 20"),
        "{text}"
    );
    assert!(
        text.contains("truncated at fetch.max_response_bytes"),
        "{text}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

/// Query passage clipping exposes the offset and explains how to resume a plain read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_read_reports_clipped_passage_continuation() {
    let pages = PagesFixture::new().await;
    let root = std::env::temp_dir().join(format!("search-cli-passage-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let settings = root.join("settings.json");
    std::fs::write(&settings, pages.settings().to_string()).unwrap();
    let output = local_command(env!("CARGO_BIN_EXE_search"), &root)
        .args([
            "read",
            &format!("{}/docs/", pages.url),
            "-query",
            "ownership",
            "-max-chars",
            "20",
            "-config",
            settings.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout)
        .contains("passage continues; read on without -query using -offset"));
    std::fs::remove_dir_all(root).unwrap();
}

/// Human research output exposes incomplete evidence and engines that ignored a requested filter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_research_reports_incomplete_evidence_and_unsupported_filters() {
    let pages = PagesFixture::new().await;
    let root = std::env::temp_dir().join(format!("search-cli-coverage-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let settings = root.join("settings.json");
    let mut config = pages.settings();
    let url = format!("{}/docs/", pages.url);
    config["engines"]["config"]["machine"]["version"] = serde_json::json!(1);
    config["engines"]["config"]["machine"]["args"] = serde_json::json!([
        "-c", format!("import json,sys; json.load(sys.stdin); json.dump({{'results':[{{'url':{url:?},'title':'Source'}}]}},sys.stdout)")
    ]);
    std::fs::write(&settings, config.to_string()).unwrap();
    let output = local_command(env!("CARGO_BIN_EXE_search"), &root)
        .args([
            "research",
            "ownership",
            "-language",
            "en",
            "-chars",
            "20",
            "-total-chars",
            "20",
            "-config",
            settings.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported filters: language"), "{stderr}");
    assert!(
        stderr.contains("research: incomplete evidence packet"),
        "{stderr}"
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("search read URL -offset"));
    std::fs::remove_dir_all(root).unwrap();
}

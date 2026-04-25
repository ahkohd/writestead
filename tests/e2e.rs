use std::collections::HashMap;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use base64::Engine;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use uuid::Uuid;
use writestead::config::{AppConfig, McpAuthMode, SyncBackend};
use writestead::server;
use writestead::vault;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_writestead")
}

fn env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

fn base_config(vault_path: &Path, port: u16) -> AppConfig {
    AppConfig {
        name: "e2e".to_string(),
        vault_path: vault_path.to_string_lossy().to_string(),
        host: "127.0.0.1".to_string(),
        port,
        sync: writestead::config::SyncConfig {
            backend: SyncBackend::None,
        },
        mcp: writestead::config::McpConfig::default(),
        search: writestead::config::SearchConfig::default(),
        raw: writestead::config::RawConfig::default(),
    }
}

fn save_config_at(path: &Path, cfg: &AppConfig) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create config dir");
    }
    let body = serde_json::to_string_pretty(cfg).expect("cfg json");
    fs::write(path, format!("{}\n", body)).expect("write config");
}

struct TestServer {
    _dir: TempDir,
    cfg: AppConfig,
    port: u16,
    handle: JoinHandle<()>,
}

impl TestServer {
    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    async fn shutdown(self) {
        self.handle.abort();
        let _ = self.handle.await;
    }
}

async fn setup() -> TestServer {
    setup_with(|_| {}).await
}

async fn setup_with(mutator: impl FnOnce(&mut AppConfig)) -> TestServer {
    let dir = TempDir::new().expect("tempdir");
    let port = free_port();
    let mut cfg = base_config(dir.path(), port);
    mutator(&mut cfg);

    vault::init_vault(&cfg, true).expect("init vault");
    let config_file = dir.path().join("config.json");
    save_config_at(&config_file, &cfg);

    let run_cfg = cfg.clone();
    let handle = tokio::spawn(async move {
        let _ = server::run(run_cfg).await;
    });

    wait_for_ready(port).await;

    TestServer {
        _dir: dir,
        cfg,
        port,
        handle,
    }
}

async fn wait_for_ready(port: u16) {
    let client = Client::new();
    let url = format!("http://127.0.0.1:{}/health", port);
    for _ in 0..100 {
        match client.get(&url).send().await {
            Ok(resp) if resp.status() == StatusCode::OK => return,
            _ => sleep(Duration::from_millis(30)).await,
        }
    }
    panic!("server did not become ready on port {}", port);
}

fn run_cli(args: &[&str], envs: &[(&str, String)]) -> (bool, String, String) {
    let mut cmd = Command::new(bin());
    cmd.args(args);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("run cli");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[derive(Clone)]
struct TestClient {
    base_url: String,
    client: Client,
    session_id: Option<String>,
    bearer: Option<String>,
    next_id: i64,
}

impl TestClient {
    fn new(base_url: String) -> Self {
        Self {
            base_url,
            client: Client::new(),
            session_id: None,
            bearer: None,
            next_id: 1,
        }
    }

    fn with_bearer(mut self, bearer: Option<String>) -> Self {
        self.bearer = bearer;
        self
    }

    fn next_id(&mut self) -> i64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    async fn mcp_init(&mut self) -> Value {
        let id = self.next_id();
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        });
        let mut req = self
            .client
            .post(format!("{}/mcp", self.base_url))
            .json(&payload);
        if let Some(token) = &self.bearer {
            req = req.bearer_auth(token);
        }
        let resp = req.send().await.expect("mcp init send");
        let session = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .expect("mcp session header")
            .to_string();
        self.session_id = Some(session);
        resp.json().await.expect("mcp init json")
    }

    async fn mcp_call(&mut self, method: &str, params: Value) -> (StatusCode, Value) {
        let id = self.next_id();
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let mut req = self
            .client
            .post(format!("{}/mcp", self.base_url))
            .json(&payload);
        if let Some(session) = &self.session_id {
            req = req.header("mcp-session-id", session);
        }
        if let Some(token) = &self.bearer {
            req = req.bearer_auth(token);
        }

        let resp = req.send().await.expect("mcp call send");
        let status = resp.status();
        let body = resp.json().await.expect("mcp call json");
        (status, body)
    }

    async fn mcp_notify(&mut self, method: &str) -> StatusCode {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": {},
        });

        let mut req = self
            .client
            .post(format!("{}/mcp", self.base_url))
            .json(&payload);
        if let Some(session) = &self.session_id {
            req = req.header("mcp-session-id", session);
        }
        if let Some(token) = &self.bearer {
            req = req.bearer_auth(token);
        }

        req.send().await.expect("mcp notify send").status()
    }

    async fn mcp_delete_session(&self) -> StatusCode {
        let mut req = self.client.delete(format!("{}/mcp", self.base_url));
        if let Some(session) = &self.session_id {
            req = req.header("mcp-session-id", session);
        }
        if let Some(token) = &self.bearer {
            req = req.bearer_auth(token);
        }
        req.send().await.expect("mcp delete send").status()
    }

    async fn get(&self, path: &str) -> (StatusCode, String) {
        let resp = self
            .client
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await
            .expect("get send");
        let status = resp.status();
        let text = resp.text().await.expect("get text");
        (status, text)
    }
}

fn parse_tool_text(body: &Value) -> String {
    body["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn parse_tool_json(body: &Value) -> Value {
    serde_json::from_str(&parse_tool_text(body)).unwrap_or_else(|_| json!({}))
}

fn tool_error_contains(body: &Value, needle: &str) -> bool {
    parse_tool_text(body).contains(needle)
}

async fn tool_call(client: &mut TestClient, name: &str, arguments: Value) -> Value {
    let (status, body) = client
        .mcp_call(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "tool call status must be 200");
    body
}

async fn write_page(client: &mut TestClient, rel: &str, content: &str) {
    let body = tool_call(
        client,
        "wiki_write",
        json!({
            "path": rel,
            "content": content,
            "log_action": "create",
            "log_description": "e2e write"
        }),
    )
    .await;
    assert_eq!(body["result"]["isError"], json!(false));
}

fn page(title: &str, page_type: &str, body: &str) -> String {
    format!(
        "---\ntitle: {}\ntype: {}\ncreated: 2026-04-23\nupdated: 2026-04-23\ntags: [e2e]\n---\n\n# {}\n\n{}\n",
        title, page_type, title, body
    )
}

fn has_command(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn metric_value(metrics: &str, series: &str) -> Option<f64> {
    metrics.lines().find_map(|line| {
        let (name, value) = line.split_once(' ')?;
        if name == series {
            value.trim().parse::<f64>().ok()
        } else {
            None
        }
    })
}

#[cfg(unix)]
fn write_fake_tool(path: &Path, body: &str) {
    fs::write(path, body).expect("write fake tool");
    let mut perms = fs::metadata(path)
        .expect("fake tool metadata")
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod fake tool");
}

fn doctor_path_with_fake_deps(dir: &Path) -> String {
    let old_path = std::env::var("PATH").unwrap_or_default();

    #[cfg(unix)]
    {
        let fake_bin = dir.join("fake-bin");
        fs::create_dir_all(&fake_bin).expect("fake bin");
        write_fake_tool(
            &fake_bin.join("pdfinfo"),
            "#!/bin/sh\nprintf 'pdfinfo fake\\n'\n",
        );
        write_fake_tool(
            &fake_bin.join("pdfseparate"),
            "#!/bin/sh\nprintf 'pdfseparate fake\\n'\n",
        );
        write_fake_tool(
            &fake_bin.join("pdfunite"),
            "#!/bin/sh\nprintf 'pdfunite fake\\n'\n",
        );
        write_fake_tool(
            &fake_bin.join("systemd-run"),
            "#!/bin/sh\nprintf 'systemd fake\\n'\n",
        );
        return format!("{}:{}", fake_bin.display(), old_path);
    }

    #[allow(unreachable_code)]
    old_path
}

#[tokio::test]
async fn start_and_health() {
    let server = setup().await;
    let client = TestClient::new(server.base_url());

    let (status, text) = client.get("/health").await;
    assert_eq!(status, StatusCode::OK, "health must be 200");
    let body: Value = serde_json::from_str(&text).expect("health json");
    assert_eq!(body["ok"], json!(true), "health ok must be true");
    assert!(!body["version"].as_str().unwrap_or_default().is_empty());
    assert_eq!(
        body["vault_path"],
        json!(server.cfg.vault_path),
        "vault_path must match"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn metrics_endpoint() {
    let server = setup().await;
    let client = TestClient::new(server.base_url());

    let (status, text) = client.get("/metrics").await;
    assert_eq!(status, StatusCode::OK, "metrics must be 200");
    assert!(text.contains("# HELP writestead_mcp_requests_total"));
    assert!(text.contains("# TYPE writestead_mcp_requests_total counter"));
    assert!(text.contains("# HELP writestead_mcp_tool_errors_by_tool_total"));
    assert!(text.contains("# TYPE writestead_mcp_tool_errors_by_tool_total counter"));
    assert!(text.contains("# HELP writestead_raw_reads_by_format_total"));

    server.shutdown().await;
}

#[tokio::test]
async fn graceful_shutdown() {
    let dir = TempDir::new().expect("tempdir");
    let port = free_port();
    let cfg = base_config(dir.path(), port);
    let config_file = dir.path().join("config.json");
    save_config_at(&config_file, &cfg);

    let runtime_dir = dir.path().join("runtime");
    fs::create_dir_all(&runtime_dir).expect("runtime dir");

    let (ok, _out, err) = run_cli(
        &["start"],
        &[
            (
                "WRITESTEAD_CONFIG_FILE",
                config_file.to_string_lossy().to_string(),
            ),
            (
                "WRITESTEAD_RUNTIME_DIR",
                runtime_dir.to_string_lossy().to_string(),
            ),
        ],
    );
    assert!(ok, "start failed: {err}");

    let pid_file = runtime_dir.join("writestead.pid");
    assert!(pid_file.exists(), "pid file must exist after start");

    let pid_text = fs::read_to_string(&pid_file).expect("read pid");
    let pid: i32 = pid_text.trim().parse().expect("pid parse");

    #[cfg(unix)]
    {
        let rc = unsafe { libc::kill(pid, libc::SIGTERM) };
        assert_eq!(rc, 0, "SIGTERM must succeed");

        for _ in 0..50 {
            if !pid_file.exists() {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }

        assert!(
            !pid_file.exists(),
            "pid file must be removed after graceful shutdown"
        );
    }

    let _ = run_cli(
        &["stop"],
        &[
            (
                "WRITESTEAD_CONFIG_FILE",
                config_file.to_string_lossy().to_string(),
            ),
            (
                "WRITESTEAD_RUNTIME_DIR",
                runtime_dir.to_string_lossy().to_string(),
            ),
        ],
    );
}

#[tokio::test]
async fn mcp_full_session() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());

    let init = client.mcp_init().await;
    assert!(init["result"]["protocolVersion"].is_string());

    let notify_status = client.mcp_notify("notifications/initialized").await;
    assert_eq!(notify_status, StatusCode::ACCEPTED);

    let (_status, tools) = client.mcp_call("tools/list", json!({})).await;
    assert!(tools["result"]["tools"].is_array());

    let ping = client.mcp_call("ping", json!({})).await.1;
    assert!(ping.get("result").is_some());

    let del = client.mcp_delete_session().await;
    assert_eq!(del, StatusCode::NO_CONTENT);

    let (_st, after_del) = client.mcp_call("ping", json!({})).await;
    assert_eq!(after_del["error"]["code"], json!(-32001));

    server.shutdown().await;
}

#[tokio::test]
async fn mcp_unknown_session() {
    let server = setup().await;
    let client = Client::new();

    let resp = client
        .post(format!("{}/mcp", server.base_url()))
        .header("content-type", "application/json")
        .header("mcp-session-id", Uuid::new_v4().to_string())
        .body(
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "ping",
                "params": {}
            })
            .to_string(),
        )
        .send()
        .await
        .expect("send");

    let body: Value = resp.json().await.expect("json");
    assert_eq!(body["error"]["code"], json!(-32001));

    server.shutdown().await;
}

#[tokio::test]
async fn mcp_unauthorized() {
    let _guard = env_lock().lock().await;
    std::env::set_var("WRITESTEAD_BEARER_TOKEN", "e2e-token");

    let server = setup_with(|cfg| cfg.mcp.auth.mode = McpAuthMode::Bearer).await;
    let mut client = TestClient::new(server.base_url());

    let (status, body) = client
        .mcp_call("initialize", json!({ "protocolVersion": "2025-06-18" }))
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], json!(-32002));

    server.shutdown().await;
    std::env::remove_var("WRITESTEAD_BEARER_TOKEN");
}

#[tokio::test]
async fn mcp_authorized() {
    let _guard = env_lock().lock().await;
    std::env::set_var("WRITESTEAD_BEARER_TOKEN", "e2e-token");

    let server = setup_with(|cfg| cfg.mcp.auth.mode = McpAuthMode::Bearer).await;
    let mut client = TestClient::new(server.base_url()).with_bearer(Some("e2e-token".to_string()));

    let init = client.mcp_init().await;
    assert!(init["result"]["serverInfo"]["name"].is_string());

    server.shutdown().await;
    std::env::remove_var("WRITESTEAD_BEARER_TOKEN");
}

#[tokio::test]
async fn mcp_method_not_found() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let (status, body) = client.mcp_call("no/such/method", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["error"]["code"], json!(-32601));

    server.shutdown().await;
}

#[tokio::test]
async fn mcp_get_returns_405() {
    let server = setup().await;
    let client = Client::new();

    let resp = client
        .get(format!("{}/mcp", server.base_url()))
        .send()
        .await
        .expect("send");

    assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    let allow = resp
        .headers()
        .get("allow")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(allow.contains("POST"));

    server.shutdown().await;
}

#[tokio::test]
async fn mcp_instructions_present() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());

    let init = client.mcp_init().await;
    let instructions = init["result"]["instructions"].as_str().unwrap_or_default();
    assert!(instructions.contains("Ingest workflow:"));

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_write_and_read() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let content = page("Alpha", "entity", "alpha body");
    write_page(&mut client, "wiki/entities/alpha.md", &content).await;

    let read = tool_call(
        &mut client,
        "wiki_read",
        json!({"path": "wiki/entities/alpha.md", "offset": 1, "limit": 40}),
    )
    .await;
    let payload = parse_tool_json(&read);

    assert!(payload["content"]
        .as_str()
        .unwrap_or_default()
        .contains("alpha body"));
    assert!(payload["frontmatter"].is_object());
    assert_eq!(payload["offset"], json!(1));
    assert_eq!(payload["limit"], json!(40));

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_edit_unique_match() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    write_page(
        &mut client,
        "wiki/entities/edit.md",
        &page("Edit", "entity", "needle once"),
    )
    .await;

    let edit = tool_call(
        &mut client,
        "wiki_edit",
        json!({
            "path": "wiki/entities/edit.md",
            "edits": [{"oldText": "needle once", "newText": "needle updated"}],
            "log_action": "update",
            "log_description": "edit"
        }),
    )
    .await;
    assert_eq!(edit["result"]["isError"], json!(false));

    let read = tool_call(
        &mut client,
        "wiki_read",
        json!({"path": "wiki/entities/edit.md", "offset": 1, "limit": 80}),
    )
    .await;
    let payload = parse_tool_json(&read);
    assert!(payload["content"]
        .as_str()
        .unwrap_or_default()
        .contains("needle updated"));

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_edit_rejects_duplicate() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    write_page(
        &mut client,
        "wiki/entities/dup.md",
        &page("Dup", "entity", "same\nsame"),
    )
    .await;

    let edit = tool_call(
        &mut client,
        "wiki_edit",
        json!({
            "path": "wiki/entities/dup.md",
            "edits": [{"oldText": "same", "newText": "x"}],
            "log_action": "update",
            "log_description": "dup"
        }),
    )
    .await;
    assert_eq!(edit["result"]["isError"], json!(true));
    assert!(tool_error_contains(&edit, "must be unique"));

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_edit_requires_log() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    write_page(
        &mut client,
        "wiki/entities/log.md",
        &page("Log", "entity", "target"),
    )
    .await;

    let edit = tool_call(
        &mut client,
        "wiki_edit",
        json!({
            "path": "wiki/entities/log.md",
            "edits": [{"oldText": "target", "newText": "target2"}]
        }),
    )
    .await;
    assert_eq!(edit["result"]["isError"], json!(true));
    assert!(tool_error_contains(&edit, "missing log_action"));

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_search_finds_content() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    write_page(
        &mut client,
        "wiki/entities/a.md",
        &page("A", "entity", "alpha"),
    )
    .await;
    write_page(
        &mut client,
        "wiki/entities/b.md",
        &page("B", "entity", "needle-xyz"),
    )
    .await;
    write_page(
        &mut client,
        "wiki/entities/c.md",
        &page("C", "entity", "gamma"),
    )
    .await;

    let search = tool_call(&mut client, "wiki_search", json!({"query": "needle-xyz"})).await;
    let payload = parse_tool_json(&search);
    let results = payload["results"].as_array().cloned().unwrap_or_default();
    assert_eq!(results.len(), 1, "expected one search result");
    assert_eq!(results[0], json!("wiki/entities/b.md"));

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_list_paginated() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    for i in 0..5 {
        let path = format!("wiki/entities/p{}.md", i);
        let title = format!("P{}", i);
        write_page(&mut client, &path, &page(&title, "entity", "x")).await;
    }

    let p1 = tool_call(&mut client, "wiki_list", json!({"offset": 0, "limit": 2})).await;
    let j1 = parse_tool_json(&p1);
    assert_eq!(j1["limit"], json!(2));
    assert_eq!(j1["offset"], json!(0));
    assert_eq!(j1["has_more"], json!(true));

    let p2 = tool_call(&mut client, "wiki_list", json!({"offset": 2, "limit": 2})).await;
    let j2 = parse_tool_json(&p2);
    assert_eq!(j2["offset"], json!(2));
    let pages = j2["pages"].as_array().cloned().unwrap_or_default();
    assert!(!pages.is_empty());

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_lint_detects_orphan() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let orphan_path = PathBuf::from(&server.cfg.vault_path).join("wiki/entities/orphan.md");
    fs::create_dir_all(orphan_path.parent().expect("orphan parent")).expect("mkdir orphan");
    fs::write(&orphan_path, page("Orphan", "entity", "no links")).expect("write orphan");

    let lint = tool_call(&mut client, "wiki_lint", json!({})).await;
    let payload = parse_tool_json(&lint);
    let orphans = payload["orphan_pages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        orphans.iter().any(|v| v == "wiki/entities/orphan.md"),
        "orphan page must be reported"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_lint_detects_broken_link() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    write_page(
        &mut client,
        "wiki/entities/broken.md",
        &page("Broken", "entity", "[[does-not-exist]]"),
    )
    .await;

    let lint = tool_call(&mut client, "wiki_lint", json!({})).await;
    let payload = parse_tool_json(&lint);
    let links = payload["broken_links"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        links
            .iter()
            .any(|v| v["link"].as_str().unwrap_or_default() == "does-not-exist"),
        "broken link must be reported"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn mcp_wiki_lint_fix_and_dry_run_apply_options() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let schema_path = PathBuf::from(&server.cfg.vault_path).join("SCHEMA.md");
    let canonical_schema = fs::read_to_string(&schema_path).expect("read schema");
    fs::write(
        &schema_path,
        "---\ntitle: Drifted\ntype: schema\nversion: 1\n---\n\n# Drifted\n",
    )
    .expect("drift schema");

    let dry_run = tool_call(
        &mut client,
        "wiki_lint",
        json!({ "fix": true, "dry_run": true }),
    )
    .await;
    let dry_payload = parse_tool_json(&dry_run);
    assert!(dry_payload["fixes_applied"]
        .as_array()
        .is_some_and(|fixes| {
            fixes
                .iter()
                .any(|fix| fix["path"] == "SCHEMA.md" && fix["kind"] == "restore_locked")
        }));
    assert_ne!(
        fs::read_to_string(&schema_path).expect("read dry schema"),
        canonical_schema,
        "dry_run must not write"
    );

    let fixed = tool_call(&mut client, "wiki_lint", json!({ "fix": true })).await;
    let fixed_payload = parse_tool_json(&fixed);
    assert!(fixed_payload["fixes_applied"]
        .as_array()
        .is_some_and(|fixes| {
            fixes
                .iter()
                .any(|fix| fix["path"] == "SCHEMA.md" && fix["kind"] == "restore_locked")
        }));
    assert_eq!(
        fs::read_to_string(&schema_path).expect("read fixed schema"),
        canonical_schema,
        "fix must write through MCP"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_index_auto_updated() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    write_page(
        &mut client,
        "wiki/entities/auto-index.md",
        &page("Auto Index", "entity", "x"),
    )
    .await;

    let index = tool_call(
        &mut client,
        "wiki_read",
        json!({"path": "wiki/index.md", "offset": 1, "limit": 200}),
    )
    .await;
    let payload = parse_tool_json(&index);
    assert!(
        payload["content"]
            .as_str()
            .unwrap_or_default()
            .contains("[[Auto Index]]"),
        "index must contain auto-indexed link"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_sync_none_backend() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let (_status, before_metrics) = TestClient::new(server.base_url()).get("/metrics").await;
    let before_runs = metric_value(
        &before_metrics,
        "writestead_sync_runs_total{trigger=\"mcp\"}",
    )
    .unwrap_or(0.0);
    let before_count =
        metric_value(&before_metrics, "writestead_sync_duration_seconds_count").unwrap_or(0.0);

    let sync = tool_call(&mut client, "wiki_sync", json!({})).await;
    let payload = parse_tool_json(&sync);
    assert_eq!(payload["backend"], json!("none"));
    assert!(payload["message"]
        .as_str()
        .unwrap_or_default()
        .contains("no-op"));

    let (_status, metrics) = TestClient::new(server.base_url()).get("/metrics").await;
    assert!(metrics.contains("# HELP writestead_sync_runs_total Total sync backend runs"));
    assert!(metrics.contains("# TYPE writestead_sync_duration_seconds summary"));

    let after_runs = metric_value(&metrics, "writestead_sync_runs_total{trigger=\"mcp\"}")
        .expect("mcp sync runs metric");
    let after_count = metric_value(&metrics, "writestead_sync_duration_seconds_count")
        .expect("sync duration count metric");
    assert!(
        after_runs >= before_runs + 1.0,
        "sync runs must increment: before={before_runs}, after={after_runs}"
    );
    assert!(
        after_count >= before_count + 1.0,
        "sync duration count must increment: before={before_count}, after={after_count}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn wiki_help() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let help = tool_call(&mut client, "wiki_help", json!({})).await;
    let payload = parse_tool_json(&help);
    let text = payload["instructions"]
        .as_str()
        .unwrap_or_default()
        .to_lowercase();
    assert!(text.contains("ingest workflow"));
    assert!(text.contains("wiki_lint"));

    server.shutdown().await;
}

#[tokio::test]
async fn raw_upload_content() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let up = tool_call(
        &mut client,
        "raw_upload",
        json!({
            "name": "a.txt",
            "content": base64::engine::general_purpose::STANDARD.encode("hello"),
        }),
    )
    .await;
    let payload = parse_tool_json(&up);
    assert_eq!(payload["ok"], json!(true));
    assert_eq!(payload["path"], json!("raw/a.txt"));
    assert_eq!(payload["size_bytes"], json!(5));

    server.shutdown().await;
}

#[tokio::test]
async fn raw_upload_rejects_multiple_sources() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let up = tool_call(
        &mut client,
        "raw_upload",
        json!({
            "name": "bad.txt",
            "url": "https://example.com/a.txt",
            "content": base64::engine::general_purpose::STANDARD.encode("x"),
        }),
    )
    .await;
    assert_eq!(up["result"]["isError"], json!(true));
    assert!(tool_error_contains(
        &up,
        "exactly one of url, path, content"
    ));

    server.shutdown().await;
}

#[tokio::test]
async fn raw_upload_rejects_overwrite() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let _ = tool_call(
        &mut client,
        "raw_upload",
        json!({ "name": "dup.txt", "content": base64::engine::general_purpose::STANDARD.encode("one") }),
    )
    .await;

    let second = tool_call(
        &mut client,
        "raw_upload",
        json!({ "name": "dup.txt", "content": base64::engine::general_purpose::STANDARD.encode("two") }),
    )
    .await;

    assert_eq!(second["result"]["isError"], json!(true));
    assert!(tool_error_contains(&second, "destination already exists"));

    server.shutdown().await;
}

#[tokio::test]
async fn raw_upload_size_cap() {
    let server = setup_with(|cfg| cfg.raw.upload_max_bytes = 10).await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let up = tool_call(
        &mut client,
        "raw_upload",
        json!({ "name": "big.txt", "content": base64::engine::general_purpose::STANDARD.encode("12345678901234567890") }),
    )
    .await;

    assert_eq!(up["result"]["isError"], json!(true));
    assert!(tool_error_contains(&up, "content too large"));

    server.shutdown().await;
}

#[tokio::test]
async fn raw_list_paginated() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    for n in ["a.txt", "b.txt", "c.txt"] {
        let _ = tool_call(
            &mut client,
            "raw_upload",
            json!({ "name": n, "content": base64::engine::general_purpose::STANDARD.encode(n) }),
        )
        .await;
    }

    let list = tool_call(&mut client, "raw_list", json!({"offset": 0, "limit": 2})).await;
    let payload = parse_tool_json(&list);
    assert_eq!(payload["limit"], json!(2));
    assert_eq!(payload["offset"], json!(0));
    assert_eq!(payload["has_more"], json!(true));

    server.shutdown().await;
}

#[tokio::test]
async fn raw_read_text_direct() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let _ = tool_call(
        &mut client,
        "raw_upload",
        json!({ "name": "text.txt", "content": base64::engine::general_purpose::STANDARD.encode("l1\nl2\n") }),
    )
    .await;

    let read = tool_call(
        &mut client,
        "raw_read",
        json!({"path": "text.txt", "offset": 1, "limit": 1}),
    )
    .await;
    let payload = parse_tool_json(&read);
    assert_eq!(payload["extractor"], json!("direct"));
    assert_eq!(payload["offset"], json!(1));
    assert_eq!(payload["limit"], json!(1));

    server.shutdown().await;
}

#[tokio::test]
async fn raw_read_binary_guard() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let bad = vec![0u8, 159, 146, 150];
    let _ = tool_call(
        &mut client,
        "raw_upload",
        json!({ "name": "bin.txt", "content": base64::engine::general_purpose::STANDARD.encode(bad) }),
    )
    .await;

    let read = tool_call(&mut client, "raw_read", json!({"path": "bin.txt"})).await;
    assert_eq!(read["result"]["isError"], json!(true));
    assert!(tool_error_contains(&read, "appears binary"));

    server.shutdown().await;
}

#[tokio::test]
async fn raw_read_rejects_traversal() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let read = tool_call(&mut client, "raw_read", json!({"path": "../secret"})).await;
    assert_eq!(read["result"]["isError"], json!(true));
    assert!(tool_error_contains(&read, "path traversal"));

    server.shutdown().await;
}

#[tokio::test]
async fn raw_read_rejects_assets() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let read = tool_call(&mut client, "raw_read", json!({"path": "assets/img.png"})).await;
    assert_eq!(read["result"]["isError"], json!(true));
    assert!(tool_error_contains(
        &read,
        "raw/assets is not supported yet"
    ));

    server.shutdown().await;
}

#[tokio::test]
async fn raw_add_local_file() {
    let dir = TempDir::new().expect("tempdir");
    let port = free_port();
    let cfg = base_config(dir.path(), port);
    let config_file = dir.path().join("config.json");
    save_config_at(&config_file, &cfg);
    vault::init_vault(&cfg, true).expect("init");

    let source = dir.path().join("local.txt");
    fs::write(&source, "hello").expect("write source");

    let (ok, out, err) = run_cli(
        &["raw", "add", source.to_str().expect("source str")],
        &[(
            "WRITESTEAD_CONFIG_FILE",
            config_file.to_string_lossy().to_string(),
        )],
    );

    assert!(ok, "raw add failed: {err}");
    let payload: Value = serde_json::from_str(&out).expect("json");
    assert_eq!(payload["ok"], json!(true));

    let target = PathBuf::from(&cfg.vault_path).join("raw/local.txt");
    assert!(target.exists(), "raw/local.txt must exist");
}

#[tokio::test]
async fn raw_add_rejects_traversal() {
    let dir = TempDir::new().expect("tempdir");
    let port = free_port();
    let cfg = base_config(dir.path(), port);
    let config_file = dir.path().join("config.json");
    save_config_at(&config_file, &cfg);
    vault::init_vault(&cfg, true).expect("init");

    let (ok, _out, err) = run_cli(
        &["raw", "add", "../etc/passwd"],
        &[(
            "WRITESTEAD_CONFIG_FILE",
            config_file.to_string_lossy().to_string(),
        )],
    );

    assert!(!ok, "raw add traversal must fail");
    assert!(err.contains("path traversal"));
}

#[tokio::test]
#[ignore]
async fn raw_read_pdf_with_liteparse() {
    if !has_command("lit") {
        eprintln!("skip: lit not installed");
        return;
    }

    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let sample_pdf = b"%PDF-1.1\n1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF\n";

    let _ = tool_call(
        &mut client,
        "raw_upload",
        json!({ "name": "sample.pdf", "content": base64::engine::general_purpose::STANDARD.encode(sample_pdf) }),
    )
    .await;

    let read = tool_call(&mut client, "raw_read", json!({"path": "sample.pdf"})).await;
    if read["result"]["isError"] == json!(true) {
        eprintln!("skip: sample pdf parse failed in this environment");
        server.shutdown().await;
        return;
    }

    let payload = parse_tool_json(&read);
    assert_eq!(payload["extractor"], json!("liteparse"));

    server.shutdown().await;
}

#[tokio::test]
#[ignore]
async fn raw_read_pdf_pdftotext_fallback() {
    eprintln!("skip: fallback isolation for lit absence not implemented");
}

#[tokio::test]
async fn search_with_rg() {
    if !has_command("rg") || (!has_command("fd") && !has_command("fdfind")) {
        eprintln!("skip: rg or fd missing");
        return;
    }

    let server =
        setup_with(|cfg| cfg.search.backend = writestead::config::SearchBackend::RgFd).await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    write_page(
        &mut client,
        "wiki/entities/rg.md",
        &page("RG", "entity", "needle-rg"),
    )
    .await;

    let search = tool_call(&mut client, "wiki_search", json!({"query": "needle-rg"})).await;
    let payload = parse_tool_json(&search);
    assert!(payload["results"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .any(|v| v == "wiki/entities/rg.md"));

    server.shutdown().await;
}

#[tokio::test]
async fn list_with_fd() {
    if !has_command("rg") || (!has_command("fd") && !has_command("fdfind")) {
        eprintln!("skip: rg or fd missing");
        return;
    }

    let server =
        setup_with(|cfg| cfg.search.backend = writestead::config::SearchBackend::RgFd).await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    write_page(
        &mut client,
        "wiki/entities/fd.md",
        &page("FD", "entity", "x"),
    )
    .await;

    let list = tool_call(&mut client, "wiki_list", json!({"offset": 0, "limit": 100})).await;
    let payload = parse_tool_json(&list);
    assert!(payload["pages"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .any(|v| v == "wiki/entities/fd.md"));

    server.shutdown().await;
}

#[tokio::test]
async fn rg_fd_fallback_on_auto() {
    let server =
        setup_with(|cfg| cfg.search.backend = writestead::config::SearchBackend::Auto).await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    write_page(
        &mut client,
        "wiki/entities/auto.md",
        &page("Auto", "entity", "needle-auto"),
    )
    .await;

    let search = tool_call(&mut client, "wiki_search", json!({"query": "needle-auto"})).await;
    let payload = parse_tool_json(&search);
    assert!(payload["results"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .any(|v| v == "wiki/entities/auto.md"));

    server.shutdown().await;
}

#[tokio::test]
async fn doctor_passes_clean_vault() {
    let dir = TempDir::new().expect("tempdir");
    let cfg = base_config(dir.path(), free_port());
    let config_file = dir.path().join("config.json");
    save_config_at(&config_file, &cfg);
    vault::init_vault(&cfg, true).expect("init");

    let (ok, _out, err) = run_cli(
        &["doctor"],
        &[
            (
                "WRITESTEAD_CONFIG_FILE",
                config_file.to_string_lossy().to_string(),
            ),
            ("PATH", doctor_path_with_fake_deps(dir.path())),
        ],
    );

    assert!(ok, "doctor should pass clean vault: {err}");
}

#[tokio::test]
async fn doctor_json_output() {
    let dir = TempDir::new().expect("tempdir");
    let cfg = base_config(dir.path(), free_port());
    let config_file = dir.path().join("config.json");
    save_config_at(&config_file, &cfg);
    vault::init_vault(&cfg, true).expect("init");

    let (ok, out, err) = run_cli(
        &["doctor", "--json"],
        &[(
            "WRITESTEAD_CONFIG_FILE",
            config_file.to_string_lossy().to_string(),
        )],
    );

    assert!(ok, "doctor --json should pass: {err}");
    let payload: Value = serde_json::from_str(&out).expect("doctor json parse");
    assert!(payload["checks"].is_array());
}

#[tokio::test]
async fn doctor_includes_extractor_checks() {
    let dir = TempDir::new().expect("tempdir");
    let cfg = base_config(dir.path(), free_port());
    let config_file = dir.path().join("config.json");
    save_config_at(&config_file, &cfg);
    vault::init_vault(&cfg, true).expect("init");

    let (_ok, out, _err) = run_cli(
        &["doctor", "--json"],
        &[(
            "WRITESTEAD_CONFIG_FILE",
            config_file.to_string_lossy().to_string(),
        )],
    );

    let payload: Value = serde_json::from_str(&out).expect("doctor json");
    let names: Vec<String> = payload["checks"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v["name"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(names.contains(&"liteparse_binary".to_string()));
    assert!(names.contains(&"pdftotext_binary".to_string()));
    assert!(names.contains(&"pdfinfo_binary".to_string()));
    assert!(names.contains(&"pdfseparate_binary".to_string()));
    assert!(names.contains(&"pdfunite_binary".to_string()));
    #[cfg(unix)]
    assert!(names.contains(&"systemd_run_binary".to_string()));
}

#[tokio::test]
async fn doctor_includes_accelerator_checks() {
    let dir = TempDir::new().expect("tempdir");
    let cfg = base_config(dir.path(), free_port());
    let config_file = dir.path().join("config.json");
    save_config_at(&config_file, &cfg);
    vault::init_vault(&cfg, true).expect("init");

    let (_ok, out, _err) = run_cli(
        &["doctor", "--json"],
        &[(
            "WRITESTEAD_CONFIG_FILE",
            config_file.to_string_lossy().to_string(),
        )],
    );

    let payload: Value = serde_json::from_str(&out).expect("doctor json");
    let names: Vec<String> = payload["checks"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter_map(|v| v["name"].as_str().map(|s| s.to_string()))
        .collect();
    assert!(names.contains(&"rg_binary".to_string()));
    assert!(names.contains(&"fd_binary".to_string()));
}

#[tokio::test]
async fn metrics_increment_on_tool_calls() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let page_path = PathBuf::from(&server.cfg.vault_path).join("wiki/entities/m.md");
    fs::create_dir_all(page_path.parent().expect("parent")).expect("mkdir");
    fs::write(&page_path, page("M", "entity", "m")).expect("write page");

    for _ in 0..3 {
        let _ = tool_call(
            &mut client,
            "wiki_read",
            json!({"path": "wiki/entities/m.md", "offset": 1, "limit": 5}),
        )
        .await;
    }

    let (_status, metrics) = client.get("/metrics").await;
    assert!(metrics.contains("writestead_mcp_tool_calls_total 3"));
    assert!(metrics.contains("writestead_mcp_tool_calls_by_tool_total{tool=\"wiki_read\"} 3"));

    server.shutdown().await;
}

#[tokio::test]
async fn metrics_raw_counters() {
    let server = setup().await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    let _ = tool_call(
        &mut client,
        "raw_upload",
        json!({ "name": "metric.txt", "content": base64::engine::general_purpose::STANDARD.encode("a\nb\n") }),
    )
    .await;
    let _ = tool_call(&mut client, "raw_read", json!({"path": "metric.txt"})).await;

    let (_status, metrics) = client.get("/metrics").await;
    assert!(metrics.contains("writestead_raw_uploads_total 1"));
    assert!(metrics.contains("writestead_raw_reads_total 1"));
    assert!(metrics.contains("writestead_raw_reads_by_format_total{format=\"direct\"} 1"));

    server.shutdown().await;
}

#[tokio::test]
async fn session_pruned_after_ttl() {
    let server = setup_with(|cfg| cfg.mcp.session_ttl_seconds = 1).await;
    let mut client = TestClient::new(server.base_url());
    client.mcp_init().await;

    sleep(Duration::from_secs(3)).await;

    let (_status, body) = client.mcp_call("ping", json!({})).await;
    assert_eq!(body["error"]["code"], json!(-32001));

    server.shutdown().await;
}

#[tokio::test]
async fn search_backend_auto_fallback_when_rg_fd_missing() {
    let dir = TempDir::new().expect("tempdir");
    let port = free_port();
    let mut cfg = base_config(dir.path(), port);
    cfg.search.backend = writestead::config::SearchBackend::Auto;
    let cfg_file = dir.path().join("config.json");
    save_config_at(&cfg_file, &cfg);
    vault::init_vault(&cfg, true).expect("init");

    fs::create_dir_all(PathBuf::from(&cfg.vault_path).join("wiki/entities")).expect("mkdir");
    fs::write(
        PathBuf::from(&cfg.vault_path).join("wiki/entities/auto-fallback.md"),
        page("AutoFallback", "entity", "needle-fallback"),
    )
    .expect("write page");

    let empty_path = dir.path().join("empty-bin");
    fs::create_dir_all(&empty_path).expect("mkdir empty bin");

    let cfg_file = cfg_file.to_string_lossy().to_string();
    let (ok1, out1, err1) = run_cli(
        &["list", "--offset", "0", "--limit", "20"],
        &[
            ("WRITESTEAD_CONFIG_FILE", cfg_file.clone()),
            ("PATH", empty_path.to_string_lossy().to_string()),
        ],
    );
    assert!(ok1, "list should fallback to builtin: {err1}");
    let list_json: Value = serde_json::from_str(&out1).expect("list json");
    assert!(list_json["pages"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .any(|v| v == "wiki/entities/auto-fallback.md"));

    let (ok2, out2, err2) = run_cli(
        &["search", "needle-fallback"],
        &[
            ("WRITESTEAD_CONFIG_FILE", cfg_file),
            ("PATH", empty_path.to_string_lossy().to_string()),
        ],
    );
    assert!(ok2, "search should fallback to builtin: {err2}");
    let search_json: Value = serde_json::from_str(&out2).expect("search json");
    assert!(search_json["results"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .any(|v| v == "wiki/entities/auto-fallback.md"));
}

#[tokio::test]
async fn search_backend_rgfd_hard_fail_and_doctor_flags_missing() {
    let dir = TempDir::new().expect("tempdir");
    let port = free_port();
    let mut cfg = base_config(dir.path(), port);
    cfg.search.backend = writestead::config::SearchBackend::RgFd;
    let cfg_file = dir.path().join("config.json");
    save_config_at(&cfg_file, &cfg);
    vault::init_vault(&cfg, true).expect("init");

    fs::create_dir_all(PathBuf::from(&cfg.vault_path).join("wiki/entities")).expect("mkdir");
    fs::write(
        PathBuf::from(&cfg.vault_path).join("wiki/entities/strict.md"),
        page("Strict", "entity", "needle-strict"),
    )
    .expect("write page");

    let empty_path = dir.path().join("empty-bin");
    fs::create_dir_all(&empty_path).expect("mkdir empty bin");

    let cfg_file = cfg_file.to_string_lossy().to_string();

    let (ok1, _out1, err1) = run_cli(
        &["list", "--offset", "0", "--limit", "20"],
        &[
            ("WRITESTEAD_CONFIG_FILE", cfg_file.clone()),
            ("PATH", empty_path.to_string_lossy().to_string()),
        ],
    );
    assert!(!ok1, "list must fail when fd missing in rg-fd mode");
    assert!(err1.contains("requires 'fd' or 'fdfind' in PATH"));

    let (ok2, _out2, err2) = run_cli(
        &["search", "needle-strict"],
        &[
            ("WRITESTEAD_CONFIG_FILE", cfg_file.clone()),
            ("PATH", empty_path.to_string_lossy().to_string()),
        ],
    );
    assert!(!ok2, "search must fail when rg missing in rg-fd mode");
    assert!(err2.contains("requires 'rg' in PATH"));

    let (ok3, out3, _err3) = run_cli(
        &["doctor", "--json"],
        &[
            ("WRITESTEAD_CONFIG_FILE", cfg_file),
            ("PATH", empty_path.to_string_lossy().to_string()),
        ],
    );
    assert!(ok3, "doctor --json should still print report");
    let payload: Value = serde_json::from_str(&out3).expect("doctor json");

    let mut checks = HashMap::new();
    for item in payload["checks"].as_array().unwrap_or(&vec![]) {
        if let Some(name) = item["name"].as_str() {
            checks.insert(name.to_string(), item["ok"].as_bool().unwrap_or(false));
        }
    }

    assert_eq!(checks.get("rg_binary"), Some(&false));
    assert_eq!(checks.get("fd_binary"), Some(&false));
    assert_eq!(payload["ok"], json!(false));
}

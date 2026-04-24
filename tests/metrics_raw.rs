use axum::body::{to_bytes, Body};
use axum::http::Request;
use base64::Engine;
use serde_json::{json, Value};
use tempfile::TempDir;
use tower::util::ServiceExt;
use writestead::config::{AppConfig, McpConfig, RawConfig, SearchConfig, SyncBackend, SyncConfig};
use writestead::server;
use writestead::vault;

fn test_config(vault_path: &str) -> AppConfig {
    AppConfig {
        name: "test".to_string(),
        vault_path: vault_path.to_string(),
        host: "127.0.0.1".to_string(),
        port: 0,
        sync: SyncConfig {
            backend: SyncBackend::None,
        },
        mcp: McpConfig::default(),
        search: SearchConfig::default(),
        raw: RawConfig::default(),
    }
}

async fn post_mcp(
    app: &axum::Router,
    session_id: Option<&str>,
    payload: Value,
) -> (axum::http::response::Parts, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("content-type", "application/json");

    if let Some(session_id) = session_id {
        builder = builder.header("mcp-session-id", session_id);
    }

    let req = builder
        .body(Body::from(payload.to_string()))
        .expect("request");
    let resp = app.clone().oneshot(req).await.expect("response");
    let (parts, body) = resp.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.expect("body bytes");
    let json: Value = serde_json::from_slice(&bytes).expect("json response");
    (parts, json)
}

#[tokio::test]
async fn raw_metrics_exported_and_incremented() {
    let dir = TempDir::new().expect("tempdir");
    let cfg = test_config(dir.path().to_str().expect("path str"));
    vault::init_vault(&cfg, true).expect("init vault");

    let state = server::build_state(cfg.clone());
    let app = server::build_app(state);

    let (init_parts, _init_body) = post_mcp(
        &app,
        None,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-06-18" }
        }),
    )
    .await;

    let session_id = init_parts
        .headers
        .get("mcp-session-id")
        .expect("session header")
        .to_str()
        .expect("session header str")
        .to_string();

    let inline_b64 = base64::engine::general_purpose::STANDARD.encode("one\ntwo\n");
    let (_upload_parts, upload_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "raw_upload",
                "arguments": {
                    "name": "metrics.txt",
                    "content": inline_b64
                }
            }
        }),
    )
    .await;
    assert_eq!(upload_body["result"]["isError"], json!(false));

    let (_read_parts, read_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "raw_read",
                "arguments": {
                    "path": "metrics.txt",
                    "offset": 1,
                    "limit": 10
                }
            }
        }),
    )
    .await;
    assert_eq!(read_body["result"]["isError"], json!(false));

    let req = Request::builder()
        .method("GET")
        .uri("/metrics")
        .body(Body::empty())
        .expect("metrics request");
    let resp = app.clone().oneshot(req).await.expect("metrics response");
    let (_parts, body) = resp.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.expect("metrics body");
    let text = String::from_utf8(bytes.to_vec()).expect("metrics utf8");

    assert!(text.contains("# HELP writestead_raw_uploads_total Total raw uploads"));
    assert!(text.contains("# TYPE writestead_raw_uploads_total counter"));
    assert!(text.contains("writestead_raw_uploads_total 1"));

    assert!(text.contains("# HELP writestead_raw_upload_bytes_total Total raw upload bytes"));
    assert!(text.contains("# TYPE writestead_raw_upload_bytes_total counter"));

    assert!(text.contains("# HELP writestead_raw_reads_total Total raw reads"));
    assert!(text.contains("# TYPE writestead_raw_reads_total counter"));
    assert!(text.contains("writestead_raw_reads_total 1"));

    assert!(text.contains("# HELP writestead_raw_reads_by_format_total Raw reads by format"));
    assert!(text.contains("# TYPE writestead_raw_reads_by_format_total counter"));
    assert!(text.contains("writestead_raw_reads_by_format_total{format=\"direct\"} 1"));
}

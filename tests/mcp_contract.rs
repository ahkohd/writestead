use std::fs;

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
async fn tools_list_matches_snapshot_and_pagination_contract() {
    let dir = TempDir::new().expect("tempdir");
    let cfg = test_config(dir.path().to_str().expect("path str"));
    vault::init_vault(&cfg, true).expect("init vault");

    let page = "---\ntitle: Demo\ntype: entity\ncreated: 2026-04-23\nupdated: 2026-04-23\ntags: [demo]\n---\n\n# Demo\n\nline1\nline2\nline3\n";
    fs::write(dir.path().join("wiki/entities/demo.md"), page).expect("write demo");
    fs::create_dir_all(dir.path().join("raw")).expect("raw dir");
    fs::write(dir.path().join("raw/source.txt"), "alpha\nbeta\n").expect("write raw source");

    let state = server::build_state(cfg.clone());
    let app = server::build_app(state);

    let (init_parts, init_body) = post_mcp(
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

    let instructions = init_body["result"]["instructions"]
        .as_str()
        .expect("initialize instructions");
    assert!(instructions.contains("Ingest workflow:"));
    assert!(instructions.contains("raw_list to discover source files"));

    let session_id = init_parts
        .headers
        .get("mcp-session-id")
        .expect("session header")
        .to_str()
        .expect("session header str")
        .to_string();

    let (_list_parts, list_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }),
    )
    .await;

    let tools = list_body["result"]["tools"].clone();
    let fixture_path = format!(
        "{}/tests/fixtures/tools_list.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let expected_tools: Value =
        serde_json::from_str(&fs::read_to_string(fixture_path).expect("fixture read"))
            .expect("fixture json");
    assert_eq!(tools, expected_tools);

    let (_read_parts, read_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "wiki_read",
                "arguments": { "path": "wiki/entities/demo.md", "offset": 1, "limit": 2 }
            }
        }),
    )
    .await;

    let read_payload_text = read_body["result"]["content"][0]["text"]
        .as_str()
        .expect("read text payload");
    let read_payload: Value = serde_json::from_str(read_payload_text).expect("read payload json");

    assert!(read_payload.get("total_lines").is_some());
    assert!(read_payload.get("has_more").is_some());
    assert_eq!(read_payload["offset"], json!(1));
    assert_eq!(read_payload["limit"], json!(2));

    let (_list_call_parts, list_call_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "wiki_list",
                "arguments": { "offset": 0, "limit": 5 }
            }
        }),
    )
    .await;

    let list_payload_text = list_call_body["result"]["content"][0]["text"]
        .as_str()
        .expect("list text payload");
    let list_payload: Value = serde_json::from_str(list_payload_text).expect("list payload json");

    assert!(list_payload.get("total").is_some());
    assert!(list_payload.get("has_more").is_some());
    assert_eq!(list_payload["offset"], json!(0));
    assert_eq!(list_payload["limit"], json!(5));

    let (_raw_list_parts, raw_list_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "raw_list",
                "arguments": { "offset": 0, "limit": 1 }
            }
        }),
    )
    .await;
    let raw_list_text = raw_list_body["result"]["content"][0]["text"]
        .as_str()
        .expect("raw list text payload");
    let raw_list_payload: Value =
        serde_json::from_str(raw_list_text).expect("raw list payload json");
    assert_eq!(raw_list_payload["total"], json!(1));
    assert_eq!(raw_list_payload["offset"], json!(0));
    assert_eq!(raw_list_payload["limit"], json!(1));
    assert_eq!(raw_list_payload["files"], json!(["source.txt"]));

    let (_raw_read_parts, raw_read_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": {
                "name": "raw_read",
                "arguments": {
                    "path": "source.txt"
                }
            }
        }),
    )
    .await;
    let raw_read_text = raw_read_body["result"]["content"][0]["text"]
        .as_str()
        .expect("raw read text payload");
    let raw_read_payload: Value =
        serde_json::from_str(raw_read_text).expect("raw read payload json");
    assert_eq!(raw_read_payload["extractor"], json!("direct"));
    assert!(raw_read_payload["content"]
        .as_str()
        .unwrap_or_default()
        .contains("alpha"));

    let inline_b64 = base64::engine::general_purpose::STANDARD.encode("inline text\n");
    let (_upload_parts, upload_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {
                "name": "raw_upload",
                "arguments": {
                    "name": "inline.txt",
                    "content": inline_b64,
                    "overwrite": false
                }
            }
        }),
    )
    .await;
    let upload_text = upload_body["result"]["content"][0]["text"]
        .as_str()
        .expect("upload text payload");
    let upload_payload: Value = serde_json::from_str(upload_text).expect("upload payload json");
    assert_eq!(upload_payload["ok"], json!(true));
    assert_eq!(upload_payload["path"], json!("raw/inline.txt"));

    let (_upload_missing_name_parts, upload_missing_name_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "raw_upload",
                "arguments": {
                    "content": base64::engine::general_purpose::STANDARD.encode("x")
                }
            }
        }),
    )
    .await;
    assert_eq!(upload_missing_name_body["result"]["isError"], json!(true));
    let missing_name_text = upload_missing_name_body["result"]["content"][0]["text"]
        .as_str()
        .expect("missing name text");
    assert!(missing_name_text.contains("missing name"));

    let (_upload_multi_mode_parts, upload_multi_mode_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {
                "name": "raw_upload",
                "arguments": {
                    "name": "bad.txt",
                    "path": "raw/source.txt",
                    "content": base64::engine::general_purpose::STANDARD.encode("x")
                }
            }
        }),
    )
    .await;
    assert_eq!(upload_multi_mode_body["result"]["isError"], json!(true));
    let multi_mode_text = upload_multi_mode_body["result"]["content"][0]["text"]
        .as_str()
        .expect("multi mode text");
    assert!(multi_mode_text.contains("exactly one of url, path, content"));

    let (_upload_bad_b64_parts, upload_bad_b64_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {
                "name": "raw_upload",
                "arguments": {
                    "name": "bad.txt",
                    "content": "%%%"
                }
            }
        }),
    )
    .await;
    assert_eq!(upload_bad_b64_body["result"]["isError"], json!(true));
    let bad_b64_text = upload_bad_b64_body["result"]["content"][0]["text"]
        .as_str()
        .expect("bad b64 text");
    assert!(bad_b64_text.contains("invalid base64 content"));

    let (_help_parts, help_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {
                "name": "wiki_help",
                "arguments": {}
            }
        }),
    )
    .await;
    let help_text = help_body["result"]["content"][0]["text"]
        .as_str()
        .expect("wiki help payload");
    let help_payload: Value = serde_json::from_str(help_text).expect("wiki help json");
    assert!(help_payload["instructions"]
        .as_str()
        .unwrap_or_default()
        .contains("Vault layout:"));

    let (_edit_parts, edit_body) = post_mcp(
        &app,
        Some(&session_id),
        json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {
                "name": "wiki_edit",
                "arguments": {
                    "path": "wiki/entities/demo.md",
                    "edits": [{"oldText": "line1", "newText": "line1"}]
                }
            }
        }),
    )
    .await;

    assert_eq!(edit_body["result"]["isError"], json!(true));
    let edit_error_text = edit_body["result"]["content"][0]["text"]
        .as_str()
        .expect("edit error text");
    assert!(edit_error_text.contains("missing log_action"));
}

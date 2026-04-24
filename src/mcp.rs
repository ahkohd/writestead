use crate::config::{effective_mcp_auth_mode, effective_mcp_bearer_token, AppConfig, McpAuthMode};
use crate::guide::wiki_help_text;
use crate::raw::{RawOps, RawReadFailure, RawReadOptions};
use crate::syncer::sync_once_with_trigger;
use crate::wiki::{LintOptions, WikiOps};
use anyhow::{Context, Result};
use axum::body::Body;
use axum::extract::{Json, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::sync::RwLock;
use uuid::Uuid;

pub const MCP_DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";
pub const MCP_SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[MCP_DEFAULT_PROTOCOL_VERSION, "2025-03-26"];

#[derive(Debug, Clone)]
pub struct McpState {
    pub config: AppConfig,
    pub wiki: Arc<WikiOps>,
    pub raw: Arc<RawOps>,
    pub sessions: Arc<RwLock<HashMap<Uuid, McpSession>>>,
    pub server_version: String,
    pub started_at: std::time::Instant,
    pub request_count: Arc<AtomicU64>,
    pub tool_call_count: Arc<AtomicU64>,
    pub tool_error_count: Arc<AtomicU64>,
    pub tool_call_by_name: Arc<RwLock<HashMap<String, u64>>>,
    pub tool_error_by_name: Arc<RwLock<HashMap<String, u64>>>,
    pub raw_upload_count: Arc<AtomicU64>,
    pub raw_upload_bytes_total: Arc<AtomicU64>,
    pub raw_read_count: Arc<AtomicU64>,
    pub raw_read_by_format: Arc<RwLock<HashMap<String, u64>>>,
    pub raw_read_failure_by_extractor: Arc<RwLock<HashMap<String, u64>>>,
}

#[derive(Debug, Clone)]
pub struct McpSession {
    pub initialized: bool,
    pub protocol_version: String,
    pub created_at: std::time::Instant,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    String(String),
    Number(i64),
    Null,
}

#[derive(Debug, Serialize)]
struct JsonRpcSuccess {
    jsonrpc: String,
    id: Option<JsonRpcId>,
    result: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct JsonRpcError {
    jsonrpc: String,
    id: Option<JsonRpcId>,
    error: JsonRpcErrorBody,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorBody {
    code: i64,
    message: String,
}

fn get_header(headers: &HeaderMap, key: &str) -> Option<String> {
    headers
        .get(key)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
}

fn select_protocol_version(requested: &str) -> String {
    if MCP_SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested.to_string()
    } else {
        MCP_DEFAULT_PROTOCOL_VERSION.to_string()
    }
}

fn json_response<T: Serialize>(
    status: StatusCode,
    payload: &T,
    session_id: Option<Uuid>,
    protocol_version: Option<String>,
) -> Response {
    let mut builder = Response::builder()
        .status(status)
        .header("content-type", "application/json");

    if let Some(sid) = session_id {
        builder = builder.header("Mcp-Session-Id", sid.to_string());
    }
    if let Some(pv) = protocol_version {
        builder = builder.header("Mcp-Protocol-Version", pv);
    }

    let body = match serde_json::to_vec(payload) {
        Ok(body) => body,
        Err(err) => {
            tracing::error!("failed to serialize json response: {}", err);
            b"{}".to_vec()
        }
    };

    builder.body(Body::from(body)).unwrap()
}

fn mcp_error(
    code: i64,
    message: &str,
    id: Option<JsonRpcId>,
    status: StatusCode,
    session_id: Option<Uuid>,
    protocol_version: Option<String>,
) -> Response {
    let body = JsonRpcError {
        jsonrpc: "2.0".to_string(),
        id,
        error: JsonRpcErrorBody {
            code,
            message: message.to_string(),
        },
    };
    json_response(status, &body, session_id, protocol_version)
}

fn mcp_success(
    id: Option<JsonRpcId>,
    result: serde_json::Value,
    session_id: Option<Uuid>,
    protocol_version: Option<String>,
) -> Response {
    let body = JsonRpcSuccess {
        jsonrpc: "2.0".to_string(),
        id,
        result,
    };

    json_response(StatusCode::OK, &body, session_id, protocol_version)
}

fn mcp_accepted(session_id: Option<Uuid>, protocol_version: Option<String>) -> Response {
    let mut builder = Response::builder().status(StatusCode::ACCEPTED);
    if let Some(sid) = session_id {
        builder = builder.header("Mcp-Session-Id", sid.to_string());
    }
    if let Some(pv) = protocol_version {
        builder = builder.header("Mcp-Protocol-Version", pv);
    }
    builder.body(Body::empty()).unwrap()
}

fn mcp_tools() -> serde_json::Value {
    json!({
        "tools": [
            {
                "name": "raw_list",
                "title": "List Raw Sources",
                "description": "List source files under raw/ (excluding raw/assets). offset is 0-indexed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "offset": { "type": "integer", "description": "0-indexed item offset (default: 0)" },
                        "limit": { "type": "integer", "description": "max items (default: 100)" }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "raw_read",
                "title": "Read Raw Source",
                "description": "Read a raw source file with optional chunking. offset is 1-indexed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "path relative to raw/" },
                        "offset": { "type": "integer", "description": "1-indexed line offset (default: 1)" },
                        "limit": { "type": "integer", "description": "max lines (default: 200)" },
                        "page_start": { "type": "integer", "description": "PDF page start, 1-indexed" },
                        "page_end": { "type": "integer", "description": "PDF page end, inclusive" }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            },
            {
                "name": "raw_upload",
                "title": "Upload Raw Source",
                "description": "Upload into raw/ via url, path, or base64 content. exactly one input mode required.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string" },
                        "path": { "type": "string" },
                        "content": { "type": "string", "description": "base64 content" },
                        "name": { "type": "string", "description": "destination filename in raw/" },
                        "overwrite": { "type": "boolean" }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }
            },
            {
                "name": "wiki_read",
                "title": "Read Wiki Page",
                "description": "Read a wiki page with optional chunking. offset is 1-indexed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offset": { "type": "integer", "description": "1-indexed line offset (default: 1)" },
                        "limit": { "type": "integer", "description": "max lines (default: 200)" }
                    },
                    "required": ["path"],
                    "additionalProperties": false
                }
            },
            {
                "name": "wiki_search",
                "title": "Search Wiki",
                "description": "Search wiki markdown by case-insensitive substring.",
                "inputSchema": {
                    "type": "object",
                    "properties": { "query": { "type": "string" } },
                    "required": ["query"],
                    "additionalProperties": false
                }
            },
            {
                "name": "wiki_edit",
                "title": "Edit Wiki Page",
                "description": "Apply exact oldText/newText replacements and append wiki log.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldText": { "type": "string" },
                                    "newText": { "type": "string" }
                                },
                                "required": ["oldText", "newText"],
                                "additionalProperties": false
                            }
                        },
                        "log_action": { "type": "string" },
                        "log_description": { "type": "string" }
                    },
                    "required": ["path", "edits", "log_action", "log_description"],
                    "additionalProperties": false
                }
            },
            {
                "name": "wiki_write",
                "title": "Write Wiki Page",
                "description": "Write full markdown file and append wiki log.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" },
                        "log_action": { "type": "string" },
                        "log_description": { "type": "string" }
                    },
                    "required": ["path", "content", "log_action", "log_description"],
                    "additionalProperties": false
                }
            },
            {
                "name": "wiki_list",
                "title": "List Wiki Pages",
                "description": "List markdown pages in vault with pagination. offset is 0-indexed.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "offset": { "type": "integer", "description": "0-indexed item offset (default: 0)" },
                        "limit": { "type": "integer", "description": "max items (default: 100)" }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "wiki_lint",
                "title": "Lint Wiki",
                "description": "Run structural lint checks for vault shape, frontmatter, links, orphans, and stale logs.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "fix": { "type": "boolean", "description": "apply safe mechanical fixes" },
                        "dry_run": { "type": "boolean", "description": "show fixes without writing" }
                    },
                    "additionalProperties": false
                }
            },
            {
                "name": "wiki_index",
                "title": "Read Index",
                "description": "Read wiki/index.md.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "wiki_sync",
                "title": "Sync Wiki",
                "description": "Run sync backend for vault.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            },
            {
                "name": "wiki_help",
                "title": "Wiki Help",
                "description": "Return workflow and authoring guide for this vault.",
                "inputSchema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }
        ]
    })
}

fn is_authorized(headers: &HeaderMap, config: &AppConfig) -> bool {
    match effective_mcp_auth_mode(config) {
        McpAuthMode::None => true,
        McpAuthMode::Bearer => {
            let Some(token) = effective_mcp_bearer_token(config) else {
                return false;
            };
            let Some(auth) = get_header(headers, "authorization") else {
                return false;
            };
            let expected = format!("Bearer {}", token);
            bool::from(auth.as_bytes().ct_eq(expected.as_bytes()))
        }
    }
}

fn to_mcp_tool_result(value: serde_json::Value) -> serde_json::Value {
    let text = if value.is_string() {
        value.as_str().unwrap_or("").to_string()
    } else {
        serde_json::to_string_pretty(&value).unwrap_or_default()
    };
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false
    })
}

pub async fn handle_mcp_get(State(state): State<McpState>) -> Response {
    state.request_count.fetch_add(1, Ordering::Relaxed);
    Response::builder()
        .status(StatusCode::METHOD_NOT_ALLOWED)
        .header("allow", "POST, DELETE")
        .body(Body::empty())
        .unwrap()
}

pub async fn handle_mcp_delete(State(state): State<McpState>, headers: HeaderMap) -> Response {
    state.request_count.fetch_add(1, Ordering::Relaxed);

    if !is_authorized(&headers, &state.config) {
        return Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .body(Body::empty())
            .unwrap();
    }

    let Some(raw_sid) = get_header(&headers, "mcp-session-id") else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap();
    };

    let Ok(session_id) = raw_sid.parse::<Uuid>() else {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::empty())
            .unwrap();
    };

    let mut sessions = state.sessions.write().await;
    if sessions.remove(&session_id).is_none() {
        return Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::empty())
            .unwrap();
    }

    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .body(Body::empty())
        .unwrap()
}

pub async fn handle_mcp(
    State(state): State<McpState>,
    headers: HeaderMap,
    Json(req): Json<JsonRpcRequest>,
) -> Response {
    state.request_count.fetch_add(1, Ordering::Relaxed);

    if req.jsonrpc != "2.0" || req.method.trim().is_empty() {
        return mcp_error(
            -32600,
            "Invalid Request",
            req.id.clone(),
            StatusCode::BAD_REQUEST,
            None,
            None,
        );
    }

    if !is_authorized(&headers, &state.config) {
        return mcp_error(
            -32002,
            "Unauthorized",
            req.id.clone(),
            StatusCode::UNAUTHORIZED,
            None,
            None,
        );
    }

    let method = req.method.as_str();
    let has_id = req.id.is_some();
    let id = req.id.clone();

    if method == "initialize" {
        if !has_id {
            return mcp_error(
                -32600,
                "initialize must include a valid id",
                None,
                StatusCode::BAD_REQUEST,
                None,
                None,
            );
        }

        let params = req.params.as_object().cloned().unwrap_or_default();
        let requested = params
            .get("protocolVersion")
            .and_then(|v| v.as_str())
            .unwrap_or(MCP_DEFAULT_PROTOCOL_VERSION);
        let version = select_protocol_version(requested);
        let session_id = Uuid::new_v4();

        {
            let mut sessions = state.sessions.write().await;
            sessions.insert(
                session_id,
                McpSession {
                    initialized: false,
                    protocol_version: version.clone(),
                    created_at: std::time::Instant::now(),
                },
            );
        }

        let result = json!({
            "protocolVersion": version,
            "capabilities": {
                "tools": { "listChanged": false }
            },
            "serverInfo": {
                "name": "writestead",
                "version": state.server_version
            },
            "instructions": wiki_help_text()
        });

        return mcp_success(id, result, Some(session_id), Some(version));
    }

    let session_id = match get_header(&headers, "mcp-session-id") {
        Some(raw) => match raw.parse::<Uuid>() {
            Ok(id) => id,
            Err(_) => {
                return mcp_error(
                    -32000,
                    "Invalid Mcp-Session-Id header",
                    id,
                    StatusCode::BAD_REQUEST,
                    None,
                    None,
                )
            }
        },
        None => {
            return mcp_error(
                -32000,
                "Missing Mcp-Session-Id header",
                id,
                StatusCode::BAD_REQUEST,
                None,
                None,
            )
        }
    };

    let session = {
        let sessions = state.sessions.read().await;
        sessions.get(&session_id).cloned()
    };

    let Some(session) = session else {
        return mcp_error(
            -32001,
            "Unknown MCP session",
            id,
            StatusCode::NOT_FOUND,
            None,
            None,
        );
    };

    let protocol_version = get_header(&headers, "mcp-protocol-version")
        .and_then(|v| {
            if MCP_SUPPORTED_PROTOCOL_VERSIONS.contains(&v.as_str()) {
                Some(v)
            } else {
                None
            }
        })
        .unwrap_or_else(|| session.protocol_version.clone());

    if !has_id {
        if method == "notifications/initialized" {
            let mut sessions = state.sessions.write().await;
            if let Some(existing) = sessions.get_mut(&session_id) {
                existing.initialized = true;
            }
        }
        return mcp_accepted(Some(session_id), Some(protocol_version));
    }

    let Some(valid_id) = id else {
        return mcp_error(
            -32600,
            "Invalid Request id",
            None,
            StatusCode::BAD_REQUEST,
            Some(session_id),
            Some(protocol_version),
        );
    };

    let result = match method {
        "ping" => json!({}),
        "tools/list" => mcp_tools(),
        "tools/call" => {
            state.tool_call_count.fetch_add(1, Ordering::Relaxed);

            let params = req.params.as_object().cloned().unwrap_or_default();
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));

            {
                // Tiny critical section. If tool throughput grows a lot,
                // replace this lock with lock-free per-tool counters.
                let mut by_tool = state.tool_call_by_name.write().await;
                *by_tool.entry(name.to_string()).or_insert(0) += 1;
            }

            match execute_tool(&state, name, args).await {
                Ok(value) => to_mcp_tool_result(value),
                Err(err) => {
                    state.tool_error_count.fetch_add(1, Ordering::Relaxed);
                    {
                        let mut by_tool = state.tool_error_by_name.write().await;
                        *by_tool.entry(name.to_string()).or_insert(0) += 1;
                    }

                    let error_text = if name == "raw_read" {
                        {
                            let extractor = err
                                .downcast_ref::<RawReadFailure>()
                                .map(RawReadFailure::extractor)
                                .unwrap_or("unknown");
                            let mut failures = state.raw_read_failure_by_extractor.write().await;
                            *failures.entry(extractor.to_string()).or_insert(0) += 1;
                        }

                        serde_json::to_string_pretty(&json!({
                            "format": "error",
                            "error": err.to_string(),
                        }))
                        .unwrap_or_else(|_| format!("{} error: {}", name, err))
                    } else {
                        format!("{} error: {}", name, err)
                    };

                    return mcp_success(
                        Some(valid_id),
                        json!({
                            "content": [{ "type": "text", "text": error_text }],
                            "isError": true
                        }),
                        Some(session_id),
                        Some(protocol_version),
                    );
                }
            }
        }
        _ => {
            // JSON-RPC method-not-found is returned in body with HTTP 200.
            return mcp_error(
                -32601,
                &format!("Method not found: {}", method),
                Some(valid_id),
                StatusCode::OK,
                Some(session_id),
                Some(protocol_version),
            );
        }
    };

    mcp_success(
        Some(valid_id),
        result,
        Some(session_id),
        Some(protocol_version),
    )
}

async fn execute_tool(
    state: &McpState,
    name: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value> {
    match name {
        "raw_list" => {
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let limit = args["limit"].as_u64().unwrap_or(100) as usize;
            let raw = state.raw.clone();
            let result =
                tokio::task::spawn_blocking(move || raw.list_sources(offset, limit)).await??;
            Ok(serde_json::to_value(result)?)
        }
        "raw_read" => {
            let path = args["path"].as_str().context("missing path")?.to_string();
            let offset = args["offset"].as_u64().unwrap_or(1) as usize;
            let limit = args["limit"].as_u64().unwrap_or(200) as usize;
            let page_start = args["page_start"].as_u64().map(|v| v as u32);
            let page_end = args["page_end"].as_u64().map(|v| v as u32);
            let result = state
                .raw
                .read_source_with_options(
                    &path,
                    RawReadOptions {
                        offset,
                        limit,
                        page_start,
                        page_end,
                    },
                )
                .await?;

            state.raw_read_count.fetch_add(1, Ordering::Relaxed);
            {
                let extractor = result.extractor.clone();
                let mut by_format = state.raw_read_by_format.write().await;
                *by_format.entry(extractor).or_insert(0) += 1;
            }

            Ok(serde_json::to_value(result)?)
        }
        "raw_upload" => {
            let name = args["name"].as_str().context("missing name")?;
            let overwrite = args["overwrite"].as_bool().unwrap_or(false);

            let url = args["url"].as_str().map(|s| s.to_string());
            let path = args["path"].as_str().map(|s| s.to_string());
            let content = args["content"].as_str().map(|s| s.to_string());

            let mut mode_count = 0;
            if url.is_some() {
                mode_count += 1;
            }
            if path.is_some() {
                mode_count += 1;
            }
            if content.is_some() {
                mode_count += 1;
            }

            if mode_count != 1 {
                anyhow::bail!("exactly one of url, path, content is required");
            }

            let result = if let Some(url) = url {
                state.raw.upload_from_url(&url, name, overwrite).await?
            } else if let Some(path) = path {
                let raw = state.raw.clone();
                let name = name.to_string();
                tokio::task::spawn_blocking(move || raw.upload_from_path(&path, &name, overwrite))
                    .await??
            } else if let Some(content) = content {
                let raw = state.raw.clone();
                let name = name.to_string();
                tokio::task::spawn_blocking(move || {
                    raw.upload_from_content(&content, &name, overwrite)
                })
                .await??
            } else {
                unreachable!();
            };

            state.raw_upload_count.fetch_add(1, Ordering::Relaxed);
            state
                .raw_upload_bytes_total
                .fetch_add(result.size_bytes, Ordering::Relaxed);

            Ok(serde_json::to_value(result)?)
        }
        "wiki_read" => {
            let path = args["path"].as_str().context("missing path")?;
            let offset = args["offset"].as_u64().unwrap_or(1) as usize;
            let limit = args["limit"].as_u64().unwrap_or(200) as usize;
            let page = state.wiki.read_page(path, offset, limit)?;
            Ok(serde_json::to_value(page)?)
        }
        "wiki_search" => {
            let query = args["query"].as_str().context("missing query")?;
            let results = state.wiki.search(query)?;
            Ok(json!({ "results": results }))
        }
        "wiki_edit" => {
            let path = args["path"].as_str().context("missing path")?;
            let edits = args["edits"].as_array().context("missing edits")?;
            let mut replacements = Vec::new();
            for edit in edits {
                let old = edit["oldText"].as_str().context("missing oldText")?;
                let new = edit["newText"].as_str().context("missing newText")?;
                replacements.push((old.to_string(), new.to_string()));
            }

            let action = args["log_action"].as_str().context("missing log_action")?;
            let description = args["log_description"]
                .as_str()
                .context("missing log_description")?;

            let _ = state.wiki.edit_page(path, &replacements)?;
            let date = Utc::now().format("%Y-%m-%d").to_string();
            state.wiki.append_log(&date, action, description)?;

            Ok(json!({ "ok": true, "path": path, "updated": true }))
        }
        "wiki_write" => {
            let path = args["path"].as_str().context("missing path")?;
            let content = args["content"].as_str().context("missing content")?;
            let action = args["log_action"].as_str().context("missing log_action")?;
            let description = args["log_description"]
                .as_str()
                .context("missing log_description")?;

            state.wiki.write_page(path, content)?;

            let date = Utc::now().format("%Y-%m-%d").to_string();
            state.wiki.append_log(&date, action, description)?;

            Ok(json!({ "ok": true, "path": path, "created": true }))
        }
        "wiki_list" => {
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let limit = args["limit"].as_u64().unwrap_or(100) as usize;
            let result = state.wiki.list_pages_paginated(offset, limit)?;
            Ok(serde_json::to_value(result)?)
        }
        "wiki_lint" => {
            let fix = args["fix"].as_bool().unwrap_or(false);
            let dry_run = args["dry_run"].as_bool().unwrap_or(false);
            let report = state.wiki.lint_with_options(LintOptions { fix, dry_run })?;
            Ok(serde_json::to_value(report)?)
        }
        "wiki_index" => {
            let page = state.wiki.read_page("wiki/index.md", 1, 2000)?;
            Ok(serde_json::to_value(page)?)
        }
        "wiki_sync" => {
            let result = sync_once_with_trigger(&state.config, "mcp").await?;
            Ok(json!({
                "ok": true,
                "backend": result.backend,
                "message": result.message,
            }))
        }
        "wiki_help" => Ok(json!({
            "instructions": wiki_help_text(),
        })),
        _ => anyhow::bail!("Unknown tool: {}", name),
    }
}

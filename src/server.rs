use crate::config::{effective_mcp_auth_mode, effective_mcp_bearer_token};
use crate::mcp::{handle_mcp, handle_mcp_delete, handle_mcp_get, McpSession, McpState};
use crate::raw::RawOps;
use crate::wiki::WikiOps;
use anyhow::Result;
use axum::{
    extract::State,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

pub async fn run(config: crate::config::AppConfig) -> Result<()> {
    let state = build_state(config.clone());
    tokio::spawn(prune_sessions_task(
        state.sessions.clone(),
        Duration::from_secs(config.mcp.session_ttl_seconds.max(1)),
    ));

    let app = build_app(state);

    let addr: std::net::SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    tracing::info!("writestead daemon listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    let serve_result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;

    if let Err(err) = crate::daemon::cleanup_pid_file_if_current_process() {
        tracing::warn!("failed to cleanup pid file on shutdown: {}", err);
    }

    serve_result?;
    Ok(())
}

pub fn build_state(config: crate::config::AppConfig) -> McpState {
    let sessions = Arc::new(RwLock::new(HashMap::<uuid::Uuid, McpSession>::new()));
    let request_count = Arc::new(AtomicU64::new(0));
    let tool_call_count = Arc::new(AtomicU64::new(0));
    let tool_error_count = Arc::new(AtomicU64::new(0));
    let tool_call_by_name = Arc::new(RwLock::new(HashMap::<String, u64>::new()));
    let tool_error_by_name = Arc::new(RwLock::new(HashMap::<String, u64>::new()));
    let raw_read_by_format = Arc::new(RwLock::new(HashMap::<String, u64>::new()));
    let raw_read_failure_by_extractor = Arc::new(RwLock::new(HashMap::<String, u64>::new()));
    let wiki = Arc::new(WikiOps::new(config.clone()));
    let raw = Arc::new(RawOps::new(config.clone()));

    McpState {
        config,
        wiki,
        raw,
        sessions,
        server_version: env!("CARGO_PKG_VERSION").to_string(),
        started_at: std::time::Instant::now(),
        request_count,
        tool_call_count,
        tool_error_count,
        tool_call_by_name,
        tool_error_by_name,
        raw_upload_count: Arc::new(AtomicU64::new(0)),
        raw_upload_bytes_total: Arc::new(AtomicU64::new(0)),
        raw_read_count: Arc::new(AtomicU64::new(0)),
        raw_read_by_format,
        raw_read_failure_by_extractor,
    }
}

pub fn build_app(state: McpState) -> Router {
    Router::new()
        .route(
            "/mcp",
            post(handle_mcp)
                .get(handle_mcp_get)
                .delete(handle_mcp_delete),
        )
        .route("/health", get(health))
        .route("/metrics", get(metrics))
        .with_state(state)
        .layer(tower_http::trace::TraceLayer::new_for_http())
}

async fn health(State(state): State<McpState>) -> Json<serde_json::Value> {
    Json(json!({
        "ok": true,
        "name": state.config.name,
        "vault_path": state.config.vault_path,
        "sync_backend": state.config.sync.backend.to_string(),
        "mcp_auth_mode": effective_mcp_auth_mode(&state.config).to_string(),
        "mcp_has_bearer_token": effective_mcp_bearer_token(&state.config).is_some(),
        "mcp_session_ttl_seconds": state.config.mcp.session_ttl_seconds,
        "mcp_requests_total": state.request_count.load(Ordering::Relaxed),
        "mcp_tool_calls_total": state.tool_call_count.load(Ordering::Relaxed),
        "mcp_tool_errors_total": state.tool_error_count.load(Ordering::Relaxed),
        "mcp_tool_calls_by_name": state.tool_call_by_name.read().await.clone(),
        "mcp_tool_errors_by_name": state.tool_error_by_name.read().await.clone(),
        "raw_uploads_total": state.raw_upload_count.load(Ordering::Relaxed),
        "raw_upload_bytes_total": state.raw_upload_bytes_total.load(Ordering::Relaxed),
        "raw_reads_total": state.raw_read_count.load(Ordering::Relaxed),
        "raw_reads_by_format": state.raw_read_by_format.read().await.clone(),
        "raw_read_failures_by_extractor": state.raw_read_failure_by_extractor.read().await.clone(),
        "uptime_sec": state.started_at.elapsed().as_secs(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn metrics(State(state): State<McpState>) -> Response<String> {
    let session_count = state.sessions.read().await.len();
    let request_count = state.request_count.load(Ordering::Relaxed);
    let tool_call_count = state.tool_call_count.load(Ordering::Relaxed);
    let tool_error_count = state.tool_error_count.load(Ordering::Relaxed);
    let by_tool = state.tool_call_by_name.read().await.clone();
    let error_by_tool = state.tool_error_by_name.read().await.clone();
    let raw_uploads_total = state.raw_upload_count.load(Ordering::Relaxed);
    let raw_upload_bytes_total = state.raw_upload_bytes_total.load(Ordering::Relaxed);
    let raw_reads_total = state.raw_read_count.load(Ordering::Relaxed);
    let raw_reads_by_format = state.raw_read_by_format.read().await.clone();
    let raw_read_failures_by_extractor = state.raw_read_failure_by_extractor.read().await.clone();

    let mut body = String::new();

    body.push_str("# HELP writestead_uptime_seconds Daemon uptime in seconds\n");
    body.push_str("# TYPE writestead_uptime_seconds gauge\n");
    body.push_str(&format!(
        "writestead_uptime_seconds {}\n",
        state.started_at.elapsed().as_secs()
    ));

    body.push_str("# HELP writestead_mcp_sessions_active Active MCP sessions\n");
    body.push_str("# TYPE writestead_mcp_sessions_active gauge\n");
    body.push_str(&format!(
        "writestead_mcp_sessions_active {}\n",
        session_count
    ));

    body.push_str("# HELP writestead_mcp_requests_total Total MCP endpoint requests\n");
    body.push_str("# TYPE writestead_mcp_requests_total counter\n");
    body.push_str(&format!(
        "writestead_mcp_requests_total {}\n",
        request_count
    ));

    body.push_str("# HELP writestead_mcp_tool_calls_total Total MCP tools/call requests\n");
    body.push_str("# TYPE writestead_mcp_tool_calls_total counter\n");
    body.push_str(&format!(
        "writestead_mcp_tool_calls_total {}\n",
        tool_call_count
    ));

    body.push_str("# HELP writestead_mcp_tool_calls_by_tool_total MCP tool calls by tool name\n");
    body.push_str("# TYPE writestead_mcp_tool_calls_by_tool_total counter\n");
    let mut names: Vec<String> = by_tool.keys().cloned().collect();
    names.sort();
    for name in names {
        if let Some(value) = by_tool.get(&name) {
            body.push_str(&format!(
                "writestead_mcp_tool_calls_by_tool_total{{tool=\"{}\"}} {}\n",
                prometheus_escape_label(&name),
                value
            ));
        }
    }

    body.push_str("# HELP writestead_mcp_tool_errors_total Total MCP tool execution errors\n");
    body.push_str("# TYPE writestead_mcp_tool_errors_total counter\n");
    body.push_str(&format!(
        "writestead_mcp_tool_errors_total {}\n",
        tool_error_count
    ));

    body.push_str(
        "# HELP writestead_mcp_tool_errors_by_tool_total MCP tool execution errors by tool name\n",
    );
    body.push_str("# TYPE writestead_mcp_tool_errors_by_tool_total counter\n");
    let mut error_names: Vec<String> = error_by_tool.keys().cloned().collect();
    error_names.sort();
    for name in error_names {
        if let Some(value) = error_by_tool.get(&name) {
            body.push_str(&format!(
                "writestead_mcp_tool_errors_by_tool_total{{tool=\"{}\"}} {}\n",
                prometheus_escape_label(&name),
                value
            ));
        }
    }

    body.push_str("# HELP writestead_raw_uploads_total Total raw uploads\n");
    body.push_str("# TYPE writestead_raw_uploads_total counter\n");
    body.push_str(&format!(
        "writestead_raw_uploads_total {}\n",
        raw_uploads_total
    ));

    body.push_str("# HELP writestead_raw_upload_bytes_total Total raw upload bytes\n");
    body.push_str("# TYPE writestead_raw_upload_bytes_total counter\n");
    body.push_str(&format!(
        "writestead_raw_upload_bytes_total {}\n",
        raw_upload_bytes_total
    ));

    body.push_str("# HELP writestead_raw_reads_total Total raw reads\n");
    body.push_str("# TYPE writestead_raw_reads_total counter\n");
    body.push_str(&format!("writestead_raw_reads_total {}\n", raw_reads_total));

    body.push_str("# HELP writestead_raw_reads_by_format_total Raw reads by format\n");
    body.push_str("# TYPE writestead_raw_reads_by_format_total counter\n");
    let mut formats: Vec<String> = raw_reads_by_format.keys().cloned().collect();
    formats.sort();
    for format_name in formats {
        if let Some(value) = raw_reads_by_format.get(&format_name) {
            body.push_str(&format!(
                "writestead_raw_reads_by_format_total{{format=\"{}\"}} {}\n",
                prometheus_escape_label(&format_name),
                value
            ));
        }
    }

    body.push_str(
        "# HELP writestead_raw_read_failures_by_extractor_total Raw read failures by extractor\n",
    );
    body.push_str("# TYPE writestead_raw_read_failures_by_extractor_total counter\n");
    let mut extractor_names: Vec<String> = raw_read_failures_by_extractor.keys().cloned().collect();
    extractor_names.sort();
    for extractor in extractor_names {
        if let Some(value) = raw_read_failures_by_extractor.get(&extractor) {
            body.push_str(&format!(
                "writestead_raw_read_failures_by_extractor_total{{extractor=\"{}\"}} {}\n",
                prometheus_escape_label(&extractor),
                value
            ));
        }
    }

    axum::response::Response::builder()
        .header("content-type", "text/plain; version=0.0.4")
        .body(body)
        .unwrap()
}

async fn prune_sessions_task(
    sessions: Arc<RwLock<HashMap<uuid::Uuid, McpSession>>>,
    session_ttl: Duration,
) {
    let prune_interval = Duration::from_secs((session_ttl.as_secs() / 12).clamp(1, 300));

    loop {
        tokio::time::sleep(prune_interval).await;

        let now = std::time::Instant::now();
        let mut guard = sessions.write().await;
        guard.retain(|_, session| now.duration_since(session.created_at) < session_ttl);
    }
}

fn prometheus_escape_label(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            let _ = signal.recv().await;
        }
    };

    #[cfg(unix)]
    {
        tokio::select! {
            _ = ctrl_c => {},
            _ = terminate => {},
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await;
    }
}

use crate::config::{AppConfig, SyncBackend};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct SyncResult {
    pub backend: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct SyncMetricsSnapshot {
    pub runs_by_trigger: HashMap<String, u64>,
    pub errors_by_trigger: HashMap<String, u64>,
    pub duration_seconds_sum: f64,
    pub duration_seconds_count: u64,
}

struct SyncMetrics {
    runs_by_trigger: Mutex<HashMap<String, u64>>,
    errors_by_trigger: Mutex<HashMap<String, u64>>,
    duration_micros_sum: AtomicU64,
    duration_count: AtomicU64,
}

fn sync_metrics() -> &'static SyncMetrics {
    static METRICS: OnceLock<SyncMetrics> = OnceLock::new();
    METRICS.get_or_init(|| SyncMetrics {
        runs_by_trigger: Mutex::new(HashMap::new()),
        errors_by_trigger: Mutex::new(HashMap::new()),
        duration_micros_sum: AtomicU64::new(0),
        duration_count: AtomicU64::new(0),
    })
}

pub fn metrics_snapshot() -> SyncMetricsSnapshot {
    let metrics = sync_metrics();
    SyncMetricsSnapshot {
        runs_by_trigger: metrics
            .runs_by_trigger
            .lock()
            .expect("sync runs metrics")
            .clone(),
        errors_by_trigger: metrics
            .errors_by_trigger
            .lock()
            .expect("sync errors metrics")
            .clone(),
        duration_seconds_sum: metrics.duration_micros_sum.load(Ordering::Relaxed) as f64
            / 1_000_000.0,
        duration_seconds_count: metrics.duration_count.load(Ordering::Relaxed),
    }
}

pub fn metrics_snapshot_json() -> serde_json::Value {
    let snapshot = metrics_snapshot();
    serde_json::json!({
        "runs_by_trigger": snapshot.runs_by_trigger,
        "errors_by_trigger": snapshot.errors_by_trigger,
        "duration_seconds_sum": snapshot.duration_seconds_sum,
        "duration_seconds_count": snapshot.duration_seconds_count,
    })
}

pub async fn sync_once(cfg: &AppConfig) -> Result<SyncResult> {
    sync_once_with_trigger(cfg, "unknown").await
}

pub async fn sync_once_with_trigger(cfg: &AppConfig, trigger: &'static str) -> Result<SyncResult> {
    let start = Instant::now();
    let result = sync_once_inner(cfg).await;
    record_sync_metrics(trigger, start.elapsed().as_micros() as u64, result.is_err());
    result
}

async fn sync_once_inner(cfg: &AppConfig) -> Result<SyncResult> {
    match cfg.sync.backend {
        SyncBackend::None => Ok(SyncResult {
            backend: "none".to_string(),
            message: "sync backend is none; no-op".to_string(),
        }),
        SyncBackend::Obsidian => sync_obsidian(cfg).await,
    }
}

fn record_sync_metrics(trigger: &str, duration_micros: u64, failed: bool) {
    let metrics = sync_metrics();
    {
        let mut runs = metrics.runs_by_trigger.lock().expect("sync runs metrics");
        *runs.entry(trigger.to_string()).or_insert(0) += 1;
    }
    if failed {
        let mut errors = metrics
            .errors_by_trigger
            .lock()
            .expect("sync errors metrics");
        *errors.entry(trigger.to_string()).or_insert(0) += 1;
    }
    metrics
        .duration_micros_sum
        .fetch_add(duration_micros, Ordering::Relaxed);
    metrics.duration_count.fetch_add(1, Ordering::Relaxed);
}

async fn sync_obsidian(cfg: &AppConfig) -> Result<SyncResult> {
    let output = Command::new("ob")
        .args(["sync", "--path", &cfg.vault_path])
        .output()
        .await
        .context("failed to execute 'ob sync'")?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        let mut message = String::from("ob sync failed");
        if !stderr.is_empty() {
            message.push_str(": ");
            message.push_str(&stderr);
        }
        if !stdout.is_empty() {
            message.push('\n');
            message.push_str(&stdout);
        }
        anyhow::bail!(message);
    }

    let message = if stdout.is_empty() {
        "ob sync ok".to_string()
    } else {
        stdout
    };

    Ok(SyncResult {
        backend: "obsidian".to_string(),
        message,
    })
}

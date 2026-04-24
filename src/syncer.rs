use crate::config::{AppConfig, SyncBackend};
use anyhow::{Context, Result};
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct SyncResult {
    pub backend: String,
    pub message: String,
}

pub async fn sync_once(cfg: &AppConfig) -> Result<SyncResult> {
    match cfg.sync.backend {
        SyncBackend::None => Ok(SyncResult {
            backend: "none".to_string(),
            message: "sync backend is none; no-op".to_string(),
        }),
        SyncBackend::Obsidian => sync_obsidian(cfg).await,
    }
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

use crate::config::AppConfig;
use crate::wiki::template_for_path_with_vault;
use anyhow::{Context, Result};
use std::fs;
use std::path::Path;

#[derive(Debug)]
pub struct InitSummary {
    pub created_files: usize,
    pub touched_files: usize,
}

pub fn init_vault(cfg: &AppConfig, force: bool) -> Result<InitSummary> {
    let vault = Path::new(&cfg.vault_path);

    fs::create_dir_all(vault)
        .with_context(|| format!("failed to create vault {}", vault.display()))?;

    let dirs = [
        "raw",
        "raw/assets",
        "wiki",
        "wiki/sources",
        "wiki/entities",
        "wiki/concepts",
        "wiki/analyses",
    ];

    for dir in dirs {
        let path = vault.join(dir);
        fs::create_dir_all(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
    }

    let mut created = 0usize;
    let mut touched = 0usize;

    let files = ["SCHEMA.md", "SKILL.md", "wiki/index.md", "wiki/log.md"];

    for rel in files {
        let path = vault.join(rel);
        let body = template_for_path_with_vault(rel, &cfg.vault_path);

        if write_file_if_needed(&path, &body, force)? {
            created += 1;
        }
        touched += 1;
    }

    Ok(InitSummary {
        created_files: created,
        touched_files: touched,
    })
}

fn write_file_if_needed(path: &Path, body: &str, force: bool) -> Result<bool> {
    if path.exists() && !force {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    fs::write(path, body).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(true)
}

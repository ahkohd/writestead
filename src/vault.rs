use crate::config::AppConfig;
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

    let files = [
        ("README.md", readme_template(cfg)),
        ("SCHEMA.md", schema_template()),
        ("SKILL.md", skill_template()),
        ("wiki/index.md", index_template()),
        ("wiki/log.md", log_template()),
    ];

    for (rel, body) in files {
        let path = vault.join(rel);
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

fn readme_template(cfg: &AppConfig) -> String {
    format!(
        "# {}\n\nPersonal knowledge wiki.\n\n- Schema: [SCHEMA.md](SCHEMA.md)\n- Skill: [SKILL.md](SKILL.md)\n- Index: [wiki/index.md](wiki/index.md)\n- Log: [wiki/log.md](wiki/log.md)\n",
        cfg.name
    )
}

fn schema_template() -> String {
    "# Writestead Wiki Schema

## Structure

```
writestead/
  raw/
  raw/assets/
  wiki/
  wiki/index.md
  wiki/log.md
  SCHEMA.md
```

## Conventions

- All wiki pages are markdown with YAML frontmatter.
- Frontmatter fields: title, type, created, updated, tags.
- Use [[wikilinks]] for cross-references.
- Keep pages focused.
- No emojis.

## Frontmatter template

```yaml
---
title: Page Title
type: source | entity | concept | analysis
created: 2026-04-23
updated: 2026-04-23
tags: [tag1, tag2]
---
```
"
    .to_string()
}

fn skill_template() -> String {
    "# Writestead Skill

- Read SCHEMA.md before writing.
- Update wiki/index.md when creating pages.
- Append to wiki/log.md after changes.
- Use writestead sync or wiki_sync when done.
"
    .to_string()
}

fn index_template() -> String {
    "# Wiki Index

## Sources

## Entities

## Concepts

## Analyses
"
    .to_string()
}

fn log_template() -> String {
    "# Wiki Log

"
    .to_string()
}

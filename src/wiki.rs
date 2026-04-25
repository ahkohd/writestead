use crate::config::{AppConfig, SearchBackend};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::LazyLock;
use walkdir::WalkDir;

static WIKILINK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\[([^\]]+)\]\]").expect("valid wikilink regex"));
static UPDATED_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?m)^updated:\s*\d{4}-\d{2}-\d{2}\s*$").expect("valid updated regex")
});
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Frontmatter {
    pub title: String,
    #[serde(rename = "type")]
    pub page_type: String,
    pub created: String,
    pub updated: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WikiPage {
    pub path: String,
    pub frontmatter: Option<Frontmatter>,
    pub content: String,
    pub outbound_links: Vec<String>,
    pub offset: usize,
    pub limit: usize,
    pub total_lines: usize,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListPagesResult {
    pub pages: Vec<String>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
}

pub use crate::wiki_lint::{
    BadFrontmatter, BrokenLink, ContentDrift, DuplicateIndexEntry, DuplicateIndexSection,
    ForeignIndexContent, ForeignLogContent, InvalidFrontmatter, LintFix, LintOptions, LintReport,
    LogIssue, LogOrderIssue, MisplacedPage, MissingIndexSection, MissingStructure,
    StaleIndexMixedBullet, UnindexedPage,
};

#[derive(Debug, Clone)]
pub struct WikiOps {
    config: AppConfig,
}

impl WikiOps {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }

    pub(crate) fn root(&self) -> PathBuf {
        PathBuf::from(&self.config.vault_path)
    }

    fn resolve_rel_path(&self, rel_path: &str) -> Result<PathBuf> {
        let cleaned = sanitize_rel_path(rel_path)?;
        Ok(self.root().join(cleaned))
    }

    pub(crate) fn to_rel_path(&self, full: &Path) -> String {
        full.strip_prefix(self.root())
            .unwrap_or(full)
            .to_string_lossy()
            .to_string()
    }

    // offset is 1-indexed. line 1 is offset=1.
    pub fn read_page(&self, rel_path: &str, offset: usize, limit: usize) -> Result<WikiPage> {
        let path = self.resolve_rel_path(rel_path)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let (frontmatter, content_only) = parse_frontmatter(&text);
        let lines: Vec<&str> = content_only.lines().collect();
        let total_lines = lines.len();

        let safe_offset = offset.max(1);
        let safe_limit = limit.max(1);

        let start = safe_offset.saturating_sub(1).min(total_lines);
        let end = (start + safe_limit).min(total_lines);
        let has_more = end < total_lines;

        let content = if start == 0 && end == total_lines {
            content_only
        } else {
            lines[start..end].join("\n")
        };

        Ok(WikiPage {
            path: rel_path.to_string(),
            frontmatter,
            content,
            outbound_links: extract_wikilinks(&text),
            offset: safe_offset,
            limit: safe_limit,
            total_lines,
            has_more,
        })
    }

    pub fn write_page(&self, rel_path: &str, content: &str) -> Result<()> {
        let path = self.resolve_rel_path(rel_path)?;
        let is_new = !path.exists();

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;

        if is_new {
            self.update_index_for_new_page(rel_path, content)?;
        }

        Ok(())
    }

    pub fn edit_page(&self, rel_path: &str, edits: &[(String, String)]) -> Result<String> {
        let path = self.resolve_rel_path(rel_path)?;
        let mut text = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        for (old, new) in edits {
            let count = text.matches(old).count();
            if count == 0 {
                anyhow::bail!(
                    "oldText not found in {}: '{}'",
                    rel_path,
                    old.replace('\n', "\\n")
                );
            }
            if count > 1 {
                anyhow::bail!(
                    "oldText matches {} times in {} (must be unique): '{}'",
                    count,
                    rel_path,
                    old.replace('\n', "\\n")
                );
            }
            text = text.replacen(old, new, 1);
        }

        text = update_frontmatter_updated(&text);

        fs::write(&path, &text).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(text)
    }

    pub fn search(&self, query: &str) -> Result<Vec<String>> {
        let mut results = match self.config.search.backend {
            SearchBackend::Builtin => self.search_builtin(query)?,
            SearchBackend::Auto => {
                if command_exists("rg") {
                    self.search_with_rg(query)
                        .or_else(|_| self.search_builtin(query))?
                } else {
                    self.search_builtin(query)?
                }
            }
            SearchBackend::RgFd => {
                if !command_exists("rg") {
                    anyhow::bail!("search.backend=rg-fd requires 'rg' in PATH");
                }
                self.search_with_rg(query)?
            }
        };

        results.sort();
        Ok(results)
    }

    // offset is 0-indexed. first item is offset=0.
    pub fn list_pages_paginated(&self, offset: usize, limit: usize) -> Result<ListPagesResult> {
        let mut all_pages = match self.config.search.backend {
            SearchBackend::Builtin => self.list_pages_builtin()?,
            SearchBackend::Auto => {
                if let Some(fd_program) = detect_fd_program() {
                    self.list_pages_with_fd(fd_program)
                        .or_else(|_| self.list_pages_builtin())?
                } else {
                    self.list_pages_builtin()?
                }
            }
            SearchBackend::RgFd => {
                let Some(fd_program) = detect_fd_program() else {
                    anyhow::bail!("search.backend=rg-fd requires 'fd' or 'fdfind' in PATH");
                };
                self.list_pages_with_fd(fd_program)?
            }
        };

        all_pages.sort();

        let total = all_pages.len();
        let safe_limit = limit.max(1);
        let safe_offset = offset.min(total);
        let end = (safe_offset + safe_limit).min(total);

        Ok(ListPagesResult {
            pages: all_pages[safe_offset..end].to_vec(),
            total,
            offset: safe_offset,
            limit: safe_limit,
            has_more: end < total,
        })
    }

    fn search_builtin(&self, query: &str) -> Result<Vec<String>> {
        let root = self.root();
        let q = query.to_lowercase();
        let mut results = Vec::new();

        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".md"))
        {
            let text = match fs::read_to_string(entry.path()) {
                Ok(text) => text,
                Err(_) => continue,
            };

            if text.to_lowercase().contains(&q) {
                results.push(self.to_rel_path(entry.path()));
            }
        }

        Ok(results)
    }

    fn search_with_rg(&self, query: &str) -> Result<Vec<String>> {
        let root = self.root();
        let output = StdCommand::new("rg")
            .current_dir(&root)
            .args([
                "--files-with-matches",
                "-i",
                "--glob",
                "*.md",
                query,
                "wiki",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .context("failed to execute rg")?;

        if !output.status.success() && output.status.code() != Some(1) {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.is_empty() {
                anyhow::bail!("rg failed");
            }
            anyhow::bail!("rg failed: {}", stderr);
        }

        let results = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| line.to_string())
            .collect();
        Ok(results)
    }

    fn list_pages_builtin(&self) -> Result<Vec<String>> {
        let root = self.root();
        let mut all_pages = Vec::new();

        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".md"))
        {
            all_pages.push(self.to_rel_path(entry.path()));
        }

        Ok(all_pages)
    }

    fn list_pages_with_fd(&self, fd_program: &str) -> Result<Vec<String>> {
        let root = self.root();
        let output = StdCommand::new(fd_program)
            .current_dir(&root)
            .args(["-t", "f", "-e", "md", ".", "wiki"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("failed to execute {}", fd_program))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.is_empty() {
                anyhow::bail!("{} failed", fd_program);
            }
            anyhow::bail!("{} failed: {}", fd_program, stderr);
        }

        let mut pages = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let value = line.trim();
            if value.is_empty() {
                continue;
            }
            if value.ends_with(".md") {
                pages.push(value.to_string());
            }
        }

        Ok(pages)
    }

    pub fn append_log(&self, date: &str, action: &str, description: &str) -> Result<()> {
        let log_path = self.resolve_rel_path("wiki/log.md")?;
        let mut text = fs::read_to_string(&log_path).unwrap_or_default();
        let entry = format!("## [{}] {} | {}\n", date, action, description);

        if let Some(byte_pos) = log_entry_insert_offset(&text) {
            text.insert_str(byte_pos, &entry);
        } else {
            if !text.ends_with('\n') {
                text.push('\n');
            }
            if !text.ends_with("\n\n") {
                text.push('\n');
            }
            text.push_str(&entry);
        }

        fs::write(&log_path, text)
            .with_context(|| format!("failed to write {}", log_path.display()))?;
        Ok(())
    }

    fn update_index_for_new_page(&self, rel_path: &str, content: &str) -> Result<()> {
        let index_path = self.resolve_rel_path("wiki/index.md")?;
        let mut text = fs::read_to_string(&index_path).unwrap_or_default();

        let (fm, _) = parse_frontmatter(content);
        let title = fm
            .as_ref()
            .map(|frontmatter| frontmatter.title.clone())
            .unwrap_or_else(|| {
                Path::new(rel_path)
                    .file_stem()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Untitled".to_string())
            });

        let page_type = fm
            .as_ref()
            .map(|frontmatter| frontmatter.page_type.clone())
            .unwrap_or_else(|| "entity".to_string());

        let link = format!("[[{}]]", title);
        if text.contains(&link) {
            return Ok(());
        }

        let section = match page_type.as_str() {
            "source" => "Sources",
            "entity" => "Entities",
            "concept" => "Concepts",
            "analysis" => "Analyses",
            _ => "Entities",
        };

        let section_header = format!("## {}", section);
        if let Some(pos) = text.find(&section_header) {
            let after_header = pos + section_header.len();
            let next_section = text[after_header..].find("\n## ");
            let insert_pos = if let Some(next) = next_section {
                after_header + next
            } else {
                text.len()
            };

            let section_content = &text[after_header..insert_pos];
            let lines: Vec<&str> = section_content.lines().collect();
            let mut insert_line = lines.len();
            for (index, line) in lines.iter().enumerate() {
                if line.trim().starts_with('-') {
                    let existing = line.trim().trim_start_matches("- ").trim();
                    if existing > link.as_str() {
                        insert_line = index;
                        break;
                    }
                }
            }

            let mut byte_pos = after_header;
            for (index, line) in lines.iter().enumerate() {
                if index >= insert_line {
                    break;
                }
                byte_pos += line.len() + 1;
            }

            let byte_pos = next_char_boundary(&text, byte_pos);
            text.insert_str(byte_pos, &format!("\n- {} -- (auto-indexed)", link));
        } else {
            text.push_str(&format!(
                "\n\n## {}\n\n- {} -- (auto-indexed)\n",
                section, link
            ));
        }

        fs::write(&index_path, text)
            .with_context(|| format!("failed to write {}", index_path.display()))?;
        Ok(())
    }
}

fn log_entry_insert_offset(text: &str) -> Option<usize> {
    let mut offset = 0;
    for segment in text.split_inclusive('\n') {
        if segment.trim_start().starts_with("## [") {
            return Some(offset);
        }
        offset += segment.len();
    }
    None
}

fn next_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn sanitize_rel_path(input: &str) -> Result<PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("path is required"));
    }

    let raw = Path::new(trimmed);
    if raw.is_absolute() {
        return Err(anyhow!("absolute path not allowed: {}", trimmed));
    }

    let mut out = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir => return Err(anyhow!("path traversal not allowed: {}", trimmed)),
            _ => return Err(anyhow!("invalid path component in {}", trimmed)),
        }
    }

    if out.as_os_str().is_empty() {
        return Err(anyhow!("path is required"));
    }

    Ok(out)
}

pub(crate) enum FrontmatterValue {
    Missing,
    Invalid(String),
    Valid(serde_yaml::Value),
}

pub(crate) fn parse_frontmatter(text: &str) -> (Option<Frontmatter>, String) {
    let normalized = text.replace("\r\n", "\n");
    let mut lines = normalized.lines();
    if lines.next() != Some("---") {
        return (None, normalized);
    }

    let mut yaml_lines = Vec::new();
    let mut found_end = false;

    for line in lines.by_ref() {
        if line.trim() == "---" {
            found_end = true;
            break;
        }
        yaml_lines.push(line);
    }

    if !found_end {
        return (None, normalized);
    }

    let content = lines.collect::<Vec<&str>>().join("\n");
    match serde_yaml::from_str::<Frontmatter>(&yaml_lines.join("\n")) {
        Ok(frontmatter) => (Some(frontmatter), content),
        Err(_) => (None, normalized),
    }
}

pub(crate) fn parse_frontmatter_value(text: &str) -> FrontmatterValue {
    let normalized = text.replace("\r\n", "\n");
    let mut lines = normalized.lines();
    if lines.next() != Some("---") {
        return FrontmatterValue::Missing;
    }

    let mut yaml_lines = Vec::new();
    let mut found_end = false;
    for line in lines.by_ref() {
        if line.trim() == "---" {
            found_end = true;
            break;
        }
        yaml_lines.push(line);
    }

    if !found_end {
        return FrontmatterValue::Invalid("unterminated frontmatter".to_string());
    }

    match serde_yaml::from_str::<serde_yaml::Value>(&yaml_lines.join("\n")) {
        Ok(value) => FrontmatterValue::Valid(value),
        Err(err) => FrontmatterValue::Invalid(err.to_string()),
    }
}

pub(crate) fn validate_frontmatter_schema(path: &str, value: &serde_yaml::Value) -> Vec<String> {
    let mut issues = Vec::new();
    let Some(map) = value.as_mapping() else {
        return vec!["frontmatter must be a mapping".to_string()];
    };

    let page_type = yaml_string_field(value, "type");
    let Some(page_type) = page_type else {
        issues.push("missing field: type".to_string());
        return issues;
    };

    if !known_page_type(&page_type) {
        issues.push(format!("unknown type: {}", page_type));
    }

    let expected_type = expected_type_for_path(path);
    if let Some(expected) = expected_type {
        if page_type != expected {
            issues.push(format!(
                "wrong type: expected '{}', got '{}'",
                expected, page_type
            ));
        }
    }

    for field in required_fields(&page_type) {
        if !mapping_contains_key(map, field) {
            issues.push(format!("missing field: {}", field));
        }
    }

    for (field, expected) in [
        ("title", "string"),
        ("type", "string"),
        ("created", "date"),
        ("updated", "date"),
        ("tags", "list"),
        ("source_url", "string"),
        ("version", "number"),
    ] {
        if mapping_contains_key(map, field) && !field_has_type(value, field, expected) {
            issues.push(format!("wrong field type: {} expected {}", field, expected));
        }
    }

    let allowed = allowed_fields(&page_type);
    for key in map.keys().filter_map(|key| key.as_str()) {
        if !allowed.contains(&key) {
            issues.push(format!("unknown field: {}", key));
        }
    }

    issues.sort();
    issues.dedup();
    issues
}

fn mapping_contains_key(map: &serde_yaml::Mapping, field: &str) -> bool {
    map.contains_key(serde_yaml::Value::String(field.to_string()))
}

pub(crate) fn yaml_string_field(value: &serde_yaml::Value, field: &str) -> Option<String> {
    value
        .as_mapping()?
        .get(serde_yaml::Value::String(field.to_string()))?
        .as_str()
        .map(str::to_string)
}

fn field_has_type(value: &serde_yaml::Value, field: &str, expected: &str) -> bool {
    let Some(field_value) = value
        .as_mapping()
        .and_then(|map| map.get(serde_yaml::Value::String(field.to_string())))
    else {
        return true;
    };

    match expected {
        "string" => field_value.as_str().is_some(),
        "date" => field_value.as_str().map(is_iso_date).unwrap_or(false),
        "list" => field_value.as_sequence().is_some(),
        "number" => field_value.as_i64().is_some() || field_value.as_u64().is_some(),
        _ => true,
    }
}

pub(crate) fn is_iso_date(value: &str) -> bool {
    value.len() == 10
        && value.as_bytes().get(4) == Some(&b'-')
        && value.as_bytes().get(7) == Some(&b'-')
        && chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
}

fn known_page_type(page_type: &str) -> bool {
    matches!(
        page_type,
        "entity" | "concept" | "source" | "analysis" | "index" | "log" | "schema" | "skill"
    )
}

fn required_fields(page_type: &str) -> &'static [&'static str] {
    match page_type {
        "entity" | "concept" | "source" | "analysis" => {
            &["title", "type", "created", "updated", "tags"]
        }
        "index" | "log" | "schema" | "skill" => &["title", "type"],
        _ => &["title", "type"],
    }
}

fn allowed_fields(page_type: &str) -> &'static [&'static str] {
    match page_type {
        "source" => &["title", "type", "created", "updated", "tags", "source_url"],
        "entity" | "concept" | "analysis" => &["title", "type", "created", "updated", "tags"],
        "schema" => &["title", "type", "version"],
        "index" | "log" | "skill" => &["title", "type"],
        _ => &["title", "type", "created", "updated", "tags"],
    }
}

pub(crate) fn expected_type_for_path(path: &str) -> Option<&'static str> {
    match path {
        "SCHEMA.md" => Some("schema"),
        "SKILL.md" => Some("skill"),
        "wiki/index.md" => Some("index"),
        "wiki/log.md" => Some("log"),
        _ if path.starts_with("wiki/entities/") && path.ends_with(".md") => Some("entity"),
        _ if path.starts_with("wiki/concepts/") && path.ends_with(".md") => Some("concept"),
        _ if path.starts_with("wiki/sources/") && path.ends_with(".md") => Some("source"),
        _ if path.starts_with("wiki/analyses/") && path.ends_with(".md") => Some("analysis"),
        _ => None,
    }
}

pub(crate) fn is_content_page(path: &str) -> bool {
    matches!(
        expected_type_for_path(path),
        Some("entity" | "concept" | "source" | "analysis")
    )
}

pub(crate) fn is_lint_scope_file(path: &str) -> bool {
    expected_type_for_path(path).is_some()
}

pub fn template_for_path(path: &str) -> String {
    match path {
        "SCHEMA.md" => "---\ntitle: Wiki Schema\ntype: schema\nversion: 1\n---\n\n# Wiki Schema\n\n<!-- describe the wiki conventions here -->\n".to_string(),
        "SKILL.md" => r#"---
title: Wiki Skill
type: skill
---

# Wiki Skill

See wiki_help for workflow.
"#.to_string(),
        "wiki/index.md" => "---\ntitle: Index\ntype: index\n---\n\n# Wiki Index\n".to_string(),
        "wiki/log.md" => "---\ntitle: Log\ntype: log\n---\n\n# Wiki Log\n".to_string(),
        _ => default_frontmatter_for_path(path),
    }
}

pub(crate) fn default_frontmatter_for_path(path: &str) -> String {
    let title = title_from_path(path);
    let page_type = expected_type_for_path(path).unwrap_or("entity");
    if matches!(page_type, "entity" | "concept" | "source" | "analysis") {
        let today = Utc::now().format("%Y-%m-%d");
        format!(
            "---\ntitle: {}\ntype: {}\ncreated: {}\nupdated: {}\ntags: []\n---\n\n",
            title, page_type, today, today
        )
    } else {
        format!("---\ntitle: {}\ntype: {}\n---\n\n", title, page_type)
    }
}

pub(crate) fn title_from_path(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().replace(['-', '_'], " "))
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "Untitled".to_string())
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

pub(crate) fn strip_markdown_code(text: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;

    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push('\n');
            continue;
        }
        if in_fence {
            out.push('\n');
            continue;
        }
        out.push_str(&strip_inline_code(line));
        out.push('\n');
    }

    out
}

fn strip_inline_code(line: &str) -> String {
    let mut out = String::new();
    let mut in_code = false;
    for ch in line.chars() {
        if ch == '`' {
            in_code = !in_code;
            out.push(' ');
        } else if in_code {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

fn update_frontmatter_updated(text: &str) -> String {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    if UPDATED_RE.is_match(text) {
        UPDATED_RE
            .replace(text, format!("updated: {}", today).as_str())
            .to_string()
    } else {
        text.to_string()
    }
}

pub(crate) fn extract_wikilinks(text: &str) -> Vec<String> {
    WIKILINK_RE
        .captures_iter(text)
        .filter_map(|caps| caps.get(1).map(|value| value.as_str().to_string()))
        .collect()
}

pub(crate) fn build_link_index<'a>(
    paths: impl Iterator<Item = &'a String>,
) -> HashMap<String, String> {
    let mut index = HashMap::new();

    for rel in paths {
        let rel_value = rel.clone();
        let rel_lower = rel_value.to_lowercase();
        index.entry(rel_lower).or_insert_with(|| rel_value.clone());

        if let Some(no_ext) = rel_value.strip_suffix(".md") {
            index
                .entry(no_ext.to_lowercase())
                .or_insert_with(|| rel_value.clone());
        }

        let stem = Path::new(&rel_value)
            .file_stem()
            .map(|value| value.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !stem.is_empty() {
            index.entry(stem).or_insert_with(|| rel_value.clone());
        }
    }

    index
}

pub(crate) fn normalize_link_key(link: &str) -> Option<String> {
    let clean = link.trim();
    if clean.is_empty() {
        return None;
    }

    let base = clean
        .split('|')
        .next()
        .unwrap_or(clean)
        .split('#')
        .next()
        .unwrap_or(clean)
        .trim();

    if base.is_empty() {
        return None;
    }

    let no_ext = base.strip_suffix(".md").unwrap_or(base);
    Some(no_ext.to_lowercase())
}

pub(crate) fn resolve_link(link: &str, link_index: &HashMap<String, String>) -> Option<String> {
    let key = normalize_link_key(link)?;
    link_index.get(&key).cloned()
}

fn detect_fd_program() -> Option<&'static str> {
    ["fd", "fdfind"]
        .into_iter()
        .find(|name| command_exists(name))
}

fn command_exists(program: &str) -> bool {
    StdCommand::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

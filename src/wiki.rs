use crate::config::{AppConfig, SearchBackend};
use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
static LOG_DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\d{4}-\d{2}-\d{2}\]").expect("valid log date regex"));

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokenLink {
    pub source: String,
    pub link: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintReport {
    pub stale_logs: Vec<String>,
    pub orphan_pages: Vec<String>,
    pub broken_links: Vec<BrokenLink>,
    pub missing_pages: Vec<String>,
    pub duplicate_titles: Vec<String>,
    pub missing_frontmatter: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WikiOps {
    config: AppConfig,
}

impl WikiOps {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(&self.config.vault_path)
    }

    fn resolve_rel_path(&self, rel_path: &str) -> Result<PathBuf> {
        let cleaned = sanitize_rel_path(rel_path)?;
        Ok(self.root().join(cleaned))
    }

    fn to_rel_path(&self, full: &Path) -> String {
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
        let entry = format!("\n## [{}] {} | {}\n", date, action, description);
        text.push_str(&entry);
        fs::write(&log_path, text)
            .with_context(|| format!("failed to write {}", log_path.display()))?;
        Ok(())
    }

    pub fn lint(&self) -> Result<LintReport> {
        let root = self.root();
        let mut pages: HashMap<String, String> = HashMap::new();
        let mut outbound: HashMap<String, Vec<String>> = HashMap::new();
        let mut titles: HashMap<String, Vec<String>> = HashMap::new();

        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_type().is_file() && entry.file_name().to_string_lossy().ends_with(".md")
            })
        {
            let rel = self.to_rel_path(entry.path());
            let text = match fs::read_to_string(entry.path()) {
                Ok(text) => text,
                Err(_) => continue,
            };

            let links = extract_wikilinks(&text);
            let (fm, _) = parse_frontmatter(&text);
            if let Some(fm) = fm {
                titles.entry(fm.title).or_default().push(rel.clone());
            }

            pages.insert(rel.clone(), text.clone());
            outbound.insert(rel, links);
        }

        let link_index = build_link_index(pages.keys());

        let mut inbound: HashMap<String, HashSet<String>> = HashMap::new();
        for (source, links) in &outbound {
            for link in links {
                if let Some(target) = resolve_link(link, &link_index) {
                    inbound.entry(target).or_default().insert(source.clone());
                }
            }
        }

        let mut orphan_pages = Vec::new();
        let mut broken_links = Vec::new();
        let mut missing_pages: HashSet<String> = HashSet::new();
        let mut missing_frontmatter = Vec::new();

        for (source, links) in &outbound {
            if !inbound.contains_key(source)
                && !source.ends_with("index.md")
                && !source.ends_with("log.md")
            {
                orphan_pages.push(source.clone());
            }

            for link in links {
                if resolve_link(link, &link_index).is_none() {
                    let clean = link.trim();
                    if !clean.is_empty() {
                        broken_links.push(BrokenLink {
                            source: source.clone(),
                            link: clean.to_string(),
                        });
                        missing_pages.insert(clean.to_string());
                    }
                }
            }

            let source_text = pages.get(source).cloned().unwrap_or_default();
            if parse_frontmatter(&source_text).0.is_none() {
                missing_frontmatter.push(source.clone());
            }
        }

        let log_text = pages.get("wiki/log.md").cloned().unwrap_or_default();
        let last_log_date =
            extract_last_log_date(&log_text).unwrap_or_else(|| "1970-01-01".to_string());

        let mut stale_logs = Vec::new();
        for (rel, text) in &pages {
            if let Some(fm) = parse_frontmatter(text).0 {
                if fm.updated > last_log_date {
                    stale_logs.push(format!("{} (updated: {})", rel, fm.updated));
                }
            }
        }

        let duplicate_titles: Vec<String> = titles
            .values()
            .filter(|entries| entries.len() > 1)
            .flat_map(|entries| entries.iter().cloned())
            .collect();

        let mut missing_pages: Vec<String> = missing_pages.into_iter().collect();
        missing_pages.sort();
        orphan_pages.sort();
        broken_links.sort_by(|a, b| a.source.cmp(&b.source).then(a.link.cmp(&b.link)));
        stale_logs.sort();
        missing_frontmatter.sort();

        Ok(LintReport {
            stale_logs,
            orphan_pages,
            broken_links,
            missing_pages,
            duplicate_titles,
            missing_frontmatter,
        })
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

fn parse_frontmatter(text: &str) -> (Option<Frontmatter>, String) {
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

fn extract_wikilinks(text: &str) -> Vec<String> {
    WIKILINK_RE
        .captures_iter(text)
        .filter_map(|caps| caps.get(1).map(|value| value.as_str().to_string()))
        .collect()
}

fn build_link_index<'a>(paths: impl Iterator<Item = &'a String>) -> HashMap<String, String> {
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

fn normalize_link_key(link: &str) -> Option<String> {
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

fn resolve_link(link: &str, link_index: &HashMap<String, String>) -> Option<String> {
    let key = normalize_link_key(link)?;
    link_index.get(&key).cloned()
}

fn extract_last_log_date(text: &str) -> Option<String> {
    let mut dates: Vec<String> = LOG_DATE_RE
        .find_iter(text)
        .map(|item| {
            item.as_str()
                .trim_start_matches('[')
                .trim_end_matches(']')
                .to_string()
        })
        .collect();

    dates.sort();
    dates.last().cloned()
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

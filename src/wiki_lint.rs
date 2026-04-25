use crate::wiki::{
    build_link_index, default_frontmatter_for_path, expected_type_for_path, extract_wikilinks,
    is_content_page, is_iso_date, is_lint_scope_file, normalize_link_key, parse_frontmatter,
    parse_frontmatter_value, resolve_link, sha256_hex, strip_markdown_code,
    template_for_path_with_vault, title_from_path, validate_frontmatter_schema, yaml_string_field,
    FrontmatterValue, WikiOps,
};
use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::sync::LazyLock;
use walkdir::WalkDir;

static LOG_ENTRY_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^## \[(\d{4}-\d{2}-\d{2})\] (create|update|delete|rename|move|link|unlink|sync) \| (.+)$",
    )
    .expect("valid log entry regex")
});
static LOG_ENTRY_LOOSE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^## \[(\d{4}-\d{2}-\d{2})\] ([^|]+) \| (.*)$")
        .expect("valid loose log entry regex")
});
static LOG_ENTRY_PAD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^## \[(\d{4})-(\d{1,2})-(\d{1,2})\](.*)$").expect("valid log date padding regex")
});
static LOG_ENTRY_MISSING_SPACE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^## \[(\d{4}-\d{2}-\d{2})\](\S.*)$").expect("valid log missing-space regex")
});

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokenLink {
    pub source: String,
    pub link: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingStructure {
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvalidFrontmatter {
    pub path: String,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BadFrontmatter {
    pub path: String,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MisplacedPage {
    pub path: String,
    pub declared_type: String,
    pub expected_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentDrift {
    pub path: String,
    pub expected_sha256: String,
    pub actual_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnindexedPage {
    pub path: String,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateIndexEntry {
    pub slug: String,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingIndexSection {
    pub page_type: String,
    pub section: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StaleIndexMixedBullet {
    pub source: String,
    pub line: usize,
    pub valid: Vec<String>,
    pub stale: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DuplicateIndexSection {
    pub section: String,
    pub occurrences: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogIssue {
    pub line: usize,
    pub entry: String,
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogOrderIssue {
    pub line: usize,
    pub entry: String,
    pub issue: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignLogContent {
    pub line: usize,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForeignIndexContent {
    pub line: usize,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintFix {
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct LintOptions {
    pub fix: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintReport {
    pub missing_structure: Vec<MissingStructure>,
    pub missing_frontmatter: Vec<String>,
    pub invalid_frontmatter: Vec<InvalidFrontmatter>,
    pub bad_frontmatter: Vec<BadFrontmatter>,
    pub misplaced_pages: Vec<MisplacedPage>,
    pub content_drift: Vec<ContentDrift>,
    pub broken_links: Vec<BrokenLink>,
    pub duplicate_titles: Vec<String>,
    pub orphan_pages: Vec<String>,
    pub stale_logs: Vec<String>,
    pub unindexed_pages: Vec<UnindexedPage>,
    pub duplicate_index_entries: Vec<DuplicateIndexEntry>,
    pub missing_index_sections: Vec<MissingIndexSection>,
    pub stale_index_mixed_bullet: Vec<StaleIndexMixedBullet>,
    pub duplicate_index_sections: Vec<DuplicateIndexSection>,
    pub foreign_index_content: Vec<ForeignIndexContent>,
    pub foreign_log_content: Vec<ForeignLogContent>,
    pub malformed_log_entries: Vec<LogIssue>,
    pub out_of_order_log_entries: Vec<LogOrderIssue>,
    pub duplicate_log_entries: Vec<String>,
    pub log_entry_count: usize,
    pub formatting_issues: Vec<LintFix>,
    pub fixes_applied: Vec<LintFix>,
}

impl WikiOps {
    pub fn lint(&self) -> Result<LintReport> {
        self.lint_with_options(LintOptions::default())
    }

    pub fn lint_with_options(&self, options: LintOptions) -> Result<LintReport> {
        let mut fixes_applied = Vec::new();
        if options.fix {
            fixes_applied.extend(self.apply_lint_fixes(options.dry_run)?);
        }

        let root = self.root();
        let mut pages: HashMap<String, String> = HashMap::new();
        let mut outbound: HashMap<String, Vec<String>> = HashMap::new();
        let mut titles: HashMap<String, Vec<String>> = HashMap::new();
        let mut missing_frontmatter = Vec::new();
        let mut invalid_frontmatter = Vec::new();
        let mut bad_frontmatter = Vec::new();
        let mut misplaced_pages = Vec::new();
        let mut content_drift = Vec::new();

        let mut missing_structure = expected_structure()
            .iter()
            .filter_map(|item| {
                let full = root.join(item.path);
                let missing = match item.kind {
                    "file" => !full.is_file(),
                    "dir" => !full.is_dir(),
                    _ => false,
                };
                missing.then(|| MissingStructure {
                    path: item.path.to_string(),
                    kind: item.kind.to_string(),
                })
            })
            .collect::<Vec<_>>();

        for item in expected_structure()
            .iter()
            .filter(|item| item.locked && item.kind == "file")
        {
            let full = root.join(item.path);
            if !full.is_file() {
                continue;
            }
            let actual = fs::read_to_string(&full).unwrap_or_default();
            let vault_path = root.to_string_lossy();
            let expected = template_for_path_with_vault(item.path, vault_path.as_ref());
            if actual != expected {
                content_drift.push(ContentDrift {
                    path: item.path.to_string(),
                    expected_sha256: sha256_hex(expected.as_bytes()),
                    actual_sha256: sha256_hex(actual.as_bytes()),
                });
            }
        }

        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_type().is_file() && entry.file_name().to_string_lossy().ends_with(".md")
            })
        {
            let rel = self.to_rel_path(entry.path());
            if !is_lint_scope_file(&rel) {
                continue;
            }
            let text = match fs::read_to_string(entry.path()) {
                Ok(text) => text,
                Err(_) => continue,
            };

            let links = extract_wikilinks(&strip_markdown_code(&text));
            match parse_frontmatter_value(&text) {
                FrontmatterValue::Missing => missing_frontmatter.push(rel.clone()),
                FrontmatterValue::Invalid(error) => invalid_frontmatter.push(InvalidFrontmatter {
                    path: rel.clone(),
                    error,
                }),
                FrontmatterValue::Valid(value) => {
                    if let Some(title) = yaml_string_field(&value, "title") {
                        titles.entry(title).or_default().push(rel.clone());
                    }

                    let issues = validate_frontmatter_schema(&rel, &value);
                    if !issues.is_empty() {
                        bad_frontmatter.push(BadFrontmatter {
                            path: rel.clone(),
                            issues,
                        });
                    }

                    if let (Some(expected_type), Some(declared_type)) = (
                        expected_type_for_path(&rel),
                        yaml_string_field(&value, "type"),
                    ) {
                        if declared_type != expected_type {
                            misplaced_pages.push(MisplacedPage {
                                path: rel.clone(),
                                declared_type,
                                expected_type: expected_type.to_string(),
                            });
                        }
                    }
                }
            }

            pages.insert(rel.clone(), text);
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
        for (source, links) in &outbound {
            if is_content_page(source) && !inbound.contains_key(source) {
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
                    }
                }
            }
        }

        let index_report = validate_index(&pages, &link_index);
        let mut foreign_index_content =
            validate_index_grammar(&pages.get("wiki/index.md").cloned().unwrap_or_default());

        let log_text = pages.get("wiki/log.md").cloned().unwrap_or_default();
        let log_validation = validate_log_entries(&log_text);
        let last_log_date = log_validation
            .latest_date
            .clone()
            .unwrap_or_else(|| "1970-01-01".to_string());

        let mut stale_logs = Vec::new();
        for (rel, text) in &pages {
            if let Some(fm) = parse_frontmatter(text).0 {
                if fm.updated > last_log_date {
                    stale_logs.push(format!("{} (updated: {})", rel, fm.updated));
                }
            }
        }

        let mut duplicate_titles: Vec<String> = titles
            .values()
            .filter(|entries| entries.len() > 1)
            .flat_map(|entries| entries.iter().cloned())
            .collect();

        missing_structure.sort_by(|a, b| a.path.cmp(&b.path));
        missing_frontmatter.sort();
        invalid_frontmatter.sort_by(|a, b| a.path.cmp(&b.path));
        bad_frontmatter.sort_by(|a, b| a.path.cmp(&b.path));
        misplaced_pages.sort_by(|a, b| a.path.cmp(&b.path));
        content_drift.sort_by(|a, b| a.path.cmp(&b.path));
        orphan_pages.sort();
        broken_links.sort_by(|a, b| a.source.cmp(&b.source).then(a.link.cmp(&b.link)));
        duplicate_titles.sort();
        stale_logs.sort();
        let mut unindexed_pages = index_report.unindexed_pages;
        let mut duplicate_index_entries = index_report.duplicate_index_entries;
        let mut missing_index_sections = index_report.missing_index_sections;
        let mut stale_index_mixed_bullet = index_report.stale_index_mixed_bullet;
        let mut duplicate_index_sections = index_report.duplicate_index_sections;
        unindexed_pages.sort_by(|a, b| a.path.cmp(&b.path));
        duplicate_index_entries.sort_by(|a, b| a.slug.cmp(&b.slug));
        missing_index_sections.sort_by(|a, b| a.section.cmp(&b.section));
        stale_index_mixed_bullet.sort_by_key(|item| item.line);
        duplicate_index_sections.sort_by(|a, b| a.section.cmp(&b.section));
        foreign_index_content.sort_by_key(|item| item.line);
        let mut foreign_log_content = log_validation.foreign_content;
        let mut malformed_log_entries = log_validation.malformed_entries;
        let mut out_of_order_log_entries = log_validation.out_of_order_entries;
        let mut duplicate_log_entries = log_validation.duplicate_entries;
        let log_entry_count = log_validation.entry_count;
        let mut formatting_issues = detect_formatting_issues(&pages);
        foreign_log_content.sort_by_key(|item| item.line);
        malformed_log_entries.sort_by_key(|item| item.line);
        out_of_order_log_entries.sort_by_key(|item| item.line);
        duplicate_log_entries.sort();
        formatting_issues.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));
        fixes_applied.sort_by(|a, b| a.path.cmp(&b.path).then(a.kind.cmp(&b.kind)));

        Ok(LintReport {
            missing_structure,
            missing_frontmatter,
            invalid_frontmatter,
            bad_frontmatter,
            misplaced_pages,
            content_drift,
            broken_links,
            duplicate_titles,
            orphan_pages,
            stale_logs,
            unindexed_pages,
            duplicate_index_entries,
            missing_index_sections,
            stale_index_mixed_bullet,
            duplicate_index_sections,
            foreign_index_content,
            foreign_log_content,
            malformed_log_entries,
            out_of_order_log_entries,
            duplicate_log_entries,
            log_entry_count,
            formatting_issues,
            fixes_applied,
        })
    }

    fn apply_lint_fixes(&self, dry_run: bool) -> Result<Vec<LintFix>> {
        let root = self.root();
        let mut fixes = Vec::new();

        for item in expected_structure() {
            let full = root.join(item.path);
            match item.kind {
                "dir" if !full.is_dir() => {
                    if !dry_run {
                        fs::create_dir_all(&full)
                            .with_context(|| format!("failed to create {}", full.display()))?;
                    }
                    fixes.push(LintFix {
                        path: item.path.to_string(),
                        kind: "create_dir".to_string(),
                    });
                }
                "file" if !full.is_file() => {
                    if let Some(parent) = full.parent() {
                        if !dry_run {
                            fs::create_dir_all(parent).with_context(|| {
                                format!("failed to create {}", parent.display())
                            })?;
                        }
                    }
                    if !dry_run {
                        let vault_path = root.to_string_lossy();
                        fs::write(
                            &full,
                            template_for_path_with_vault(item.path, vault_path.as_ref()),
                        )
                        .with_context(|| format!("failed to write {}", full.display()))?;
                    }
                    fixes.push(LintFix {
                        path: item.path.to_string(),
                        kind: "create_file".to_string(),
                    });
                }
                _ => {}
            }
        }

        for item in expected_structure()
            .iter()
            .filter(|item| item.locked && item.kind == "file")
        {
            let full = root.join(item.path);
            if !full.is_file() {
                continue;
            }
            let actual = fs::read_to_string(&full).unwrap_or_default();
            let vault_path = root.to_string_lossy();
            let expected = template_for_path_with_vault(item.path, vault_path.as_ref());
            if actual != expected {
                if !dry_run {
                    fs::write(&full, expected)
                        .with_context(|| format!("failed to write {}", full.display()))?;
                }
                fixes.push(LintFix {
                    path: item.path.to_string(),
                    kind: "restore_locked".to_string(),
                });
            }
        }

        let log_path = root.join("wiki/log.md");
        if log_path.is_file() {
            let text = fs::read_to_string(&log_path).unwrap_or_default();
            let (updated, log_fixes) = fix_log_text(&text);
            if !log_fixes.is_empty() {
                if !dry_run {
                    fs::write(&log_path, updated)
                        .with_context(|| format!("failed to write {}", log_path.display()))?;
                }
                fixes.extend(log_fixes.into_iter().map(|kind| LintFix {
                    path: "wiki/log.md".to_string(),
                    kind,
                }));
            }
        }

        fixes.extend(self.apply_page_format_fixes(dry_run)?);

        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_type().is_file() && entry.file_name().to_string_lossy().ends_with(".md")
            })
        {
            let rel = self.to_rel_path(entry.path());
            if !is_lint_scope_file(&rel) {
                continue;
            }
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            if !matches!(parse_frontmatter_value(&text), FrontmatterValue::Missing) {
                continue;
            }

            let frontmatter = default_frontmatter_for_path(&rel);
            if !dry_run {
                fs::write(entry.path(), format!("{}{}", frontmatter, text))
                    .with_context(|| format!("failed to write {}", entry.path().display()))?;
            }
            fixes.push(LintFix {
                path: rel,
                kind: "prepend_frontmatter".to_string(),
            });
        }

        fixes.extend(self.apply_index_fixes(dry_run)?);

        Ok(fixes)
    }

    fn apply_page_format_fixes(&self, dry_run: bool) -> Result<Vec<LintFix>> {
        let root = self.root();
        let mut fixes = Vec::new();

        for entry in WalkDir::new(&root)
            .into_iter()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_type().is_file() && entry.file_name().to_string_lossy().ends_with(".md")
            })
        {
            let rel = self.to_rel_path(entry.path());
            if !is_page_format_scope(&rel) {
                continue;
            }
            let text = fs::read_to_string(entry.path()).unwrap_or_default();
            let (updated, page_fixes) = fix_page_whitespace_text(&text);
            if page_fixes.is_empty() {
                continue;
            }
            if !dry_run {
                fs::write(entry.path(), updated)
                    .with_context(|| format!("failed to write {}", entry.path().display()))?;
            }
            fixes.extend(page_fixes.into_iter().map(|kind| LintFix {
                path: rel.clone(),
                kind,
            }));
        }

        Ok(fixes)
    }

    fn apply_index_fixes(&self, dry_run: bool) -> Result<Vec<LintFix>> {
        let root = self.root();
        let index_path = root.join("wiki/index.md");
        if !index_path.is_file() {
            return Ok(Vec::new());
        }

        let mut text = fs::read_to_string(&index_path)
            .with_context(|| format!("failed to read {}", index_path.display()))?;
        let mut fixes = Vec::new();

        if let Some(updated) = fix_index_yank_foreign_text(&text) {
            text = updated;
            fixes.push(LintFix {
                path: "wiki/index.md".to_string(),
                kind: "fix_index_yank_foreign".to_string(),
            });
        }

        let pages = collect_indexable_pages(&root, |path| self.to_rel_path(path))?;
        let page_set: HashSet<String> = pages.iter().map(|page| page.path.clone()).collect();
        let link_index = build_link_index(page_set.iter());

        let stale_analysis = analyze_index_stale_bullets(&text, &link_index);
        if !stale_analysis.remove_lines.is_empty() {
            text = remove_lines_by_number(&text, &stale_analysis.remove_lines);
            fixes.push(LintFix {
                path: "wiki/index.md".to_string(),
                kind: "fix_index_remove_stale".to_string(),
            });
        }

        let missing_sections = missing_sections_for_pages(&text, &pages);
        if !missing_sections.is_empty() {
            for section in &missing_sections {
                text.push_str(&format!("\n\n## {}\n", section.section));
            }
            fixes.push(LintFix {
                path: "wiki/index.md".to_string(),
                kind: "fix_index_add_section".to_string(),
            });
        }

        let link_index = build_link_index(page_set.iter());
        let indexed_targets = index_resolved_targets(&text, &link_index);
        let duplicate_sections = duplicate_index_sections(&text);
        let duplicate_section_names: HashSet<String> = duplicate_sections
            .iter()
            .map(|section| section.section.trim_start_matches("## ").to_string())
            .collect();
        let missing_pages: Vec<IndexablePage> = pages
            .into_iter()
            .filter(|page| !indexed_targets.contains(&page.path))
            .filter(|page| {
                !duplicate_section_names.contains(section_for_page_type(&page.page_type))
            })
            .collect();
        if !missing_pages.is_empty() {
            text = insert_missing_index_entries(&text, &missing_pages);
            fixes.push(LintFix {
                path: "wiki/index.md".to_string(),
                kind: "fix_index_insert_missing".to_string(),
            });
        }

        if let Some(updated) = fix_index_compact_whitespace_text(&text) {
            text = updated;
            fixes.push(LintFix {
                path: "wiki/index.md".to_string(),
                kind: "fix_index_compact_whitespace".to_string(),
            });
        }

        if !fixes.is_empty() && !dry_run {
            fs::write(&index_path, text)
                .with_context(|| format!("failed to write {}", index_path.display()))?;
        }

        Ok(fixes)
    }
}

#[derive(Debug, Clone)]
struct IndexReport {
    unindexed_pages: Vec<UnindexedPage>,
    duplicate_index_entries: Vec<DuplicateIndexEntry>,
    missing_index_sections: Vec<MissingIndexSection>,
    stale_index_mixed_bullet: Vec<StaleIndexMixedBullet>,
    duplicate_index_sections: Vec<DuplicateIndexSection>,
}

#[derive(Debug, Clone)]
struct IndexablePage {
    path: String,
    slug: String,
    title: String,
    page_type: String,
}

fn validate_index(
    pages: &HashMap<String, String>,
    link_index: &HashMap<String, String>,
) -> IndexReport {
    let raw_index_text = pages.get("wiki/index.md").cloned().unwrap_or_default();
    let index_text = fix_index_yank_foreign_text(&raw_index_text).unwrap_or(raw_index_text);
    let indexed_targets = index_resolved_targets(&index_text, link_index);
    let target_counts = primary_index_target_counts(&index_text, link_index);

    let content_pages = pages
        .iter()
        .filter_map(|(path, text)| indexable_page_from_text(path, text))
        .collect::<Vec<_>>();

    let unindexed_pages = content_pages
        .iter()
        .filter(|page| !indexed_targets.contains(&page.path))
        .map(|page| UnindexedPage {
            path: page.path.clone(),
            slug: page.slug.clone(),
        })
        .collect();

    let duplicate_index_entries = target_counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(target, count)| DuplicateIndexEntry {
            slug: slug_for_path(&target),
            count,
        })
        .collect();

    let missing_index_sections = missing_sections_for_pages(&index_text, &content_pages);
    let stale_index_mixed_bullet = analyze_index_stale_bullets(&index_text, link_index).mixed;
    let duplicate_index_sections = duplicate_index_sections(&index_text);

    IndexReport {
        unindexed_pages,
        duplicate_index_entries,
        missing_index_sections,
        stale_index_mixed_bullet,
        duplicate_index_sections,
    }
}

fn collect_indexable_pages(
    root: &Path,
    to_rel: impl Fn(&Path) -> String,
) -> Result<Vec<IndexablePage>> {
    let mut pages = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry.file_type().is_file() && entry.file_name().to_string_lossy().ends_with(".md")
        })
    {
        let rel = to_rel(entry.path());
        if !is_content_page(&rel) {
            continue;
        }
        let text = fs::read_to_string(entry.path())
            .with_context(|| format!("failed to read {}", entry.path().display()))?;
        if let Some(page) = indexable_page_from_text(&rel, &text) {
            pages.push(page);
        }
    }
    Ok(pages)
}

fn indexable_page_from_text(path: &str, text: &str) -> Option<IndexablePage> {
    let FrontmatterValue::Valid(value) = parse_frontmatter_value(text) else {
        return None;
    };
    let page_type = yaml_string_field(&value, "type")?;
    if !matches!(
        page_type.as_str(),
        "entity" | "concept" | "source" | "analysis"
    ) {
        return None;
    }
    Some(IndexablePage {
        path: path.to_string(),
        slug: slug_for_path(path),
        title: yaml_string_field(&value, "title").unwrap_or_else(|| title_from_path(path)),
        page_type,
    })
}

#[derive(Default)]
struct IndexGrammarState {
    seen_h1: bool,
    in_section: bool,
}

impl IndexGrammarState {
    fn accepts(&mut self, line: &str) -> bool {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return true;
        }
        if trimmed == "# Wiki Index" {
            if self.seen_h1 || self.in_section {
                return false;
            }
            self.seen_h1 = true;
            return true;
        }
        if matches!(
            trimmed,
            "## Entities" | "## Concepts" | "## Sources" | "## Analyses"
        ) {
            if !self.seen_h1 {
                return false;
            }
            self.in_section = true;
            return true;
        }
        if trimmed.starts_with("### ") && !trimmed.starts_with("####") {
            return self.in_section;
        }
        if trimmed.starts_with("- ") {
            return self.in_section
                && !contains_raw_markdown_link(trimmed)
                && !extract_wikilinks(trimmed).is_empty();
        }
        false
    }
}

fn validate_index_grammar(text: &str) -> Vec<ForeignIndexContent> {
    let mut foreign = Vec::new();
    let mut in_frontmatter = false;
    let mut state = IndexGrammarState::default();

    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line_number == 1 && line == "---" {
            in_frontmatter = true;
            continue;
        }
        if in_frontmatter {
            if line == "---" {
                in_frontmatter = false;
            }
            continue;
        }

        if state.accepts(line) {
            continue;
        }
        foreign.push(ForeignIndexContent {
            line: line_number,
            content: line.to_string(),
        });
    }

    foreign
}

fn detect_formatting_issues(pages: &HashMap<String, String>) -> Vec<LintFix> {
    let mut issues = Vec::new();
    let mut formatted_index = None;
    let mut formatted_log = None;

    for (path, text) in pages {
        if !is_page_format_scope(path) {
            continue;
        }
        let (updated, page_fixes) = fix_page_whitespace_text(text);
        if page_fixes.is_empty() {
            continue;
        }
        if path == "wiki/index.md" {
            formatted_index = Some(updated);
        } else if path == "wiki/log.md" {
            formatted_log = Some(updated);
        }
        issues.extend(page_fixes.into_iter().map(|kind| LintFix {
            path: path.clone(),
            kind,
        }));
    }

    let index_text = formatted_index
        .as_deref()
        .or_else(|| pages.get("wiki/index.md").map(String::as_str));
    if let Some(text) = index_text {
        if fix_index_compact_whitespace_text(text).is_some() {
            issues.push(LintFix {
                path: "wiki/index.md".to_string(),
                kind: "fix_index_compact_whitespace".to_string(),
            });
        }
    }

    let log_text = formatted_log
        .as_deref()
        .or_else(|| pages.get("wiki/log.md").map(String::as_str));
    if let Some(text) = log_text {
        let (_, fixes) = fix_log_text(text);
        issues.extend(
            fixes
                .into_iter()
                .filter(|kind| is_formatting_fix(kind))
                .map(|kind| LintFix {
                    path: "wiki/log.md".to_string(),
                    kind,
                }),
        );
    }

    issues
}

fn is_formatting_fix(kind: &str) -> bool {
    matches!(
        kind,
        "fix_log_action_case"
            | "fix_log_compact_whitespace"
            | "fix_log_date_padding"
            | "fix_log_space_after_date"
            | "fix_log_trailing_whitespace"
            | "fix_page_blank_line_whitespace"
            | "fix_page_final_newline"
            | "fix_page_leading_whitespace"
            | "fix_page_trailing_whitespace"
    )
}

fn is_locked_file(path: &str) -> bool {
    expected_structure()
        .iter()
        .any(|item| item.path == path && item.kind == "file" && item.locked)
}

fn is_page_format_scope(path: &str) -> bool {
    is_lint_scope_file(path) && !is_locked_file(path)
}

fn fix_page_whitespace_text(text: &str) -> (String, Vec<String>) {
    let mut current = text.to_string();
    let mut fixes = Vec::new();

    if let Some(updated) = fix_page_leading_whitespace_text(&current) {
        current = updated;
        fixes.push("fix_page_leading_whitespace".to_string());
    }
    if let Some(updated) = fix_page_trailing_whitespace_text(&current) {
        current = updated;
        fixes.push("fix_page_trailing_whitespace".to_string());
    }
    if let Some(updated) = fix_page_blank_line_whitespace_text(&current) {
        current = updated;
        fixes.push("fix_page_blank_line_whitespace".to_string());
    }
    if let Some(updated) = fix_page_final_newline_text(&current) {
        current = updated;
        fixes.push("fix_page_final_newline".to_string());
    }

    (current, fixes)
}

fn fix_page_leading_whitespace_text(text: &str) -> Option<String> {
    let updated = text.trim_start().to_string();
    (updated.len() != text.len()).then_some(updated)
}

fn fix_page_trailing_whitespace_text(text: &str) -> Option<String> {
    rewrite_page_lines_fence_aware(text, |line| {
        if line.trim().is_empty() {
            return None;
        }
        let trimmed = line.trim_end_matches([' ', '\t']);
        (trimmed != line).then(|| trimmed.to_string())
    })
}

fn fix_page_blank_line_whitespace_text(text: &str) -> Option<String> {
    rewrite_page_lines_fence_aware(text, |line| {
        (!line.is_empty() && line.trim().is_empty()).then(String::new)
    })
}

fn fix_page_final_newline_text(text: &str) -> Option<String> {
    (!text.ends_with('\n')).then(|| format!("{text}\n"))
}

fn rewrite_page_lines_fence_aware(
    text: &str,
    rewrite: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    let mut changed = false;
    let mut out = Vec::new();
    let mut fence: Option<&'static str> = None;

    for line in text.lines() {
        if let Some(marker) = fence_marker(line) {
            if fence == Some(marker) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(marker);
            }
            out.push(line.to_string());
            continue;
        }
        if fence.is_some() {
            out.push(line.to_string());
            continue;
        }
        if let Some(updated) = rewrite(line) {
            out.push(updated);
            changed = true;
        } else {
            out.push(line.to_string());
        }
    }

    changed.then(|| finish_rewrite(text, out))
}

fn fence_marker(line: &str) -> Option<&'static str> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some("```")
    } else if trimmed.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

fn fix_index_compact_whitespace_text(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0usize;
    let mut out = Vec::new();

    if lines.first() == Some(&"---") {
        while index < lines.len() {
            let line = lines[index].trim_end_matches([' ', '\t']);
            out.push(line.to_string());
            index += 1;
            if index > 1 && line == "---" {
                break;
            }
        }
    }

    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }

    if index >= lines.len() || !matches!(lines[index].trim(), "# Wiki Index" | "# Index") {
        return None;
    }
    index += 1;

    if !out.is_empty() {
        out.push(String::new());
    }
    out.push("# Wiki Index".to_string());

    for line in lines[index..]
        .iter()
        .map(|line| line.trim_end_matches([' ', '\t']))
        .filter(|line| !line.trim().is_empty())
    {
        let is_heading = line.starts_with("## ") || line.starts_with("### ");
        let previous_is_heading = out
            .iter()
            .rev()
            .find(|line| !line.is_empty())
            .map(|line| line.starts_with('#'))
            .unwrap_or(false);
        if (is_heading || previous_is_heading)
            && !out.last().map(|line| line.is_empty()).unwrap_or(false)
        {
            out.push(String::new());
        }
        out.push(line.to_string());
    }

    let mut updated = out.join("\n");
    updated.push('\n');
    (updated != text).then_some(updated)
}

fn fix_index_yank_foreign_text(text: &str) -> Option<String> {
    let mut changed = false;
    let mut out = Vec::new();
    let mut in_frontmatter = false;
    let mut state = IndexGrammarState::default();

    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line_number == 1 && line == "---" {
            in_frontmatter = true;
            out.push(line.to_string());
            continue;
        }
        if in_frontmatter {
            out.push(line.to_string());
            if line == "---" {
                in_frontmatter = false;
            }
            continue;
        }

        if line == "# Index" && state.accepts("# Wiki Index") {
            out.push("# Wiki Index".to_string());
            changed = true;
            continue;
        }
        if !state.seen_h1 && !line.trim().is_empty() && line != "# Wiki Index" {
            state.accepts("# Wiki Index");
            out.push("# Wiki Index".to_string());
            changed = true;
        }
        if state.accepts(line) {
            out.push(line.to_string());
        } else {
            changed = true;
        }
    }

    changed.then(|| finish_rewrite(text, out))
}

fn contains_raw_markdown_link(text: &str) -> bool {
    static RAW_MARKDOWN_LINK_RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\[[^\]]+\]\([^)]+\)").expect("valid regex"));
    RAW_MARKDOWN_LINK_RE.is_match(text)
}

fn primary_index_target_counts(
    text: &str,
    link_index: &HashMap<String, String>,
) -> HashMap<String, usize> {
    let mut target_counts = HashMap::new();
    for line in strip_markdown_code(text).lines() {
        if !line.trim_start().starts_with("- ") {
            continue;
        }
        let Some(link) = extract_wikilinks(line).into_iter().next() else {
            continue;
        };
        if let Some(target) = resolve_link(&link, link_index) {
            *target_counts.entry(target).or_insert(0) += 1;
        }
    }
    target_counts
}

fn index_resolved_targets(text: &str, link_index: &HashMap<String, String>) -> HashSet<String> {
    extract_wikilinks(&strip_markdown_code(text))
        .into_iter()
        .filter_map(|link| resolve_link(&link, link_index))
        .collect()
}

fn missing_sections_for_pages(text: &str, pages: &[IndexablePage]) -> Vec<MissingIndexSection> {
    let mut needed = HashSet::new();
    for page in pages {
        needed.insert(page.page_type.as_str());
    }
    let mut missing = Vec::new();
    for page_type in needed {
        let section = section_for_page_type(page_type);
        if !text
            .lines()
            .any(|line| line.trim() == format!("## {}", section))
        {
            missing.push(MissingIndexSection {
                page_type: page_type.to_string(),
                section: section.to_string(),
            });
        }
    }
    missing
}

struct IndexStaleAnalysis {
    remove_lines: HashSet<usize>,
    mixed: Vec<StaleIndexMixedBullet>,
}

fn analyze_index_stale_bullets(
    text: &str,
    link_index: &HashMap<String, String>,
) -> IndexStaleAnalysis {
    let mut remove_lines = HashSet::new();
    let mut mixed = Vec::new();

    for (index, line) in text.lines().enumerate() {
        if !line.trim_start().starts_with('-') {
            continue;
        }
        let links = extract_wikilinks(line);
        if links.is_empty() {
            continue;
        }

        let mut valid = Vec::new();
        let mut stale = Vec::new();
        for link in links {
            if let Some(target) = resolve_link(&link, link_index) {
                valid.push(slug_for_path(&target));
            } else if let Some(key) = normalize_link_key(&link) {
                stale.push(key);
            }
        }

        if stale.is_empty() {
            continue;
        }
        if valid.is_empty() {
            remove_lines.insert(index + 1);
        } else {
            valid.sort();
            valid.dedup();
            stale.sort();
            stale.dedup();
            mixed.push(StaleIndexMixedBullet {
                source: "wiki/index.md".to_string(),
                line: index + 1,
                valid,
                stale,
            });
        }
    }

    IndexStaleAnalysis {
        remove_lines,
        mixed,
    }
}

fn duplicate_index_sections(text: &str) -> Vec<DuplicateIndexSection> {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if matches!(
            trimmed,
            "## Entities" | "## Concepts" | "## Sources" | "## Analyses"
        ) {
            *counts.entry(trimmed.to_string()).or_insert(0) += 1;
        }
    }

    counts
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(section, occurrences)| DuplicateIndexSection {
            section,
            occurrences,
        })
        .collect()
}

fn remove_lines_by_number(text: &str, lines_to_remove: &HashSet<usize>) -> String {
    let mut out = text
        .lines()
        .enumerate()
        .filter(|(index, _)| !lines_to_remove.contains(&(index + 1)))
        .map(|(_, line)| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn insert_missing_index_entries(text: &str, pages: &[IndexablePage]) -> String {
    let mut out = text.to_string();
    let mut pages_by_section: HashMap<&'static str, Vec<&IndexablePage>> = HashMap::new();
    for page in pages {
        pages_by_section
            .entry(section_for_page_type(&page.page_type))
            .or_default()
            .push(page);
    }

    let mut sections: Vec<&'static str> = pages_by_section.keys().copied().collect();
    sections.sort();
    for section in sections {
        let Some(items) = pages_by_section.get(section) else {
            continue;
        };
        let header = format!("## {}", section);
        let insert_pos = find_section_append_pos(&out, &header).unwrap_or(out.len());
        let mut block = String::new();
        for page in items {
            block.push_str(&format!("\n- [[{}|{}]]", page.slug, page.title));
        }
        let pos = next_char_boundary(&out, insert_pos);
        out.insert_str(pos, &block);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn find_section_append_pos(text: &str, header: &str) -> Option<usize> {
    let start = text.find(header)? + header.len();
    let next = text[start..].find("\n## ").map(|pos| start + pos);
    Some(next.unwrap_or(text.len()))
}

fn slug_for_path(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_else(|| path.trim_end_matches(".md").to_string())
}

fn section_for_page_type(page_type: &str) -> &'static str {
    match page_type {
        "source" => "Sources",
        "concept" => "Concepts",
        "analysis" => "Analyses",
        _ => "Entities",
    }
}

fn next_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[derive(Debug, Clone, Copy)]
struct ExpectedStructure {
    path: &'static str,
    kind: &'static str,
    locked: bool,
}

const EXPECTED_STRUCTURE: &[ExpectedStructure] = &[
    ExpectedStructure {
        path: "SCHEMA.md",
        kind: "file",
        locked: true,
    },
    ExpectedStructure {
        path: "SKILL.md",
        kind: "file",
        locked: true,
    },
    ExpectedStructure {
        path: "wiki/index.md",
        kind: "file",
        locked: false,
    },
    ExpectedStructure {
        path: "wiki/log.md",
        kind: "file",
        locked: false,
    },
    ExpectedStructure {
        path: "wiki/entities",
        kind: "dir",
        locked: false,
    },
    ExpectedStructure {
        path: "wiki/concepts",
        kind: "dir",
        locked: false,
    },
    ExpectedStructure {
        path: "wiki/sources",
        kind: "dir",
        locked: false,
    },
    ExpectedStructure {
        path: "wiki/analyses",
        kind: "dir",
        locked: false,
    },
];

fn expected_structure() -> &'static [ExpectedStructure] {
    EXPECTED_STRUCTURE
}

struct LogValidation {
    foreign_content: Vec<ForeignLogContent>,
    malformed_entries: Vec<LogIssue>,
    out_of_order_entries: Vec<LogOrderIssue>,
    duplicate_entries: Vec<String>,
    entry_count: usize,
    latest_date: Option<String>,
}

fn validate_log_entries(text: &str) -> LogValidation {
    let mut foreign_content = Vec::new();
    let mut malformed_entries = Vec::new();
    let mut out_of_order_entries = Vec::new();
    let mut duplicate_entries = Vec::new();
    let mut seen_entries = HashSet::new();
    let mut previous_date: Option<String> = None;
    let mut latest_date: Option<String> = None;
    let mut entry_count = 0usize;
    let mut frontmatter_done = false;
    let mut in_frontmatter = false;

    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line_number == 1 && line == "---" {
            in_frontmatter = true;
            continue;
        }
        if in_frontmatter {
            if line == "---" {
                in_frontmatter = false;
                frontmatter_done = true;
            }
            continue;
        }

        if line.trim().is_empty() || line == "# Wiki Log" || (!frontmatter_done && line == "# Log")
        {
            continue;
        }

        if !is_log_entry_candidate(line) {
            foreign_content.push(ForeignLogContent {
                line: line_number,
                content: line.to_string(),
            });
            continue;
        }

        let entry = line.to_string();
        let mut issues = Vec::new();

        if let Some(caps) = LOG_ENTRY_RE.captures(line) {
            let date = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            if !is_iso_date(date) {
                issues.push(format!("invalid date: '{}'", date));
            }

            let duplicate_key = entry.trim_end().to_string();
            if !seen_entries.insert(duplicate_key.clone()) {
                duplicate_entries.push(duplicate_key);
            }

            if let Some(previous) = &previous_date {
                if date > previous.as_str() {
                    out_of_order_entries.push(LogOrderIssue {
                        line: line_number,
                        entry: entry.clone(),
                        issue: format!("date follows older entry [{}]", previous),
                    });
                }
            }
            previous_date = Some(date.to_string());
            latest_date = Some(
                latest_date
                    .map(|current| current.max(date.to_string()))
                    .unwrap_or_else(|| date.to_string()),
            );
            entry_count += 1;
        } else if let Some(caps) = LOG_ENTRY_LOOSE_RE.captures(line) {
            let date = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
            let action = caps.get(2).map(|m| m.as_str().trim()).unwrap_or_default();
            let description = caps.get(3).map(|m| m.as_str().trim()).unwrap_or_default();

            if !is_iso_date(date) {
                issues.push(format!("invalid date: '{}'", date));
            }
            if action.to_lowercase() != action {
                issues.push(format!("action not lowercase: '{}'", action));
            }
            if !is_allowed_log_action(&action.to_lowercase()) {
                issues.push(format!("unknown action: '{}'", action));
            }
            if description.is_empty() {
                issues.push("empty description".to_string());
            }
        } else {
            issues.push("malformed log entry".to_string());
        }

        if !issues.is_empty() {
            malformed_entries.push(LogIssue {
                line: line_number,
                entry,
                issues,
            });
        }
    }

    duplicate_entries.sort();
    duplicate_entries.dedup();

    LogValidation {
        foreign_content,
        malformed_entries,
        out_of_order_entries,
        duplicate_entries,
        entry_count,
        latest_date,
    }
}

fn is_log_entry_candidate(line: &str) -> bool {
    line.starts_with("## [") || LOG_ENTRY_PAD_RE.is_match(line)
}

fn is_allowed_log_action(action: &str) -> bool {
    matches!(
        action,
        "create" | "update" | "delete" | "rename" | "move" | "link" | "unlink" | "sync"
    )
}

fn normalize_log_action(action: &str) -> Option<&'static str> {
    match action.trim().to_lowercase().as_str() {
        "create" | "created" => Some("create"),
        "update" | "updated" => Some("update"),
        "delete" | "deleted" => Some("delete"),
        "rename" | "renamed" => Some("rename"),
        "move" | "moved" => Some("move"),
        "link" | "linked" => Some("link"),
        "unlink" | "unlinked" => Some("unlink"),
        "sync" | "synced" => Some("sync"),
        _ => None,
    }
}

fn fix_log_text(text: &str) -> (String, Vec<String>) {
    let mut current = text.to_string();
    let mut fixes = Vec::new();

    if let Some(updated) = fix_log_yank_foreign_text(&current) {
        current = updated;
        fixes.push("fix_log_yank_foreign".to_string());
    }
    if let Some(updated) = fix_log_trailing_whitespace_text(&current) {
        current = updated;
        fixes.push("fix_log_trailing_whitespace".to_string());
    }
    if let Some(updated) = fix_log_date_padding_text(&current) {
        current = updated;
        fixes.push("fix_log_date_padding".to_string());
    }
    if let Some(updated) = fix_log_space_after_date_text(&current) {
        current = updated;
        fixes.push("fix_log_space_after_date".to_string());
    }
    if let Some(updated) = fix_log_action_case_text(&current) {
        current = updated;
        fixes.push("fix_log_action_case".to_string());
    }
    if let Some(updated) = fix_log_dedupe_text(&current) {
        current = updated;
        fixes.push("fix_log_dedupe_duplicate".to_string());
    }
    if let Some(updated) = fix_log_sort_text(&current) {
        current = updated;
        fixes.push("fix_log_sort_entries".to_string());
    }
    if let Some(updated) = fix_log_compact_whitespace_text(&current) {
        current = updated;
        fixes.push("fix_log_compact_whitespace".to_string());
    }

    (current, fixes)
}

fn fix_log_compact_whitespace_text(text: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut index = 0usize;
    let mut out = Vec::new();

    if lines.first() == Some(&"---") {
        while index < lines.len() {
            let line = lines[index];
            out.push(line.to_string());
            index += 1;
            if index > 1 && line == "---" {
                break;
            }
        }
    }

    while index < lines.len() && lines[index].trim().is_empty() {
        index += 1;
    }

    let mut saw_heading = false;
    if index < lines.len() && matches!(lines[index], "# Wiki Log" | "# Log") {
        saw_heading = true;
        index += 1;
    }

    if !saw_heading {
        return None;
    }

    if !out.is_empty() {
        out.push(String::new());
    }
    out.push("# Wiki Log".to_string());

    let entries = lines[index..]
        .iter()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.to_string())
        .collect::<Vec<_>>();
    if !entries.is_empty() {
        out.push(String::new());
        out.extend(entries);
    }

    let mut updated = out.join("\n");
    updated.push('\n');
    (updated != text).then_some(updated)
}

fn fix_log_yank_foreign_text(text: &str) -> Option<String> {
    let mut changed = false;
    let mut out = Vec::new();
    let mut in_frontmatter = false;

    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line_number == 1 && line == "---" {
            in_frontmatter = true;
            out.push(line.to_string());
            continue;
        }
        if in_frontmatter {
            out.push(line.to_string());
            if line == "---" {
                in_frontmatter = false;
            }
            continue;
        }
        if line.trim().is_empty() || line == "# Wiki Log" {
            out.push(line.to_string());
            continue;
        }
        if line == "# Log" {
            out.push("# Wiki Log".to_string());
            changed = true;
            continue;
        }
        if is_log_entry_candidate(line) {
            out.push(line.to_string());
            continue;
        }
        changed = true;
    }

    changed.then(|| finish_rewrite(text, out))
}

fn fix_log_trailing_whitespace_text(text: &str) -> Option<String> {
    rewrite_log_lines(text, |line| {
        let trimmed = line.trim_end_matches([' ', '\t']);
        (trimmed != line).then(|| trimmed.to_string())
    })
}

fn fix_log_date_padding_text(text: &str) -> Option<String> {
    rewrite_log_lines(text, |line| {
        let caps = LOG_ENTRY_PAD_RE.captures(line)?;
        let year = caps.get(1)?.as_str();
        let month = caps.get(2)?.as_str().parse::<u32>().ok()?;
        let day = caps.get(3)?.as_str().parse::<u32>().ok()?;
        let rest = caps.get(4).map(|m| m.as_str()).unwrap_or_default();
        if month > 12 || day > 31 {
            return None;
        }
        let fixed = format!("## [{}-{month:02}-{day:02}]{}", year, rest);
        (fixed != line).then_some(fixed)
    })
}

fn fix_log_space_after_date_text(text: &str) -> Option<String> {
    rewrite_log_lines(text, |line| {
        let caps = LOG_ENTRY_MISSING_SPACE_RE.captures(line)?;
        let date = caps.get(1)?.as_str();
        let rest = caps.get(2)?.as_str();
        Some(format!("## [{}] {}", date, rest))
    })
}

fn fix_log_action_case_text(text: &str) -> Option<String> {
    rewrite_log_lines(text, |line| {
        let caps = LOG_ENTRY_LOOSE_RE.captures(line)?;
        let date = caps.get(1).map(|m| m.as_str()).unwrap_or_default();
        let action = caps.get(2).map(|m| m.as_str().trim()).unwrap_or_default();
        let description = caps.get(3).map(|m| m.as_str()).unwrap_or_default();
        let normalized = normalize_log_action(action)?;
        (action != normalized).then(|| format!("## [{}] {} | {}", date, normalized, description))
    })
}

fn fix_log_sort_text(text: &str) -> Option<String> {
    let mut prefix = Vec::new();
    let mut entries = Vec::new();
    let mut in_entries = false;

    for line in text.lines() {
        if LOG_ENTRY_RE.is_match(line) {
            in_entries = true;
            entries.push(line.to_string());
        } else if in_entries && !line.trim().is_empty() {
            return None;
        } else if in_entries {
            continue;
        } else {
            prefix.push(line.to_string());
        }
    }

    let sorted = {
        let mut copy = entries.clone();
        copy.sort_by_key(|entry| std::cmp::Reverse(log_entry_date(entry)));
        copy
    };
    if sorted == entries {
        return None;
    }

    let mut out = prefix;
    if !out.last().map(|line| line.is_empty()).unwrap_or(false) {
        out.push(String::new());
    }
    out.extend(sorted);
    Some(finish_rewrite(text, out))
}

fn log_entry_date(line: &str) -> String {
    LOG_ENTRY_RE
        .captures(line)
        .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
        .unwrap_or_default()
}

fn fix_log_dedupe_text(text: &str) -> Option<String> {
    let mut changed = false;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let mut skipping_duplicate_block = false;

    for line in text.lines() {
        if is_log_entry_candidate(line) {
            let key = line.trim_end_matches([' ', '\t']).to_string();
            if seen.contains(&key) {
                changed = true;
                skipping_duplicate_block = true;
                continue;
            }
            seen.insert(key);
            skipping_duplicate_block = false;
            out.push(line.to_string());
            continue;
        }

        if skipping_duplicate_block {
            continue;
        }
        out.push(line.to_string());
    }

    changed.then(|| finish_rewrite(text, out))
}

fn rewrite_log_lines(text: &str, rewrite: impl Fn(&str) -> Option<String>) -> Option<String> {
    let mut changed = false;
    let mut out = Vec::new();
    let mut in_fence = false;

    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push(line.to_string());
            continue;
        }
        if in_fence || !is_log_entry_candidate(line) {
            out.push(line.to_string());
            continue;
        }
        if let Some(updated) = rewrite(line) {
            out.push(updated);
            changed = true;
        } else {
            out.push(line.to_string());
        }
    }

    changed.then(|| finish_rewrite(text, out))
}

fn finish_rewrite(original: &str, lines: Vec<String>) -> String {
    let mut out = lines.join("\n");
    if original.ends_with('\n') {
        out.push('\n');
    }
    out
}

use std::fs;

use tempfile::TempDir;
use writestead::config::{AppConfig, McpConfig, RawConfig, SearchConfig, SyncBackend, SyncConfig};
use writestead::vault;
use writestead::wiki::{LintOptions, WikiOps};

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

fn setup_wiki() -> (TempDir, AppConfig, WikiOps) {
    let dir = TempDir::new().expect("tempdir");
    let cfg = test_config(dir.path().to_str().expect("path str"));
    vault::init_vault(&cfg, true).expect("init vault");
    let wiki = WikiOps::new(cfg.clone());
    (dir, cfg, wiki)
}

fn sample_page(title: &str, body: &str) -> String {
    format!(
        "---\ntitle: {}\ntype: entity\ncreated: 2026-04-23\nupdated: 2026-04-23\ntags: [test]\n---\n\n# {}\n\n{}\n",
        title, title, body
    )
}

#[test]
fn fresh_init_lints_clean() {
    let (_dir, _cfg, wiki) = setup_wiki();
    let report = wiki.lint().expect("lint");

    assert!(report.missing_structure.is_empty());
    assert!(report.missing_frontmatter.is_empty());
    assert!(report.invalid_frontmatter.is_empty());
    assert!(report.bad_frontmatter.is_empty());
    assert!(report.misplaced_pages.is_empty());
    assert!(report.content_drift.is_empty());
    assert!(report.broken_links.is_empty());
    assert!(report.orphan_pages.is_empty());
}

#[test]
fn rejects_path_traversal() {
    let (_dir, _cfg, wiki) = setup_wiki();
    let err = wiki
        .read_page("../etc/passwd", 1, 20)
        .expect_err("must fail");
    assert!(err.to_string().contains("path traversal"));
}

#[test]
fn edit_requires_unique_old_text() {
    let (_dir, _cfg, wiki) = setup_wiki();
    wiki.write_page(
        "wiki/entities/repeat.md",
        &sample_page("Repeat", "dup\ndup"),
    )
    .expect("write page");

    let err = wiki
        .edit_page(
            "wiki/entities/repeat.md",
            &[("dup".to_string(), "x".to_string())],
        )
        .expect_err("must fail due duplicate oldText");

    assert!(err.to_string().contains("must be unique"));
}

#[test]
fn read_returns_pagination_metadata() {
    let (_dir, _cfg, wiki) = setup_wiki();
    wiki.write_page(
        "wiki/entities/demo.md",
        &sample_page("Demo", "line1\nline2\nline3\nline4"),
    )
    .expect("write page");

    let page = wiki
        .read_page("wiki/entities/demo.md", 2, 2)
        .expect("read page");

    assert_eq!(page.offset, 2);
    assert_eq!(page.limit, 2);
    assert!(page.total_lines >= 4);
    assert!(page.has_more);
}

#[test]
fn list_pages_is_paginated() {
    let (_dir, _cfg, wiki) = setup_wiki();

    wiki.write_page("wiki/entities/a.md", &sample_page("A", "a"))
        .expect("write a");
    wiki.write_page("wiki/entities/b.md", &sample_page("B", "b"))
        .expect("write b");
    wiki.write_page("wiki/entities/c.md", &sample_page("C", "c"))
        .expect("write c");

    let page1 = wiki.list_pages_paginated(0, 2).expect("list page1");
    let page2 = wiki.list_pages_paginated(2, 2).expect("list page2");

    assert_eq!(page1.offset, 0);
    assert_eq!(page1.limit, 2);
    assert_eq!(page1.pages.len(), 2);
    assert!(page1.has_more);

    assert_eq!(page2.offset, 2);
    assert_eq!(page2.limit, 2);
    assert!(page2.total >= page1.total);
}

#[test]
fn lint_reports_structural_frontmatter_and_code_aware_links() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::remove_file(dir.path().join("SCHEMA.md")).expect("remove schema");
    fs::write(
        dir.path().join("wiki/entities/bad-yaml.md"),
        "---\ntitle: [\n---\n\n# Bad\n",
    )
    .expect("bad yaml");
    fs::write(
        dir.path().join("wiki/entities/bad-fm.md"),
        "---\ntitle: Bad\ntype: entity\ncreated: 2026-04-23\nupdated: nope\nextra: x\n---\n\n# Bad\n",
    )
    .expect("bad fm");
    fs::write(
        dir.path().join("wiki/entities/misplaced.md"),
        "---\ntitle: Misplaced\ntype: concept\ncreated: 2026-04-23\nupdated: 2026-04-23\ntags: []\n---\n\n# Misplaced\n",
    )
    .expect("misplaced");
    fs::write(
        dir.path().join("wiki/entities/code.md"),
        "---\ntitle: Code\ntype: entity\ncreated: 2026-04-23\nupdated: 2026-04-23\ntags: []\n---\n\n`[[not-a-link]]`\n\n```\n[[also-not-a-link]]\n```\n",
    )
    .expect("code");

    let report = wiki.lint().expect("lint");

    assert!(report
        .missing_structure
        .iter()
        .any(|item| item.path == "SCHEMA.md" && item.kind == "file"));
    assert!(report
        .invalid_frontmatter
        .iter()
        .any(|item| item.path == "wiki/entities/bad-yaml.md"));
    assert!(report.bad_frontmatter.iter().any(|item| {
        item.path == "wiki/entities/bad-fm.md"
            && item
                .issues
                .iter()
                .any(|issue| issue.contains("missing field: tags"))
            && item
                .issues
                .iter()
                .any(|issue| issue.contains("unknown field: extra"))
    }));
    assert!(report.misplaced_pages.iter().any(|item| {
        item.path == "wiki/entities/misplaced.md"
            && item.declared_type == "concept"
            && item.expected_type == "entity"
    }));
    assert!(!report
        .broken_links
        .iter()
        .any(|item| item.link.contains("not-a-link")));
}

#[test]
fn lint_detects_and_restores_locked_file_drift() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(dir.path().join("SCHEMA.md"), "abc\n").expect("drift schema");

    let report = wiki.lint().expect("lint");
    assert!(report
        .content_drift
        .iter()
        .any(|item| item.path == "SCHEMA.md"));

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix locked drift");
    assert!(fixed
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "SCHEMA.md" && fix.kind == "restore_locked"));

    let clean = wiki.lint().expect("lint clean");
    assert!(clean.content_drift.is_empty());
}

#[test]
fn lint_ignores_root_readme() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(dir.path().join("README.md"), "abc\n").expect("write readme");

    let report = wiki.lint().expect("lint");
    assert!(!report
        .missing_structure
        .iter()
        .any(|item| item.path == "README.md"));
    assert!(!report
        .missing_frontmatter
        .iter()
        .any(|path| path == "README.md"));
}

#[test]
fn lint_fix_creates_structure_and_is_idempotent() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::remove_file(dir.path().join("SCHEMA.md")).expect("remove schema");

    let dry = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: true,
        })
        .expect("dry lint fix");
    assert!(dry
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "SCHEMA.md" && fix.kind == "create_file"));
    assert!(!dir.path().join("SCHEMA.md").exists());

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("lint fix");
    assert!(dir.path().join("SCHEMA.md").exists());
    assert!(!fixed.fixes_applied.is_empty());

    let second = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("lint fix again");
    assert!(second.fixes_applied.is_empty());
}

#[test]
fn lint_validates_log_entries_and_fix_action_case() {
    let (dir, _cfg, wiki) = setup_wiki();
    let log = "---\ntitle: Log\ntype: log\n---\n\n# Log\n\n## [2026-04-24] update | first\nbody allowed\n## [2026-04-25] update | newer below older\n## [2026-04-23] Updated | case issue\n## [2026-04-23] update | dup\n## [2026-04-23] update | dup   \n## [2026-04-22] unknown | bad action\n## [2026-04-21] update | \n## [2026-4-20] update | bad date\n## [2026-04-19]update | missing space\n## [2026-04-18] update - bad pipe\n```\n## [2026-04-17] nope\n```\n";
    fs::write(dir.path().join("wiki/log.md"), log).expect("write log");

    let report = wiki.lint().expect("lint");
    assert_eq!(report.log_entry_count, 4);
    assert!(report
        .out_of_order_log_entries
        .iter()
        .any(|item| item.entry.contains("newer below older")));
    assert!(report
        .duplicate_log_entries
        .iter()
        .any(|entry| entry == "## [2026-04-23] update | dup"));
    assert!(report.malformed_log_entries.iter().any(|item| {
        item.entry.contains("Updated")
            && item
                .issues
                .iter()
                .any(|issue| issue.contains("action not lowercase"))
    }));
    assert!(report.malformed_log_entries.iter().any(|item| {
        item.entry.contains("unknown")
            && item
                .issues
                .iter()
                .any(|issue| issue.contains("unknown action"))
    }));
    assert!(report.malformed_log_entries.iter().any(|item| {
        item.entry == "## [2026-04-21] update | "
            && item
                .issues
                .iter()
                .any(|issue| issue.contains("empty description"))
    }));
    assert!(report.malformed_log_entries.iter().any(|item| {
        item.entry.contains("2026-4-20")
            && item
                .issues
                .iter()
                .any(|issue| issue.contains("malformed log entry"))
    }));
    assert!(report.malformed_log_entries.iter().any(|item| {
        item.entry.contains("]update")
            && item
                .issues
                .iter()
                .any(|issue| issue.contains("malformed log entry"))
    }));
    assert!(report.malformed_log_entries.iter().any(|item| {
        item.entry.contains("bad pipe")
            && item
                .issues
                .iter()
                .any(|issue| issue.contains("malformed log entry"))
    }));
    assert!(report
        .foreign_log_content
        .iter()
        .any(|item| item.content == "body allowed"));
    assert!(report
        .foreign_log_content
        .iter()
        .any(|item| item.content == "```"));

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix log");
    assert!(fixed
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "wiki/log.md" && fix.kind == "fix_log_action_case"));
    let fixed_log = fs::read_to_string(dir.path().join("wiki/log.md")).expect("fixed log");
    assert!(fixed_log.contains("## [2026-04-23] update | case issue"));
}

#[test]
fn lint_fix_applies_safe_log_repairs_idempotently() {
    let (dir, _cfg, wiki) = setup_wiki();
    let log = "---\ntitle: Log\ntype: log\n---\n\n# Log\n\n## [2026-4-5] Updated | padded and case   \nbody removed with duplicate\n## [2026-04-05] update | padded and case\nkept body\n## [2026-04-04]update | missing space\n## [2026-04-06] update | sorted first\n### Notes\n- bullet\n";
    fs::write(dir.path().join("wiki/log.md"), log).expect("write log");

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix log");
    let kinds: Vec<String> = fixed
        .fixes_applied
        .iter()
        .filter(|fix| fix.path == "wiki/log.md")
        .map(|fix| fix.kind.clone())
        .collect();
    assert!(kinds.contains(&"fix_log_trailing_whitespace".to_string()));
    assert!(kinds.contains(&"fix_log_date_padding".to_string()));
    assert!(kinds.contains(&"fix_log_space_after_date".to_string()));
    assert!(kinds.contains(&"fix_log_action_case".to_string()));
    assert!(kinds.contains(&"fix_log_dedupe_duplicate".to_string()));
    assert!(kinds.contains(&"fix_log_yank_foreign".to_string()));
    assert!(kinds.contains(&"fix_log_sort_entries".to_string()));

    let expected = "---\ntitle: Log\ntype: log\n---\n\n# Wiki Log\n\n## [2026-04-06] update | sorted first\n## [2026-04-05] update | padded and case\n## [2026-04-04] update | missing space\n";
    let actual = fs::read_to_string(dir.path().join("wiki/log.md")).expect("fixed log");
    assert_eq!(actual, expected);

    let second = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix again");
    assert!(second
        .fixes_applied
        .iter()
        .all(|fix| fix.path != "wiki/log.md"));
}

#[test]
fn lint_reports_and_fixes_unindexed_pages() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(
        dir.path().join("wiki/entities/manual.md"),
        sample_page("Manual", "not auto indexed"),
    )
    .expect("write manual");

    let report = wiki.lint().expect("lint");
    assert!(report
        .unindexed_pages
        .iter()
        .any(|page| page.path == "wiki/entities/manual.md" && page.slug == "manual"));

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix index");
    assert!(fixed
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "wiki/index.md" && fix.kind == "fix_index_insert_missing"));
    let index = fs::read_to_string(dir.path().join("wiki/index.md")).expect("index");
    assert!(index.contains("- [[manual|Manual]]"));

    let second = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix again");
    assert!(second
        .fixes_applied
        .iter()
        .all(|fix| fix.path != "wiki/index.md"));
}

#[test]
fn lint_reports_and_yanks_foreign_index_content() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(
        dir.path().join("wiki/entities/kept.md"),
        sample_page("Kept", "body"),
    )
    .expect("write kept");
    fs::write(
        dir.path().join("wiki/index.md"),
        "---\ntitle: Index\ntype: index\n---\n\n# Wiki Index\n\nThis is the wiki index.\n\n## Entities\n\n#### Too deep\n- [desc](https://example.com)\n1. numbered\n- plain bullet\n- [[kept|Kept]] -- desc\n\n## Sources\n\n## Concepts\n\n## Analyses\n",
    )
    .expect("write index");

    let report = wiki.lint().expect("lint");
    for content in [
        "This is the wiki index.",
        "#### Too deep",
        "- [desc](https://example.com)",
        "1. numbered",
        "- plain bullet",
    ] {
        assert!(report
            .foreign_index_content
            .iter()
            .any(|item| item.content == content));
    }

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix index");
    assert!(fixed
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "wiki/index.md" && fix.kind == "fix_index_yank_foreign"));
    let index = fs::read_to_string(dir.path().join("wiki/index.md")).expect("index");
    assert!(!index.contains("This is the wiki index."));
    assert!(!index.contains("#### Too deep"));
    assert!(!index.contains("[desc](https://example.com)"));
    assert!(!index.contains("1. numbered"));
    assert!(!index.contains("plain bullet"));
    assert!(index.contains("- [[kept|Kept]] -- desc"));

    let second = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix again");
    assert!(second.foreign_index_content.is_empty());
    assert!(second
        .fixes_applied
        .iter()
        .all(|fix| fix.path != "wiki/index.md"));
}

#[test]
fn lint_migrates_legacy_and_missing_index_h1() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(
        dir.path().join("wiki/index.md"),
        "---\ntitle: Index\ntype: index\n---\n\n# Index\n\n## Entities\n",
    )
    .expect("write legacy index");

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix legacy h1");
    assert!(fixed
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "wiki/index.md" && fix.kind == "fix_index_yank_foreign"));
    let index = fs::read_to_string(dir.path().join("wiki/index.md")).expect("index");
    assert!(index.contains("# Wiki Index"));
    assert!(!index.contains("# Index\n"));

    fs::write(
        dir.path().join("wiki/index.md"),
        "---\ntitle: Index\ntype: index\n---\n\n## Entities\n",
    )
    .expect("write missing h1 index");
    wiki.lint_with_options(LintOptions {
        fix: true,
        dry_run: false,
    })
    .expect("fix missing h1");
    let index = fs::read_to_string(dir.path().join("wiki/index.md")).expect("index");
    assert!(index.contains("# Wiki Index\n## Entities"));
}

#[test]
fn lint_allows_valid_index_subsections() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(
        dir.path().join("wiki/entities/router.md"),
        sample_page("Router", "body"),
    )
    .expect("write router");
    fs::write(
        dir.path().join("wiki/sources/paper.md"),
        "---\ntitle: Paper\ntype: source\nupdated: 2026-04-24\n---\n\n# Paper\n",
    )
    .expect("write paper");
    let index = "---\ntitle: Index\ntype: index\n---\n\n# Wiki Index\n\n## Entities\n\n### Infrastructure\n\n- [[router|Router]] -- edge device\n\n## Sources\n\n### Papers\n\n- [[paper|Paper]]\n\n## Concepts\n\n## Analyses\n";
    fs::write(dir.path().join("wiki/index.md"), index).expect("write index");

    let report = wiki.lint().expect("lint");
    assert!(report.foreign_index_content.is_empty());

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix index");
    assert!(!fixed
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "wiki/index.md" && fix.kind == "fix_index_yank_foreign"));
    let actual = fs::read_to_string(dir.path().join("wiki/index.md")).expect("index");
    assert_eq!(actual, index);
}

#[test]
fn lint_reports_duplicate_index_entries() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(
        dir.path().join("wiki/entities/dupe.md"),
        sample_page("Dupe", "target"),
    )
    .expect("write dupe");
    fs::write(
        dir.path().join("wiki/index.md"),
        "---\ntitle: Index\ntype: index\n---\n\n# Index\n\n## Entities\n\n- [[dupe|Dupe]]\n- [[dupe]]\n\n## Sources\n\n## Concepts\n\n## Analyses\n",
    )
    .expect("write index");

    let report = wiki.lint().expect("lint");
    assert!(report
        .duplicate_index_entries
        .iter()
        .any(|entry| entry.slug == "dupe" && entry.count == 2));
}

#[test]
fn lint_reports_mixed_stale_index_bullet_without_removing_it() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(
        dir.path().join("wiki/entities/valid.md"),
        sample_page("Valid", "body"),
    )
    .expect("write valid");
    fs::write(
        dir.path().join("wiki/index.md"),
        "---\ntitle: Index\ntype: index\n---\n\n# Index\n\n## Entities\n\n- [[valid]] and [[stale]] -- note\n\n## Sources\n\n## Concepts\n\n## Analyses\n",
    )
    .expect("write index");

    let report = wiki.lint().expect("lint");
    assert!(report.stale_index_mixed_bullet.iter().any(|item| {
        item.valid == vec!["valid".to_string()] && item.stale == vec!["stale".to_string()]
    }));

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix index");
    assert!(!fixed
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "wiki/index.md" && fix.kind == "fix_index_remove_stale"));
    let index = fs::read_to_string(dir.path().join("wiki/index.md")).expect("index");
    assert!(index.contains("[[valid]] and [[stale]]"));
}

#[test]
fn lint_fix_removes_stale_index_entries() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(
        dir.path().join("wiki/index.md"),
        "---\ntitle: Index\ntype: index\n---\n\n# Index\n\n## Entities\n\n- [[missing|Missing]]\n- [[also-missing]]\n\n## Sources\n\n## Concepts\n\n## Analyses\n",
    )
    .expect("write index");

    let report = wiki.lint().expect("lint");
    assert!(report
        .broken_links
        .iter()
        .any(|link| link.source == "wiki/index.md"));

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix index");
    assert!(fixed
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "wiki/index.md" && fix.kind == "fix_index_remove_stale"));
    let index = fs::read_to_string(dir.path().join("wiki/index.md")).expect("index");
    assert!(!index.contains("missing"));
}

#[test]
fn lint_reports_duplicate_index_sections_and_skips_insert_for_section() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(
        dir.path().join("wiki/entities/no-insert.md"),
        sample_page("No Insert", "body"),
    )
    .expect("write page");
    fs::write(
        dir.path().join("wiki/index.md"),
        "---\ntitle: Index\ntype: index\n---\n\n# Index\n\n## Entities\n\n## Sources\n\n## Entities\n\n## Concepts\n\n## Analyses\n",
    )
    .expect("write index");

    let report = wiki.lint().expect("lint");
    assert!(report
        .duplicate_index_sections
        .iter()
        .any(|section| section.section == "## Entities" && section.occurrences == 2));
    assert!(report
        .unindexed_pages
        .iter()
        .any(|page| page.path == "wiki/entities/no-insert.md"));

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix index");
    assert!(!fixed
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "wiki/index.md" && fix.kind == "fix_index_insert_missing"));
    let index = fs::read_to_string(dir.path().join("wiki/index.md")).expect("index");
    assert!(!index.contains("[[no-insert|No Insert]]"));
}

#[test]
fn lint_fix_adds_missing_index_section() {
    let (dir, _cfg, wiki) = setup_wiki();
    fs::write(
        dir.path().join("wiki/entities/no-section.md"),
        sample_page("No Section", "body"),
    )
    .expect("write page");
    fs::write(
        dir.path().join("wiki/index.md"),
        "---\ntitle: Index\ntype: index\n---\n\n# Index\n\n## Sources\n\n## Concepts\n\n## Analyses\n",
    )
    .expect("write index");

    let report = wiki.lint().expect("lint");
    assert!(report
        .missing_index_sections
        .iter()
        .any(|section| section.section == "Entities"));

    let fixed = wiki
        .lint_with_options(LintOptions {
            fix: true,
            dry_run: false,
        })
        .expect("fix index");
    assert!(fixed
        .fixes_applied
        .iter()
        .any(|fix| fix.path == "wiki/index.md" && fix.kind == "fix_index_add_section"));
    let index = fs::read_to_string(dir.path().join("wiki/index.md")).expect("index");
    assert!(index.contains("## Entities"));
    assert!(index.contains("[[no-section|No Section]]"));
}

#[test]
fn lint_resolves_alias_and_section_links() {
    let (_dir, cfg, wiki) = setup_wiki();

    wiki.write_page("wiki/entities/page-b.md", &sample_page("Page B", "target"))
        .expect("write page b");
    wiki.write_page(
        "wiki/entities/page-a.md",
        &sample_page("Page A", "[[page-b|alias]]\n[[page-b#section]]"),
    )
    .expect("write page a");

    let report = wiki.lint().expect("lint");

    let broken_from_a: Vec<_> = report
        .broken_links
        .iter()
        .filter(|b| b.source == "wiki/entities/page-a.md")
        .collect();
    assert!(
        broken_from_a.is_empty(),
        "unexpected broken links: {broken_from_a:?}"
    );

    // sanity: files exist where expected
    assert!(
        fs::metadata(format!("{}/wiki/entities/page-a.md", cfg.vault_path)).is_ok(),
        "page-a exists"
    );
}

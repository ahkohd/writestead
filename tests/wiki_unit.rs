use std::fs;

use tempfile::TempDir;
use writestead::config::{AppConfig, McpConfig, RawConfig, SearchConfig, SyncBackend, SyncConfig};
use writestead::vault;
use writestead::wiki::WikiOps;

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

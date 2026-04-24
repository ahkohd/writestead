use std::fs;

use tempfile::TempDir;
use writestead::config::{AppConfig, McpConfig, RawConfig, SearchConfig, SyncBackend, SyncConfig};
use writestead::raw::RawOps;
use writestead::vault;

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

fn setup_raw() -> (TempDir, AppConfig, RawOps) {
    let dir = TempDir::new().expect("tempdir");
    let cfg = test_config(dir.path().to_str().expect("path str"));
    vault::init_vault(&cfg, true).expect("init vault");
    let raw = RawOps::new(cfg.clone());
    (dir, cfg, raw)
}

#[tokio::test]
async fn raw_add_local_file_smoke() {
    let (dir, _cfg, raw) = setup_raw();

    let source = dir.path().join("source.txt");
    fs::write(&source, "alpha\n").expect("write source");

    let result = raw
        .add_source(source.to_str().expect("source path"), None, false)
        .await
        .expect("raw add");

    assert!(result.ok);
    assert_eq!(result.path, "raw/source.txt");
    assert_eq!(result.source, "local");
}

#[tokio::test]
async fn raw_add_rejects_path_traversal() {
    let (_dir, _cfg, raw) = setup_raw();

    let err = raw
        .add_source("../etc/passwd", None, false)
        .await
        .expect_err("must fail");

    assert!(err.to_string().contains("path traversal"));
}

#[tokio::test]
async fn raw_add_rejects_overwrite_without_force() {
    let (dir, _cfg, raw) = setup_raw();

    let source = dir.path().join("source.txt");
    fs::write(&source, "alpha\n").expect("write source");

    raw.add_source(source.to_str().expect("source path"), None, false)
        .await
        .expect("first add");

    let err = raw
        .add_source(source.to_str().expect("source path"), None, false)
        .await
        .expect_err("must fail overwrite");

    assert!(err.to_string().contains("destination already exists"));
}

#[tokio::test]
async fn raw_add_enforces_size_cap() {
    let (dir, mut cfg, _raw) = setup_raw();
    cfg.raw.upload_max_bytes = 4;
    let raw = RawOps::new(cfg);

    let source = dir.path().join("big.txt");
    fs::write(&source, "12345").expect("write big source");

    let err = raw
        .add_source(source.to_str().expect("source path"), None, false)
        .await
        .expect_err("must fail size cap");

    assert!(err.to_string().contains("file too large"));
}

#[test]
fn raw_list_pagination_smoke() {
    let (dir, _cfg, raw) = setup_raw();

    fs::write(dir.path().join("raw/a.txt"), "a\n").expect("write a");
    fs::write(dir.path().join("raw/b.txt"), "b\n").expect("write b");

    let list = raw.list_sources(1, 1).expect("raw list");
    assert_eq!(list.total, 2);
    assert_eq!(list.offset, 1);
    assert_eq!(list.limit, 1);
    assert_eq!(list.files, vec!["b.txt"]);
    assert!(!list.has_more);
}

#[tokio::test]
async fn raw_read_text_file_with_pagination() {
    let (dir, _cfg, raw) = setup_raw();

    fs::write(dir.path().join("raw/source.txt"), "l1\nl2\nl3\n").expect("source text");
    let read = raw.read_source("source.txt", 2, 1).await.expect("raw read");

    assert_eq!(read.format, "text");
    assert_eq!(read.offset, 2);
    assert_eq!(read.limit, 1);
    assert_eq!(read.content, "l2");
    assert!(read.has_more);
}

#[tokio::test]
async fn raw_read_unsupported_format_rejected() {
    let (dir, _cfg, raw) = setup_raw();

    fs::write(dir.path().join("raw/file.bin"), "abc").expect("bin source");
    let err = raw
        .read_source("file.bin", 1, 50)
        .await
        .expect_err("must fail unsupported");

    assert!(err.to_string().contains("unsupported file type"));
}

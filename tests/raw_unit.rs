use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::sync::OnceLock;

use tempfile::TempDir;
use writestead::config::{AppConfig, McpConfig, RawConfig, SearchConfig, SyncBackend, SyncConfig};
use writestead::raw::{RawOps, RawReadOptions};
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
    let mut cfg = test_config(dir.path().to_str().expect("path str"));
    cfg.search.backend = writestead::config::SearchBackend::Builtin;
    vault::init_vault(&cfg, true).expect("init vault");
    let raw = RawOps::new(cfg.clone());
    (dir, cfg, raw)
}

#[cfg(unix)]
fn env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(unix)]
fn write_executable(path: &std::path::Path, body: &str) {
    fs::write(path, body).expect("write script");
    let mut perms = fs::metadata(path).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(path, perms).expect("chmod");
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

#[cfg(unix)]
#[tokio::test]
async fn raw_read_large_pdf_routes_to_pdftotext_without_liteparse() {
    let _guard = env_lock().lock().await;
    let old_path = std::env::var("PATH").unwrap_or_default();

    let (dir, _cfg, raw) = setup_raw();
    let fake_bin = dir.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");

    let lit_marker = dir.path().join("lit-called");
    let args_file = dir.path().join("pdftotext-args");

    write_executable(
        &fake_bin.join("pdfinfo"),
        "#!/bin/sh\nprintf 'Pages:          31\\n'\n",
    );
    write_executable(
        &fake_bin.join("pdftotext"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf 'manual text\\n'\n",
            args_file.display()
        ),
    );
    write_executable(
        &fake_bin.join("lit"),
        &format!(
            "#!/bin/sh\ntouch '{}'\nprintf 'lit text\\n'\n",
            lit_marker.display()
        ),
    );

    std::env::set_var("PATH", format!("{}:{}", fake_bin.display(), old_path));
    fs::write(dir.path().join("raw/manual.pdf"), b"%PDF test").expect("pdf");

    let read = raw
        .read_source("manual.pdf", 1, 20)
        .await
        .expect("raw read");

    std::env::set_var("PATH", old_path);

    assert_eq!(read.extractor, "pdftotext");
    assert_eq!(read.content, "manual text");
    assert!(
        !lit_marker.exists(),
        "liteparse must not run for large PDFs"
    );
    assert!(fs::read_to_string(args_file)
        .expect("args")
        .contains("-layout"));
}

#[cfg(unix)]
#[tokio::test]
async fn raw_read_pdf_page_range_uses_pdftotext_range() {
    let _guard = env_lock().lock().await;
    let old_path = std::env::var("PATH").unwrap_or_default();

    let (dir, _cfg, raw) = setup_raw();
    let fake_bin = dir.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");

    let args_file = dir.path().join("pdftotext-args");
    write_executable(
        &fake_bin.join("pdfinfo"),
        "#!/bin/sh\nprintf 'Pages:          300\\n'\n",
    );
    write_executable(
        &fake_bin.join("pdftotext"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf 'page slice\\n'\n",
            args_file.display()
        ),
    );

    std::env::set_var("PATH", format!("{}:{}", fake_bin.display(), old_path));
    fs::write(dir.path().join("raw/manual.pdf"), b"%PDF test").expect("pdf");

    let read = raw
        .read_source_with_options(
            "manual.pdf",
            RawReadOptions {
                offset: 1,
                limit: 20,
                page_start: Some(5),
                page_end: Some(45),
            },
        )
        .await
        .expect("raw read");

    std::env::set_var("PATH", old_path);

    assert_eq!(read.extractor, "pdftotext");
    let args = fs::read_to_string(args_file).expect("args");
    assert!(args.contains("-f 5"));
    assert!(args.contains("-l 45"));
}

#[cfg(unix)]
#[tokio::test]
async fn raw_read_small_pdf_page_range_uses_liteparse_slice() {
    let _guard = env_lock().lock().await;
    let old_path = std::env::var("PATH").unwrap_or_default();

    let (dir, mut cfg, _raw) = setup_raw();
    cfg.raw.pdf_liteparse_mem_limit_mb = 0;
    let raw = RawOps::new(cfg);
    let fake_bin = dir.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");

    let lit_args = dir.path().join("lit-args");
    write_executable(
        &fake_bin.join("pdfinfo"),
        "#!/bin/sh\nprintf 'Pages:          300\\n'\n",
    );
    write_executable(
        &fake_bin.join("pdfseparate"),
        "#!/bin/sh\nstart=$2\nend=$4\npattern=$6\ni=$start\nwhile [ \"$i\" -le \"$end\" ]; do out=$(printf \"$pattern\" \"$i\"); printf 'page %s\\n' \"$i\" > \"$out\"; i=$((i + 1)); done\n",
    );
    write_executable(
        &fake_bin.join("pdfunite"),
        "#!/bin/sh\nfor last do :; done\nprintf 'slice\\n' > \"$last\"\n",
    );
    write_executable(
        &fake_bin.join("lit"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf 'lite slice\\n'\n",
            lit_args.display()
        ),
    );

    std::env::set_var("PATH", format!("{}:{}", fake_bin.display(), old_path));
    fs::write(dir.path().join("raw/manual.pdf"), b"%PDF test").expect("pdf");

    let read = raw
        .read_source_with_options(
            "manual.pdf",
            RawReadOptions {
                offset: 1,
                limit: 20,
                page_start: Some(5),
                page_end: Some(7),
            },
        )
        .await
        .expect("raw read");

    std::env::set_var("PATH", old_path);

    assert_eq!(read.extractor, "liteparse");
    assert_eq!(read.content, "lite slice");
    assert!(fs::read_to_string(lit_args)
        .expect("lit args")
        .contains("slice.pdf"));
}

#[cfg(unix)]
#[tokio::test]
async fn raw_read_pdf_range_falls_back_when_pdfseparate_missing() {
    let _guard = env_lock().lock().await;
    let old_path = std::env::var("PATH").unwrap_or_default();

    let (dir, mut cfg, _raw) = setup_raw();
    cfg.raw.pdf_liteparse_mem_limit_mb = 0;
    let raw = RawOps::new(cfg);
    let fake_bin = dir.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");

    let pdftotext_args = dir.path().join("pdftotext-args");
    let lit_marker = dir.path().join("lit-called");
    write_executable(
        &fake_bin.join("pdfinfo"),
        "#!/bin/sh\nprintf 'Pages:          300\\n'\n",
    );
    write_executable(
        &fake_bin.join("pdftotext"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf 'fallback text\\n'\n",
            pdftotext_args.display()
        ),
    );
    write_executable(
        &fake_bin.join("lit"),
        &format!(
            "#!/bin/sh\ntouch '{}'\nprintf 'lit text\\n'\n",
            lit_marker.display()
        ),
    );

    std::env::set_var("PATH", fake_bin.display().to_string());
    fs::write(dir.path().join("raw/manual.pdf"), b"%PDF test").expect("pdf");

    let read = raw
        .read_source_with_options(
            "manual.pdf",
            RawReadOptions {
                offset: 1,
                limit: 20,
                page_start: Some(5),
                page_end: Some(7),
            },
        )
        .await
        .expect("raw read");

    std::env::set_var("PATH", old_path);

    assert_eq!(read.extractor, "pdftotext");
    assert_eq!(read.content, "fallback text");
    assert!(!lit_marker.exists(), "lit must not run when split fails");
    let args = fs::read_to_string(pdftotext_args).expect("pdftotext args");
    assert!(args.contains("-f 5"));
    assert!(args.contains("-l 7"));
}

#[cfg(unix)]
#[tokio::test]
async fn raw_read_pdf_range_falls_back_when_pdfunite_missing() {
    let _guard = env_lock().lock().await;
    let old_path = std::env::var("PATH").unwrap_or_default();

    let (dir, mut cfg, _raw) = setup_raw();
    cfg.raw.pdf_liteparse_mem_limit_mb = 0;
    let raw = RawOps::new(cfg);
    let fake_bin = dir.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");

    let pdftotext_args = dir.path().join("pdftotext-args");
    write_executable(
        &fake_bin.join("pdfinfo"),
        "#!/bin/sh\nprintf 'Pages:          300\\n'\n",
    );
    write_executable(
        &fake_bin.join("pdfseparate"),
        "#!/bin/sh\nstart=$2\nend=$4\npattern=$6\ni=$start\nwhile [ \"$i\" -le \"$end\" ]; do out=$(printf \"$pattern\" \"$i\"); printf 'page %s\\n' \"$i\" > \"$out\"; i=$((i + 1)); done\n",
    );
    write_executable(
        &fake_bin.join("pdftotext"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf 'fallback text\\n'\n",
            pdftotext_args.display()
        ),
    );

    std::env::set_var("PATH", fake_bin.display().to_string());
    fs::write(dir.path().join("raw/manual.pdf"), b"%PDF test").expect("pdf");

    let read = raw
        .read_source_with_options(
            "manual.pdf",
            RawReadOptions {
                offset: 1,
                limit: 20,
                page_start: Some(5),
                page_end: Some(7),
            },
        )
        .await
        .expect("raw read");

    std::env::set_var("PATH", old_path);

    assert_eq!(read.extractor, "pdftotext");
    let args = fs::read_to_string(pdftotext_args).expect("pdftotext args");
    assert!(args.contains("-f 5"));
    assert!(args.contains("-l 7"));
}

#[cfg(unix)]
#[tokio::test]
async fn raw_read_pdf_range_liteparse_timeout_falls_back_to_original_range() {
    let _guard = env_lock().lock().await;
    let old_path = std::env::var("PATH").unwrap_or_default();

    let (dir, mut cfg, _raw) = setup_raw();
    cfg.raw.pdf_liteparse_timeout_ms = 100;
    cfg.raw.pdf_liteparse_mem_limit_mb = 0;
    let raw = RawOps::new(cfg);
    let fake_bin = dir.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");

    let pdftotext_args = dir.path().join("pdftotext-args");
    write_executable(
        &fake_bin.join("pdfinfo"),
        "#!/bin/sh\nprintf 'Pages:          300\\n'\n",
    );
    write_executable(
        &fake_bin.join("pdfseparate"),
        "#!/bin/sh\nstart=$2\nend=$4\npattern=$6\ni=$start\nwhile [ \"$i\" -le \"$end\" ]; do out=$(printf \"$pattern\" \"$i\"); printf 'page %s\\n' \"$i\" > \"$out\"; i=$((i + 1)); done\n",
    );
    write_executable(
        &fake_bin.join("pdfunite"),
        "#!/bin/sh\nfor last do :; done\nprintf 'slice\\n' > \"$last\"\n",
    );
    write_executable(
        &fake_bin.join("lit"),
        "#!/bin/sh\n/bin/sleep 2\nprintf 'late\\n'\n",
    );
    write_executable(
        &fake_bin.join("pdftotext"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{}'\nprintf 'fallback text\\n'\n",
            pdftotext_args.display()
        ),
    );

    std::env::set_var("PATH", fake_bin.display().to_string());
    fs::write(dir.path().join("raw/manual.pdf"), b"%PDF test").expect("pdf");

    let read = raw
        .read_source_with_options(
            "manual.pdf",
            RawReadOptions {
                offset: 1,
                limit: 20,
                page_start: Some(5),
                page_end: Some(7),
            },
        )
        .await
        .expect("raw read");

    std::env::set_var("PATH", old_path);

    assert_eq!(read.extractor, "pdftotext");
    let args = fs::read_to_string(pdftotext_args).expect("pdftotext args");
    assert!(args.contains("-f 5"));
    assert!(args.contains("-l 7"));
}

#[cfg(unix)]
#[tokio::test]
async fn raw_read_liteparse_timeout_is_tagged() {
    let _guard = env_lock().lock().await;
    let old_path = std::env::var("PATH").unwrap_or_default();

    let (dir, mut cfg, _raw) = setup_raw();
    cfg.raw.pdf_liteparse_timeout_ms = 100;
    cfg.raw.pdf_liteparse_mem_limit_mb = 0;
    let raw = RawOps::new(cfg);

    let fake_bin = dir.path().join("bin");
    fs::create_dir_all(&fake_bin).expect("fake bin");
    write_executable(
        &fake_bin.join("lit"),
        "#!/bin/sh\nsleep 2\nprintf 'late\\n'\n",
    );

    std::env::set_var("PATH", format!("{}:{}", fake_bin.display(), old_path));
    fs::write(dir.path().join("raw/slow.docx"), b"docx").expect("docx");

    let err = raw
        .read_source("slow.docx", 1, 20)
        .await
        .expect_err("timeout must fail");

    std::env::set_var("PATH", old_path);

    let failure = err
        .downcast_ref::<writestead::raw::RawReadFailure>()
        .expect("tagged raw read failure");
    assert_eq!(failure.extractor(), "liteparse");
    assert!(
        failure.to_string().contains("timed out"),
        "failure was: {}",
        failure
    );
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

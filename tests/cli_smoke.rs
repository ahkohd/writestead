use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_writestead")
}

fn run(args: &[&str], config_file: &Path) -> (bool, String, String) {
    let output = Command::new(bin())
        .args(args)
        .env("WRITESTEAD_CONFIG_FILE", config_file)
        .output()
        .expect("run command");

    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).to_string(),
        String::from_utf8_lossy(&output.stderr).to_string(),
    )
}

#[test]
fn cli_smoke_temp_vault() {
    let dir = TempDir::new().expect("tempdir");
    let config_file = dir.path().join("config.json");
    let vault_path = dir.path().join("vault");
    let page_file = dir.path().join("page.md");

    fs::write(
        &page_file,
        "---\ntitle: Cli Demo\ntype: entity\ncreated: 2026-04-23\nupdated: 2026-04-23\ntags: [demo]\n---\n\n# Cli Demo\n\nalpha\nbeta\n",
    )
    .expect("write page file");

    let (ok, out, err) = run(
        &[
            "init",
            "--vault-path",
            vault_path.to_str().expect("vault path"),
            "--name",
            "cli-test",
            "--sync-backend",
            "none",
            "--force",
        ],
        &config_file,
    );
    assert!(ok, "init failed: {err}");
    assert!(out.contains("initialized vault"));

    let (ok, out, err) = run(
        &[
            "write",
            "wiki/entities/cli-demo.md",
            "--content-file",
            page_file.to_str().expect("page file"),
            "--log-action",
            "create",
            "--log-description",
            "create cli demo",
        ],
        &config_file,
    );
    assert!(ok, "write failed: {err}");
    let payload: Value = serde_json::from_str(&out).expect("write json");
    assert_eq!(payload["ok"], Value::Bool(true));

    let raw_source = dir.path().join("source.txt");
    fs::write(&raw_source, "raw alpha\nraw beta\n").expect("raw source");

    let (ok, _out, err) = run(
        &["raw", "add", raw_source.to_str().expect("raw source path")],
        &config_file,
    );
    assert!(ok, "raw add failed: {err}");

    let (ok, out, err) = run(
        &["raw", "list", "--offset", "0", "--limit", "20"],
        &config_file,
    );
    assert!(ok, "raw list failed: {err}");
    let payload: Value = serde_json::from_str(&out).expect("raw list json");
    assert_eq!(payload["files"], Value::from(vec!["source.txt"]));

    let (ok, out, err) = run(&["raw", "read", "source.txt"], &config_file);
    assert!(ok, "raw read failed: {err}");
    let payload: Value = serde_json::from_str(&out).expect("raw read json");
    assert_eq!(payload["extractor"], Value::from("direct"));

    let (ok, out, err) = run(&["list", "--offset", "0", "--limit", "2"], &config_file);
    assert!(ok, "list failed: {err}");
    let payload: Value = serde_json::from_str(&out).expect("list json");
    assert!(payload.get("pages").is_some());
    assert_eq!(payload["offset"], Value::from(0));
    assert_eq!(payload["limit"], Value::from(2));

    let (ok, out, err) = run(
        &[
            "edit",
            "wiki/entities/cli-demo.md",
            "--old-text",
            "beta",
            "--new-text",
            "beta2",
            "--log-action",
            "update",
            "--log-description",
            "rename beta",
        ],
        &config_file,
    );
    assert!(ok, "edit failed: {err}");
    let payload: Value = serde_json::from_str(&out).expect("edit json");
    assert_eq!(payload["updated"], Value::Bool(true));

    let (ok, out, err) = run(
        &["read", "wiki/log.md", "--offset", "1", "--limit", "50"],
        &config_file,
    );
    assert!(ok, "read log failed: {err}");
    let payload: Value = serde_json::from_str(&out).expect("read log json");
    let content = payload["content"].as_str().unwrap_or_default();
    assert!(content.contains("create cli demo"));
    assert!(content.contains("rename beta"));

    let (ok, _out, _err) = run(&["status", "--json"], &config_file);
    assert!(ok, "status --json failed");

    let (ok, out, err) = run(&["help-wiki"], &config_file);
    assert!(ok, "help-wiki failed: {err}");
    assert!(out.contains("Writestead workflow guide"));
}

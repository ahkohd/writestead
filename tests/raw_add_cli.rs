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

fn init_vault(config_file: &Path, vault_path: &Path) {
    let (ok, _out, err) = run(
        &[
            "init",
            "--vault-path",
            vault_path.to_str().expect("vault path"),
            "--sync-backend",
            "none",
            "--force",
        ],
        config_file,
    );
    assert!(ok, "init failed: {err}");
}

#[test]
fn raw_add_local_file_smoke() {
    let dir = TempDir::new().expect("tempdir");
    let config_file = dir.path().join("config.json");
    let vault_path = dir.path().join("vault");
    init_vault(&config_file, &vault_path);

    let source = dir.path().join("source.txt");
    fs::write(&source, "alpha\n").expect("source file");

    let (ok, out, err) = run(
        &["raw", "add", source.to_str().expect("source path")],
        &config_file,
    );
    assert!(ok, "raw add failed: {err}");

    let payload: Value = serde_json::from_str(&out).expect("raw add json");
    assert_eq!(payload["ok"], Value::Bool(true));
    assert_eq!(payload["path"], Value::from("raw/source.txt"));
}

#[test]
fn raw_add_rejects_path_traversal() {
    let dir = TempDir::new().expect("tempdir");
    let config_file = dir.path().join("config.json");
    let vault_path = dir.path().join("vault");
    init_vault(&config_file, &vault_path);

    let (ok, _out, err) = run(&["raw", "add", "../etc/passwd"], &config_file);
    assert!(!ok, "raw add should fail");
    assert!(err.contains("path traversal"), "unexpected err: {err}");
}

#[test]
fn raw_add_rejects_overwrite_without_force() {
    let dir = TempDir::new().expect("tempdir");
    let config_file = dir.path().join("config.json");
    let vault_path = dir.path().join("vault");
    init_vault(&config_file, &vault_path);

    let source = dir.path().join("source.txt");
    fs::write(&source, "alpha\n").expect("source file");

    let (ok, _out, err) = run(
        &["raw", "add", source.to_str().expect("source path")],
        &config_file,
    );
    assert!(ok, "first raw add failed: {err}");

    let (ok, _out, err) = run(
        &["raw", "add", source.to_str().expect("source path")],
        &config_file,
    );
    assert!(!ok, "second raw add should fail");
    assert!(
        err.contains("destination already exists"),
        "unexpected err: {err}"
    );
}

#[test]
fn raw_add_size_cap_enforced() {
    let dir = TempDir::new().expect("tempdir");
    let config_file = dir.path().join("config.json");
    let vault_path = dir.path().join("vault");
    init_vault(&config_file, &vault_path);

    let (ok, _out, err) = run(
        &["config", "set", "raw.upload_max_bytes", "4"],
        &config_file,
    );
    assert!(ok, "config set failed: {err}");

    let source = dir.path().join("big.txt");
    fs::write(&source, "12345").expect("big source");

    let (ok, _out, err) = run(
        &["raw", "add", source.to_str().expect("source path")],
        &config_file,
    );
    assert!(!ok, "raw add should fail by size cap");
    assert!(err.contains("file too large"), "unexpected err: {err}");
}

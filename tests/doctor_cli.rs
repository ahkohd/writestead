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
fn doctor_includes_extractor_and_accelerator_checks() {
    let dir = TempDir::new().expect("tempdir");
    let config_file = dir.path().join("config.json");
    let vault_path = dir.path().join("vault");

    let (ok, _out, err) = run(
        &[
            "init",
            "--vault-path",
            vault_path.to_str().expect("vault path"),
            "--sync-backend",
            "none",
            "--force",
        ],
        &config_file,
    );
    assert!(ok, "init failed: {err}");

    let (ok, out, err) = run(&["doctor", "--json"], &config_file);
    assert!(ok, "doctor failed: {err}");

    let payload: Value = serde_json::from_str(&out).expect("doctor json");
    let checks = payload["checks"].as_array().expect("checks array");

    let names: Vec<&str> = checks.iter().filter_map(|v| v["name"].as_str()).collect();

    assert!(names.contains(&"liteparse_binary"));
    assert!(names.contains(&"pdftotext_binary"));
    assert!(names.contains(&"liteparse_formats"));
    assert!(names.contains(&"rg_binary"));
    assert!(names.contains(&"fd_binary"));
}

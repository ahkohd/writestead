use crate::config::{
    self, effective_mcp_auth_mode, effective_mcp_bearer_token, McpAuthMode, SearchBackend,
    SyncBackend,
};
use anyhow::Result;
use serde::Serialize;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

#[derive(Debug, Clone, Serialize)]
struct Check {
    name: String,
    ok: bool,
    detail: String,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorReport {
    ok: bool,
    config_file: String,
    vault_path: String,
    sync_backend: String,
    search_backend: String,
    checks: Vec<Check>,
}

pub async fn run(json_output: bool) -> Result<()> {
    let cfg = config::load_or_default()?;
    let config_file = config::config_file_path().display().to_string();
    let mut checks: Vec<Check> = Vec::new();

    checks.push(Check {
        name: "config_load".to_string(),
        ok: true,
        detail: "config loaded".to_string(),
    });

    let vault = Path::new(&cfg.vault_path);
    let vault_exists = vault.exists() && vault.is_dir();
    checks.push(Check {
        name: "vault_exists".to_string(),
        ok: vault_exists,
        detail: if vault_exists {
            format!("{}", vault.display())
        } else {
            format!("missing or not a directory: {}", vault.display())
        },
    });

    let vault_writable = if vault_exists {
        let probe = vault.join(".writestead-doctor-write-test");
        match std::fs::write(&probe, "ok\n") {
            Ok(_) => {
                let _ = std::fs::remove_file(&probe);
                true
            }
            Err(_) => false,
        }
    } else {
        false
    };

    checks.push(Check {
        name: "vault_writable".to_string(),
        ok: vault_writable,
        detail: if vault_writable {
            "write probe ok".to_string()
        } else if !vault_exists {
            "skipped: vault does not exist".to_string()
        } else {
            "write probe failed".to_string()
        },
    });

    let auth_mode = effective_mcp_auth_mode(&cfg);
    let token_present = effective_mcp_bearer_token(&cfg).is_some();

    checks.push(Check {
        name: "mcp_auth_mode".to_string(),
        ok: true,
        detail: auth_mode.to_string(),
    });

    match auth_mode {
        McpAuthMode::None => checks.push(Check {
            name: "mcp_bearer_token".to_string(),
            ok: true,
            detail: "skipped: mcp.auth.mode=none".to_string(),
        }),
        McpAuthMode::Bearer => checks.push(Check {
            name: "mcp_bearer_token".to_string(),
            ok: token_present,
            detail: if token_present {
                "WRITESTEAD_BEARER_TOKEN present".to_string()
            } else {
                "missing WRITESTEAD_BEARER_TOKEN".to_string()
            },
        }),
    }

    let liteparse = run_cmd("lit", &["--version"], 5).await;
    let liteparse_found = liteparse.ok;
    checks.push(Check {
        name: "liteparse_binary".to_string(),
        ok: true,
        detail: if liteparse_found {
            format!(
                "lit {}",
                clean_line(&format!("{} {}", liteparse.stdout, liteparse.stderr))
            )
        } else {
            "not found".to_string()
        },
    });

    let pdftotext = run_cmd("pdftotext", &["--help"], 5).await;
    let pdftotext_found = pdftotext.ok;
    checks.push(Check {
        name: "pdftotext_binary".to_string(),
        ok: true,
        detail: if pdftotext_found {
            clean_line(&format!("{} {}", pdftotext.stdout, pdftotext.stderr))
        } else {
            "not found".to_string()
        },
    });

    checks.push(Check {
        name: "liteparse_formats".to_string(),
        ok: true,
        detail: "PDF, DOCX, PPTX, XLSX, PNG/JPG/TIFF/WebP OCR".to_string(),
    });

    if !liteparse_found && !pdftotext_found {
        checks.push(Check {
            name: "raw_extractors".to_string(),
            ok: true,
            detail: "warning: lit and pdftotext not found, raw_read text files still work"
                .to_string(),
        });
    }

    let require_fast_backend = matches!(cfg.search.backend, SearchBackend::RgFd);

    let rg = run_cmd("rg", &["--version"], 5).await;
    let rg_found = rg.ok;
    checks.push(Check {
        name: "rg_binary".to_string(),
        ok: if require_fast_backend { rg_found } else { true },
        detail: if rg_found {
            clean_line(&rg.stdout)
        } else if require_fast_backend {
            "not found (required by search.backend=rg-fd)".to_string()
        } else {
            "not found (using builtin search)".to_string()
        },
    });

    let fd_check = check_fd_binary().await;
    checks.push(Check {
        name: "fd_binary".to_string(),
        ok: if require_fast_backend {
            fd_check.found
        } else {
            true
        },
        detail: if fd_check.found {
            fd_check.detail
        } else if require_fast_backend {
            "not found (required by search.backend=rg-fd)".to_string()
        } else {
            "not found (using builtin list)".to_string()
        },
    });

    match cfg.sync.backend {
        SyncBackend::None => {
            checks.push(Check {
                name: "obsidian_binary".to_string(),
                ok: true,
                detail: "skipped: sync.backend=none".to_string(),
            });
            checks.push(Check {
                name: "obsidian_login".to_string(),
                ok: true,
                detail: "skipped: sync.backend=none".to_string(),
            });
            checks.push(Check {
                name: "obsidian_vault".to_string(),
                ok: true,
                detail: "skipped: sync.backend=none".to_string(),
            });
        }
        SyncBackend::Obsidian => {
            let bin = run_cmd("ob", &["--version"], 5).await;
            checks.push(Check {
                name: "obsidian_binary".to_string(),
                ok: bin.ok,
                detail: if bin.ok {
                    format!("ob {}", clean_line(&bin.stdout))
                } else {
                    failure_detail(&bin)
                },
            });

            if bin.ok {
                let login = run_cmd("ob", &["login"], 8).await;
                let login_text = format!("{}\n{}", login.stdout, login.stderr).to_lowercase();
                let signed_in = login.ok && login_text.contains("logged in as");

                checks.push(Check {
                    name: "obsidian_login".to_string(),
                    ok: signed_in,
                    detail: if signed_in {
                        clean_line(&format!("{} {}", login.stdout, login.stderr))
                    } else if login.ok {
                        format!("unexpected login output: {}", clean_line(&login.stdout))
                    } else {
                        failure_detail(&login)
                    },
                });

                let status = run_cmd("ob", &["sync-status", "--path", &cfg.vault_path], 10).await;
                let location_matches = status_location_matches(&status.stdout, &cfg.vault_path);
                let status_ok = status.ok && location_matches;
                checks.push(Check {
                    name: "obsidian_vault".to_string(),
                    ok: status_ok,
                    detail: if status.ok {
                        let summary = summarize_sync_status(&status.stdout);
                        if location_matches {
                            summary
                        } else {
                            format!("location mismatch: {}", summary)
                        }
                    } else {
                        failure_detail(&status)
                    },
                });
            } else {
                checks.push(Check {
                    name: "obsidian_login".to_string(),
                    ok: false,
                    detail: "skipped: ob binary check failed".to_string(),
                });
                checks.push(Check {
                    name: "obsidian_vault".to_string(),
                    ok: false,
                    detail: "skipped: ob binary check failed".to_string(),
                });
            }
        }
    }

    let overall_ok = checks.iter().all(|c| c.ok);

    let report = DoctorReport {
        ok: overall_ok,
        config_file,
        vault_path: cfg.vault_path,
        sync_backend: cfg.sync.backend.to_string(),
        search_backend: cfg.search.backend.to_string(),
        checks,
    };

    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    println!("doctor: {}", if report.ok { "ok" } else { "fail" });
    println!("config: {}", report.config_file);
    println!("vault: {}", report.vault_path);
    println!("sync backend: {}", report.sync_backend);
    println!("search backend: {}", report.search_backend);
    println!();
    for check in &report.checks {
        println!(
            "- {}: {} ({})",
            check.name,
            if check.ok { "ok" } else { "fail" },
            check.detail
        );
    }

    if !report.ok {
        anyhow::bail!("doctor checks failed");
    }

    Ok(())
}

#[derive(Debug)]
struct CmdResult {
    ok: bool,
    stdout: String,
    stderr: String,
    reason: Option<String>,
}

#[derive(Debug)]
struct FdBinaryCheck {
    found: bool,
    detail: String,
}

async fn run_cmd(program: &str, args: &[&str], timeout_sec: u64) -> CmdResult {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let out = timeout(Duration::from_secs(timeout_sec), cmd.output()).await;
    match out {
        Ok(Ok(output)) => CmdResult {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).trim().to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            reason: None,
        },
        Ok(Err(err)) => CmdResult {
            ok: false,
            stdout: String::new(),
            stderr: String::new(),
            reason: Some(err.to_string()),
        },
        Err(_) => CmdResult {
            ok: false,
            stdout: String::new(),
            stderr: String::new(),
            reason: Some(format!("timeout after {}s", timeout_sec)),
        },
    }
}

async fn check_fd_binary() -> FdBinaryCheck {
    let fd = run_cmd("fd", &["--version"], 5).await;
    if fd.ok {
        return FdBinaryCheck {
            found: true,
            detail: clean_line(&fd.stdout),
        };
    }

    let fdfind = run_cmd("fdfind", &["--version"], 5).await;
    if fdfind.ok {
        return FdBinaryCheck {
            found: true,
            detail: format!("fdfind {}", clean_line(&fdfind.stdout)),
        };
    }

    FdBinaryCheck {
        found: false,
        detail: "not found".to_string(),
    }
}

fn clean_line(text: &str) -> String {
    text.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

fn find_prefixed_line<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.lines()
        .map(|line| line.trim())
        .find_map(|line| line.strip_prefix(prefix).map(|v| v.trim()))
}

fn summarize_sync_status(text: &str) -> String {
    let vault = find_prefixed_line(text, "Vault:").unwrap_or("unknown");
    let location = find_prefixed_line(text, "Location:").unwrap_or("unknown");
    let mode = find_prefixed_line(text, "Sync mode:").unwrap_or("unknown");
    format!("vault={} location={} mode={}", vault, location, mode)
}

fn status_location_matches(status_stdout: &str, configured_vault_path: &str) -> bool {
    let Some(location) = find_prefixed_line(status_stdout, "Location:") else {
        return false;
    };

    let status_path = std::path::Path::new(location);
    let configured_path = std::path::Path::new(configured_vault_path);

    let status_norm =
        std::fs::canonicalize(status_path).unwrap_or_else(|_| status_path.to_path_buf());
    let configured_norm =
        std::fs::canonicalize(configured_path).unwrap_or_else(|_| configured_path.to_path_buf());

    status_norm == configured_norm
}

fn failure_detail(result: &CmdResult) -> String {
    if let Some(reason) = &result.reason {
        return reason.clone();
    }

    let stderr = clean_line(&result.stderr);
    if !stderr.is_empty() {
        return stderr;
    }

    let stdout = clean_line(&result.stdout);
    if !stdout.is_empty() {
        return stdout;
    }

    "command failed".to_string()
}

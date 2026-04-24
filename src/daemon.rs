use crate::config::{self, AppConfig};
use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub async fn start_background(
    config: &AppConfig,
    host: Option<String>,
    port: Option<u16>,
) -> Result<()> {
    config::ensure_runtime_dir()?;

    if let Some(pid) = read_pid()? {
        if process_alive(pid) {
            println!("already running (pid {})", pid);
            if let Some(health) = fetch_health(config).await? {
                println!("health: {}", health);
            }
            return Ok(());
        }
        remove_pid_file()?;
    }

    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    let log_path = config::log_file_path();
    let mut log_file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open log file {}", log_path.display()))?;
    writeln!(log_file, "[writestead] starting daemon")?;
    let log_file_err = log_file
        .try_clone()
        .context("failed to clone daemon log file handle")?;

    let mut cmd = Command::new(exe);
    cmd.arg("start").arg("--foreground");
    if let Some(h) = &host {
        cmd.arg("--host").arg(h);
    }
    if let Some(p) = port {
        cmd.arg("--port").arg(p.to_string());
    }

    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(log_file_err));

    let child = cmd.spawn().context("failed to spawn background daemon")?;
    let pid_u32 = child.id();
    let pid = i32::try_from(pid_u32).map_err(|_| anyhow!("pid overflow: {}", pid_u32))?;
    write_pid(pid)?;

    let probe_host = host.unwrap_or_else(|| config.host.clone());
    let probe_port = port.unwrap_or(config.port);

    let mut started = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;

        if !process_alive(pid) {
            break;
        }

        if probe_port_open(&probe_host, probe_port).await {
            started = true;
            break;
        }
    }

    if !started {
        remove_pid_file()?;
        return Err(anyhow!(
            "daemon failed to become ready; check log {}",
            log_path.display()
        ));
    }

    println!("started daemon (pid {})", pid);
    println!("daemon url: {}", config::daemon_url(config));
    println!("log: {}", log_path.display());
    Ok(())
}

pub fn write_pid(pid: i32) -> Result<()> {
    let pid_path = config::pid_file_path();
    if let Some(parent) = pid_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&pid_path, format!("{}\n", pid))
        .with_context(|| format!("failed to write pid file {}", pid_path.display()))?;
    Ok(())
}

pub fn read_pid() -> Result<Option<i32>> {
    let pid_path = config::pid_file_path();
    if !pid_path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&pid_path)
        .with_context(|| format!("failed to read pid file {}", pid_path.display()))?;
    let pid = raw
        .trim()
        .parse::<i32>()
        .with_context(|| format!("invalid pid in {}", pid_path.display()))?;
    Ok(Some(pid))
}

pub fn remove_pid_file() -> Result<()> {
    let pid_path = config::pid_file_path();
    if pid_path.exists() {
        fs::remove_file(&pid_path)
            .with_context(|| format!("failed to remove pid file {}", pid_path.display()))?;
    }
    Ok(())
}

pub fn cleanup_pid_file_if_current_process() -> Result<()> {
    let Some(pid_from_file) = read_pid()? else {
        return Ok(());
    };

    let self_pid = i32::try_from(std::process::id())
        .map_err(|_| anyhow!("self pid overflow: {}", std::process::id()))?;

    if pid_from_file == self_pid {
        remove_pid_file()?;
    }
    Ok(())
}

pub fn process_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }

    let rc = unsafe { libc::kill(pid, 0) };
    if rc == 0 {
        return true;
    }

    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EPERM)
    )
}

pub fn stop_process(pid: i32) -> Result<()> {
    if pid <= 0 {
        return Ok(());
    }

    let _ = send_signal(pid, libc::SIGTERM)?;

    for _ in 0..20 {
        if !process_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    let _ = send_signal(pid, libc::SIGKILL)?;

    for _ in 0..10 {
        if !process_alive(pid) {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    Err(anyhow!("failed to stop pid {}", pid))
}

pub async fn fetch_health(config: &AppConfig) -> Result<Option<Value>> {
    let host = if config.host == "0.0.0.0" {
        "127.0.0.1".to_string()
    } else {
        config.host.clone()
    };
    fetch_health_raw(&host, config.port).await
}

pub async fn fetch_health_raw(host: &str, port: u16) -> Result<Option<Value>> {
    let target = format!("{}:{}", host, port);

    let stream = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(&target)).await;
    let Ok(Ok(mut stream)) = stream else {
        return Ok(None);
    };

    let request = format!(
        "GET /health HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n\r\n",
        host, port
    );

    tokio::time::timeout(Duration::from_secs(2), stream.write_all(request.as_bytes()))
        .await
        .context("health request write timed out")?
        .context("failed to write health request")?;

    let mut buf = Vec::new();
    let read_result =
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf)).await;
    let Ok(Ok(_)) = read_result else {
        return Ok(None);
    };

    let response = String::from_utf8_lossy(&buf);
    let Some((_, body)) = response.split_once("\r\n\r\n") else {
        return Ok(None);
    };

    let parsed = serde_json::from_str::<Value>(body.trim()).ok();
    Ok(parsed)
}

fn send_signal(pid: i32, signal: i32) -> Result<bool> {
    let rc = unsafe { libc::kill(pid, signal) };
    if rc == 0 {
        return Ok(true);
    }

    let err = std::io::Error::last_os_error();
    if matches!(err.raw_os_error(), Some(libc::ESRCH)) {
        return Ok(false);
    }

    Err(anyhow!(
        "failed to send signal {} to pid {}: {}",
        signal,
        pid,
        err
    ))
}

async fn probe_port_open(host: &str, port: u16) -> bool {
    let probe_host = if host == "0.0.0.0" { "127.0.0.1" } else { host };
    let target = format!("{}:{}", probe_host, port);

    let attempt = tokio::time::timeout(Duration::from_secs(1), TcpStream::connect(target)).await;
    matches!(attempt, Ok(Ok(_)))
}

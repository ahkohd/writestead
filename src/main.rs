use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use writestead::config::{self, AppConfig, SyncBackend};
use writestead::raw::RawOps;
use writestead::wiki::WikiOps;
use writestead::{daemon, doctor, guide, server, syncer, vault};

#[derive(Debug, Parser)]
#[command(name = "writestead")]
#[command(about = "LLM Wiki")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    Init {
        #[arg(long)]
        vault_path: Option<String>,

        #[arg(long)]
        name: Option<String>,

        #[arg(long, value_parser = ["obsidian", "none"])]
        sync_backend: Option<String>,

        #[arg(long)]
        host: Option<String>,

        #[arg(long)]
        port: Option<u16>,

        #[arg(long)]
        force: bool,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },
    Raw {
        #[command(subcommand)]
        command: RawCommands,
    },
    #[command(hide = true)]
    RawList {
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    #[command(hide = true)]
    RawRead {
        path: String,
        #[arg(long, default_value_t = 1)]
        offset: usize,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long)]
        page_start: Option<u32>,
        #[arg(long)]
        page_end: Option<u32>,
    },
    #[command(hide = true)]
    RawAdd {
        source: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        force: bool,
    },
    Read {
        path: String,
        #[arg(long, default_value_t = 1)]
        offset: usize,
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    Search {
        query: String,
    },
    Edit {
        path: String,
        #[arg(long)]
        old_text: String,
        #[arg(long)]
        new_text: String,
        #[arg(long)]
        log_action: String,
        #[arg(long)]
        log_description: String,
    },
    Write {
        path: String,
        #[arg(long)]
        content: Option<String>,
        #[arg(long)]
        content_file: Option<String>,
        #[arg(long, default_value = "create")]
        log_action: String,
        #[arg(long)]
        log_description: Option<String>,
    },
    List {
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Lint {
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        dry_run: bool,
    },
    Index {
        #[arg(long, default_value_t = 1)]
        offset: usize,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    Sync,
    Doctor {
        #[arg(long)]
        json: bool,
    },
    HelpWiki,
    Start {
        #[arg(long)]
        host: Option<String>,

        #[arg(long)]
        port: Option<u16>,

        #[arg(long)]
        foreground: bool,
    },
    Stop,
    Status {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum ConfigCommands {
    Path,
    Show,
    Get { key: String },
    Set { key: String, value: String },
    Unset { key: String },
}

#[derive(Debug, Subcommand)]
enum RawCommands {
    List {
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    Read {
        path: String,
        #[arg(long, default_value_t = 1)]
        offset: usize,
        #[arg(long, default_value_t = 200)]
        limit: usize,
        #[arg(long)]
        page_start: Option<u32>,
        #[arg(long)]
        page_end: Option<u32>,
    },
    Add {
        source: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        force: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "writestead=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Init {
            vault_path,
            name,
            sync_backend,
            host,
            port,
            force,
        } => cmd_init(vault_path, name, sync_backend, host, port, force),
        Commands::Config { command } => cmd_config(command),
        Commands::Raw { command } => cmd_raw(command).await,
        Commands::RawList { offset, limit } => cmd_raw_list(offset, limit),
        Commands::RawRead {
            path,
            offset,
            limit,
            page_start,
            page_end,
        } => cmd_raw_read(path, offset, limit, page_start, page_end).await,
        Commands::RawAdd {
            source,
            name,
            force,
        } => cmd_raw_add(source, name, force).await,
        Commands::Read {
            path,
            offset,
            limit,
        } => cmd_read(path, offset, limit),
        Commands::Search { query } => cmd_search(query),
        Commands::Edit {
            path,
            old_text,
            new_text,
            log_action,
            log_description,
        } => cmd_edit(path, old_text, new_text, log_action, log_description),
        Commands::Write {
            path,
            content,
            content_file,
            log_action,
            log_description,
        } => cmd_write(path, content, content_file, log_action, log_description),
        Commands::List { offset, limit } => cmd_list(offset, limit),
        Commands::Lint { fix, dry_run } => cmd_lint(fix, dry_run),
        Commands::Index { offset, limit } => cmd_index(offset, limit),
        Commands::Sync => cmd_sync().await,
        Commands::Doctor { json } => doctor::run(json).await,
        Commands::HelpWiki => cmd_help_wiki(),
        Commands::Start {
            host,
            port,
            foreground,
        } => cmd_start(host, port, foreground).await,
        Commands::Stop => cmd_stop(),
        Commands::Status { json } => cmd_status(json).await,
    }
}

fn cmd_init(
    vault_path: Option<String>,
    name: Option<String>,
    sync_backend: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    force: bool,
) -> Result<()> {
    let mut cfg = config::load_or_default()?;

    if let Some(path) = vault_path {
        cfg.vault_path = config::expand_tilde(&path);
    }
    if let Some(name) = name {
        cfg.name = name;
    }
    if let Some(backend) = sync_backend {
        cfg.sync.backend = backend.parse::<SyncBackend>()?;
    }
    if let Some(host) = host {
        cfg.host = host;
    }
    if let Some(port) = port {
        cfg.port = port;
    }

    let summary = vault::init_vault(&cfg, force)?;
    config::save(&cfg)?;

    println!("initialized vault at {}", cfg.vault_path);
    println!("config: {}", config::config_file_path().display());
    println!(
        "files created: {}/{}",
        summary.created_files, summary.touched_files
    );
    println!("sync backend: {}", cfg.sync.backend);

    Ok(())
}

fn cmd_config(command: ConfigCommands) -> Result<()> {
    match command {
        ConfigCommands::Path => {
            println!("{}", config::config_file_path().display());
        }
        ConfigCommands::Show => {
            let cfg = config::load_or_default()?;
            println!("{}", serde_json::to_string_pretty(&cfg)?);
        }
        ConfigCommands::Get { key } => {
            let cfg = config::load_or_default()?;
            let value = config::get_value(&cfg, &key)?;
            if let Some(s) = value.as_str() {
                println!("{}", s);
            } else {
                println!("{}", value);
            }
        }
        ConfigCommands::Set { key, value } => {
            let mut cfg = config::load_or_default()?;
            config::set_value(&mut cfg, &key, &value)?;
            config::save(&cfg)?;
            println!("set {}", key);
        }
        ConfigCommands::Unset { key } => {
            let mut cfg = config::load_or_default()?;
            config::unset_value(&mut cfg, &key)?;
            config::save(&cfg)?;
            println!("unset {}", key);
        }
    }
    Ok(())
}

async fn cmd_raw(command: RawCommands) -> Result<()> {
    match command {
        RawCommands::List { offset, limit } => cmd_raw_list(offset, limit),
        RawCommands::Read {
            path,
            offset,
            limit,
            page_start,
            page_end,
        } => cmd_raw_read(path, offset, limit, page_start, page_end).await,
        RawCommands::Add {
            source,
            name,
            force,
        } => cmd_raw_add(source, name, force).await,
    }
}

fn cmd_raw_list(offset: usize, limit: usize) -> Result<()> {
    let cfg = config::load_or_default()?;
    let raw = RawOps::new(cfg);
    let result = raw.list_sources(offset, limit)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn cmd_raw_read(
    path: String,
    offset: usize,
    limit: usize,
    page_start: Option<u32>,
    page_end: Option<u32>,
) -> Result<()> {
    let cfg = config::load_or_default()?;
    let raw = RawOps::new(cfg);
    let result = raw
        .read_source_with_options(
            &path,
            writestead::raw::RawReadOptions {
                offset,
                limit,
                page_start,
                page_end,
            },
        )
        .await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn cmd_raw_add(source: String, name: Option<String>, force: bool) -> Result<()> {
    let cfg = config::load_or_default()?;
    let raw = RawOps::new(cfg);
    let result = raw.add_source(&source, name.as_deref(), force).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn cmd_read(path: String, offset: usize, limit: usize) -> Result<()> {
    let cfg = config::load_or_default()?;
    let wiki = WikiOps::new(cfg);
    let page = wiki.read_page(&path, offset, limit)?;
    println!("{}", serde_json::to_string_pretty(&page)?);
    Ok(())
}

fn cmd_search(query: String) -> Result<()> {
    let cfg = config::load_or_default()?;
    let wiki = WikiOps::new(cfg);
    let results = wiki.search(&query)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({ "results": results }))?
    );
    Ok(())
}

fn cmd_edit(
    path: String,
    old_text: String,
    new_text: String,
    log_action: String,
    log_description: String,
) -> Result<()> {
    let cfg = config::load_or_default()?;
    let wiki = WikiOps::new(cfg);

    let edits = vec![(old_text, new_text)];
    wiki.edit_page(&path, &edits)?;

    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    wiki.append_log(&date, &log_action, &log_description)?;

    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({ "ok": true, "path": path, "updated": true })
        )?
    );
    Ok(())
}

fn cmd_write(
    path: String,
    content: Option<String>,
    content_file: Option<String>,
    log_action: String,
    log_description: Option<String>,
) -> Result<()> {
    let cfg = config::load_or_default()?;
    let wiki = WikiOps::new(cfg);

    let body = resolve_content_arg(content, content_file)?;
    wiki.write_page(&path, &body)?;

    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let description = log_description.as_deref().unwrap_or(&path);
    wiki.append_log(&date, &log_action, description)?;

    println!(
        "{}",
        serde_json::to_string_pretty(
            &serde_json::json!({ "ok": true, "path": path, "created": true })
        )?
    );
    Ok(())
}

fn cmd_list(offset: usize, limit: usize) -> Result<()> {
    let cfg = config::load_or_default()?;
    let wiki = WikiOps::new(cfg);
    let result = wiki.list_pages_paginated(offset, limit)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

fn cmd_lint(fix: bool, dry_run: bool) -> Result<()> {
    let cfg = config::load_or_default()?;
    let wiki = WikiOps::new(cfg);
    let report = wiki.lint_with_options(writestead::wiki::LintOptions { fix, dry_run })?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn cmd_index(offset: usize, limit: usize) -> Result<()> {
    let cfg = config::load_or_default()?;
    let wiki = WikiOps::new(cfg);
    let page = wiki.read_page("wiki/index.md", offset, limit)?;
    println!("{}", serde_json::to_string_pretty(&page)?);
    Ok(())
}

async fn cmd_sync() -> Result<()> {
    let cfg = config::load_or_default()?;
    let result = syncer::sync_once_with_trigger(&cfg, "cli").await?;
    println!("sync backend: {}", result.backend);
    println!("{}", result.message);
    Ok(())
}

fn cmd_help_wiki() -> Result<()> {
    println!("{}", guide::wiki_help_text());
    Ok(())
}

async fn cmd_start(host: Option<String>, port: Option<u16>, foreground: bool) -> Result<()> {
    let mut cfg: AppConfig = config::load_or_default()?;
    if let Some(host) = &host {
        cfg.host = host.clone();
    }
    if let Some(port) = port {
        cfg.port = port;
    }

    if foreground {
        return server::run(cfg).await;
    }

    daemon::start_background(&cfg, host, Some(cfg.port)).await
}

fn cmd_stop() -> Result<()> {
    let pid = daemon::read_pid()?;
    let Some(pid) = pid else {
        println!("not running (no pid file)");
        return Ok(());
    };

    if !daemon::process_alive(pid) {
        daemon::remove_pid_file()?;
        println!("not running (stale pid {})", pid);
        return Ok(());
    }

    daemon::stop_process(pid)?;
    daemon::remove_pid_file()?;
    println!("stopped daemon pid {}", pid);
    Ok(())
}

async fn cmd_status(json_output: bool) -> Result<()> {
    let cfg = config::load_or_default()?;
    let pid = daemon::read_pid()?;
    let health = daemon::fetch_health(&cfg).await?;

    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "pid": pid,
                "pid_alive": pid.map(daemon::process_alive).unwrap_or(false),
                "daemon_url": config::daemon_url(&cfg),
                "health": health,
            }))?
        );
        return Ok(());
    }

    if let Some(pid) = pid {
        let alive = daemon::process_alive(pid);
        println!("pid: {} ({})", pid, if alive { "alive" } else { "dead" });
    } else {
        println!("pid: none");
    }

    println!("daemon url: {}", config::daemon_url(&cfg));

    match health {
        Some(health) => {
            println!("health: ok");
            println!("{}", serde_json::to_string_pretty(&health)?);
        }
        None => {
            println!("health: unreachable");
        }
    }

    Ok(())
}

fn resolve_content_arg(content: Option<String>, content_file: Option<String>) -> Result<String> {
    match (content, content_file) {
        (Some(body), None) => Ok(body),
        (None, Some(path)) => {
            let full = config::expand_tilde(&path);
            std::fs::read_to_string(&full).with_context(|| format!("failed to read {}", full))
        }
        (Some(_), Some(_)) => Err(anyhow!("use either --content or --content-file, not both")),
        (None, None) => Err(anyhow!("missing content: use --content or --content-file")),
    }
}

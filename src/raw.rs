use crate::config::{AppConfig, SearchBackend};
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::time::Duration;
use tokio::process::Command;
use walkdir::WalkDir;

const MAX_DIRECT_READ_BYTES: u64 = 10 * 1024 * 1024;
const MAX_EXTRACTED_TEXT_CHARS: usize = 2_000_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawListResult {
    pub files: Vec<String>,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawReadResult {
    pub path: String,
    pub size_bytes: u64,
    pub content: String,
    pub format: String,
    pub extractor: String,
    pub offset: usize,
    pub limit: usize,
    pub total_lines: usize,
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawAddResult {
    pub ok: bool,
    pub path: String,
    pub size_bytes: u64,
    pub source: String,
}

#[derive(Debug, Clone, Copy)]
pub struct RawReadOptions {
    pub offset: usize,
    pub limit: usize,
    pub page_start: Option<u32>,
    pub page_end: Option<u32>,
}

impl RawReadOptions {
    pub fn lines(offset: usize, limit: usize) -> Self {
        Self {
            offset,
            limit,
            page_start: None,
            page_end: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct PdfPageRange {
    start: u32,
    end: u32,
}

#[derive(Debug)]
pub struct RawReadFailure {
    extractor: &'static str,
    message: String,
}

impl RawReadFailure {
    pub fn extractor(&self) -> &'static str {
        self.extractor
    }
}

impl Display for RawReadFailure {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for RawReadFailure {}

fn raw_read_failure(extractor: &'static str, err: anyhow::Error) -> anyhow::Error {
    anyhow::Error::new(RawReadFailure {
        extractor,
        message: format!("{:#}", err),
    })
}

#[derive(Debug, Clone)]
pub struct RawOps {
    config: AppConfig,
}

impl RawOps {
    pub fn new(config: AppConfig) -> Self {
        Self { config }
    }

    pub fn list_sources(&self, offset: usize, limit: usize) -> Result<RawListResult> {
        let mut all_files = self.collect_source_files()?;
        all_files.sort();

        let total = all_files.len();
        let safe_limit = limit.max(1);
        let safe_offset = offset.min(total);
        let end = (safe_offset + safe_limit).min(total);

        Ok(RawListResult {
            files: all_files[safe_offset..end].to_vec(),
            total,
            offset: safe_offset,
            limit: safe_limit,
            has_more: end < total,
        })
    }

    fn collect_source_files(&self) -> Result<Vec<String>> {
        let root = self.raw_root();
        if !root.exists() {
            return Ok(vec![]);
        }

        match self.config.search.backend {
            SearchBackend::Builtin => self.collect_source_files_builtin(&root),
            SearchBackend::Auto => {
                if let Some(fd_program) = detect_fd_program() {
                    self.collect_source_files_with_fd(&root, fd_program)
                } else {
                    self.collect_source_files_builtin(&root)
                }
            }
            SearchBackend::RgFd => {
                let Some(fd_program) = detect_fd_program() else {
                    anyhow::bail!("search.backend=rg-fd requires 'fd' or 'fdfind' in PATH");
                };
                self.collect_source_files_with_fd(&root, fd_program)
            }
        }
    }

    fn collect_source_files_builtin(&self, root: &Path) -> Result<Vec<String>> {
        let mut all_files = Vec::new();
        for entry in WalkDir::new(root)
            .into_iter()
            .filter_entry(|entry| !is_assets_dir(entry.path(), root))
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_file())
        {
            let rel = entry
                .path()
                .strip_prefix(root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .to_string();
            all_files.push(rel);
        }
        Ok(all_files)
    }

    fn collect_source_files_with_fd(&self, root: &Path, fd_program: &str) -> Result<Vec<String>> {
        let vault_root = PathBuf::from(&self.config.vault_path);
        let output = StdCommand::new(fd_program)
            .current_dir(&vault_root)
            .args(["-t", "f", ".", "raw", "--exclude", "assets"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .with_context(|| format!("failed to execute {}", fd_program))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            if stderr.is_empty() {
                anyhow::bail!("{} failed", fd_program);
            }
            anyhow::bail!("{} failed: {}", fd_program, stderr);
        }

        let mut files = Vec::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let value = line.trim();
            if value.is_empty() {
                continue;
            }

            let rel = Path::new(value);
            let rel_from_raw = rel.strip_prefix("raw").unwrap_or(rel);
            if rel_from_raw.as_os_str().is_empty() {
                continue;
            }

            let normalized = rel_from_raw.to_string_lossy().to_string();
            if normalized.starts_with("assets/") {
                continue;
            }

            let full = root.join(&normalized);
            if full.is_file() {
                files.push(normalized);
            }
        }

        Ok(files)
    }

    pub async fn read_source(
        &self,
        raw_path: &str,
        offset: usize,
        limit: usize,
    ) -> Result<RawReadResult> {
        self.read_source_with_options(raw_path, RawReadOptions::lines(offset, limit))
            .await
    }

    pub async fn read_source_with_options(
        &self,
        raw_path: &str,
        options: RawReadOptions,
    ) -> Result<RawReadResult> {
        let rel = sanitize_raw_rel_path(raw_path)?;
        if is_assets_rel(&rel) {
            anyhow::bail!("raw/assets is not supported yet: {}", raw_path);
        }

        let full = self.raw_root().join(&rel);
        let meta =
            fs::metadata(&full).with_context(|| format!("failed to stat {}", full.display()))?;
        if !meta.is_file() {
            anyhow::bail!("not a file: {}", full.display());
        }

        let ext = full
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();

        let path_out = format!("raw/{}", rel.to_string_lossy());
        let safe_offset = options.offset.max(1);
        let safe_limit = options.limit.max(1);

        if is_text_ext(&ext) {
            let text = read_text_file_guarded(&full, meta.len())?;
            let page = paginate_lines(&text, safe_offset, safe_limit);
            return Ok(RawReadResult {
                path: path_out,
                size_bytes: meta.len(),
                content: page.content,
                format: "text".to_string(),
                extractor: "direct".to_string(),
                offset: page.offset,
                limit: page.limit,
                total_lines: page.total_lines,
                has_more: page.has_more,
            });
        }

        if ext == "pdf" {
            let requested_range = pdf_page_range(options.page_start, options.page_end)?;
            let page_count = pdf_page_count(&full).await.ok();
            let range = clamp_pdf_page_range(requested_range, page_count)?;

            let max_liteparse_pages = self.config.raw.pdf_liteparse_max_pages.max(1);
            let should_use_liteparse = range.is_none()
                && page_count
                    .map(|pages| pages <= max_liteparse_pages)
                    .unwrap_or(true);

            let (parsed, extractor) = if should_use_liteparse {
                match run_liteparse(
                    &full,
                    self.config.raw.pdf_liteparse_timeout_ms,
                    self.config.raw.pdf_liteparse_mem_limit_mb,
                )
                .await
                {
                    Ok(text) => (text, "liteparse".to_string()),
                    Err(err) => {
                        tracing::warn!(
                            "liteparse failed for {}; falling back to pdftotext: {}",
                            full.display(),
                            err
                        );
                        (
                            run_pdftotext(&full, range)
                                .await
                                .map_err(|err| raw_read_failure("pdftotext", err))?,
                            "pdftotext".to_string(),
                        )
                    }
                }
            } else {
                (
                    run_pdftotext(&full, range)
                        .await
                        .map_err(|err| raw_read_failure("pdftotext", err))?,
                    "pdftotext".to_string(),
                )
            };

            let bounded = clamp_text(parsed);
            let page = paginate_lines(&bounded, safe_offset, safe_limit);

            return Ok(RawReadResult {
                path: path_out,
                size_bytes: meta.len(),
                content: page.content,
                format: "parsed".to_string(),
                extractor,
                offset: page.offset,
                limit: page.limit,
                total_lines: page.total_lines,
                has_more: page.has_more,
            });
        }

        if is_lit_only_ext(&ext) {
            let parsed = run_liteparse(
                &full,
                self.config.raw.pdf_liteparse_timeout_ms,
                self.config.raw.pdf_liteparse_mem_limit_mb,
            )
            .await
            .with_context(|| {
                format!(
                    "failed to parse {} with liteparse, install 'lit' if missing",
                    full.display()
                )
            })
            .map_err(|err| raw_read_failure("liteparse", err))?;
            let bounded = clamp_text(parsed);
            let page = paginate_lines(&bounded, safe_offset, safe_limit);
            return Ok(RawReadResult {
                path: path_out,
                size_bytes: meta.len(),
                content: page.content,
                format: "parsed".to_string(),
                extractor: "liteparse".to_string(),
                offset: page.offset,
                limit: page.limit,
                total_lines: page.total_lines,
                has_more: page.has_more,
            });
        }

        anyhow::bail!("unsupported file type: .{}", ext);
    }

    pub async fn add_source(
        &self,
        source: &str,
        name: Option<&str>,
        force: bool,
    ) -> Result<RawAddResult> {
        if is_http_url(source) {
            self.add_from_url(source, name, force).await
        } else {
            self.add_from_local_source(source, name, force)
        }
    }

    pub fn upload_from_path(
        &self,
        source_path: &str,
        name: &str,
        overwrite: bool,
    ) -> Result<RawAddResult> {
        // Trust boundary: MCP path mode is sandboxed to vault paths only.
        // CLI raw add intentionally allows absolute paths outside vault.
        let vault_root = PathBuf::from(&self.config.vault_path);
        let source = resolve_vault_source_path(&vault_root, source_path)?;
        self.copy_local_file_to_raw(&source, name, overwrite, "path")
    }

    pub async fn upload_from_url(
        &self,
        url: &str,
        name: &str,
        overwrite: bool,
    ) -> Result<RawAddResult> {
        self.download_url_to_raw(url, Some(name), overwrite, "url")
            .await
    }

    pub fn upload_from_content(
        &self,
        b64_content: &str,
        name: &str,
        overwrite: bool,
    ) -> Result<RawAddResult> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(b64_content)
            .context("invalid base64 content")?;

        let max = self.config.raw.upload_max_bytes;
        let size = bytes.len() as u64;
        if size > max {
            anyhow::bail!(
                "content too large: {} bytes exceeds {} bytes limit",
                size,
                max
            );
        }

        self.write_bytes_to_raw(bytes, name, overwrite, "content")
    }

    fn add_from_local_source(
        &self,
        source: &str,
        name: Option<&str>,
        force: bool,
    ) -> Result<RawAddResult> {
        let local_path = resolve_local_source_path(source)?;
        self.copy_local_file_to_raw(&local_path, name.unwrap_or(""), force, "local")
    }

    async fn add_from_url(
        &self,
        source: &str,
        name: Option<&str>,
        force: bool,
    ) -> Result<RawAddResult> {
        self.download_url_to_raw(source, name, force, "url").await
    }

    async fn download_url_to_raw(
        &self,
        source: &str,
        name_override: Option<&str>,
        overwrite: bool,
        source_kind: &str,
    ) -> Result<RawAddResult> {
        let url =
            reqwest::Url::parse(source).with_context(|| format!("invalid url: {}", source))?;
        let file_name = if let Some(name) = name_override {
            sanitize_dest_file_name(name)?
        } else {
            infer_name_from_url(&url)?
        };

        let timeout = Duration::from_secs(self.config.raw.url_timeout_seconds.max(1));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("failed to build http client")?;

        let resp = client
            .get(url)
            .send()
            .await
            .context("failed to download url")?;

        if !resp.status().is_success() {
            anyhow::bail!("download failed with status {}", resp.status());
        }

        let max = self.config.raw.upload_max_bytes;
        if let Some(len) = resp.content_length() {
            if len > max {
                anyhow::bail!(
                    "content too large: {} bytes exceeds {} bytes limit",
                    len,
                    max
                );
            }
        }

        let bytes = resp.bytes().await.context("failed to read response body")?;
        if bytes.len() as u64 > max {
            anyhow::bail!(
                "content too large: {} bytes exceeds {} bytes limit",
                bytes.len(),
                max
            );
        }

        self.write_bytes_to_raw(bytes.to_vec(), &file_name, overwrite, source_kind)
    }

    fn copy_local_file_to_raw(
        &self,
        source_path: &Path,
        name_or_empty: &str,
        overwrite: bool,
        source_kind: &str,
    ) -> Result<RawAddResult> {
        let meta = fs::metadata(source_path)
            .with_context(|| format!("failed to stat {}", source_path.display()))?;
        if !meta.is_file() {
            anyhow::bail!("not a file: {}", source_path.display());
        }

        if meta.len() > self.config.raw.upload_max_bytes {
            anyhow::bail!(
                "file too large: {} bytes exceeds {} bytes limit",
                meta.len(),
                self.config.raw.upload_max_bytes
            );
        }

        let file_name = if name_or_empty.is_empty() {
            source_path
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| anyhow!("failed to infer destination filename from source path"))?
                .to_string()
        } else {
            name_or_empty.to_string()
        };
        let file_name = sanitize_dest_file_name(&file_name)?;

        let dest = self.dest_path_for_name(&file_name)?;
        if dest.exists() && !overwrite {
            anyhow::bail!(
                "destination already exists: {} (use --force/overwrite)",
                dest.display()
            );
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(source_path, &dest).with_context(|| {
            format!(
                "failed to copy {} to {}",
                source_path.display(),
                dest.display()
            )
        })?;

        Ok(RawAddResult {
            ok: true,
            path: format!("raw/{}", file_name),
            size_bytes: meta.len(),
            source: source_kind.to_string(),
        })
    }

    fn write_bytes_to_raw(
        &self,
        bytes: Vec<u8>,
        name: &str,
        overwrite: bool,
        source_kind: &str,
    ) -> Result<RawAddResult> {
        let file_name = sanitize_dest_file_name(name)?;
        let dest = self.dest_path_for_name(&file_name)?;

        if dest.exists() && !overwrite {
            anyhow::bail!(
                "destination already exists: {} (use --force/overwrite)",
                dest.display()
            );
        }

        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        fs::write(&dest, &bytes).with_context(|| format!("failed to write {}", dest.display()))?;

        Ok(RawAddResult {
            ok: true,
            path: format!("raw/{}", file_name),
            size_bytes: bytes.len() as u64,
            source: source_kind.to_string(),
        })
    }

    fn dest_path_for_name(&self, file_name: &str) -> Result<PathBuf> {
        let root = self.raw_root();
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create {}", root.display()))?;
        Ok(root.join(file_name))
    }

    fn raw_root(&self) -> PathBuf {
        PathBuf::from(&self.config.vault_path).join("raw")
    }
}

fn is_http_url(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

fn sanitize_raw_rel_path(input: &str) -> Result<PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("path is required"));
    }

    let no_prefix = trimmed.strip_prefix("raw/").unwrap_or(trimmed);
    let raw = Path::new(no_prefix);
    if raw.is_absolute() {
        return Err(anyhow!("absolute path not allowed: {}", trimmed));
    }

    let mut out = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir => return Err(anyhow!("path traversal not allowed: {}", trimmed)),
            _ => return Err(anyhow!("invalid path component in {}", trimmed)),
        }
    }

    if out.as_os_str().is_empty() {
        return Err(anyhow!("path is required"));
    }

    Ok(out)
}

fn sanitize_vault_rel_path(input: &str) -> Result<PathBuf> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("path is required"));
    }

    let raw = Path::new(trimmed);
    if raw.is_absolute() {
        return Err(anyhow!("relative path expected: {}", trimmed));
    }

    let mut out = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir => return Err(anyhow!("path traversal not allowed: {}", trimmed)),
            _ => return Err(anyhow!("invalid path component in {}", trimmed)),
        }
    }

    if out.as_os_str().is_empty() {
        return Err(anyhow!("path is required"));
    }

    Ok(out)
}

fn sanitize_dest_file_name(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("name is required"));
    }

    let path = Path::new(trimmed);
    if path.is_absolute() {
        anyhow::bail!("name must be a filename, not absolute path: {}", trimmed);
    }
    if path.components().count() != 1 {
        anyhow::bail!("name must be a filename without directories: {}", trimmed);
    }
    if matches!(
        path.components().next(),
        Some(Component::ParentDir | Component::CurDir)
    ) {
        anyhow::bail!("invalid name: {}", trimmed);
    }

    Ok(trimmed.to_string())
}

fn infer_name_from_url(url: &reqwest::Url) -> Result<String> {
    let last = url
        .path_segments()
        .and_then(|mut segs| segs.next_back())
        .unwrap_or("")
        .trim();
    if last.is_empty() {
        anyhow::bail!("could not infer filename from url, use --name");
    }
    sanitize_dest_file_name(last)
}

fn resolve_local_source_path(source: &str) -> Result<PathBuf> {
    let path = Path::new(source);

    if path.is_absolute() {
        let canon = fs::canonicalize(path)
            .with_context(|| format!("failed to resolve source path {}", path.display()))?;
        return Ok(canon);
    }

    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        anyhow::bail!("path traversal not allowed: {}", source);
    }

    let canon = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve source path {}", path.display()))?;
    Ok(canon)
}

fn resolve_vault_source_path(vault_root: &Path, source_path: &str) -> Result<PathBuf> {
    let source = Path::new(source_path);

    if source.is_absolute() {
        let source_canon = fs::canonicalize(source)
            .with_context(|| format!("failed to resolve {}", source.display()))?;
        let vault_canon = fs::canonicalize(vault_root)
            .with_context(|| format!("failed to resolve {}", vault_root.display()))?;

        if !source_canon.starts_with(&vault_canon) {
            anyhow::bail!("absolute path must stay inside vault: {}", source.display());
        }
        return Ok(source_canon);
    }

    let rel = sanitize_vault_rel_path(source_path)?;
    Ok(vault_root.join(rel))
}

fn is_assets_rel(rel: &Path) -> bool {
    rel.components()
        .next()
        .map(|c| c.as_os_str() == "assets")
        .unwrap_or(false)
}

fn is_assets_dir(path: &Path, raw_root: &Path) -> bool {
    if !path.is_dir() {
        return false;
    }

    path.strip_prefix(raw_root)
        .ok()
        .and_then(|rel| rel.components().next())
        .map(|c| c.as_os_str() == "assets")
        .unwrap_or(false)
}

fn is_text_ext(ext: &str) -> bool {
    matches!(
        ext,
        "md" | "markdown"
            | "txt"
            | "json"
            | "yaml"
            | "yml"
            | "csv"
            | "tsv"
            | "html"
            | "xml"
            | "rst"
            | "tex"
            | "log"
    )
}

fn is_lit_only_ext(ext: &str) -> bool {
    matches!(
        ext,
        "docx" | "pptx" | "xlsx" | "png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tiff" | "tif"
    )
}

fn read_text_file_guarded(path: &Path, size: u64) -> Result<String> {
    if size > MAX_DIRECT_READ_BYTES {
        anyhow::bail!(
            "file too large for direct text read: {} bytes exceeds {} bytes limit",
            size,
            MAX_DIRECT_READ_BYTES
        );
    }

    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    if looks_binary(&bytes) {
        anyhow::bail!(
            "file appears binary and cannot be read as text: {}",
            path.display()
        );
    }

    Ok(clamp_text(String::from_utf8_lossy(&bytes).to_string()))
}

fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    if bytes.contains(&0) {
        return true;
    }

    let sample_len = bytes.len().min(4096);
    let sample = &bytes[..sample_len];
    let mut suspicious = 0usize;
    for b in sample {
        if *b < 0x09 || (*b > 0x0D && *b < 0x20) {
            suspicious += 1;
        }
    }

    let ratio = suspicious as f64 / sample_len as f64;
    ratio > 0.30
}

fn pdf_page_range(page_start: Option<u32>, page_end: Option<u32>) -> Result<Option<PdfPageRange>> {
    if page_start.is_none() && page_end.is_none() {
        return Ok(None);
    }

    let start = page_start.unwrap_or(1);
    let end = page_end.unwrap_or(start);
    if start == 0 || end == 0 {
        anyhow::bail!("PDF page_start/page_end are 1-indexed");
    }
    if end < start {
        anyhow::bail!("PDF page_end must be greater than or equal to page_start");
    }

    Ok(Some(PdfPageRange { start, end }))
}

fn clamp_pdf_page_range(
    range: Option<PdfPageRange>,
    page_count: Option<u32>,
) -> Result<Option<PdfPageRange>> {
    let Some(range) = range else {
        return Ok(None);
    };

    let Some(page_count) = page_count else {
        return Ok(Some(range));
    };

    if range.start > page_count {
        anyhow::bail!(
            "PDF page_start {} exceeds page count {}",
            range.start,
            page_count
        );
    }

    Ok(Some(PdfPageRange {
        start: range.start,
        end: range.end.min(page_count),
    }))
}

async fn pdf_page_count(path: &Path) -> Result<u32> {
    let output = match Command::new("pdfinfo")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
    {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("'pdfinfo' not found in PATH; install poppler-utils")
        }
        Err(err) => return Err(err).context("failed to execute 'pdfinfo'"),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        if stderr.is_empty() {
            anyhow::bail!("pdfinfo failed");
        }
        anyhow::bail!("pdfinfo failed: {}", stderr);
    }

    for line in stdout.lines() {
        let Some(raw_pages) = line.strip_prefix("Pages:") else {
            continue;
        };
        let pages = raw_pages
            .trim()
            .parse::<u32>()
            .with_context(|| format!("invalid pdfinfo Pages value '{}'", raw_pages.trim()))?;
        if pages == 0 {
            anyhow::bail!("pdfinfo reported zero pages");
        }
        return Ok(pages);
    }

    anyhow::bail!("pdfinfo did not report page count")
}

async fn run_liteparse(path: &Path, timeout_ms: u64, mem_limit_mb: u64) -> Result<String> {
    let path_text = path.to_string_lossy().to_string();
    let mut command = liteparse_command(&path_text, mem_limit_mb);
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let child = match command.spawn() {
        Ok(child) => child,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("'lit' not found in PATH; install liteparse CLI")
        }
        Err(err) => return Err(err).context("failed to execute 'lit parse'"),
    };

    let timeout_ms = timeout_ms.max(1);
    let output =
        match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output())
            .await
        {
            Ok(Ok(output)) => output,
            Ok(Err(err)) => return Err(err).context("failed to wait for 'lit parse'"),
            Err(_) => anyhow::bail!("lit parse timed out after {} ms", timeout_ms),
        };

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        if stderr.is_empty() {
            anyhow::bail!("lit parse failed");
        }
        anyhow::bail!("lit parse failed: {}", stderr);
    }

    if stdout.is_empty() {
        anyhow::bail!("lit parse produced empty output");
    }

    Ok(stdout)
}

fn liteparse_command(path_text: &str, mem_limit_mb: u64) -> Command {
    #[cfg(unix)]
    {
        if mem_limit_mb > 0 && command_exists("lit") && command_exists("systemd-run") {
            let mut command = Command::new("systemd-run");
            command.args([
                "--user".to_string(),
                "--scope".to_string(),
                "--quiet".to_string(),
                "--wait".to_string(),
                "--collect".to_string(),
                "-p".to_string(),
                format!("MemoryMax={}M", mem_limit_mb),
                "--".to_string(),
                "lit".to_string(),
                "parse".to_string(),
                "--format".to_string(),
                "text".to_string(),
                "-q".to_string(),
                path_text.to_string(),
            ]);
            return command;
        }
    }

    let mut command = Command::new("lit");
    command.args(["parse", "--format", "text", "-q", path_text]);
    command
}

async fn run_pdftotext(path: &Path, range: Option<PdfPageRange>) -> Result<String> {
    let mut command = Command::new("pdftotext");
    command.arg("-layout");
    if let Some(range) = range {
        command
            .arg("-f")
            .arg(range.start.to_string())
            .arg("-l")
            .arg(range.end.to_string());
    }
    command
        .arg(path)
        .arg("-")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let output = match command.output().await {
        Ok(output) => output,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!("'pdftotext' not found in PATH; install poppler-utils")
        }
        Err(err) => return Err(err).context("failed to execute 'pdftotext'"),
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() {
        if stderr.is_empty() {
            anyhow::bail!("pdftotext failed");
        }
        anyhow::bail!("pdftotext failed: {}", stderr);
    }

    if stdout.trim().is_empty() {
        anyhow::bail!("pdftotext produced empty output");
    }

    Ok(stdout)
}

struct PageChunk {
    content: String,
    offset: usize,
    limit: usize,
    total_lines: usize,
    has_more: bool,
}

fn paginate_lines(text: &str, offset: usize, limit: usize) -> PageChunk {
    let lines: Vec<&str> = text.lines().collect();
    let total_lines = lines.len();

    let safe_offset = offset.max(1);
    let safe_limit = limit.max(1);

    let start = safe_offset.saturating_sub(1).min(total_lines);
    let end = (start + safe_limit).min(total_lines);

    PageChunk {
        content: lines[start..end].join("\n"),
        offset: safe_offset,
        limit: safe_limit,
        total_lines,
        has_more: end < total_lines,
    }
}

fn clamp_text(input: String) -> String {
    input.chars().take(MAX_EXTRACTED_TEXT_CHARS).collect()
}

fn detect_fd_program() -> Option<&'static str> {
    ["fd", "fdfind"]
        .into_iter()
        .find(|name| command_exists(name))
}

fn command_exists(program: &str) -> bool {
    StdCommand::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

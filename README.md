# writestead

[![CI](https://github.com/ahkohd/writestead/actions/workflows/ci.yml/badge.svg)](https://github.com/ahkohd/writestead/actions/workflows/ci.yml) [![npm version](https://img.shields.io/npm/v/@ahkohd/writestead.svg)](https://www.npmjs.com/package/@ahkohd/writestead) [![License](https://img.shields.io/badge/License-MIT-blue.svg)](https://opensource.org/licenses/MIT)

LLM Wiki

Inspired by [Karpathy's LLM OS wiki concept](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f) — persistent knowledge that compounds over time. Humans curate sources, agents maintain the structure.

Writestead gives you:
- **Structured wiki**: frontmatter, wikilinks, page types (source, entity, concept, analysis)
- **Raw ingest**: add local files or URLs, extract text from PDF/DOCX/PPTX/images via liteparse
- **MCP server**: expose wiki tools to any MCP client (Hermes, Claude Code, etc.)
- **Obsidian sync**: headless Obsidian Sync via `ob` CLI
- **Lint**: detect orphans, broken links, stale logs, duplicate titles, missing frontmatter

## Install

```bash
# npm (macOS, Linux, WSL)
npm i -g @ahkohd/writestead

# homebrew (macOS, Linux)
brew install ahkohd/tap/writestead

# cargo
cargo install writestead --locked --force

# verify
writestead --version
```

Optional tools (install any you need, `writestead doctor` checks availability):
- [`lit`](https://github.com/run-llama/liteparse) — PDF/DOCX/PPTX/image text extraction
- [`poppler-utils`](https://poppler.freedesktop.org/) — PDF utilities
- [`rg`](https://github.com/BurntSushi/ripgrep), [`fd`](https://github.com/sharkdp/fd) — faster search/listing
- [`ob`](https://obsidian.md/help/sync/headless) — headless Obsidian Sync

## Quick start

### New vault (no existing Obsidian Sync)

```bash
writestead init --vault-path ~/Documents/writestead --sync-backend obsidian
writestead doctor
writestead start
```

### Existing Obsidian Sync vault

If you already have a vault syncing via Obsidian Sync, set up sync **before** init so existing files are preserved:

```bash
# 1. login and link to remote vault
ob login
ob sync-list-remote
ob sync-setup --path ~/Documents/writestead --vault <vault-id>
ob sync --path ~/Documents/writestead

# 2. init without --force (skips files that already exist)
writestead init --vault-path ~/Documents/writestead --sync-backend obsidian

# 3. start
writestead doctor
writestead start
```

### Docker

```bash
docker run -d \
  -v writestead-vault:/vault \
  --name writestead \
  ghcr.io/ahkohd/writestead:latest

# setup sync inside container
docker exec -it writestead bash
ob login
ob sync-list-remote
ob sync-setup --path /vault --vault <vault-id>
ob sync --path /vault
writestead init --vault-path /vault --sync-backend obsidian
exit

# restart to pick up synced vault
docker restart writestead
```

## Commands

- `writestead init` — create vault structure and config
- `writestead start` / `stop` / `status` — daemon lifecycle (`start --foreground` for attached mode, `status --json` for machine output)
- `writestead doctor` — health checks for vault, sync, extractors, accelerators (`--json` for structured output)
- `writestead sync` — run sync backend
- `writestead help-wiki` — print workflow guide and conventions

### Wiki

- `writestead read <path>` — read wiki page with line pagination
- `writestead search <query>` — case-insensitive content search
- `writestead edit <path> --old-text ... --new-text ... --log-action ... --log-description ...` — exact-match replacement
- `writestead write <path> --content-file ... --log-action ... --log-description ...` — write full page
- `writestead list` — list wiki pages with pagination
- `writestead lint` — run lint checks
- `writestead index` — read wiki/index.md

### Raw

- `writestead raw add <source>` — add local file or URL to raw/ (`--name`, `--force`)
- `writestead raw list` — list raw source files with pagination
- `writestead raw read <path>` — extract text from raw source with pagination

### Config

- `writestead config path` / `show` / `get <key>` / `set <key> <value>` / `unset <key>`

## HTTP API

The CLI talks to a local HTTP daemon (default: `http://127.0.0.1:8765`).

Endpoints:
- `GET /health`
- `GET /metrics` (Prometheus format)
- `POST /mcp` (MCP over HTTP JSON-RPC)
- `GET /mcp` (returns 405)
- `DELETE /mcp` (terminate MCP session)

Configure bind address with config keys (`host`, `port`) or env (`WRITESTEAD_HOST`, `WRITESTEAD_PORT`).

## MCP over HTTP

`POST /mcp` exposes the writestead MCP server. Tools are discoverable via `tools/list`:

| Tool | Description |
|---|---|
| `wiki_read` | Read wiki page (1-indexed line pagination) |
| `wiki_search` | Case-insensitive content search |
| `wiki_edit` | Exact oldText/newText replacement with log |
| `wiki_write` | Write full page with log |
| `wiki_list` | List pages (0-indexed item pagination) |
| `wiki_lint` | Detect orphans, broken links, stale logs |
| `wiki_index` | Read wiki/index.md |
| `wiki_sync` | Run sync backend |
| `wiki_help` | Print workflow guide |
| `raw_list` | List raw source files (0-indexed pagination) |
| `raw_read` | Extract text from raw source (1-indexed line pagination) |
| `raw_upload` | Add source via url, path, or base64 content |

MCP clients receive workflow instructions automatically on `initialize`.

## MCP client setup

Local no-auth:

```yaml
writestead:
  url: http://127.0.0.1:8765/mcp
  tools:
    resources: false
    prompts: false
```

Bearer auth:

```yaml
writestead:
  url: http://127.0.0.1:8765/mcp
  headers:
    Authorization: Bearer ${WRITESTEAD_BEARER_TOKEN}
  tools:
    resources: false
    prompts: false
```

## Configuration

### Config file

Default path: `~/.config/writestead/config.json` (or `$XDG_CONFIG_HOME/writestead/config.json`).

```json
{
  "name": "writestead",
  "vault_path": "~/Documents/writestead",
  "host": "127.0.0.1",
  "port": 8765,
  "sync": { "backend": "obsidian" },
  "mcp": { "auth": { "mode": "none" }, "session_ttl_seconds": 3600 },
  "search": { "backend": "auto" },
  "raw": { "upload_max_bytes": 52428800, "url_timeout_seconds": 30 }
}
```

### Config keys

- `name` — vault display name
- `vault_path` — path to vault root
- `host` — daemon bind address (default: `127.0.0.1`)
- `port` — daemon port (default: `8765`)
- `sync.backend` — `obsidian` | `none` (default: `obsidian`)
- `mcp.auth.mode` — `none` | `bearer` (default: `none`)
- `mcp.session_ttl_seconds` — session expiry (default: `3600`)
- `search.backend` — `auto` | `builtin` | `rg-fd` (default: `auto`)
- `raw.upload_max_bytes` — upload size cap (default: `52428800`)
- `raw.url_timeout_seconds` — URL download timeout (default: `30`)
- `raw.pdf_liteparse_max_pages` — max PDF pages routed to liteparse (default: `30`)
- `raw.pdf_liteparse_timeout_ms` — liteparse timeout (default: `60000`)
- `raw.pdf_liteparse_mem_limit_mb` — liteparse memory cap (default: `4096`)

### Environment variables

- `WRITESTEAD_CONFIG_FILE` — config file path override
- `WRITESTEAD_RUNTIME_DIR` — runtime directory override
- `WRITESTEAD_PID_FILE` — PID file path override
- `WRITESTEAD_LOG_FILE` — log file path override
- `WRITESTEAD_BEARER_TOKEN` — bearer token (required when `mcp.auth.mode=bearer`)
- `WRITESTEAD_MCP_AUTH_MODE` — auth mode override

### Bearer auth

Token is env-only. Setting `mcp.auth.bearer_token` in config is blocked by design.

```bash
writestead config set mcp.auth.mode bearer
export WRITESTEAD_BEARER_TOKEN='your-token'
writestead start
```

## Raw source conventions

- `raw add` detects mode by prefix: `http://` / `https://` downloads, otherwise copies local file
- `raw read` supports:
  - `.md` / `.txt` / `.json` / `.yaml` / `.csv` / `.html` / `.xml` / `.rst` / `.tex` / `.log` — direct text read
  - `.pdf` — `lit parse` or `pdftotext` by size
  - `.docx` / `.pptx` / `.xlsx` — `lit parse`
  - images (`.png` / `.jpg` / `.tiff` / `.webp`) — `lit parse` with OCR
  - unknown types rejected
- `raw upload` (MCP) accepts exactly one of: `url`, `path` (vault-relative), or `content` (base64)
- `raw/assets/` is excluded from listing and reading (deferred)
- PDF page windows: `writestead raw read manual.pdf --page-start 1 --page-end 20`

## Search acceleration

When `search.backend=auto` (default), writestead uses `rg` and `fd` if found in PATH, falling back to built-in search. Set `search.backend=rg-fd` to require them.

```bash
# install (arch)
pacman -S ripgrep fd

# install (macOS)
brew install ripgrep fd

# verify
writestead doctor
```

## Pagination

- `wiki_read` / `raw_read` / `writestead read`: offset is **1-indexed** (line number)
- `wiki_list` / `raw_list` / `writestead list`: offset is **0-indexed** (item index)
- All paginated responses include: `offset`, `limit`, `total` (or `total_lines`), `has_more`

## Sync backend

- `obsidian` (default): runs `ob sync --path <vault_path>` — headless Obsidian Sync
- `none`: explicit no-op

## Observability

`GET /metrics` exports Prometheus counters and gauges:

```
writestead_uptime_seconds
writestead_mcp_sessions_active
writestead_mcp_requests_total
writestead_mcp_tool_calls_total
writestead_mcp_tool_calls_by_tool_total{tool="..."}
writestead_mcp_tool_errors_total
writestead_mcp_tool_errors_by_tool_total{tool="..."}
writestead_raw_uploads_total
writestead_raw_upload_bytes_total
writestead_raw_reads_total
writestead_raw_reads_by_format_total{format="..."}
```

Alert suggestions:
- Tool error spike: `increase(writestead_mcp_tool_errors_total[5m]) > 10`
- Per-tool regressions: watch `writestead_mcp_tool_errors_by_tool_total{tool=...}`
- Upload pressure: sustained growth in `writestead_raw_upload_bytes_total`

## Troubleshooting

- Run `writestead doctor` first
- If daemon won't start, check `writestead status` and `~/.config/writestead/writestead.log`
- If MCP auth fails, verify `WRITESTEAD_BEARER_TOKEN` is set and `mcp.auth.mode=bearer`
- If raw reads fail for PDF/DOCX, install `lit` (`npm i -g @llamaindex/liteparse`)
- For large PDFs, install `poppler-utils`
- If search is slow on large vaults, install `rg` and `fd`

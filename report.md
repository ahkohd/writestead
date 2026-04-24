# raw_read OOM incident + hardening proposal

## incident

`raw_upload` of ASRock WRX90 WS EVO manual (5.5 MB, ~300 pages) succeeded. Subsequent `raw_read` called `lit parse` inside the MWS container, which grew to 6.4 GB RSS / 1.8 TB VSZ. MWS LXC had 8 GB RAM, 0 swap. PSI memory full = 88%, host load avg ~170. Traefik and `pct exec` both stalled. Resolved by killing the `lit parse` process and bumping MWS to 18 GB.

Root cause: liteparse loads per-page ONNX layout/table models into CPU RAM. For large PDFs the working set grows unbounded, no internal cap, no timeout from our side. Current code only falls back to `pdftotext` if `lit parse` returns an error — hang/OOM never surfaces as an error, so fallback never fires.

## upload cap

Already present: `raw.upload_max_bytes` default 50 MB (src/config.rs:187). This gates size on disk. It does not gate parser cost, which is driven by **page count** not byte count. A 5 MB PDF can be 300+ pages; a 40 MB PDF can be 20 image scans. The byte cap is necessary but not sufficient.

Proposal: keep byte cap at 50 MB default, but add parser-aware gates below.

## slower/smarter parsing

Current path (src/raw.rs:206):

```
if ext == "pdf":
    try run_liteparse(full)
    on error: run_pdftotext(full)
```

Proposed path:

1. **Cheap probe first.** Count PDF pages via `pdfinfo` (poppler) — microseconds, no model load.
2. **Route by size.**
   - pages ≤ `pdf.liteparse_max_pages` (default 30): run liteparse for layout-aware extraction.
   - pages > threshold: go straight to `pdftotext` with `-layout`. Good enough for indexing, near-zero memory.
3. **Timeout + memory cap on liteparse.** Wrap the `lit` subprocess with a tokio `timeout` (default 60 s) and `prlimit --as=4G` or systemd-run `MemoryMax=4G`. On timeout or OOM, fall through to `pdftotext`.
4. **Page-range mode.** Extend `raw_read` with optional `page_start` / `page_end`. For liteparse, pre-split via `pdftk`/`qpdf` or `pdfinfo` + `pdftotext -f/-l` so the parser only sees the requested slice. This makes big manuals usable incrementally (parse pages 1–20 now, 21–40 later) instead of all-or-nothing.
5. **Cache per-page parsed output.** Key by `(file_hash, page, extractor)`. Re-reading the same PDF range is free after first parse. Storage cheap vs re-running liteparse.

## config additions

```rust
pub struct RawConfig {
    pub upload_max_bytes: u64,          // existing: 50 MB
    pub url_timeout_seconds: u64,       // existing: 30 s
    pub pdf_liteparse_max_pages: u32,   // new, default 30
    pub pdf_liteparse_timeout_ms: u64,  // new, default 60_000
    pub pdf_liteparse_mem_limit_mb: u64,// new, default 4096
}
```

## priorities

- P0: timeout + mem limit on `lit parse`. Prevents host-wide stall even if everything else stays dumb.
- P1: page-count routing. Biggest win for user experience on mixed PDFs.
- P2: page-range reads. Makes 300-page manuals actually useful.
- P3: per-page parsed-output cache. Perf nicety.

## notes

- The metric `writestead_raw_reads_by_format_total{format="parsed"}` never incremented during this incident because the parser never returned. Once a timeout path lands, failed-parse should still emit `{format="failed", extractor="liteparse"}` so the dashboard reflects reality.
- Consider exposing `lit` CPU/RSS via `/metrics` while the subprocess runs — would have caught this visually.
- `pdfinfo` and `pdftotext` already in the image (poppler-utils). `pdftk`/`qpdf` would need to be added for splitting, but `pdftotext -f N -l M` alone is enough for page-range extraction without splitting.

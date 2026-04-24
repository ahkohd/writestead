# sync metrics proposal

## current

Sync activity is only visible via `writestead_mcp_tool_calls_by_tool_total{tool="wiki_sync"}` and `writestead_mcp_tool_errors_by_tool_total{tool="wiki_sync"}`. These count explicit MCP `wiki_sync` invocations.

Sync backend runs triggered by other paths are uncounted:
- background periodic sync (if any)
- sync-on-write triggered by `wiki_write` / `wiki_edit`
- CLI `writestead sync`

Effect: Grafana's "Sync Activity" panel stays flat unless someone manually calls the `wiki_sync` MCP tool — misleading, since syncs are happening on writes.

## proposed

Expose dedicated sync counters that cover every code path that runs the sync backend.

```
writestead_sync_runs_total{trigger="..."}    # counter
writestead_sync_errors_total{trigger="..."}  # counter
writestead_sync_duration_seconds_sum
writestead_sync_duration_seconds_count
```

Trigger labels: `mcp` | `write` | `edit` | `cli` | `background` (extensible).

Error label could carry `reason` too if useful (e.g., `auth`, `network`, `conflict`), but start without — the counter alone unblocks the dashboard.

## implementation sketch

1. Define the counters alongside existing raw/MCP counters in `server.rs` / wherever `McpState` lives.
2. In `syncer::sync_once` (or the single chokepoint that runs `ob sync`), take a `trigger: &'static str` arg, time the call, bump:
   - `runs_total{trigger}` always
   - `errors_total{trigger}` on error path
   - `duration_seconds_{sum,count}` unconditionally
3. Every caller of `sync_once` passes its trigger label:
   - MCP `wiki_sync` tool handler → `"mcp"`
   - Post-write sync in `wiki_write` → `"write"`
   - Post-edit sync in `wiki_edit` → `"edit"`
   - CLI `writestead sync` → `"cli"`
   - Any timer/background path → `"background"`
4. Expose the new series in `/metrics` (sorted label output, same style as `raw_reads_by_format`).

## dashboard follow-up

Grafana's Sync Activity panel switches to:

```
success/min:  sum(rate(writestead_sync_runs_total[5m])) * 60
              - sum(rate(writestead_sync_errors_total[5m])) * 60
errors/min:   sum(rate(writestead_sync_errors_total[5m])) * 60
```

Optional: add per-trigger breakdown panel.

## notes

- Don't remove the existing `wiki_sync` MCP tool counter. It's still useful for "manual sync" observability.
- `sync_once` is already the natural chokepoint — no need to sprinkle metric bumps across callers, just thread the trigger label.
- This is pure observability work. No behavior change.

## priority

Low. Incident-safety already landed. Dashboard is slightly misleading, nothing is broken.

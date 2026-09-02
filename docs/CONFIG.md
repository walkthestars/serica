# Configuration

Serica uses layered configuration: **defaults → TOML file → environment variables** (later layers win).

Set `SERICA_CONFIG_FILE` (or pass `--config <path>`) to point at a TOML file, or place a `serica.toml` in the working directory. Every setting below has an environment-variable form; see [serica.toml](../serica.toml) for the same settings in TOML shape.

Two loading rules worth knowing:

- A **missing `serica.toml`** in the working directory is fine — defaults apply. But a path given explicitly via `--config`/`SERICA_CONFIG_FILE` that doesn't exist is a hard startup error, so a typo'd mount path fails loudly instead of silently running on defaults.
- An **empty** `SERICA_CONFIG_FILE=` (as shipped in `.env.example`) counts as unset, not as the literal path `""`. This matches `redis_url`/`cors.origin`/`api_key`, where blank means "disabled."

Values are range-checked at startup, before anything else runs, so a bad setting fails immediately with a message naming both the TOML key and the env var.

## Environment variables

| Variable | Default | Description |
|----------|---------|-------------|
| `SERICA_SERVER__HOST` | `127.0.0.1` | Bind address |
| `SERICA_SERVER__PORT` | `3000` | HTTP port |
| `SERICA_SERVER__LOG_LEVEL` | `info` | Log level (`trace`/`debug`/`info`/`warn`/`error`). At the default, successful requests are not logged with their search terms; failed and rate-limited requests log the full request URI (query string included). |
| `SERICA_SERVER__LOG_FORMAT` | `json` | Log format (`json` or `pretty`) |
| `SERICA_SERVER__RATE_LIMIT_PER_MINUTE` | `30` | Requests per client IP per minute, enforced as a rolling-window token bucket via `tower_governor`. `/api/v1/extract` gets a quarter of this budget. Exceeding it returns `429` with `Retry-After`/`X-RateLimit-*` headers. Keyed on peer IP — see [PROXY.md](PROXY.md) for the reverse-proxy caveat. |
| `SERICA_SERVER__SHUTDOWN_DRAIN_TIMEOUT_SECONDS` | `10` | Graceful shutdown drain timeout |
| `SERICA_SERVER__API_KEY` | — | Optional key gating `/api/v1/stats` (`Authorization: Bearer …` or `X-API-Key: …`). Unset leaves the endpoint open. |
| `SERICA_SERVER__MAX_CONCURRENT_EXTRACTS` | `2` | Caps how many `/api/v1/extract`/`serica_extract` requests may run their fetch+parse span concurrently (`1..=16`) |
| `SERICA_SEARCH__DEFAULT_TIMEOUT_MS` | `4000` | Search timeout in milliseconds |
| `SERICA_SEARCH__MAX_RESULTS_PER_PAGE` | `20` | Results per page |
| `SERICA_SEARCH__SAFE_SEARCH_DEFAULT` | `1` | Default safe search level (`0` off / `1` moderate / `2` strict) |
| `SERICA_CACHE__TTL_SECONDS` | `300` | Cache TTL for search responses (when Redis is enabled) |
| `SERICA_CACHE__EXTRACT_TTL_SECONDS` | `3600` | Cache TTL for extract responses (when Redis is enabled) — longer than search's TTL since article pages change less often than rankings |
| `SERICA_CACHE__REDIS_URL` | — | Redis connection string (disabled if empty). **Requires a binary built with the non-default `redis-cache` feature** — see [Redis caching is a build-time feature](#redis-caching-is-a-build-time-feature) below. |
| `SERICA_MCP__ENABLED` | `true` | Enable MCP stdio server |
| `SERICA_CORS__ORIGIN` | — | CORS origin; unset disables CORS entirely |
| `SERICA_EXTRACT__MAX_CONTENT_CHARS` | `50000` | Cap on the size of extracted content returned by `/api/v1/extract`/`serica_extract` — independent of the raw fetched-HTML byte cap below |
| `SERICA_EXTRACT__OUTPUT_FORMAT` | `markdown` | Shape of the returned `content` field: `text` or `markdown` (GitHub-Flavored Markdown). Server-wide, not per-request. |
| `SERICA_EXTRACT__MAX_RAW_BYTES` | `4194304` (4 MB) | Cap on the raw HTML byte count buffered before extraction runs, validated `64KB..=16MB`. A memory/DoS ceiling, not a parse-correctness limit — large real pages are cheap to parse regardless of raw size since script/style/noscript content is stripped before extraction runs. Exceeding it returns `413`. |
| `SERICA_EXTRACT__TIMEOUT_MS` | `30000` | Wall-clock budget (ms) for the full extract span, fetch through parse. Exceeding it returns `504`. |
| `SERICA_PROXY__SEARCH_URL` | — | Outbound proxy for DuckDuckGo search traffic — see [PROXY.md](PROXY.md) |
| `SERICA_PROXY__EXTRACT_URL` | — | Outbound proxy for extract traffic. **Read [PROXY.md](PROXY.md) before setting this** — it disables Serica's DNS-rebinding protection for that path. |
| `SERICA_CONFIG_FILE` | — | Path to TOML config file (same as `--config`). Empty is treated as unset. |

## Redis caching is a build-time feature

`redis-cache` is an optional Cargo feature and is **off by default**. `cargo install serica-search` and the released Linux binary are built without it and run uncached; the published Docker image is built with it.

Build it in explicitly:

```bash
cargo install serica-search --features redis-cache
# or, from source:
cargo build --release --features redis-cache
```

Setting `SERICA_CACHE__REDIS_URL` on a binary that lacks the feature **fails at startup** rather than falling back to the no-op cache — a configured cache that silently never caches is a worse outcome than a startup error that names the cause.

Redis's own memory ceiling is a server-side concern, set on the Redis process (see `docker-compose.yml`'s `redis-server --maxmemory ...`), not through Serica.

## CLI subcommands

| Command | Purpose |
|---------|---------|
| `serica` / `serica server` | Start the HTTP server (default) |
| `serica mcp` | Start the stdio JSON-RPC MCP server. Refuses to start if `SERICA_MCP__ENABLED=false`. Logs go to **stderr** here, keeping stdout clean for JSON-RPC. |
| `serica healthcheck` | Probe `127.0.0.1:<port>/api/v1/health/live` and exit `0`/`1`. Backs the container `HEALTHCHECK` so no `curl` ships in the runtime image. |

`--config <path>` applies to all of them.

## Notes

- Both proxy variables accept any URL `reqwest` understands: `http://`, `https://`, or `socks5://`, with optional `user:pass@` credentials.
- If neither proxy variable is set, Serica explicitly ignores `HTTP_PROXY`/`HTTPS_PROXY`-style environment auto-detection for the extract client, so an unrelated system-wide proxy setting can't silently weaken SSRF protection.
- `extract.output_format` is server-wide, not per-request: both gateways return whichever shape the server was configured for.

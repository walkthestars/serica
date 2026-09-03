<p align="center">
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/rust-stable-orange?logo=rust" alt="Rust"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-AGPLv3-blue" alt="License"></a>
  <img src="https://img.shields.io/badge/API-REST%20%2B%20MCP-brightgreen" alt="API">
</p>

<h1 align="center">Serica</h1>

<p align="center">A single-binary web search and article-extraction service in Rust.<br>
One ~16&nbsp;MB static binary. No API keys. Serves REST and MCP from the same code.</p>

<p align="center">
  <code>cargo install serica-search</code> ·
  <a href="#quick-start">Quick start</a> ·
  <a href="#api">API</a> ·
  <a href="#mcp">MCP</a> ·
  <a href="docs/CONFIG.md">Configuration</a>
</p>

---

Search runs through DuckDuckGo's HTML endpoint and returns clean JSON. Extract fetches any URL and returns just the article — main text as GitHub-Flavored Markdown, ads and navigation stripped. Both are exposed over HTTP and as MCP tools, so an agent can call them directly or you can wire them into your own stack. Nothing to configure to get started: no keys, no database, no Redis unless you want caching (and that needs a build flag — see [Caching](#caching)).

## Quick start

```bash
cargo install serica-search

serica                                    # HTTP server on 127.0.0.1:3000
```

```bash
curl "http://localhost:3000/api/v1/search?q=rust+programming"
curl "http://localhost:3000/api/v1/extract?url=https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html"
```

From source: `git clone https://github.com/walkthestars/serica && cd serica && cargo build --release`. Minimum Rust version is **1.88**.

## API

### `GET /api/v1/search`

| Parameter | Type | Default | Description |
|-----------|------|---------|-------------|
| `q` | string | **required** | Search query (1–500 chars) |
| `page` | integer | `1` | Page number (1–50) |
| `safe` | integer | `1` | Safe search: `0`=off, `1`=moderate, `2`=strict |
| `lang` | string | — | One of `en`, `de`, `fr`, `es`; anything else is rejected |

```json
{
  "success": true,
  "data": {
    "results": [
      {
        "url": "https://www.rust-lang.org",
        "title": "Rust Programming Language",
        "snippet": "A language empowering everyone to build reliable and efficient software."
      }
    ],
    "total_results": 20,
    "page": 1,
    "timing_ms": 423,
    "cached": false
  }
}
```

### `GET /api/v1/extract`

Fetches a URL (`https://` only) and extracts the main content.

```json
{
  "success": true,
  "data": {
    "url": "https://doc.rust-lang.org/book/ch04-00-understanding-ownership.html",
    "title": "Understanding Ownership - The Rust Programming Language",
    "author": null,
    "date": null,
    "sitename": "Rust Documentation",
    "page_type": "documentation",
    "content": "## Understanding Ownership\n\nOwnership is Rust's most unique feature...",
    "excerpt": "Ownership is a set of rules that governs how a Rust program manages memory.",
    "timing_ms": 423,
    "cached": false
  }
}
```

`content` is GFM by default (`SERICA_EXTRACT__OUTPUT_FORMAT=text` for flattened plain text) and capped at 50,000 characters (`SERICA_EXTRACT__MAX_CONTENT_CHARS`). The fetched page itself is capped at 4 MB by default, configurable 64 KB–16 MB (`SERICA_EXTRACT__MAX_RAW_BYTES`).

### Other endpoints

- `GET /api/v1/health/live` — always `200` while running; no I/O. Use for liveness probes. `GET /api/v1/health` is an alias for the same handler.
- `GET /api/v1/health/ready` — `200` when the upstream engine is healthy, else `503`. Also `503` while draining on shutdown. Cached probe, never rate-limited.
- `GET /api/v1/stats` — `total_searches`, `avg_latency_ms`, `total_errors`, `total_blocked`, `total_challenged` (upstream anti-bot challenges), `total_empty_parses`. Optionally gated by an API key via `SERICA_SERVER__API_KEY`.

Errors are structured: `{"success": false, "error": {"code": "INVALID_QUERY", "message": "..."}}`. The full set: `INVALID_QUERY`, `INVALID_PAGE`, `INVALID_SAFE_SEARCH`, `INVALID_LANGUAGE`, `UNAUTHORIZED` (401), `RATE_LIMITED` (429), `CONTENT_TOO_LARGE` (413), `EXTRACT_TIMEOUT` (504), `SERVICE_UNAVAILABLE` (503), `INTERNAL_ERROR` (500).

## MCP

Run `serica mcp` for a stdio JSON-RPC MCP server exposing two tools, `serica_search` and `serica_extract`, backed by the same code as the HTTP API:

```json
{
  "mcpServers": {
    "serica": {
      "command": "/usr/local/bin/serica",
      "args": ["mcp"]
    }
  }
}
```

This works in Claude Desktop, Cursor, and any other MCP-compatible client.

## Configuration

Defaults work out of the box. The variables people actually set:

| Variable | Default | Description |
|----------|---------|-------------|
| `SERICA_SERVER__HOST` | `127.0.0.1` | Bind address |
| `SERICA_SERVER__PORT` | `3000` | HTTP port |
| `SERICA_SERVER__LOG_LEVEL` | `info` | Default level does not log query terms on successful requests; failed and rate-limited requests log the full URI (query included) |
| `SERICA_SERVER__RATE_LIMIT_PER_MINUTE` | `30` | Per client IP; `/api/v1/extract` gets ¼ of this |
| `SERICA_CACHE__REDIS_URL` | — | Enable Redis caching if set — requires a binary built with `--features redis-cache` |
| `SERICA_EXTRACT__OUTPUT_FORMAT` | `markdown` | Or `text` |
| `SERICA_PROXY__SEARCH_URL` | — | Outbound proxy for search traffic |
| `SERICA_PROXY__EXTRACT_URL` | — | Outbound proxy for extract traffic — [read this first](docs/PROXY.md#outbound-proxy-support) |

Full reference in [docs/CONFIG.md](docs/CONFIG.md).

If you put Serica behind a reverse proxy, read [docs/PROXY.md](docs/PROXY.md#rate-limiting-does-not-survive-being-fronted-by-a-proxy): the built-in rate limiter keys on peer IP and degrades to one shared bucket behind a front-end, and Serica deliberately ignores `X-Forwarded-For`.

## Caching

Caching is optional and **off unless you build it in**. `redis-cache` is a non-default Cargo feature, so `cargo install serica-search` and the released Linux binary ship without it — they run uncached, which is a supported configuration.

```bash
cargo install serica-search --features redis-cache
# or, from source:
cargo build --release --features redis-cache
```

Then point Serica at a Redis instance:

```bash
SERICA_CACHE__REDIS_URL=redis://localhost:6379/0 serica
```

Setting `SERICA_CACHE__REDIS_URL` on a binary built *without* the feature is a startup error, not a silent fallback — caching that quietly never happens is worse than a failure that tells you why. Search responses are cached for `SERICA_CACHE__TTL_SECONDS` (300s) and extract responses for `SERICA_CACHE__EXTRACT_TTL_SECONDS` (3600s, longer because article pages change less often than rankings). Both responses carry a `cached` boolean.

The published Docker image is built with `--features redis-cache`, so it needs no rebuild — just a `SERICA_CACHE__REDIS_URL`.

## Docker

Published images cover `linux/amd64` and `linux/arm64` and are built with the
`redis-cache` feature, so they need no rebuild to use Redis:

```bash
docker run -p 3000:3000 ghcr.io/walkthestars/serica            # latest
docker run -p 3000:3000 ghcr.io/walkthestars/serica:v0.2.0     # pinned
```

A `docker-compose.yml` wiring Serica to a Redis container is also included.

## Development

```bash
cargo test --all-features       # unit + integration tests (upstream mocked, no network needed)
cargo clippy --all-features --all-targets -- -D warnings
cargo fmt --all -- --check
cargo build --release
```

`--all-features` matters: `redis-cache` is off by default, so without it the Redis cache layer is never compiled and can break unnoticed. CI runs every check with it.

Layout: `src/gateway/http.rs` and `src/gateway/mcp.rs` are the two frontends; `src/engines/duckduckgo.rs` is the search adapter; `src/util/fetch.rs` and `src/util/ssrf.rs` handle outbound fetching and URL validation; extraction lives behind `rs-trafilatura` with local patches in `src/util/`.

## Security notes

- **SSRF** — extract resolves the target hostname, rejects private/loopback/link-local/cloud-metadata addresses (IPv4 and IPv6), pins the connection to exactly that validated address, requires HTTPS, and caps response size. Redirects are followed manually, up to 5 hops, re-running the full validation and re-pinning a fresh client on **every** hop — the HTTP client's own automatic redirect following stays off, so no hop is ever dialled unvalidated. A bad hop fails the whole request closed.
- **Safe search** — filtering happens primarily at DuckDuckGo via its own parameter, backed by a small local keyword blocklist. Best-effort defense-in-depth, not a content-safety guarantee.
- **Privacy** — no API keys, no cookie store between requests. Search terms are not logged on successful requests; failed and rate-limited requests log the full request URI (query string included), and upstream is DuckDuckGo, which doesn't track users.

## Known limitations

- **Extraction is article-tuned.** The main-content selector targets article-shaped HTML. On giant single-page documents (RFC full-texts, `print.html` book exports) it can keep one section out of many. Raw HTML of 512&nbsp;KB or more gets a plausibility audit: a selection representing under 5% of the document's text is replaced with the whole-document text (capped as usual by `SERICA_EXTRACT__MAX_CONTENT_CHARS`); larger selections are trusted. Pages below that size are governed by the small-page floors only.
- **No JavaScript.** Extract never runs JS. Sites that inject their content client-side (mdbook guides, some SPA docs) yield only the server-rendered shell — often just navigation and an intro paragraph. Prefer a raw-markdown or plain-HTML source when one exists.
- **HTML only.** Extract accepts `text/html` and `application/xhtml` responses; PDFs, plain text, and JSON endpoints are rejected. Pair Serica with a dedicated fetcher for those.

## Contributing

Contributions welcome — see [CONTRIBUTING.md](CONTRIBUTING.md). Two things are asked of contributors: a `Signed-off-by` line on every commit (the Developer Certificate of Origin, `git commit -s`; CI checks it), and a one-time acceptance of the [Contributor License Agreement](https://github.com/walkthestars/serica/blob/master/CLA.md) on a contributor's first pull request.

## License

AGPL-3.0-only. See [LICENSE](LICENSE) and [COPYRIGHT](COPYRIGHT). Running unmodified Serica puts no obligations on your own code; hosting or distributing a *modified* Serica triggers the network clause — offer those modifications under the same license.

*DuckDuckGo is a trademark of DuckDuckGo, Inc. Serica is not affiliated with DuckDuckGo.*

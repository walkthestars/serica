// MCP Gateway: JSON-RPC 2.0 over stdio server
// Implements initialize, tools/list, and tools/call for serica_search

use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Semaphore};

use crate::cache::CacheLayer;
use crate::config::Config;
use crate::engines::duckduckgo::{
    ddg_region_for_language, DuckDuckGoAdapter, SearchError, SUPPORTED_LANGUAGES,
};
use crate::error::AppError;
use crate::gateway::{cache_key, finalize_results};
use crate::models::search::{SearchRequest, SearchResponse};
use crate::util::extract::{run_extract, ExtractLimiter};
use crate::util::metrics::MetricsCollector;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// === JSON-RPC 2.0 message types ===

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct JsonRpcRequest {
    jsonrpc: String,
    // A JSON-RPC 2.0 notification omits `id` entirely rather than sending it
    // as null, so this must be `Option` with a `default` — without it,
    // `serde_json::from_str` rejects every notification (e.g.
    // `notifications/initialized`, which every real MCP client sends right
    // after `initialize`) as a parse error before we ever get a chance to
    // recognize it as one.
    #[serde(default)]
    id: Option<Value>,
    method: String,
    params: Option<Value>,
}

/// A real, current, date-stamped MCP protocol version. MCP clients negotiate
/// against a known set of these and refuse anything else, so this must never
/// regress to something ad hoc like "0.1.0".
const SUPPORTED_PROTOCOL_VERSION: &str = "2025-06-18";

/// MCP protocol versions are date-stamped (`YYYY-MM-DD`); anything else
/// cannot be a version a real client would recognize, so it isn't worth
/// echoing back.
fn looks_like_protocol_version(v: &str) -> bool {
    let bytes = v.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes.iter().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                true
            } else {
                b.is_ascii_digit()
            }
        })
}

/// Wrap a successfully-serialized tool result in the MCP `tools/call` result
/// shape (`{"content": [...], "isError": false}`). Real MCP clients cannot
/// parse a bare struct as a tool result, and a serialization failure must
/// become a JSON-RPC error rather than silently degrading to a `null`
/// "success" (the previous `unwrap_or_default()` behavior).
fn tool_call_result(id: &Value, value: &impl Serialize) -> JsonRpcResponse {
    match serde_json::to_string_pretty(value) {
        Ok(text) => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            result: Some(json!({
                "content": [{ "type": "text", "text": text }],
                "isError": false
            })),
            error: None,
        },
        Err(e) => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            result: None,
            error: Some(jsonrpc_error(-32603, &format!("Serialization failed: {e}"))),
        },
    }
}

#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcErrorObject>,
}

#[derive(Debug, Serialize)]
struct JsonRpcErrorObject {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

fn jsonrpc_error(code: i32, message: &str) -> JsonRpcErrorObject {
    JsonRpcErrorObject {
        code,
        message: message.to_string(),
        data: None,
    }
}

fn jsonrpc_error_with_data(code: i32, message: &str, data: Value) -> JsonRpcErrorObject {
    JsonRpcErrorObject {
        code,
        message: message.to_string(),
        data: Some(data),
    }
}

/// MCP Gateway — serves search and extract tools via JSON-RPC 2.0 over stdio.
pub struct McpGateway {
    config: Arc<Config>,
    engine: Arc<DuckDuckGoAdapter>,
    cache: Arc<dyn CacheLayer>,
    metrics: Arc<MetricsCollector>,
    /// Extract pipeline plan, Change 6: shared with `HttpGateway`'s
    /// `AppState`, constructed once in `main.rs`.
    limiter: ExtractLimiter,
}

impl McpGateway {
    pub fn new(
        config: Arc<Config>,
        engine: Arc<DuckDuckGoAdapter>,
        cache: Arc<dyn CacheLayer>,
        metrics: Arc<MetricsCollector>,
        limiter: ExtractLimiter,
    ) -> Self {
        Self {
            config,
            engine,
            cache,
            metrics,
            limiter,
        }
    }

    /// Run the MCP server on stdin/stdout. Blocks until stdin closes.
    ///
    /// Each incoming line is dispatched onto its own `tokio::spawn`ed task
    /// (dispatched rather than awaited inline) so a slow `serica_extract`
    /// call cannot block the read loop — and every other in-flight
    /// request — for its full timeout budget. The MCP spec permits
    /// concurrent in-flight requests and does not require in-order
    /// responses, so this intentionally does not try to preserve response
    /// ordering relative to request ordering; doing so would defeat the
    /// point of running them concurrently.
    pub async fn run_stdio(self) -> Result<(), Box<dyn std::error::Error>> {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        // Writes must still be serialized: two tasks finishing at the same
        // moment must not interleave their partial writes onto stdout, or a
        // client reading it strictly one JSON value per line would desync.
        // Hoisted once here rather than re-acquired per iteration/response.
        let stdout = Arc::new(Mutex::new(tokio::io::stdout()));
        let this = Arc::new(self);

        // Bounds the number of concurrently in-flight tasks so a flood of
        // pipelined tool calls can't spawn unboundedly and exhaust memory.
        // There's no dedicated MCP concurrency knob in `Config` to anchor
        // There is no dedicated knob to anchor this to, so 32 is a plain
        // headroom choice, consistent with the DDG connection pool sizing.
        let semaphore = Arc::new(Semaphore::new(32));

        loop {
            line.clear();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break;
            }

            // Decide emptiness via a borrowed `trim()` first — only the
            // non-empty case needs an owned copy (to move into the spawned
            // task), so the common case never allocates a copy just to
            // discover the line is blank.
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let msg = trimmed.to_owned();

            let this = this.clone();
            let stdout = stdout.clone();
            let semaphore = semaphore.clone();
            tokio::spawn(async move {
                // Held for the task's whole lifetime (not just around
                // `handle_message`) so the bound also covers responses
                // queued waiting on the stdout lock, not just requests being
                // processed.
                let _permit = semaphore
                    .acquire_owned()
                    .await
                    .expect("semaphore is never closed");

                // A notification produces no response at all, per JSON-RPC
                // 2.0 — not even an empty line. Writing anything here would
                // desync clients that read stdout strictly one JSON value
                // per request.
                let Some(response) = this.handle_message(&msg).await else {
                    return;
                };

                let response_json = match serde_json::to_string(&response) {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::error!(error = %e, "Failed to serialize MCP response");
                        return;
                    }
                };

                let mut out = stdout.lock().await;
                if let Err(e) = out.write_all(response_json.as_bytes()).await {
                    tracing::error!(error = %e, "Failed to write MCP response");
                    return;
                }
                if let Err(e) = out.write_all(b"\n").await {
                    tracing::error!(error = %e, "Failed to write MCP response");
                    return;
                }
                if let Err(e) = out.flush().await {
                    tracing::error!(error = %e, "Failed to flush MCP response");
                }
            });
        }

        Ok(())
    }

    /// Returns `None` for JSON-RPC notifications (no `id`), since a server
    /// MUST NOT reply to those — success or error — per JSON-RPC 2.0. A
    /// malformed message that fails to even parse still gets an `id: null`
    /// error response, since we can't know whether it was a notification.
    async fn handle_message(&self, line: &str) -> Option<JsonRpcResponse> {
        let request: JsonRpcRequest = match serde_json::from_str(line) {
            Ok(req) => req,
            Err(e) => {
                return Some(JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: json!(null),
                    result: None,
                    error: Some(jsonrpc_error(-32700, &format!("Parse error: {}", e))),
                });
            }
        };

        // Defensive: treat `notifications/initialized` as a no-op regardless
        // of whether an `id` is (incorrectly) attached, since it is the one
        // handshake message every real MCP client sends and it must never
        // fall through to the "unknown method" error path.
        if request.method == "notifications/initialized" {
            return None;
        }

        let id = request.id?;

        let response = match request.method.as_str() {
            "initialize" => self.handle_initialize(&id, request.params.as_ref()),
            "tools/list" => self.handle_tools_list(&id),
            "tools/call" => {
                let params = request.params.unwrap_or(json!({}));
                self.handle_tools_call(&id, params).await
            }
            _ => JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id,
                result: None,
                error: Some(jsonrpc_error(
                    -32601,
                    &format!("Method not found: {}", request.method),
                )),
            },
        };
        Some(response)
    }

    fn handle_initialize(&self, id: &Value, params: Option<&Value>) -> JsonRpcResponse {
        // Echo the client's requested version back when it's at least
        // shaped like a real MCP version. We don't vary behavior by
        // negotiated version today, but echoing a value the client already
        // knows it supports is safer than unilaterally imposing ours.
        let protocol_version = params
            .and_then(|p| p.get("protocolVersion"))
            .and_then(|v| v.as_str())
            .filter(|v| looks_like_protocol_version(v))
            .unwrap_or(SUPPORTED_PROTOCOL_VERSION);

        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            result: Some(json!({
                "protocolVersion": protocol_version,
                "serverInfo": {
                    "name": "serica",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "capabilities": {
                    "tools": {}
                }
            })),
            error: None,
        }
    }

    fn handle_tools_list(&self, id: &Value) -> JsonRpcResponse {
        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            result: Some(json!({
                "tools": [
                    {
                        "name": "serica_search",
                        "description": "Search the web using DuckDuckGo.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "query": {
                                    "type": "string",
                                    "description": "Search query (1-500 characters)"
                                },
                                "page": {
                                    "type": "integer",
                                    "description": "Page number (1-50)",
                                    "default": 1
                                },
                                "safe_search": {
                                    "type": "integer",
                                    "description": "Safe search level: 0=off, 1=moderate, 2=strict",
                                    "default": 1,
                                    "enum": [0, 1, 2]
                                },
                                "language": {
                                    "type": "string",
                                    // This is an exhaustive enum, not an "e.g." pattern —
                                    // these are the only codes DuckDuckGo's `kl` region
                                    // parameter is mapped for (see
                                    // duckduckgo::SUPPORTED_LANGUAGES); any other code is
                                    // rejected with an error rather than silently ignored.
                                    "description": "2-letter ISO 639-1 language code. Only 'en', 'de', 'fr', and 'es' are supported — results are constrained via DuckDuckGo's own region parameter for these. Any other code is rejected.",
                                    "enum": ["en", "de", "fr", "es"]
                                }
                            },
                            "required": ["query"]
                        }
                    },
                    {
                        "name": "serica_extract",
                        "description": "Fetch a URL and extract clean article content (title, author, date, main text, excerpt). Strips ads, nav, and boilerplate. Content is returned as Markdown by default (server-configured).",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "url": {
                                    "type": "string",
                                    "description": "The URL of the page to extract content from",
                                    "format": "uri"
                                }
                            },
                            "required": ["url"]
                        }
                    }
                ]
            })),
            error: None,
        }
    }

    async fn handle_tools_call(&self, id: &Value, params: Value) -> JsonRpcResponse {
        let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");

        let args = match params.get("arguments") {
            Some(a) => a,
            None => {
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: None,
                    error: Some(jsonrpc_error(-32602, "Missing 'arguments' field")),
                };
            }
        };

        // === serica_search handler ===
        if name == "serica_search" {
            let query = match args.get("query").and_then(|v| v.as_str()) {
                Some(q) => q.trim().to_string(),
                None => {
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id.clone(),
                        result: None,
                        error: Some(jsonrpc_error(-32602, "Missing required 'query' parameter")),
                    };
                }
            };

            // Count chars, not UTF-8 bytes — see the matching note on
            // gateway/http.rs::SearchRequest::try_from_params. Kept in sync
            // with that check since MCP validates the query independently
            // rather than going through try_from_params (which only
            // covers the post-search filter/cache path).
            if query.is_empty() || query.chars().count() > 500 {
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: None,
                    error: Some(jsonrpc_error_with_data(
                        -32602,
                        "Invalid 'query' parameter",
                        json!({"field": "query", "message": "must be 1-500 characters"}),
                    )),
                };
            }

            let page = args.get("page").and_then(|v| v.as_u64()).unwrap_or(1) as u32;
            if !(1..=50).contains(&page) {
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: None,
                    error: Some(jsonrpc_error_with_data(
                        -32602,
                        "Invalid 'page' parameter",
                        json!({"field": "page", "message": "must be between 1 and 50"}),
                    )),
                };
            }

            let safe_search = args
                .get("safe_search")
                .and_then(|v| v.as_u64())
                .unwrap_or(1) as u8;
            if safe_search > 2 {
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: None,
                    error: Some(jsonrpc_error_with_data(
                        -32602,
                        "Invalid 'safe_search' parameter",
                        json!({"field": "safe_search", "message": "must be 0, 1, or 2"}),
                    )),
                };
            }

            // Validate language (optional)
            let language = args
                .get("language")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            if let Some(ref lang) = language {
                if lang.len() != 2 || !lang.chars().all(|c| c.is_ascii_lowercase()) {
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id.clone(),
                        result: None,
                        error: Some(jsonrpc_error_with_data(
                            -32602,
                            "Invalid 'language' parameter",
                            json!({"field": "language", "message": "must be a 2-letter ISO 639-1 code"}),
                        )),
                    };
                }
                // A well-formed code isn't necessarily one DuckDuckGo's
                // `kl` region parameter is mapped for (`ddg_region_for_language`).
                // Reject anything unmapped instead of silently searching
                // unconstrained — see the matching check in
                // gateway/http.rs::SearchRequest::try_from_params.
                if ddg_region_for_language(lang).is_none() {
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id.clone(),
                        result: None,
                        error: Some(jsonrpc_error_with_data(
                            -32602,
                            "Unsupported 'language' parameter",
                            json!({
                                "field": "language",
                                "message": format!(
                                    "unsupported language code '{}'; supported: {}",
                                    lang,
                                    SUPPORTED_LANGUAGES.join(", ")
                                )
                            }),
                        )),
                    };
                }
            }

            // Cache key + finalize (filter + paginate) are shared with the
            // HTTP gateway (src/gateway/mod.rs) — both write into the same
            // cache, so both must apply safe_search/language identically or
            // one gateway's cache writes silently defeat the other's
            // filtering.
            let search_req = SearchRequest {
                query: query.clone(),
                page,
                safe_search,
                language: language.clone(),
            };
            let key = cache_key(&search_req);

            // Cache-hit latency is measured from here: `timing_ms` must
            // report THIS request's cost, not replay the original fetch's
            // timing that was frozen into the cached payload. A hit does
            // no I/O, so this
            // lands at single-digit milliseconds.
            let start = std::time::Instant::now();

            // Check cache first
            if let Some(cached_bytes) = self.cache.get(&key).await {
                match serde_json::from_slice::<SearchResponse>(&cached_bytes) {
                    Ok(mut response) => {
                        response.cached = true;
                        response.timing_ms = start.elapsed().as_millis() as u64;
                        return tool_call_result(id, &response);
                    }
                    Err(e) => {
                        // A bad cache entry shouldn't break the request — log
                        // and fall through to the normal non-cached path.
                        tracing::error!(key = %key, error = %e, "Failed to deserialize cached response");
                    }
                }
            }

            let search_results = match self
                .engine
                .search(&query, page, language.as_deref(), safe_search)
                .await
            {
                Ok(results) => results,
                // Not a client-facing error — see the analogous match in
                // gateway/http.rs::search_handler for why a genuinely-empty
                // query must not become an error response here.
                Err(SearchError::ParseEmpty) => {
                    self.metrics.record_empty_parse();
                    Vec::new()
                }
                Err(SearchError::Blocked) => {
                    self.metrics.record_blocked();
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id.clone(),
                        result: None,
                        error: Some(jsonrpc_error(
                            -32000,
                            "Search engine is currently blocked upstream",
                        )),
                    };
                }
                // Distinct from `Blocked` (403) — see the analogous arm
                // in gateway/http.rs::map_search_error.
                Err(SearchError::Challenged) => {
                    self.metrics.record_challenged();
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id.clone(),
                        result: None,
                        error: Some(jsonrpc_error(
                            -32000,
                            "Search engine is presenting an anti-bot challenge upstream",
                        )),
                    };
                }
                Err(e @ (SearchError::Upstream(_) | SearchError::Transport(_))) => {
                    self.metrics.record_error();
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id.clone(),
                        result: None,
                        error: Some(jsonrpc_error(-32000, &e.to_string())),
                    };
                }
                // The parser only runs on the blocking pool; reaching
                // here means that blocking task itself panicked — a server
                // bug, not an upstream/availability problem, so this uses the
                // JSON-RPC "Internal error" code rather than -32000.
                Err(e @ SearchError::ParsePanicked) => {
                    self.metrics.record_error();
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id.clone(),
                        result: None,
                        error: Some(jsonrpc_error(-32603, &e.to_string())),
                    };
                }
            };
            let timing_ms = start.elapsed().as_millis() as u64;
            self.metrics.record_search(timing_ms);

            let search_resp = finalize_results(
                search_results,
                &search_req,
                self.config.search.max_results_per_page,
                timing_ms,
            );

            // Store in cache (best-effort)
            let ttl = std::time::Duration::from_secs(self.config.cache.ttl_seconds);
            match serde_json::to_vec(&search_resp) {
                Ok(bytes) => self.cache.set(&key, &bytes, ttl).await,
                Err(e) => {
                    tracing::error!(key = %key, error = %e, "Failed to serialize response for caching");
                }
            }

            return tool_call_result(id, &search_resp);
        }

        // === serica_extract handler ===
        //
        // Intentionally thin — URL validation (below) is the only thing left
        // here (see `run_extract`'s doc comment in `util::extract` for why:
        // HTTP and MCP report a parse failure through different error
        // shapes, so parsing can't move into the shared pipeline). Cache
        // lookup, SSRF-validated fetch, capped body read, extraction, and
        // cache-on-success all live in `util::extract::run_extract`, shared
        // with `gateway::http::extract_handler`, and `run_extract` uses one
        // real timer for both gateways, so `timing_ms` is accurate on the
        // MCP path too.
        if name == "serica_extract" {
            let url = match args.get("url").and_then(|v| v.as_str()) {
                Some(u) => u.trim().to_string(),
                None => {
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id.clone(),
                        result: None,
                        error: Some(jsonrpc_error(-32602, "Missing required 'url' parameter")),
                    };
                }
            };

            if url.is_empty() {
                return JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: None,
                    error: Some(jsonrpc_error_with_data(
                        -32602,
                        "Invalid 'url' parameter",
                        json!({"field": "url", "message": "must be a valid https:// URL"}),
                    )),
                };
            }

            let parsed_url = match url::Url::parse(&url) {
                Ok(u) => u,
                Err(_) => {
                    return JsonRpcResponse {
                        jsonrpc: "2.0".to_string(),
                        id: id.clone(),
                        result: None,
                        error: Some(jsonrpc_error_with_data(
                            -32602,
                            "Invalid 'url' parameter",
                            json!({"field": "url", "message": "could not be parsed"}),
                        )),
                    };
                }
            };

            return match run_extract(&self.config, &self.cache, &self.limiter, &parsed_url).await {
                Ok(response) => tool_call_result(id, &response),
                // Covers every per-hop SSRF/scheme validation failure (bad
                // scheme, unresolvable/forbidden host, missing host), a bad
                // redirect target/Location header, the redirect-hop cap
                // being exceeded, and the new content-type-rejection message
                // — all surfaced by `run_extract` as `AppError::InvalidQuery`,
                // the same mapping the HTTP gateway applies to fetch-level
                // validation failures.
                //
                // The reason is folded into the top-line message, not left
                // only in `data`: some MCP clients (Hermes among them) strip
                // the `data` payload, and a bare "Invalid 'url' parameter"
                // sends callers debugging the wrong layer entirely. The
                // "Invalid 'url' parameter" prefix stays stable for callers
                // matching on it; `data` remains for structured consumers.
                Err(AppError::InvalidQuery(msg)) => JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: None,
                    error: Some(jsonrpc_error_with_data(
                        -32602,
                        &format!("Invalid 'url' parameter: {msg}"),
                        json!({"field": "url", "message": msg}),
                    )),
                },
                // A hop's connection/send failed, or the target returned a
                // non-2xx status. Opaque-message cases (enforced
                // inside `fetch_extract_target`) can carry resolved IPs, DNS
                // failure detail, and TLS certificate subjects a caller
                // could use to map the internal network by diffing error
                // strings; the HTTP-status case's message is safe to
                // include as-is.
                Err(AppError::ServiceUnavailable(msg)) => JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: None,
                    error: Some(jsonrpc_error(-32000, &msg)),
                },
                // Extract pipeline plan, Change 1: the raw fetched HTML
                // exceeded `config.extract.max_raw_bytes`. No JSON-RPC code
                // maps cleanly to HTTP 413, so this follows the existing
                // informal convention above of using -32000 ("Server
                // error") for gateway-side failures that aren't shaped like
                // bad caller input — the message (which includes the actual
                // limit) is included rather than dropped into a generic
                // catch-all.
                Err(AppError::ContentTooLarge(msg)) => JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: None,
                    error: Some(jsonrpc_error(-32000, &msg)),
                },
                // Extract pipeline plan, Change 1: the full extract span
                // (fetch through parse) exceeded `config.extract.timeout_ms`.
                // Same -32000 convention as `ContentTooLarge` above.
                Err(AppError::ExtractTimeout(msg)) => JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: None,
                    error: Some(jsonrpc_error(-32000, &msg)),
                },
                // True fallback only: `AppError::Internal` (e.g. the
                // extraction blocking task panicking) and the startup-only
                // variants (`Bind`/`Config`/`Redis`) that can't actually
                // occur on this path but exist on the enum.
                Err(e) => JsonRpcResponse {
                    jsonrpc: "2.0".to_string(),
                    id: id.clone(),
                    result: None,
                    error: Some(jsonrpc_error(
                        -32000,
                        &format!("Failed to prepare request: {}", e),
                    )),
                },
            };
        }

        // Unknown tool
        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: id.clone(),
            result: None,
            error: Some(jsonrpc_error(
                -32601,
                &format!("Method not found: {}", name),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engines::duckduckgo::DuckDuckGoAdapter;
    // Only needed by the extract-cache-hit tests below, which construct a
    // canned cache entry directly rather than going through `run_extract` —
    // the non-test code above references neither name — cache-key
    // computation and URL normalization live inside
    // `util::extract::run_extract`.
    use crate::gateway::extract_cache_key;
    use crate::models::extract::ExtractResponse;
    use crate::util::ssrf::normalize_extract_url;

    fn setup_gateway() -> McpGateway {
        use crate::cache::noop::NoopCache;
        let config = Arc::new(Config::default());
        let engine = Arc::new(DuckDuckGoAdapter::new());
        let cache: Arc<dyn CacheLayer> = Arc::new(NoopCache);
        let metrics = Arc::new(MetricsCollector::new());
        McpGateway::new(config, engine, cache, metrics, ExtractLimiter::new(2))
    }

    // Lets MCP tests point `serica_search` at a `wiremock` server
    // instead of the real DuckDuckGo endpoint `DuckDuckGoAdapter::new()`
    // would use — see `test_tools_call_search_success_returns_parsed_results`
    // below. Every other `tools/call` test in this module exercises an error
    // path (missing/invalid params) that never reaches the engine at all, or
    // relies on `DelayedCache` always short-circuiting to a cache hit
    // (`test_concurrent_requests_do_not_serialize`) — none of them actually
    // proved a *successful* `serica_search` call works end to end.
    fn setup_gateway_with_engine(engine: DuckDuckGoAdapter) -> McpGateway {
        use crate::cache::noop::NoopCache;
        let config = Arc::new(Config::default());
        let cache: Arc<dyn CacheLayer> = Arc::new(NoopCache);
        let metrics = Arc::new(MetricsCollector::new());
        McpGateway::new(
            config,
            Arc::new(engine),
            cache,
            metrics,
            ExtractLimiter::new(2),
        )
    }

    #[tokio::test]
    async fn test_initialize() {
        let gateway = setup_gateway();
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let response = gateway
            .handle_message(msg)
            .await
            .expect("request must get a response");
        assert_eq!(response.jsonrpc, "2.0");
        assert!(response.result.is_some());
        assert!(response.error.is_none());
        let result = response.result.unwrap();
        assert_eq!(result["serverInfo"]["name"], "serica");
    }

    #[tokio::test]
    async fn test_initialize_protocol_version_is_date_stamped() {
        let gateway = setup_gateway();
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
        let response = gateway.handle_message(msg).await.unwrap();
        let result = response.result.unwrap();
        let version = result["protocolVersion"].as_str().unwrap();
        assert_ne!(version, "0.1.0");
        assert!(
            looks_like_protocol_version(version),
            "expected a YYYY-MM-DD version, got {version}"
        );
    }

    #[tokio::test]
    async fn test_initialize_echoes_supported_client_version() {
        let gateway = setup_gateway();
        let msg = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#;
        let response = gateway.handle_message(msg).await.unwrap();
        let result = response.result.unwrap();
        assert_eq!(result["protocolVersion"], "2024-11-05");
    }

    #[tokio::test]
    async fn test_tools_list() {
        let gateway = setup_gateway();
        let msg = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
        let response = gateway.handle_message(msg).await.unwrap();
        assert!(response.result.is_some());
        let result = response.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "serica_search");
        assert_eq!(tools[1]["name"], "serica_extract");
    }

    #[tokio::test]
    async fn test_unknown_method() {
        let gateway = setup_gateway();
        let msg = r#"{"jsonrpc":"2.0","id":3,"method":"nonexistent","params":{}}"#;
        let response = gateway.handle_message(msg).await.unwrap();
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().code, -32601);
    }

    #[tokio::test]
    async fn test_invalid_json() {
        let gateway = setup_gateway();
        let msg = r#"invalid json"#;
        let response = gateway.handle_message(msg).await.unwrap();
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().code, -32700);
    }

    #[tokio::test]
    async fn test_tools_call_missing_query() {
        let gateway = setup_gateway();
        let msg = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"serica_search","arguments":{}},"id":4}"#;
        let response = gateway.handle_message(msg).await.unwrap();
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().code, -32602);
    }

    #[tokio::test]
    async fn test_tools_call_extract_invalid_url_message_carries_reason() {
        // The `InvalidQuery` arm must not emit the bare string
        // "Invalid 'url' parameter" as the top-line JSON-RPC message with
        // the actual reason (scheme/SSRF/content-type rejection) only in
        // the `data` payload — some MCP clients strip `data`, leaving agents
        // with an unusable error. The reason rides in the message itself,
        // while `data` stays for structured consumers.
        let gateway = setup_gateway();
        let msg = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"serica_extract","arguments":{"url":"http://example.com/not-https"}},"id":9}"#;
        let response = gateway.handle_message(msg).await.unwrap();
        let error = response.error.expect("http:// URL must be rejected");
        assert_eq!(error.code, -32602);
        assert!(
            error.message.starts_with("Invalid 'url' parameter"),
            "message must keep the stable prefix, got: {}",
            error.message
        );
        assert!(
            error.message.contains("Only https"),
            "message must carry the actual rejection reason, got: {}",
            error.message
        );
        let data = error
            .data
            .expect("structured data payload must be preserved");
        assert_eq!(data["field"], "url");
    }

    #[tokio::test]
    async fn test_tools_call_invalid_page() {
        let gateway = setup_gateway();
        let msg = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"serica_search","arguments":{"query":"test","page":0}},"id":5}"#;
        let response = gateway.handle_message(msg).await.unwrap();
        assert!(response.error.is_some());
        assert_eq!(response.error.unwrap().code, -32602);
    }

    // The length check must count Unicode scalar values,
    // not UTF-8 bytes. 500 copies of "中" (3 bytes each, 1500 bytes total)
    // is exactly 500 chars, matching the documented/schema'd limit — it must
    // not be rejected as a query-length error. Paired with an invalid page
    // so the request still fails fast without an outbound network call, and
    // the error's `field` confirms *why* it failed: "page", not "query".
    #[tokio::test]
    async fn test_tools_call_accepts_500_char_cjk_query_despite_exceeding_500_bytes() {
        let gateway = setup_gateway();
        let query: String = "中".repeat(500);
        assert_eq!(query.chars().count(), 500);
        assert!(query.len() > 500, "sanity check: this must be >500 bytes");
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {"name": "serica_search", "arguments": {"query": query, "page": 0}},
            "id": 7
        })
        .to_string();
        let response = gateway.handle_message(&msg).await.unwrap();
        let error = response
            .error
            .expect("page 0 is invalid and must be rejected");
        assert_eq!(error.data.unwrap()["field"], "page");
    }

    // One char over the limit is still rejected, even
    // when that single extra char keeps the byte count small relative to
    // what a byte-based check of the same "500" number would allow.
    #[tokio::test]
    async fn test_tools_call_rejects_501_char_cjk_query() {
        let gateway = setup_gateway();
        let query: String = "中".repeat(501);
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "tools/call",
            "params": {"name": "serica_search", "arguments": {"query": query}},
            "id": 8
        })
        .to_string();
        let response = gateway.handle_message(&msg).await.unwrap();
        let error = response.error.expect("501-char query must be rejected");
        assert_eq!(error.code, -32602);
        assert_eq!(error.data.unwrap()["field"], "query");
    }

    // "ja" is a well-formed 2-letter ISO 639-1 code but has
    // no DuckDuckGo `kl` region mapping (`SUPPORTED_LANGUAGES` only covers
    // en/de/fr/es). It must be rejected with a clear error rather than
    // silently searching unconstrained.
    #[tokio::test]
    async fn test_tools_call_unmapped_language_is_rejected() {
        let gateway = setup_gateway();
        let msg = r#"{"jsonrpc":"2.0","method":"tools/call","params":{"name":"serica_search","arguments":{"query":"test","language":"ja"}},"id":6}"#;
        let response = gateway.handle_message(msg).await.unwrap();
        let error = response.error.expect("unmapped language must be rejected");
        assert_eq!(error.code, -32602);
    }

    #[tokio::test]
    async fn test_notification_gets_no_response() {
        let gateway = setup_gateway();
        // No `id` field at all — this is what every real MCP client sends
        // immediately after `initialize`.
        let msg = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let response = gateway.handle_message(msg).await;
        assert!(
            response.is_none(),
            "a notification must never get a response"
        );
    }

    #[tokio::test]
    async fn test_notification_initialized_ignored_even_with_id() {
        let gateway = setup_gateway();
        // Defensive case: some clients may (incorrectly) attach an id to
        // this specific notification. It must still be treated as a no-op.
        let msg = r#"{"jsonrpc":"2.0","id":99,"method":"notifications/initialized"}"#;
        let response = gateway.handle_message(msg).await;
        assert!(response.is_none());
    }

    #[tokio::test]
    async fn test_arbitrary_notification_gets_no_response() {
        let gateway = setup_gateway();
        // Any message with no `id` is a notification per JSON-RPC 2.0, even
        // for a method the server doesn't otherwise recognize.
        let msg = r#"{"jsonrpc":"2.0","method":"notifications/some_future_event"}"#;
        let response = gateway.handle_message(msg).await;
        assert!(response.is_none());
    }

    // The only success-path test for `serica_search` in this module,
    // and the only one that actually calls a (mocked) engine — every other
    // `tools/call` test above either exercises an error path that returns
    // before the engine is ever invoked, or (in
    // `test_concurrent_requests_do_not_serialize` below) relies on a cache
    // that always hits and so also never calls it. This mocks the DDG
    // endpoint with the same real-page fixture the HTTP gateway's tests use
    // and asserts on the parsed body inside `result.content[0].text`
    // (`tool_call_result` embeds the `SearchResponse` as pretty-printed
    // JSON text there), not just "no error".
    #[tokio::test]
    async fn test_tools_call_search_success_returns_parsed_results() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fixture = include_str!("../../tests/fixtures/ddg_rust_search.html");
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
            .mount(&server)
            .await;
        let engine = DuckDuckGoAdapter::with_base_url(format!("{}/html/", server.uri()));
        let gateway = setup_gateway_with_engine(engine);

        let msg = r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"serica_search","arguments":{"query":"rust"}}}"#;
        let response = gateway
            .handle_message(msg)
            .await
            .expect("request must get a response");
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        let result = response.result.expect("expected a successful tool result");
        assert_eq!(result["isError"], false);
        let content = result["content"]
            .as_array()
            .expect("content must be an array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");

        let search_response: Value = serde_json::from_str(content[0]["text"].as_str().unwrap())
            .expect("content text must be the SearchResponse JSON");
        let results = search_response["results"]
            .as_array()
            .expect("results must be an array");
        assert!(
            !results.is_empty(),
            "expected the mocked DDG fixture to parse into at least one result"
        );
        assert!(results[0]["url"]
            .as_str()
            .unwrap()
            .contains("rust-lang.org"));
        assert_eq!(search_response["page"], 1);
        assert_eq!(search_response["cached"], false);
    }

    // Genuinely network-dependent, same reason as the HTTP gateway's
    // `extract_valid_url_returns_200`/`extract_non_html_content_type_returns_400`
    // (tests/api_test.rs): `serica_extract` shares
    // `util::ssrf::validate_extract_target`, which requires `https://` and
    // rejects loopback/private addresses — exactly what a local
    // `wiremock::MockServer` is. Run explicitly via `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "reaches the live internet (example.com); see comment above"]
    async fn test_tools_call_extract_result_has_mcp_content_shape() {
        let gateway = setup_gateway();
        let msg = r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"serica_extract","arguments":{"url":"https://example.com"}}}"#;
        let response = gateway.handle_message(msg).await.unwrap();
        assert!(
            response.error.is_none(),
            "unexpected error: {:?}",
            response.error
        );
        let result = response.result.expect("expected a successful tool result");
        assert_eq!(result["isError"], false);
        let content = result["content"]
            .as_array()
            .expect("content must be an array");
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert!(content[0]["text"].as_str().unwrap().contains("example.com"));
    }

    // A cache hit for `serica_extract` must
    // return the cached `ExtractResponse` directly, with `cached: true` set,
    // never reaching `fetch_extract_target`. Proven without any network
    // dependency by pointing the request at a `http://` URL: SSRF validation
    // (`validate_extract_target`, invoked by `fetch_extract_target`) rejects
    // any non-`https` scheme immediately, so if the cache check were
    // accidentally skipped — or the fetch path were reached at all — this
    // would come back as a `-32602 Invalid 'url' parameter` error instead of
    // a cached success, making a short-circuit regression fail loudly rather
    // than silently.
    #[tokio::test]
    async fn test_tools_call_extract_cache_hit_skips_fetch() {
        use crate::cache::CacheLayer;

        struct CannedExtractCache {
            key: String,
            bytes: Vec<u8>,
        }

        #[async_trait::async_trait]
        impl CacheLayer for CannedExtractCache {
            async fn get(&self, key: &str) -> Option<Vec<u8>> {
                if key == self.key {
                    Some(self.bytes.clone())
                } else {
                    None
                }
            }
            async fn set(&self, _key: &str, _value: &[u8], _ttl: std::time::Duration) {}
        }

        let url = "http://example.com/cached-article";
        let parsed = url::Url::parse(url).unwrap();
        let key = extract_cache_key(&normalize_extract_url(&parsed));

        let canned = ExtractResponse {
            url: url.to_string(),
            title: Some("Cached Title".into()),
            author: None,
            date: None,
            sitename: None,
            page_type: None,
            content: Some("Cached content".into()),
            excerpt: None,
            timing_ms: 0,
            cached: false,
        };
        let bytes = serde_json::to_vec(&canned).unwrap();
        let cache: Arc<dyn CacheLayer> = Arc::new(CannedExtractCache { key, bytes });

        let gateway = McpGateway::new(
            Arc::new(Config::default()),
            Arc::new(DuckDuckGoAdapter::new()),
            cache,
            Arc::new(MetricsCollector::new()),
            ExtractLimiter::new(2),
        );

        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/call",
            "params": {"name": "serica_extract", "arguments": {"url": url}}
        })
        .to_string();

        let response = gateway
            .handle_message(&msg)
            .await
            .expect("request must get a response");
        assert!(
            response.error.is_none(),
            "expected a cache-hit success, got error: {:?}",
            response.error
        );
        let result = response.result.expect("expected a successful tool result");
        let content = result["content"].as_array().unwrap();
        let extract_response: Value =
            serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(extract_response["cached"], true);
        assert_eq!(extract_response["title"], "Cached Title");
        assert_eq!(extract_response["content"], "Cached content");
    }

    /// Two concurrent `handle_message` calls,
    /// one artificially slow and one fast, must both complete without the
    /// fast one waiting on the slow one. `run_stdio` proves this in
    /// production by dispatching each line onto its own `tokio::spawn`ed
    /// task instead of awaiting them one at a time; this test reproduces
    /// that same dispatch pattern directly against `McpGateway` so the
    /// property can be exercised deterministically (via a paused clock) and
    /// without a live network dependency.
    ///
    /// The "slow" request is a cache *hit* that sleeps before returning,
    /// rather than a real `serica_extract`/`serica_search` call — this
    /// keeps the test hermetic while still exercising the real
    /// `handle_message` code path end to end.
    #[tokio::test(start_paused = true)]
    async fn test_concurrent_requests_do_not_serialize() {
        use crate::cache::CacheLayer;
        use crate::models::search::SearchRequest as SReq;
        use std::time::Duration;

        struct DelayedCache {
            slow_key: String,
            response_bytes: Vec<u8>,
        }

        #[async_trait::async_trait]
        impl CacheLayer for DelayedCache {
            async fn get(&self, key: &str) -> Option<Vec<u8>> {
                if key == self.slow_key {
                    // Long enough that, under the old strictly-serial loop,
                    // a fast request queued behind this one would have had
                    // to wait the full duration. The paused clock makes
                    // this cost no real wall-clock time.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
                Some(self.response_bytes.clone())
            }
            async fn set(&self, _key: &str, _value: &[u8], _ttl: Duration) {}
        }

        let slow_req = SReq {
            query: "slow query".to_string(),
            page: 1,
            safe_search: 1,
            language: None,
        };
        let slow_key = cache_key(&slow_req);
        let response = SearchResponse {
            results: vec![],
            total_results: 0,
            page: 1,
            timing_ms: 0,
            cached: false,
        };
        let response_bytes = serde_json::to_vec(&response).unwrap();
        let cache: Arc<dyn CacheLayer> = Arc::new(DelayedCache {
            slow_key,
            response_bytes,
        });
        let gateway = Arc::new(McpGateway::new(
            Arc::new(Config::default()),
            Arc::new(DuckDuckGoAdapter::new()),
            cache,
            Arc::new(MetricsCollector::new()),
            ExtractLimiter::new(2),
        ));

        let slow_msg = r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"serica_search","arguments":{"query":"slow query"}}}"#.to_string();
        let fast_msg = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"serica_search","arguments":{"query":"fast query"}}}"#.to_string();

        // Mirrors run_stdio's per-line dispatch: each request runs on its
        // own spawned task rather than being awaited inline.
        let slow_gateway = gateway.clone();
        let slow_handle = tokio::spawn(async move { slow_gateway.handle_message(&slow_msg).await });

        // Let the slow task actually get scheduled and enter its sleep
        // before racing the fast one against it.
        tokio::task::yield_now().await;

        let fast_gateway = gateway.clone();
        let fast_result = tokio::time::timeout(Duration::from_secs(5), async move {
            fast_gateway.handle_message(&fast_msg).await
        })
        .await
        .expect("fast request must not be blocked behind the slow in-flight one");
        assert!(fast_result.unwrap().error.is_none());

        // The slow task must still complete on its own; nothing here
        // implies it gets abandoned, only that it doesn't block others.
        let slow_result = slow_handle.await.unwrap();
        assert!(slow_result.unwrap().error.is_none());
    }

    // A `serica_search` cache hit must report THIS
    // request's latency in `timing_ms`, not replay the original fetch's
    // timing frozen into the cached payload. The canned entry carries an
    // absurd 1_000_000 ms so replaying it verbatim cannot pass by luck of
    // a fast machine. A wrong key would make the
    // cache miss and fall through to the engine — for which
    // `DuckDuckGoAdapter::new()` has no stub, so a key mismatch cannot fake
    // a pass: it would error or hang loudly, not silently return a hit.
    #[tokio::test]
    async fn test_tools_call_search_cache_hit_remeasures_timing_ms() {
        use crate::cache::CacheLayer;
        use crate::models::search::SearchRequest as SReq;

        struct CannedSearchCache {
            key: String,
            bytes: Vec<u8>,
        }

        #[async_trait::async_trait]
        impl CacheLayer for CannedSearchCache {
            async fn get(&self, key: &str) -> Option<Vec<u8>> {
                if key == self.key {
                    Some(self.bytes.clone())
                } else {
                    None
                }
            }
            async fn set(&self, _key: &str, _value: &[u8], _ttl: std::time::Duration) {}
        }

        // Must match what the handler derives from the arguments below:
        // page defaults to 1 and safe_search to 1 when absent.
        let canned_req = SReq {
            query: "canned timing query".to_string(),
            page: 1,
            safe_search: 1,
            language: None,
        };
        let key = cache_key(&canned_req);
        let canned = SearchResponse {
            results: vec![],
            total_results: 0,
            page: 1,
            timing_ms: 1_000_000,
            cached: false,
        };
        let bytes = serde_json::to_vec(&canned).unwrap();
        let cache: Arc<dyn CacheLayer> = Arc::new(CannedSearchCache { key, bytes });

        let gateway = McpGateway::new(
            Arc::new(Config::default()),
            Arc::new(DuckDuckGoAdapter::new()),
            cache,
            Arc::new(MetricsCollector::new()),
            ExtractLimiter::new(2),
        );

        let msg = r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"serica_search","arguments":{"query":"canned timing query"}}}"#;
        let response = gateway
            .handle_message(msg)
            .await
            .expect("request must get a response");
        assert!(
            response.error.is_none(),
            "expected a cache-hit success, got error: {:?}",
            response.error
        );
        let result = response.result.expect("expected a successful tool result");
        let content = result["content"].as_array().unwrap();
        let search_response: Value =
            serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(
            search_response["cached"], true,
            "handler must report this as a cache hit"
        );
        let timing = search_response["timing_ms"]
            .as_u64()
            .expect("timing_ms must be a number");
        assert!(
            timing < 1_000_000,
            "cache hit must re-measure timing_ms, not replay the canned 1_000_000 ms (got {timing})"
        );
        assert!(
            timing < 5_000,
            "a no-I/O cache hit should complete in well under five seconds even on a loaded CI runner, got {timing} ms"
        );
    }
}

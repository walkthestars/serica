// HTTP Gateway: axum server, routes, middleware, state

use axum::extract::Query;
use axum::http::header::{ACCEPT, AUTHORIZATION};
use axum::http::{HeaderMap, HeaderName, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::{response::Json, routing::get, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::PeerIpKeyExtractor;
use tower_governor::{GovernorError, GovernorLayer};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

use crate::cache::CacheLayer;
use crate::config::Config;
use crate::engines::duckduckgo::{
    ddg_region_for_language, DuckDuckGoAdapter, EngineHealth, SearchError, SUPPORTED_LANGUAGES,
};
use crate::error::AppError;
use crate::gateway::{cache_key, finalize_results};
use crate::models::search::{SearchRequest, SearchResponse};
use crate::util::extract::{run_extract, ExtractLimiter};
use crate::util::metrics::MetricsCollector;
use crate::util::ssrf::EXTRACT_TIMEOUT;

/// Header carrying the per-request correlation ID. Shared between
/// `SetRequestIdLayer` (stamps it onto the request if absent),
/// `PropagateRequestIdLayer` (echoes it onto the response), and the
/// `TraceLayer` span-builder below (reads it into the `request_id` span
/// field) so all three agree on the exact same header name.
const REQUEST_ID_HEADER: HeaderName = HeaderName::from_static("x-request-id");

/// Query parameters for the search endpoint.
#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub page: Option<u32>,
    pub safe: Option<u8>,
    pub lang: Option<String>,
}

impl SearchRequest {
    fn try_from_params(params: SearchParams, default_safe_search: u8) -> Result<Self, AppError> {
        let query = params.q.unwrap_or_default();
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Err(AppError::InvalidQuery("Query is empty".into()));
        }
        // Count Unicode scalar values, not UTF-8 bytes — `str::len()`
        // would reject legitimate CJK queries around ~167 chars and
        // emoji-containing queries around ~125 chars, well short of the 500
        // this limit (and the README/MCP schema/error text) promise. The
        // resulting worst case is still tiny (a 500-char query is at most
        // 500 * 4 = 2000 bytes, since no UTF-8 scalar value exceeds 4 bytes),
        // so there's no meaningful memory-abuse tradeoff in switching.
        if trimmed.chars().count() > 500 {
            return Err(AppError::InvalidQuery(
                "Query exceeds maximum length of 500 characters".into(),
            ));
        }

        let page = params.page.unwrap_or(1);
        if !(1..=50).contains(&page) {
            return Err(AppError::InvalidPage(format!(
                "Page must be between 1 and 50, got {}",
                page
            )));
        }

        let safe_search = params.safe.unwrap_or(default_safe_search);
        if safe_search > 2 {
            return Err(AppError::InvalidSafeSearch(safe_search));
        }

        let language = params.lang.map(|l| l.trim().to_lowercase());
        if let Some(ref lang) = language {
            if lang.len() != 2 || !lang.chars().all(|c| c.is_ascii_lowercase()) {
                return Err(AppError::InvalidLanguage(format!(
                    "Invalid language code: {}. Expected 2-letter ISO 639-1 code",
                    lang
                )));
            }
            // A well-formed 2-letter code is not necessarily one this
            // service can actually constrain a search by. Language filtering
            // is delegated to DuckDuckGo's own `kl` region parameter
            // (`ddg_region_for_language`), which only understands a small
            // set of languages — silently searching unconstrained for
            // anything else would leave a caller requesting `lang=ja` with
            // no way to know the filter never applied. Reject it instead,
            // the same way an out-of-range `page` or `safe` value is
            // rejected above.
            if ddg_region_for_language(lang).is_none() {
                return Err(AppError::InvalidLanguage(format!(
                    "Unsupported language code: {}. Supported languages: {}",
                    lang,
                    SUPPORTED_LANGUAGES.join(", ")
                )));
            }
        }

        Ok(SearchRequest {
            query: trimmed.to_string(),
            page,
            safe_search,
            language,
        })
    }
}

/// Shared application state available to all handlers
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub engine: Arc<DuckDuckGoAdapter>,
    pub engine_health: Arc<EngineHealth>,
    pub metrics: Arc<MetricsCollector>,
    pub startup_time: Instant,
    pub is_shutting_down: Arc<AtomicBool>,
    pub cache: Arc<dyn CacheLayer>,
    /// Extract pipeline plan, Change 6: bounds how many extract requests may
    /// run their fetch+parse span concurrently, shared with `run_extract`.
    pub limiter: ExtractLimiter,
}

/// HTTP Gateway — manages the axum router, middleware stack, and route handlers
pub struct HttpGateway {
    config: Arc<Config>,
    engine: Arc<DuckDuckGoAdapter>,
    engine_health: Arc<EngineHealth>,
    metrics: Arc<MetricsCollector>,
    cache: Arc<dyn CacheLayer>,
    // Owned by `main()` and shared with the `with_graceful_shutdown`
    // future so flipping this to `true` during drain is visible to the
    // health handlers below via the same `AppState` they read from — rather
    // than `router()` minting its own, permanently-`false`, unshared flag.
    is_shutting_down: Arc<AtomicBool>,
    // Captured once in `main()` before config load / Redis connect,
    // not at router-build time, so reported uptime reflects true process age.
    started_at: Instant,
    // Extract pipeline plan, Change 6: constructed once in `main.rs` and
    // shared (cloned) with `McpGateway` too, so both gateways' extracts draw
    // from the same concurrency budget.
    limiter: ExtractLimiter,
}

impl HttpGateway {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<Config>,
        engine: Arc<DuckDuckGoAdapter>,
        engine_health: Arc<EngineHealth>,
        metrics: Arc<MetricsCollector>,
        cache: Arc<dyn CacheLayer>,
        is_shutting_down: Arc<AtomicBool>,
        started_at: Instant,
        limiter: ExtractLimiter,
    ) -> Self {
        Self {
            config,
            engine,
            engine_health,
            metrics,
            cache,
            is_shutting_down,
            started_at,
            limiter,
        }
    }

    /// Build the axum Router with all routes and middleware.
    ///
    /// Returns `Err` if `cors.origin` is configured to a value that fails to
    /// parse as a header value: a typo in the origin must fail
    /// startup, not silently widen to a wildcard.
    pub fn router(&self) -> Result<Router, String> {
        // Real per-client token-bucket rate limiting: a bare `Semaphore`
        // bounds in-flight concurrency, not request rate, so a
        // client issuing unlimited fast requests is never throttled and 30
        // concurrent slow `/extract` calls could 429 every other caller.
        // `/api/v1/extract` gets its own, tighter budget since it is the
        // most expensive (rs-trafilatura parse) and most dangerous (SSRF
        // surface) endpoint; `/api/v1/health` is left off both
        // limiters entirely below so abuse of other endpoints can't flap
        // orchestrator health checks.
        let general_rate_limit_per_minute = self.config.server.rate_limit_per_minute;
        let extract_rate_limit_per_minute = (general_rate_limit_per_minute / 4).max(1);

        let general_governor_layer =
            GovernorLayer::new(build_governor_config(general_rate_limit_per_minute))
                .error_handler(rate_limit_error_handler);
        let extract_governor_layer =
            GovernorLayer::new(build_governor_config(extract_rate_limit_per_minute))
                .error_handler(rate_limit_error_handler);

        let state = AppState {
            config: self.config.clone(),
            engine: self.engine.clone(),
            engine_health: self.engine_health.clone(),
            metrics: self.metrics.clone(),
            startup_time: self.started_at,
            is_shutting_down: self.is_shutting_down.clone(),
            cache: self.cache.clone(),
            limiter: self.limiter.clone(),
        };

        // CORS layer respecting config: `None`/unset attaches no
        // layer at all — same-origin and server-to-server/MCP callers never
        // send or need `Access-Control-*` headers, so the deny-by-default
        // posture costs those consumers nothing while denying browser-based
        // cross-origin callers by default instead of allowing them for free.
        let cors = build_cors_layer(self.config.cors.origin.as_deref())?;

        // Health is intentionally excluded from both `route_layer` calls
        // below: it's added to the top-level router directly so
        // liveness checks can never be starved by abuse of the other routes.
        // `/api/v1/health/ready` gets the same treatment — a
        // readiness probe that only reads cached atomics is cheap enough
        // that rate-limiting it would just add a needless failure mode.
        // Bound every route with a `TimeoutLayer` so a stalled code
        // path outside the per-client `reqwest` timeouts (DNS resolution, a
        // slow parse, future Redis latency) can't hang indefinitely while
        // holding a rate-limit permit and a connection open. `/api/v1/search`
        // and `/api/v1/stats` share `search.default_timeout_ms`. Wrapped
        // outermost (added last, see the `route_layer` ordering note below)
        // so it bounds the governor check plus the handler together.
        let general_timeout_layer = TimeoutLayer::with_status_code(
            TIMEOUT_MARKER_STATUS,
            Duration::from_millis(self.config.search.default_timeout_ms),
        );

        // `/api/v1/extract` gets its own, larger budget rather than sharing
        // `search.default_timeout_ms`: its handler's per-hop `reqwest`
        // timeout (`EXTRACT_TIMEOUT`, 15s, in `util::ssrf`) needs to win the
        // race and produce its own more specific error whenever it can. With
        // With redirect-following, `EXTRACT_TIMEOUT` bounds each
        // individual hop's request, not the request as a whole — a slow
        // multi-hop chain (up to `MAX_REDIRECT_HOPS`) can take a multiple of
        // 15s in the worst case, so a long enough chain can still hit this
        // outer, more generic timeout first instead of the inner one. Either
        // way the request times out correctly; this layer's job is only to
        // be the final backstop across the SSRF DNS/IP check(s), however
        // many hops are followed, the blocking-pool `rs_trafilatura::extract`
        // parse, and response serialization — none of which are
        // covered by the inner per-hop `reqwest` timeout. The 5s of headroom
        // is sized for a single hop's non-fetch work, not multiplied per
        // hop, so it's deliberately not generous for a long chain.
        let extract_timeout_layer = TimeoutLayer::with_status_code(
            TIMEOUT_MARKER_STATUS,
            EXTRACT_TIMEOUT + Duration::from_secs(5),
        );

        let general_routes = Router::new()
            .route("/api/v1/search", get(search_handler))
            .route("/api/v1/stats", get(stats_handler))
            .route_layer(general_governor_layer)
            .route_layer(general_timeout_layer);

        let extract_routes = Router::new()
            .route("/api/v1/extract", axum::routing::get(extract_handler))
            .route_layer(extract_governor_layer)
            .route_layer(extract_timeout_layer);

        let router = Router::new()
            // `/api/v1/health` is kept as an alias for `/live` (rather than
            // removed) so the Dockerfile's existing `HEALTHCHECK` and any
            // external monitoring already pointed at it keep working
            // unchanged. Left off the timeout layers above for the same
            // reason it's left off both rate limiters: a
            // liveness/readiness probe that only reads cached atomics must
            // never be able to fail because of a middleware layer meant for
            // the slower, network-bound routes.
            .route("/api/v1/health", get(health_live_handler))
            .route("/api/v1/health/live", get(health_live_handler))
            .route("/api/v1/health/ready", get(health_ready_handler))
            .merge(general_routes)
            .merge(extract_routes)
            // `tower_http::timeout::TimeoutLayer` (unlike
            // `tower::timeout::Timeout`) completes the response itself with
            // an empty body and `TIMEOUT_MARKER_STATUS` rather than
            // returning an `Err`, so there's no rejected `Result` for a
            // `HandleErrorLayer` to convert — this `map_response` layer is
            // the equivalent hook on the response side, rewriting that
            // marker into the same `{"success": false, "error": {...}}`
            // envelope every other error path uses. Applied once here, after
            // both route groups are merged, so it catches a timeout from
            // either budget above.
            .layer(middleware::map_response(convert_timeout_response))
            .layer(RequestBodyLimitLayer::new(8 * 1024));

        // `.layer()` requires a concrete type, so the "no CORS" case is
        // expressed by skipping the call entirely rather than passing an
        // inert layer — there is then no `CorsLayer` in the stack at all for
        // callers to introspect or rely on.
        let router = match cors {
            Some(cors) => router.layer(cors),
            None => router,
        };

        // Tracing/observability layers are added *last*, which in
        // axum makes them *outermost* (each `.layer()` call wraps
        // everything added before it — see the `route_layer` note above).
        // Wrapping literally everything else — compression, CORS, the body
        // limit, and, embedded inside `general_routes`/`extract_routes`
        // above, the per-route rate limiter and `TimeoutLayer` — gives
        // two properties:
        //   1. A 429 rate-limit rejection or a timeout happens
        //      *inside* the `TraceLayer` span instead of short-circuiting
        //      before `TraceLayer` ever sees the request, so both are
        //      traced/logged like every other response instead of leaving
        //      no record for an operator during an incident.
        //   2. `SetRequestIdLayer` runs *before* `TraceLayer`, so the
        //      `x-request-id` header exists on the request by the time the
        //      span is built and can be read into it (see `make_span_with`
        //      below) — every log line carries the correlation ID.
        //
        // Final order, outermost to innermost:
        //   SetRequestId -> PropagateRequestId -> Trace -> Compression ->
        //   CORS -> BodyLimit -> convert_timeout_response -> (per-route rate
        //   limit -> per-route timeout) -> handler
        let router = router
            .layer(CompressionLayer::new())
            .layer(
                TraceLayer::new_for_http().make_span_with(|request: &axum::http::Request<_>| {
                    // Only has a value to read because `SetRequestIdLayer`
                    // (below — and therefore outside/before this layer) has
                    // already stamped the header onto the request by the
                    // time this closure runs.
                    let request_id = request
                        .headers()
                        .get(&REQUEST_ID_HEADER)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default();
                    tracing::info_span!(
                        "http_request",
                        method = %request.method(),
                        uri = %request.uri(),
                        request_id = %request_id,
                    )
                }),
            )
            // Echoes `x-request-id` back to the client, so a caller can
            // quote the same ID back in a support request that an operator
            // can then grep for in the logs.
            // Positioned between `SetRequestIdLayer` and `TraceLayer`
            // (tower-http's documented order): inside `SetRequestIdLayer` so
            // it can see the header on the way in, outside `TraceLayer` so
            // the response it produces is still covered by the span above.
            .layer(PropagateRequestIdLayer::new(REQUEST_ID_HEADER))
            .layer(SetRequestIdLayer::new(REQUEST_ID_HEADER, MakeRequestUuid));

        Ok(router.with_state(state))
    }
}

/// Turn the configured `cors.origin` into the `CorsLayer` to attach, if any.
///
/// - `None`/empty: no layer (the default — see `CorsConfig::default`).
/// - `"*"`: an explicit, deliberate opt-in to allow any origin. This still
///   avoids `CorsLayer::permissive()`, which also mirrors back whatever
///   `Access-Control-Request-Headers` the caller sends rather than a fixed
///   allow-list — broader than a read-only GET/JSON API needs even under an
///   intentional wildcard.
/// - anything else: parsed as a single allowed origin. A value that fails to
///   parse as a header value is a startup error (`Err`), not a silent
///   fallback to `"*"` — a typo must not turn into a wildcard.
fn build_cors_layer(origin: Option<&str>) -> Result<Option<CorsLayer>, String> {
    match origin {
        None => Ok(None),
        Some("") => Ok(None),
        Some("*") => Ok(Some(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(vec![axum::http::Method::GET])
                .allow_headers(vec![ACCEPT]),
        )),
        Some(o) => {
            let value = o.parse::<axum::http::HeaderValue>().map_err(|e| {
                format!(
                    "invalid cors.origin '{o}': {e} (set SERICA_CORS__ORIGIN or [cors].origin \
                     in serica.toml to a valid origin, e.g. https://example.com, or leave it \
                     unset to disable CORS)"
                )
            })?;
            Ok(Some(
                CorsLayer::new()
                    .allow_origin(value)
                    .allow_methods(vec![axum::http::Method::GET])
                    .allow_headers(vec![ACCEPT]),
            ))
        }
    }
}

/// Build a per-client token-bucket rate limiter for `requests_per_minute`.
///
/// Keyed on `PeerIpKeyExtractor` (the TCP peer address) rather than
/// `SmartIpKeyExtractor`/`X-Forwarded-For`: Serica has no reverse-proxy
/// story today (no proxy config in docker-compose.yml or README), so
/// trusting a client-supplied `X-Forwarded-For` header would let any caller
/// forge unlimited identities and defeat the limiter entirely. If Serica is
/// ever deployed behind a trusted reverse proxy, swap in
/// `SmartIpKeyExtractor` at that point — not before.
///
/// `requests_per_minute` is spent as `burst_size` (a client may use its
/// whole per-minute budget in one burst) and refilled continuously at
/// `period_ms = 60_000 / requests_per_minute`, so the limiter enforces the
/// configured rate as a rolling window rather than a fixed-size bucket that
/// resets on the minute.
fn build_governor_config(
    requests_per_minute: u32,
) -> tower_governor::governor::GovernorConfig<
    PeerIpKeyExtractor,
    governor::middleware::StateInformationMiddleware,
> {
    // `GovernorConfigBuilder::finish()` returns `None` for a zero burst size
    // or a zero-length refill period — both nonsensical to the underlying
    // token bucket. `requests_per_minute == 0` (a literal config value of
    // `0`) would produce both, so clamp to a floor of 1/minute: the config
    // value becomes "effectively disabled" rather than panicking at
    // startup via `.expect()` below.
    let effective_rpm = requests_per_minute.max(1);
    let period_ms = (60_000 / effective_rpm as u64).max(1);

    GovernorConfigBuilder::default()
        .period(Duration::from_millis(period_ms))
        .burst_size(effective_rpm)
        .use_headers()
        .finish()
        // Unreachable: `effective_rpm` and `period_ms` are both clamped to
        // at least 1 above, so `finish()` never sees the zero values that
        // are its only `None` case.
        .expect("rate limiter config: burst_size and period are guaranteed non-zero")
}

/// Map a `tower_governor` rejection onto the same `{"success": false,
/// "error": {...}}` envelope every other error path uses: the
/// crate's default rejection is a bare status code with a plaintext body,
/// which bypasses `AppError`'s JSON contract entirely. `tower_governor`
/// already computed the correct `Retry-After`/`X-RateLimit-*` headers by
/// the time this runs, so they're layered on top of the standard body
/// rather than recomputed.
fn rate_limit_error_handler(err: GovernorError) -> axum::response::Response {
    if let GovernorError::TooManyRequests { headers, .. } = &err {
        let mut response = AppError::RateLimited.into_response();
        if let Some(extra_headers) = headers {
            response.headers_mut().extend(extra_headers.clone());
        }
        return response;
    }
    err.into()
}

/// Status code the `TimeoutLayer`s installed in `router()` stamp on
/// a response when a route's timeout budget elapses.
///
/// Not unique to this layer: `AppError::ExtractTimeout` (`run_extract`'s
/// inner `tokio::time::timeout`) also produces a real `GATEWAY_TIMEOUT`,
/// with its own already-correct `EXTRACT_TIMEOUT` JSON envelope.
/// `convert_timeout_response` below cannot tell the two apart by status
/// code alone — see its own doc comment for how it disambiguates them.
const TIMEOUT_MARKER_STATUS: StatusCode = StatusCode::GATEWAY_TIMEOUT;

/// Rewrite a bare `TimeoutLayer` timeout into the same `{"success":
/// false, "error": {...}}` envelope every other error path uses, WITHOUT
/// clobbering a legitimate `AppError::ExtractTimeout` response that also
/// happens to carry `TIMEOUT_MARKER_STATUS` (504).
///
/// `TimeoutLayer::with_status_code` always pairs the marker status with an
/// EMPTY body (see `tower_http`'s docs) — `AppError::ExtractTimeout::into_response()`
/// never does, it always carries a real JSON envelope. Body emptiness is
/// therefore what actually distinguishes "the outer layer fired, there is no
/// envelope yet" from "a handler already built the correct envelope, this
/// just happens to also be a 504" — status code alone is not enough,
/// because a second, legitimate producer of 504 exists (see the const's
/// doc comment above). Only buffers the body when the status is already 504
/// (rare), so this keeps the original "don't pay to inspect every response"
/// property for the overwhelming majority of non-timeout responses.
async fn convert_timeout_response(response: Response) -> Response {
    if response.status() != TIMEOUT_MARKER_STATUS {
        return response;
    }
    let (parts, body) = response.into_parts();
    match axum::body::to_bytes(body, 8192).await {
        Ok(bytes) if bytes.is_empty() => {
            AppError::ServiceUnavailable("Request timed out".into()).into_response()
        }
        // Non-empty: a handler (e.g. `run_extract` via `AppError::ExtractTimeout`)
        // already produced the correct envelope — pass it through unchanged.
        Ok(bytes) => Response::from_parts(parts, axum::body::Body::from(bytes)),
        // Body couldn't be read at all — neither known producer of this
        // status should hit this, but fail safe to the generic envelope
        // rather than surface a raw body-read error to the caller.
        Err(_) => AppError::ServiceUnavailable("Request timed out".into()).into_response(),
    }
}

/// GET /api/v1/health, /api/v1/health/live — liveness: 200 whenever this
/// process is running, no I/O. This is what the Dockerfile `HEALTHCHECK` and
/// any orchestrator liveness probe should target. `engine_healthy`/
/// `engine_last_checked` are informational only (read from the
/// background-refreshed cache, see `engines::duckduckgo::spawn_health_monitor`)
/// and never affect the status code here — coupling liveness to upstream
/// reachability is exactly what let DDG's health drag Serica's container
/// into restart loops.
async fn health_live_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Json<Value> {
    let uptime = state.startup_time.elapsed().as_secs();
    let engine_healthy = state.engine_health.is_healthy();
    let engine_last_checked = state.engine_health.last_checked();

    let status = if state.is_shutting_down.load(Ordering::Relaxed) {
        "shutting_down"
    } else {
        "ok"
    };

    Json(json!({
        "success": true,
        "data": {
            "status": status,
            "version": env!("CARGO_PKG_VERSION"),
            "uptime_seconds": uptime,
            "engine_healthy": engine_healthy,
            "engine_last_checked": engine_last_checked,
        }
    }))
}

/// GET /api/v1/health/ready — readiness: reflects the cached upstream status
/// maintained by the fixed-interval background probe rather than making a
/// live DDG request per call, so this can be polled by a load
/// balancer at any frequency without turning into a DDG amplification
/// vector. Returns 503 while the cached state is unhealthy or the process
/// is shutting down, so it can drive actual load-balancer removal decisions
/// the way `/live` deliberately does not.
async fn health_ready_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> (axum::http::StatusCode, Json<Value>) {
    let uptime = state.startup_time.elapsed().as_secs();
    let engine_healthy = state.engine_health.is_healthy();
    let engine_last_checked = state.engine_health.last_checked();
    let shutting_down = state.is_shutting_down.load(Ordering::Relaxed);

    let (status, code) = if shutting_down {
        ("shutting_down", axum::http::StatusCode::SERVICE_UNAVAILABLE)
    } else if engine_healthy {
        ("ok", axum::http::StatusCode::OK)
    } else {
        ("degraded", axum::http::StatusCode::SERVICE_UNAVAILABLE)
    };

    (
        code,
        Json(json!({
            "success": true,
            "data": {
                "status": status,
                "version": env!("CARGO_PKG_VERSION"),
                "uptime_seconds": uptime,
                "engine_healthy": engine_healthy,
                "engine_last_checked": engine_last_checked,
            }
        })),
    )
}

/// Maps a `SearchError` to the client-facing outcome, independent of any I/O
/// or metrics side effects, so the mapping itself can be unit-tested without
/// a live DDG connection. `None` means "not a client-facing error" — this is
/// only ever `ParseEmpty`: a query can legitimately have zero real-world
/// matches, so the HTTP response must stay a normal empty-results 200 even
/// though the caller is still expected to record the outcome as a metric.
fn map_search_error(e: &SearchError) -> Option<AppError> {
    match e {
        SearchError::ParseEmpty => None,
        SearchError::Blocked => Some(AppError::ServiceUnavailable(
            "Search engine is currently blocked upstream".into(),
        )),
        // Distinct from `Blocked` (403) — DDG served its interactive
        // anti-bot challenge instead of results. Client-facing, same as
        // `Blocked`: unlike `ParseEmpty`, this isn't "the query legitimately
        // had no results," so it must not be silently reported as an empty
        // 200.
        SearchError::Challenged => Some(AppError::ServiceUnavailable(
            "Search engine is presenting an anti-bot challenge upstream".into(),
        )),
        SearchError::Upstream(_) | SearchError::Transport(_) => {
            Some(AppError::ServiceUnavailable(e.to_string()))
        }
        // The parser only runs on the blocking pool; reaching this
        // arm means that blocking task itself panicked, which is a server
        // bug rather than an upstream/availability problem.
        SearchError::ParsePanicked => Some(AppError::Internal(e.to_string())),
    }
}

/// GET /api/v1/search — executes a DuckDuckGo search
async fn search_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Json<Value>, AppError> {
    let default_safe = state.config.search.safe_search_default;
    let request = SearchRequest::try_from_params(params, default_safe)?;

    // Cache key + finalize (filter + paginate) are shared with the MCP
    // gateway (src/gateway/mod.rs) — both write into the same cache, so
    // both must compute keys and cached payloads identically.
    let key = cache_key(&request);

    // Cache-hit latency is measured from here: `timing_ms` must report THIS
    // request's cost, not replay the original fetch's timing that was
    // frozen into the cached payload. A hit
    // does no I/O, so this lands at single-digit milliseconds.
    let start = std::time::Instant::now();

    // Check cache first
    if let Some(cached_bytes) = state.cache.get(&key).await {
        match serde_json::from_slice::<SearchResponse>(&cached_bytes) {
            Ok(mut response) => {
                response.cached = true;
                response.timing_ms = start.elapsed().as_millis() as u64;
                return Ok(Json(json!({
                    "success": true,
                    "data": response
                })));
            }
            Err(e) => {
                // A bad cache entry shouldn't break the request — log and
                // fall through to the normal non-cached path.
                tracing::error!(key = %key, error = %e, "Failed to deserialize cached response");
            }
        }
    }

    let results = match state
        .engine
        .search(
            &request.query,
            request.page,
            request.language.as_deref(),
            request.safe_search,
        )
        .await
    {
        Ok(results) => results,
        Err(e) => {
            match &e {
                SearchError::ParseEmpty => state.metrics.record_empty_parse(),
                SearchError::Blocked => state.metrics.record_blocked(),
                SearchError::Challenged => state.metrics.record_challenged(),
                SearchError::Upstream(_)
                | SearchError::Transport(_)
                | SearchError::ParsePanicked => state.metrics.record_error(),
            }
            match map_search_error(&e) {
                Some(app_err) => return Err(app_err),
                None => Vec::new(),
            }
        }
    };
    let timing_ms = start.elapsed().as_millis() as u64;
    state.metrics.record_search(timing_ms);

    let response = finalize_results(
        results,
        &request,
        state.config.search.max_results_per_page,
        timing_ms,
    );

    // Store in cache (best-effort, fire-and-forget — errors are logged internally)
    let ttl = Duration::from_secs(state.config.cache.ttl_seconds);
    match serde_json::to_vec(&response) {
        Ok(bytes) => state.cache.set(&key, &bytes, ttl).await,
        Err(e) => {
            tracing::error!(key = %key, error = %e, "Failed to serialize response for caching");
        }
    }

    Ok(Json(json!({
        "success": true,
        "data": response
    })))
}

/// Pull the caller-supplied API key out of `Authorization: Bearer <key>` or
/// `X-API-Key: <key>` header. `Authorization` is checked first since it's
/// the more standard header for this; `X-API-Key` is offered as a fallback
/// for callers (e.g. simple `curl`/monitoring setups) that find a bare
/// header easier to set than the `Bearer ` prefix. Malformed/non-UTF-8
/// header values are treated as "no key provided" rather than a parse
/// error — they can never match a configured key anyway.
fn extract_provided_key(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if let Some(token) = value.strip_prefix("Bearer ") {
            return Some(token.to_string());
        }
    }
    headers
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// Constant-time string comparison so a wrong-but-close guess can't be
/// distinguished from a wrong-and-nowhere-close one by response timing.
/// Cheap enough (the key is a handful of bytes, compared once per request)
/// that there's no reason to compare the naive way instead.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// `/api/v1/stats` exposes `total_searches`/`avg_latency_ms`, which is
/// enough to infer another deployment's traffic volume over time, so the
/// endpoint can optionally be gated. `configured_key` is
/// `state.config.server.api_key`;
/// `None` or `Some("")` (an explicitly-blanked env var, same convention as
/// `cors.origin`'s `Some("")` case in `build_cors_layer`) both mean the gate
/// is off and this always returns `Ok` — the endpoint stays exactly as open
/// as it is by default unless an operator opts in. When a key *is*
/// configured, a missing or non-matching header is rejected as
/// `AppError::Unauthorized`, which renders through the same JSON envelope
/// every other error path uses rather than a bare status code.
fn check_api_key(configured_key: Option<&str>, headers: &HeaderMap) -> Result<(), AppError> {
    let Some(configured_key) = configured_key.filter(|k| !k.is_empty()) else {
        return Ok(());
    };
    match extract_provided_key(headers) {
        Some(provided) if constant_time_eq(&provided, configured_key) => Ok(()),
        _ => Err(AppError::Unauthorized("Missing or invalid API key".into())),
    }
}

/// GET /api/v1/stats — returns system metrics
async fn stats_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, AppError> {
    check_api_key(state.config.server.api_key.as_deref(), &headers)?;
    let snapshot = state.metrics.snapshot();
    Ok(Json(json!({
        "success": true,
        "data": snapshot
    })))
}

/// GET /api/v1/extract — fetch a URL and extract clean content
///
/// This handler is intentionally thin — URL validation (below) is the only
/// thing that stays here (see `run_extract`'s doc comment in
/// `util::extract` for why: HTTP and MCP report a parse failure through
/// different error shapes, so parsing can't move into the shared pipeline).
/// Everything else — cache lookup, SSRF-validated fetch, capped body read,
/// extraction, and cache-on-success — lives in `util::extract::run_extract`,
/// shared with `gateway::mcp::serica_extract`.
async fn extract_handler(
    axum::extract::State(state): axum::extract::State<AppState>,
    Query(params): Query<crate::models::extract::ExtractRequest>,
) -> Result<Json<Value>, AppError> {
    let url = params.url.trim().to_string();
    if url.is_empty() {
        return Err(AppError::InvalidQuery("URL is empty".into()));
    }

    let parsed_url = url::Url::parse(&url)
        .map_err(|_| AppError::InvalidQuery("URL could not be parsed".into()))?;

    let response = run_extract(&state.config, &state.cache, &state.limiter, &parsed_url).await?;

    Ok(Json(json!({
        "success": true,
        "data": response
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gateway::apply_safe_search_filter;
    use crate::models::extract::ExtractResponse;
    use crate::models::search::SearchResult;
    use axum::body::Body;
    use axum::http::Request;
    use axum::http::StatusCode;
    use std::sync::Mutex;
    use tower::ServiceExt;
    use tracing::field::{Field, Visit};
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::Registry;

    // Test support: captures the `request_id` field of every
    // `"http_request"` span `TraceLayer`'s `make_span_with` closure creates,
    // so tests can assert (a) the field is populated at all — proving
    // `SetRequestIdLayer` ran before `TraceLayer` and stamped the header in
    // time — and (b) a span was created for a request that got rejected
    // downstream (429/timeout), proving that rejection happened inside
    // `TraceLayer`'s span rather than short-circuiting before it.
    //
    // A fresh `Registry` must NOT be installed as the *thread-local* default
    // (`tracing::subscriber::set_default`) per test. `tracing` caches each
    // callsite's `Interest` process-wide the first time it fires; under the
    // full test binary's default parallelism, some *other*, unrelated test
    // in this file reliably wins the race to be the first to ever construct
    // an `"http_request"` span — without any subscriber installed — which
    // permanently caches `Interest::never()` for that callsite for the rest
    // of the process, silently no-op'ing span creation for every later test
    // on every thread, including these two — an intermittent,
    // order/thread-scheduling-dependent 0-spans-captured failure.
    //
    // Instead, install exactly one *global* default for the whole test
    // binary (`set_global_default`, `Once`-guarded so only the first caller
    // actually installs it) plus one `rebuild_interest_cache()` to force a
    // fresh interest check against it — permanently fixing the callsite's
    // cached interest to "always" for the rest of the process, regardless
    // of what any other test already did. Every `http_request` span from
    // every test then lands in one shared, mutex-guarded log; each test
    // identifies its own span(s) by the unique `request_id` echoed back to
    // it in the `x-request-id` response header, rather than by an exact
    // total count (which concurrent unrelated tests would also perturb).
    #[derive(Default)]
    struct RequestIdVisitor(Option<String>);

    impl Visit for RequestIdVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            if field.name() == "request_id" {
                self.0 = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            if field.name() == "request_id" && self.0.is_none() {
                self.0 = Some(format!("{value:?}"));
            }
        }
    }

    fn global_request_id_log() -> &'static Mutex<Vec<String>> {
        static LOG: std::sync::OnceLock<Mutex<Vec<String>>> = std::sync::OnceLock::new();
        LOG.get_or_init(|| Mutex::new(Vec::new()))
    }

    struct GlobalRequestIdCaptureLayer;

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for GlobalRequestIdCaptureLayer {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if attrs.metadata().name() != "http_request" {
                return;
            }
            let mut visitor = RequestIdVisitor::default();
            attrs.record(&mut visitor);
            if let Some(id) = visitor.0 {
                global_request_id_log().lock().unwrap().push(id);
            }
        }
    }

    fn ensure_global_trace_capture_installed() {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            let subscriber = Registry::default().with(GlobalRequestIdCaptureLayer);
            // Ignore `Err`: harmless if some other global default beat us
            // to it (won't happen in this binary today, but `Once` doesn't
            // need to assume that to stay correct).
            let _ = tracing::subscriber::set_global_default(subscriber);
            tracing::callsite::rebuild_interest_cache();
        });
    }

    // `map_search_error` is the pure decision function behind the
    // search handler's Result handling — testable without a live DDG call.
    #[test]
    fn map_search_error_parse_empty_is_not_a_client_error() {
        assert!(map_search_error(&SearchError::ParseEmpty).is_none());
    }

    #[test]
    fn map_search_error_blocked_becomes_service_unavailable() {
        let app_err = map_search_error(&SearchError::Blocked).expect("must be a client error");
        let (status, code, _) = app_err.to_http_parts();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code, "SERVICE_UNAVAILABLE");
    }

    #[test]
    fn map_search_error_challenged_becomes_service_unavailable() {
        let app_err = map_search_error(&SearchError::Challenged).expect("must be a client error");
        let (status, code, _) = app_err.to_http_parts();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(code, "SERVICE_UNAVAILABLE");
    }

    #[test]
    fn map_search_error_upstream_becomes_service_unavailable() {
        let app_err = map_search_error(&SearchError::Upstream(StatusCode::BAD_GATEWAY))
            .expect("must be a client error");
        let (status, _, _) = app_err.to_http_parts();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn map_search_error_transport_becomes_service_unavailable() {
        let app_err = map_search_error(&SearchError::Transport("connection refused".into()))
            .expect("must be a client error");
        let (status, _, _) = app_err.to_http_parts();
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    fn test_router() -> Router {
        test_router_with_shutdown_flag().0
    }

    // Lets tests point the router's engine at a `wiremock` server
    // instead of the real `html.duckduckgo.com` endpoint — including for
    // `test_search_valid_query` below, not just for `tests/api_test.rs`.
    fn test_router_with_engine(engine: DuckDuckGoAdapter) -> Router {
        use crate::cache::noop::NoopCache;
        let config = Arc::new(Config::default());
        let engine = Arc::new(engine);
        let engine_health = Arc::new(EngineHealth::new());
        let metrics = Arc::new(MetricsCollector::new());
        let cache: Arc<dyn CacheLayer> = Arc::new(NoopCache);
        let gateway = HttpGateway::new(
            config,
            engine,
            engine_health,
            metrics,
            cache,
            Arc::new(AtomicBool::new(false)),
            Instant::now(),
            ExtractLimiter::new(2),
        );
        gateway
            .router()
            .expect("test config produces a valid router")
    }

    // Variant of `test_router()` that also hands back the
    // `is_shutting_down` flag `HttpGateway` was built with, so tests can
    // flip it and observe the health handlers react through the same
    // `AppState` a real graceful-shutdown future would mutate.
    fn test_router_with_shutdown_flag() -> (Router, Arc<AtomicBool>) {
        use crate::cache::noop::NoopCache;
        let config = Arc::new(Config::default());
        let engine = Arc::new(DuckDuckGoAdapter::new());
        let engine_health = Arc::new(EngineHealth::new());
        let metrics = Arc::new(MetricsCollector::new());
        let cache: Arc<dyn CacheLayer> = Arc::new(NoopCache);
        let is_shutting_down = Arc::new(AtomicBool::new(false));
        let gateway = HttpGateway::new(
            config,
            engine,
            engine_health,
            metrics,
            cache,
            is_shutting_down.clone(),
            Instant::now(),
            ExtractLimiter::new(2),
        );
        let router = gateway
            .router()
            .expect("test config produces a valid router");
        (router, is_shutting_down)
    }

    // The governor layer's `PeerIpKeyExtractor` reads `ConnectInfo<SocketAddr>`
    // from the request extensions, which is normally populated by
    // `into_make_service_with_connect_info` at serve time (see main.rs).
    // `Router::oneshot` bypasses that, so every test request needs the
    // extension set manually or key extraction fails closed (500) rather
    // than being rate-limited.
    fn get(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))))
            .body(Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn test_health_endpoint_returns_ok() {
        let app = test_router();
        let response = app.oneshot(get("/api/v1/health")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // Liveness/readiness must be servable from a freshly
    // constructed gateway that never spawned the background probe task —
    // `EngineHealth::new()`'s optimistic default is what makes this work.
    #[tokio::test]
    async fn test_health_live_and_ready_routes() {
        let app = test_router();
        let live = app
            .clone()
            .oneshot(get("/api/v1/health/live"))
            .await
            .unwrap();
        assert_eq!(live.status(), StatusCode::OK);

        let ready = app.oneshot(get("/api/v1/health/ready")).await.unwrap();
        assert_eq!(ready.status(), StatusCode::OK);
    }

    // The health handlers must not perform network I/O, so they must
    // complete quickly even though this test process has no route to
    // html.duckduckgo.com's real network path exercised by `probe_upstream`.
    // A live-call implementation would hang for up to the client's 10s
    // timeout instead.
    #[tokio::test(start_paused = true)]
    async fn test_health_handlers_do_not_block_on_network() {
        let app = test_router();
        let start = tokio::time::Instant::now();
        let response = app.oneshot(get("/api/v1/health/ready")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    // `is_shutting_down` must be the same `Arc<AtomicBool>`
    // `HttpGateway::new` was constructed with, not a fresh one minted inside
    // `router()` — otherwise flipping the flag (as the real
    // `with_graceful_shutdown` future does in `main.rs`) would be invisible
    // to the handlers below and both endpoints would keep reporting healthy
    // throughout a drain.
    #[tokio::test]
    async fn test_health_reflects_shutdown_flag_flip() {
        let (app, is_shutting_down) = test_router_with_shutdown_flag();

        let live = app
            .clone()
            .oneshot(get("/api/v1/health/live"))
            .await
            .unwrap();
        assert_eq!(live.status(), StatusCode::OK);
        let ready = app
            .clone()
            .oneshot(get("/api/v1/health/ready"))
            .await
            .unwrap();
        assert_eq!(ready.status(), StatusCode::OK);

        // Simulate what the graceful-shutdown future in `main.rs` does after
        // `shutdown_signal().await` resolves.
        is_shutting_down.store(true, Ordering::SeqCst);

        let live = app
            .clone()
            .oneshot(get("/api/v1/health/live"))
            .await
            .unwrap();
        assert_eq!(live.status(), StatusCode::OK, "liveness must stay 200");
        let live_body = axum::body::to_bytes(live.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let live_json: Value = serde_json::from_slice(&live_body).unwrap();
        assert_eq!(live_json["data"]["status"], "shutting_down");

        let ready = app.oneshot(get("/api/v1/health/ready")).await.unwrap();
        assert_eq!(
            ready.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "readiness must flip to 503 once draining starts"
        );
        let ready_body = axum::body::to_bytes(ready.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let ready_json: Value = serde_json::from_slice(&ready_body).unwrap();
        assert_eq!(ready_json["data"]["status"], "shutting_down");
    }

    // `with_base_url` points the router at a `wiremock` server carrying
    // the same DDG HTML fixture
    // `test_parse_results_from_live_html` (src/engines/duckduckgo.rs) and
    // `tests/api_test.rs` both use, so the parser has something real to
    // parse instead of an empty/synthetic body.
    #[tokio::test]
    async fn test_search_valid_query() {
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

        let app = test_router_with_engine(engine);
        let response = app.oneshot(get("/api/v1/search?q=rust")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            !json["data"]["results"].as_array().unwrap().is_empty(),
            "expected the mocked DDG fixture to parse into at least one result"
        );
    }

    #[tokio::test]
    async fn test_stats_endpoint() {
        let app = test_router();
        let response = app.oneshot(get("/api/v1/stats")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // `check_api_key` is the pure decision function behind the
    // `/api/v1/stats` gate — testable without spinning up a router.
    #[test]
    fn check_api_key_allows_when_unconfigured() {
        let headers = HeaderMap::new();
        assert!(check_api_key(None, &headers).is_ok());
    }

    #[test]
    fn check_api_key_allows_when_configured_as_empty_string() {
        // Same "blank env var means disabled" convention as
        // `build_cors_layer`'s `Some("") => Ok(None)` arm.
        let headers = HeaderMap::new();
        assert!(check_api_key(Some(""), &headers).is_ok());
    }

    #[test]
    fn check_api_key_rejects_missing_header_when_configured() {
        let headers = HeaderMap::new();
        let err = check_api_key(Some("secret"), &headers).expect_err("must reject");
        let (status, code, _) = err.to_http_parts();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(code, "UNAUTHORIZED");
    }

    #[test]
    fn check_api_key_rejects_wrong_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer wrong-key".parse().unwrap(),
        );
        assert!(check_api_key(Some("secret"), &headers).is_err());
    }

    #[test]
    fn check_api_key_accepts_matching_bearer_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::AUTHORIZATION,
            "Bearer secret".parse().unwrap(),
        );
        assert!(check_api_key(Some("secret"), &headers).is_ok());
    }

    #[test]
    fn check_api_key_accepts_matching_x_api_key_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "secret".parse().unwrap());
        assert!(check_api_key(Some("secret"), &headers).is_ok());
    }

    // Builds a router whose `server.api_key` is set, so the API-key gate on
    // `/api/v1/stats` is active — mirrors `test_cors_configured_origin_...`'s
    // pattern of constructing a non-default `Config` for a targeted test.
    fn router_with_api_key(api_key: &str) -> Router {
        use crate::cache::noop::NoopCache;
        let mut config = Config::default();
        config.server.api_key = Some(api_key.to_string());
        let config = Arc::new(config);
        let engine = Arc::new(DuckDuckGoAdapter::new());
        let engine_health = Arc::new(EngineHealth::new());
        let metrics = Arc::new(MetricsCollector::new());
        let cache: Arc<dyn CacheLayer> = Arc::new(NoopCache);
        let gateway = HttpGateway::new(
            config,
            engine,
            engine_health,
            metrics,
            cache,
            Arc::new(AtomicBool::new(false)),
            Instant::now(),
            ExtractLimiter::new(2),
        );
        gateway
            .router()
            .expect("test config produces a valid router")
    }

    fn get_with_header(uri: &str, header_name: &str, header_value: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header(header_name, header_value)
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))))
            .body(Body::empty())
            .unwrap()
    }

    // With `server.api_key` unset (the default, `test_router()`'s
    // `Config::default()`), `/api/v1/stats` stays open — already covered by
    // `test_stats_endpoint` above, restated here so the "no auth configured
    // -> unchanged behavior" guarantee has an explicit regression test of
    // its own.
    #[tokio::test]
    async fn test_stats_open_by_default_when_api_key_unset() {
        let app = test_router();
        let response = app.oneshot(get("/api/v1/stats")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // (b) A configured key rejects a request with no Authorization/X-API-Key
    // header at all.
    #[tokio::test]
    async fn test_stats_rejects_missing_api_key_when_configured() {
        let app = router_with_api_key("s3cr3t");
        let response = app.oneshot(get("/api/v1/stats")).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["error"]["code"], "UNAUTHORIZED");
    }

    // (b) A configured key rejects a request carrying the wrong key.
    #[tokio::test]
    async fn test_stats_rejects_wrong_api_key() {
        let app = router_with_api_key("s3cr3t");
        let request = get_with_header("/api/v1/stats", "authorization", "Bearer wrong");
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    // (c) A request carrying the correct `Authorization: Bearer` header
    // succeeds.
    #[tokio::test]
    async fn test_stats_accepts_correct_bearer_api_key() {
        let app = router_with_api_key("s3cr3t");
        let request = get_with_header("/api/v1/stats", "authorization", "Bearer s3cr3t");
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    // (c) A request carrying the correct `X-API-Key` header succeeds too.
    #[tokio::test]
    async fn test_stats_accepts_correct_x_api_key_header() {
        let app = router_with_api_key("s3cr3t");
        let request = get_with_header("/api/v1/stats", "x-api-key", "s3cr3t");
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_search_empty_query_returns_400() {
        let app = test_router();
        let response = app.oneshot(get("/api/v1/search?q=")).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // A well-formed 2-letter ISO 639-1 code that this
    // service has no `kl` region mapping for must be rejected with a clear
    // 400 rather than silently searching unconstrained (the caller would
    // otherwise have no way to know their language filter was ignored).
    // "ja" (Japanese) is a valid ISO 639-1 code but not in
    // `duckduckgo::SUPPORTED_LANGUAGES`.
    #[tokio::test]
    async fn test_search_unmapped_language_returns_400() {
        let app = test_router();
        let response = app
            .oneshot(get("/api/v1/search?q=rust&lang=ja"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["error"]["code"], "INVALID_LANGUAGE");
    }

    // Languages this service does have a `kl` mapping for
    // must still pass validation.
    #[test]
    fn try_from_params_accepts_every_supported_language() {
        for &lang in crate::engines::duckduckgo::SUPPORTED_LANGUAGES {
            let params = SearchParams {
                q: Some("rust".into()),
                page: None,
                safe: None,
                lang: Some(lang.to_string()),
            };
            let req = SearchRequest::try_from_params(params, 1)
                .unwrap_or_else(|e| panic!("{lang} should be accepted, got error: {e:?}"));
            assert_eq!(req.language.as_deref(), Some(lang));
        }
    }

    #[test]
    fn try_from_params_rejects_unmapped_language() {
        let params = SearchParams {
            q: Some("rust".into()),
            page: None,
            safe: None,
            lang: Some("ja".into()),
        };
        let err = SearchRequest::try_from_params(params, 1)
            .expect_err("ja has no kl mapping and must be rejected");
        let (status, code, _) = err.to_http_parts();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code, "INVALID_LANGUAGE");
    }

    // The length check must count Unicode scalar values,
    // not UTF-8 bytes. "\u{4e2d}" (中) is 3 bytes but 1 char, so 500 of them
    // is 1500 bytes / exactly 500 chars — exactly at the documented
    // 500-character limit, so it must be accepted despite the larger byte
    // count.
    #[test]
    fn try_from_params_accepts_500_char_cjk_query_despite_exceeding_500_bytes() {
        let query: String = "中".repeat(500);
        assert_eq!(query.chars().count(), 500);
        assert!(query.len() > 500, "sanity check: this must be >500 bytes");
        let params = SearchParams {
            q: Some(query.clone()),
            page: None,
            safe: None,
            lang: None,
        };
        let req = SearchRequest::try_from_params(params, 1)
            .unwrap_or_else(|e| panic!("500-char query should be accepted, got error: {e:?}"));
        assert_eq!(req.query, query);
    }

    // One char over the limit is still rejected, even
    // when that single extra char keeps the byte count small.
    #[test]
    fn try_from_params_rejects_501_char_cjk_query() {
        let query: String = "中".repeat(501);
        let params = SearchParams {
            q: Some(query),
            page: None,
            safe: None,
            lang: None,
        };
        let err =
            SearchRequest::try_from_params(params, 1).expect_err("501-char query must be rejected");
        let (status, code, _) = err.to_http_parts();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code, "INVALID_QUERY");
    }

    // `page`/`safe` boundary behavior is exercised end-to-end only
    // indirectly, through integration tests (`search_invalid_page_returns_400`
    // etc. in tests/api_test.rs). `try_from_params` never touches the engine,
    // so these boundaries are directly and cheaply unit-testable here.
    fn params_with_page(page: Option<u32>) -> SearchParams {
        SearchParams {
            q: Some("rust".into()),
            page,
            safe: None,
            lang: None,
        }
    }

    #[test]
    fn try_from_params_rejects_page_zero() {
        let err = SearchRequest::try_from_params(params_with_page(Some(0)), 1)
            .expect_err("page 0 must be rejected");
        let (status, code, _) = err.to_http_parts();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code, "INVALID_PAGE");
    }

    #[test]
    fn try_from_params_accepts_page_one() {
        let req = SearchRequest::try_from_params(params_with_page(Some(1)), 1)
            .expect("page 1 must be accepted");
        assert_eq!(req.page, 1);
    }

    #[test]
    fn try_from_params_accepts_page_fifty() {
        let req = SearchRequest::try_from_params(params_with_page(Some(50)), 1)
            .expect("page 50 (the documented upper bound) must be accepted");
        assert_eq!(req.page, 50);
    }

    #[test]
    fn try_from_params_rejects_page_fifty_one() {
        let err = SearchRequest::try_from_params(params_with_page(Some(51)), 1)
            .expect_err("page 51 must be rejected");
        let (status, code, _) = err.to_http_parts();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code, "INVALID_PAGE");
    }

    #[test]
    fn try_from_params_defaults_missing_page_to_one() {
        let req = SearchRequest::try_from_params(params_with_page(None), 1)
            .expect("missing page must default rather than error");
        assert_eq!(req.page, 1);
    }

    fn params_with_safe(safe: Option<u8>) -> SearchParams {
        SearchParams {
            q: Some("rust".into()),
            page: None,
            safe,
            lang: None,
        }
    }

    #[test]
    fn try_from_params_accepts_every_documented_safe_search_level() {
        for level in 0..=2u8 {
            let req = SearchRequest::try_from_params(params_with_safe(Some(level)), 1)
                .unwrap_or_else(|e| panic!("safe_search={level} should be accepted, got {e:?}"));
            assert_eq!(req.safe_search, level);
        }
    }

    #[test]
    fn try_from_params_rejects_safe_search_three() {
        let err = SearchRequest::try_from_params(params_with_safe(Some(3)), 1)
            .expect_err("safe_search 3 must be rejected");
        let (status, code, _) = err.to_http_parts();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code, "INVALID_SAFE_SEARCH");
    }

    #[test]
    fn try_from_params_missing_safe_search_falls_back_to_configured_default() {
        let req = SearchRequest::try_from_params(params_with_safe(None), 2)
            .expect("missing safe must fall back to the caller-provided default, not error");
        assert_eq!(req.safe_search, 2);
    }

    // A bare concurrency semaphore with no time window would never
    // throttle a client issuing fast sequential requests. With a real
    // per-client token
    // bucket, a `rate_limit_per_minute` of 1 (burst_size clamps to 1) means
    // exactly one request succeeds before the next is rejected — and the
    // rejection must carry the standard AppError JSON envelope plus a
    // Retry-After header instead of tower_governor's bare plaintext body.
    #[tokio::test]
    async fn test_rate_limit_returns_429_with_envelope_and_retry_after() {
        use crate::cache::noop::NoopCache;
        let mut config = Config::default();
        config.server.rate_limit_per_minute = 1;
        let config = Arc::new(config);
        let engine = Arc::new(DuckDuckGoAdapter::new());
        let engine_health = Arc::new(EngineHealth::new());
        let metrics = Arc::new(MetricsCollector::new());
        let cache: Arc<dyn CacheLayer> = Arc::new(NoopCache);
        let gateway = HttpGateway::new(
            config,
            engine,
            engine_health,
            metrics,
            cache,
            Arc::new(AtomicBool::new(false)),
            Instant::now(),
            ExtractLimiter::new(2),
        );
        let app = gateway
            .router()
            .expect("test config produces a valid router");

        let first = app.clone().oneshot(get("/api/v1/stats")).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app.clone().oneshot(get("/api/v1/stats")).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(second.headers().contains_key("retry-after"));

        let body = axum::body::to_bytes(second.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["error"]["code"], "RATE_LIMITED");

        // Health must stay reachable while other endpoints are throttled
        // (the health-flapping scenario). This also covers the /live-vs-/ready
        // split: neither sits behind either governor
        // layer, so abuse of /search or /extract can't starve either.
        let health = app.clone().oneshot(get("/api/v1/health")).await.unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        let live = app
            .clone()
            .oneshot(get("/api/v1/health/live"))
            .await
            .unwrap();
        assert_eq!(live.status(), StatusCode::OK);
        let ready = app.oneshot(get("/api/v1/health/ready")).await.unwrap();
        assert_eq!(ready.status(), StatusCode::OK);
    }

    // `x-request-id` is generated by `SetRequestIdLayer`
    // and must also be echoed back to the client by `PropagateRequestIdLayer`
    // — without that layer the header never leaves the server.
    #[tokio::test]
    async fn test_response_echoes_x_request_id_header() {
        let app = test_router();
        let response = app.oneshot(get("/api/v1/health")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let request_id = response
            .headers()
            .get("x-request-id")
            .expect("x-request-id must be echoed back to the client")
            .to_str()
            .unwrap();
        assert!(!request_id.is_empty());
    }

    // `SetRequestIdLayer` must run *before* `TraceLayer`
    // (outside it in axum's wrapping order) so the `x-request-id` header
    // exists on the request by the time the trace span is built and can be
    // read into the span's `request_id` field — running it *inside*
    // `TraceLayer` would leave the field unavailable/empty.
    #[tokio::test]
    async fn test_trace_span_is_populated_with_request_id() {
        ensure_global_trace_capture_installed();

        let app = test_router();
        let response = app.oneshot(get("/api/v1/health")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response_request_id = response
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            !response_request_id.is_empty(),
            "the echoed x-request-id must be populated, not empty"
        );

        let log = global_request_id_log().lock().unwrap();
        assert!(
            log.contains(&response_request_id),
            "the span must have recorded the same request ID that was echoed to the client"
        );
    }

    // A 429 rate-limit rejection must still produce a
    // traced `http_request` span with a populated `request_id` — proving the
    // governor layer's rejection happens *inside* `TraceLayer`'s span rather
    // than short-circuiting the response before `TraceLayer` ever sees it,
    // which is exactly what an operator needs during a throttling incident.
    #[tokio::test]
    async fn test_rate_limited_response_is_traced_and_echoes_request_id() {
        use crate::cache::noop::NoopCache;
        let mut config = Config::default();
        config.server.rate_limit_per_minute = 1;
        let config = Arc::new(config);
        let engine = Arc::new(DuckDuckGoAdapter::new());
        let engine_health = Arc::new(EngineHealth::new());
        let metrics = Arc::new(MetricsCollector::new());
        let cache: Arc<dyn CacheLayer> = Arc::new(NoopCache);
        let gateway = HttpGateway::new(
            config,
            engine,
            engine_health,
            metrics,
            cache,
            Arc::new(AtomicBool::new(false)),
            Instant::now(),
            ExtractLimiter::new(2),
        );
        let app = gateway
            .router()
            .expect("test config produces a valid router");

        ensure_global_trace_capture_installed();

        let first = app.clone().oneshot(get("/api/v1/stats")).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first_id = first
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(!first_id.is_empty());

        let second = app.oneshot(get("/api/v1/stats")).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        let second_id = second
            .headers()
            .get("x-request-id")
            .expect("a rejected request must still get an echoed x-request-id")
            .to_str()
            .unwrap()
            .to_string();
        assert!(!second_id.is_empty());

        let log = global_request_id_log().lock().unwrap();
        assert!(
            log.contains(&first_id),
            "the accepted request must produce a traced span with its request id"
        );
        assert!(
            log.contains(&second_id),
            "the rate-limited request must still produce a traced span with its request id"
        );
    }

    // The default config must attach no CORS layer at
    // all, so even a request carrying a cross-origin `Origin` header gets no
    // `Access-Control-Allow-Origin` back.
    #[tokio::test]
    async fn test_cors_header_absent_by_default() {
        let app = test_router();
        let request = Request::builder()
            .uri("/api/v1/health")
            .header("origin", "https://evil.example.com")
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response
            .headers()
            .contains_key("access-control-allow-origin"));
    }

    // A configured, valid origin gets a real, restrictive
    // CorsLayer (not `CorsLayer::permissive()`) that echoes back exactly the
    // configured origin.
    #[tokio::test]
    async fn test_cors_configured_origin_returns_restrictive_header() {
        use crate::cache::noop::NoopCache;
        let mut config = Config::default();
        config.cors.origin = Some("https://example.com".into());
        let config = Arc::new(config);
        let engine = Arc::new(DuckDuckGoAdapter::new());
        let engine_health = Arc::new(EngineHealth::new());
        let metrics = Arc::new(MetricsCollector::new());
        let cache: Arc<dyn CacheLayer> = Arc::new(NoopCache);
        let gateway = HttpGateway::new(
            config,
            engine,
            engine_health,
            metrics,
            cache,
            Arc::new(AtomicBool::new(false)),
            Instant::now(),
            ExtractLimiter::new(2),
        );
        let app = gateway.router().expect("valid cors origin builds a router");

        let request = Request::builder()
            .uri("/api/v1/health")
            .header("origin", "https://example.com")
            .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
                [127, 0, 0, 1],
                12345,
            ))))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("access-control-allow-origin")
                .unwrap(),
            "https://example.com"
        );
    }

    // An unparseable configured origin must fail startup,
    // never silently widen to "*".
    #[test]
    fn test_cors_invalid_origin_fails_fast_instead_of_widening_to_wildcard() {
        let result = build_cors_layer(Some("https://example.com\r\nX-Injected: 1"));
        assert!(result.is_err());
    }

    #[test]
    fn test_router_build_fails_fast_on_invalid_cors_origin() {
        use crate::cache::noop::NoopCache;
        let mut config = Config::default();
        config.cors.origin = Some("https://example.com\r\nX-Injected: 1".into());
        let config = Arc::new(config);
        let engine = Arc::new(DuckDuckGoAdapter::new());
        let engine_health = Arc::new(EngineHealth::new());
        let metrics = Arc::new(MetricsCollector::new());
        let cache: Arc<dyn CacheLayer> = Arc::new(NoopCache);
        let gateway = HttpGateway::new(
            config,
            engine,
            engine_health,
            metrics,
            cache,
            Arc::new(AtomicBool::new(false)),
            Instant::now(),
            ExtractLimiter::new(2),
        );
        assert!(gateway.router().is_err());
    }

    // An explicit "*" is honored as a deliberate opt-in, distinct
    // from it being the default.
    #[test]
    fn test_cors_wildcard_is_explicit_opt_in() {
        assert!(build_cors_layer(Some("*")).unwrap().is_some());
    }

    // A route wrapped with an intentionally tiny
    // `TimeoutLayer` budget must produce the same `{"success": false,
    // "error": {...}}` envelope every other error path uses, not
    // `tower_http::timeout::TimeoutLayer`'s raw empty-body
    // `TIMEOUT_MARKER_STATUS` response. Built as a standalone router (not via
    // `test_router()`/`HttpGateway::router`, which would need a live search
    // to actually be slow) with a handler that never resolves, so this has
    // no live-network dependency. `start_paused = true` lets tokio
    // auto-advance its virtual clock straight to the `TimeoutLayer`'s 10ms
    // deadline instead of this test taking 10ms of real wall-clock time —
    // the same pattern `test_health_handlers_do_not_block_on_network` uses
    // above.
    #[tokio::test(start_paused = true)]
    async fn test_timeout_layer_produces_standard_error_envelope() {
        async fn never_responds() -> &'static str {
            std::future::pending::<()>().await;
            unreachable!("timeout must win the race before this future is ever polled again")
        }

        let app = Router::new()
            // Fully qualified: this test module's own `get(uri: &str)`
            // request-builder helper (below) shadows `axum::routing::get`
            // for the rest of this file via `use super::*;`.
            .route("/slow", axum::routing::get(never_responds))
            .layer(TimeoutLayer::with_status_code(
                TIMEOUT_MARKER_STATUS,
                Duration::from_millis(10),
            ))
            .layer(middleware::map_response(convert_timeout_response));

        let request = Request::builder().uri("/slow").body(Body::empty()).unwrap();
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["error"]["code"], "SERVICE_UNAVAILABLE");
    }

    // `AppError::ExtractTimeout` also produces a real 504
    // (`TIMEOUT_MARKER_STATUS`), which `convert_timeout_response` would
    // clobber into a generic "SERVICE_UNAVAILABLE" envelope,
    // indistinguishable from any other timeout — exactly the outcome the
    // dedicated `EXTRACT_TIMEOUT` code exists to avoid. A
    // handler-produced 504 with a real (non-empty) body must survive
    // `convert_timeout_response` completely unchanged, while the sibling
    // test above (a genuinely empty-bodied `TimeoutLayer` timeout) must
    // still be rewritten — the two must NOT be conflated.
    #[tokio::test]
    async fn test_extract_timeout_response_survives_convert_timeout_response() {
        async fn returns_extract_timeout() -> Response {
            AppError::ExtractTimeout("extract exceeded 3000ms".into()).into_response()
        }

        let app = Router::new()
            .route("/slow", axum::routing::get(returns_extract_timeout))
            .layer(middleware::map_response(convert_timeout_response));

        let request = Request::builder().uri("/slow").body(Body::empty()).unwrap();
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["error"]["code"], "EXTRACT_TIMEOUT");
        assert_eq!(json["error"]["message"], "extract exceeded 3000ms");
    }

    #[test]
    fn test_safe_search_word_boundary() {
        let results = vec![SearchResult {
            url: "https://example.com".into(),
            title: "Breast cancer research findings".into(),
            snippet: "Medical study on breast cancer".into(),
        }];
        // "breast cancer" should NOT be filtered — no blocklist word appears as whole word
        let filtered = apply_safe_search_filter(results, 2);
        assert_eq!(filtered.len(), 1);
    }

    #[test]
    fn test_safe_search_blocks_escort_service() {
        let results = vec![SearchResult {
            url: "https://example.com".into(),
            title: "Escort service for elderly".into(),
            snippet: "Care services".into(),
        }];
        // "escort" as whole word should be blocked
        let filtered = apply_safe_search_filter(results, 1);
        assert_eq!(filtered.len(), 0);
    }

    // A cache hit for `/api/v1/extract` must
    // return the cached `ExtractResponse` directly, with `cached: true` set,
    // never reaching `fetch_extract_target`. Proven without any network
    // dependency by pointing the request at a `http://` URL: SSRF validation
    // (`validate_extract_target`, invoked by `fetch_extract_target`) rejects
    // any non-`https` scheme immediately, so if the cache check were
    // accidentally skipped — or the fetch path were reached at all — this
    // would come back as a 400 `INVALID_QUERY` instead of a cached 200,
    // making a short-circuit regression fail loudly rather than silently.
    #[tokio::test]
    async fn test_extract_cache_hit_skips_fetch() {
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
            async fn set(&self, _key: &str, _value: &[u8], _ttl: Duration) {}
        }

        let url = "http://example.com/cached-article";
        let parsed = url::Url::parse(url).unwrap();
        let key =
            crate::gateway::extract_cache_key(&crate::util::ssrf::normalize_extract_url(&parsed));

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

        let config = Arc::new(Config::default());
        let engine = Arc::new(DuckDuckGoAdapter::new());
        let engine_health = Arc::new(EngineHealth::new());
        let metrics = Arc::new(MetricsCollector::new());
        let gateway = HttpGateway::new(
            config,
            engine,
            engine_health,
            metrics,
            cache,
            Arc::new(AtomicBool::new(false)),
            Instant::now(),
            ExtractLimiter::new(2),
        );
        let app = gateway
            .router()
            .expect("test config produces a valid router");

        let request = get(&format!("/api/v1/extract?url={}", url));
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "expected a cache-hit 200, not a fetch attempt against an http:// URL"
        );

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["data"]["cached"], true);
        assert_eq!(json["data"]["title"], "Cached Title");
        assert_eq!(json["data"]["content"], "Cached content");
    }

    // A search cache hit must report THIS request's
    // latency in `timing_ms`, not replay the original fetch's timing frozen
    // into the cached payload. The canned entry carries an absurd 1_000_000
    // ms so replaying it verbatim cannot pass by luck of a fast machine.
    // A wrong key would make the cache miss and fall
    // through to the engine — for which `DuckDuckGoAdapter::new()` has no
    // stub and no DDG endpoint to reach, so a key mismatch cannot fake a
    // pass: it would error or hang loudly, not silently return a canned hit.
    #[tokio::test]
    async fn test_search_cache_hit_remeasures_timing_ms() {
        use crate::cache::CacheLayer;

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
            async fn set(&self, _key: &str, _value: &[u8], _ttl: Duration) {}
        }

        let canned_req = SearchRequest {
            query: "canned timing query".into(),
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

        let config = Arc::new(Config::default());
        let engine = Arc::new(DuckDuckGoAdapter::new());
        let engine_health = Arc::new(EngineHealth::new());
        let metrics = Arc::new(MetricsCollector::new());
        let gateway = HttpGateway::new(
            config,
            engine,
            engine_health,
            metrics,
            cache,
            Arc::new(AtomicBool::new(false)),
            Instant::now(),
            ExtractLimiter::new(2),
        );
        let app = gateway
            .router()
            .expect("test config produces a valid router");

        let request = get("/api/v1/search?q=canned%20timing%20query");
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::OK,
            "expected a cache-hit 200, not a fetch attempt"
        );

        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["data"]["cached"], true,
            "handler must report this as a cache hit"
        );
        let timing = json["data"]["timing_ms"]
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

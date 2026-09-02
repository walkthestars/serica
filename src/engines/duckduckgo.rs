// DuckDuckGo HTML engine adapter
// Targets the no-JS HTML endpoint: html.duckduckgo.com
// Parses server-rendered HTML and extracts result URLs from DDG redirect links.

use scraper::{Html, Selector};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

use crate::models::search::SearchResult;

/// Distinguishes *why* a search produced no usable results, so callers can
/// tell "DuckDuckGo is unreachable/blocking us" (a real failure the client
/// should see as unavailable) apart from "this query has zero matches" (a
/// normal, cacheable, 200-worthy outcome). `ParseEmpty` sits in between: the
/// HTTP layer succeeded but the selector-based parser found nothing, which
/// is the first symptom of DDG changing its markup — *or* of DDG's own
/// anti-bot heuristics being one increment more aggressive than what
/// `Challenged` currently pattern-matches, since `is_challenge_page` matches
/// on a specific known signature rather than "any non-200 success status."
/// It must never surface to callers as a client-facing error — a query CAN
/// legitimately have no results — but it must still be distinguishable from
/// `Ok` so it can be counted as an alertable metric instead of silently
/// looking like a normal empty search.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SearchError {
    #[error("upstream blocked the request (403)")]
    Blocked,
    /// DDG returned its interactive anti-bot challenge (an `anomaly-modal`
    /// CAPTCHA page, HTTP 202) instead of results. Distinct from `Blocked`
    /// (403) and `ParseEmpty` (a 2xx with no recognizable challenge or
    /// result markup — status alone doesn't distinguish "genuinely 0
    /// results" from "challenged," which is why detection matches on body
    /// content, not just the 202 status; see `is_challenge_page`).
    #[error("upstream returned an anti-bot challenge (202) instead of results")]
    Challenged,
    #[error("upstream returned non-success status: {0}")]
    Upstream(reqwest::StatusCode),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("upstream returned a success status but zero results were parsed")]
    ParseEmpty,
    #[error("search result parser task panicked")]
    ParsePanicked,
}

/// Detects DuckDuckGo's interactive anti-bot challenge page. Matches on body
/// content, not status alone: 202 in principle could be a legitimate
/// response for something else in the future, and the `anomaly-modal`/
/// `challenge-form` markup is what actually identifies "this is DDG's CAPTCHA
/// page," not the status code by itself. Confirmed against a live capture —
/// see `tests/fixtures/ddg_challenge_202.html` — which contains
/// `anomaly-modal` (as a CSS class, e.g. `anomaly-modal__mask`) and the
/// `id="challenge-form"` element that wraps the actual CAPTCHA puzzle.
fn is_challenge_page(status: reqwest::StatusCode, body: &str) -> bool {
    status == reqwest::StatusCode::ACCEPTED
        && (body.contains("anomaly-modal") || body.contains("challenge-form"))
}

/// DDG wraps all result links in redirect URLs of the form:
///   //duckduckgo.com/l/?uddg=<url-encoded-target>&rut=<token>
/// `Url::query_pairs()` already percent-decodes the value, so we get the
/// decoded target URL directly.
fn extract_uddg_url(href: &str) -> Option<String> {
    // href may be protocol-relative: //duckduckgo.com/l/...
    let absolute = if href.starts_with("//") {
        format!("https:{}", href)
    } else if href.starts_with("http") {
        href.to_string()
    } else {
        return None;
    };

    let parsed = Url::parse(&absolute).ok()?;
    // query_pairs() percent-decodes values automatically
    let target = parsed
        .query_pairs()
        .find(|(k, _)| k == "uddg")
        .map(|(_, v)| v.to_string())?;

    if target.is_empty() {
        return None;
    }

    // Reject non-HTTP(S) schemes at the parse boundary rather than trusting
    // the upstream: DDG's `uddg` value is attacker-influenceable (it echoes
    // whatever the results page contains), and a `javascript:`/`data:`/
    // `vbscript:` URL passed through verbatim becomes stored XSS in any
    // consumer that renders results as links, or a direct action target for
    // MCP clients that feed URLs straight to an LLM agent.
    let parsed = Url::parse(&target).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }

    Some(parsed.to_string())
}

/// ISO 639-1 codes this service accepts for the `lang`/`language` request
/// parameter. Kept as one canonical list (rather than duplicated inline
/// wherever a "supported languages" message is built) so
/// `ddg_region_for_language`'s match arms, `SearchRequest::try_from_params`'s
/// rejection message (src/gateway/http.rs), and the MCP tool schema's
/// `language.enum` (src/gateway/mcp.rs) can't quietly drift apart the way the
/// old per-language `common_words_for_language` table in src/gateway/mod.rs.
pub const SUPPORTED_LANGUAGES: &[&str] = &["en", "de", "fr", "es"];

/// Maps a supported ISO 639-1 language code to DuckDuckGo's `kl` region
/// parameter (`<country>-<lang>`, e.g. `us-en`, `de-de`). Returns `None` for
/// anything not in `SUPPORTED_LANGUAGES` — callers must treat that as "this
/// service has no region mapping for this language" and reject the request
/// up front (see `SearchRequest::try_from_params`) rather than silently
/// searching unconstrained: a caller requesting `lang=ja` would otherwise
/// have no way to know their filter was ignored.
///
/// This is a deliberate alternative to a local post-filter that tries to detect language
/// from result text via `common_words.iter().any(|w| text.contains(w))` — a
/// substring test against short common words that matched inside unrelated
/// text almost everywhere (English "is" inside "th-IS", "informat-ION").
/// Filtering via `kl` happens at the source instead, which is both accurate
/// (DuckDuckGo's own region logic, not a hand-rolled heuristic) and avoids
/// a class of bug where a post-filter shrinks a results page after
/// DDG has already committed pagination/offsets to the unfiltered count.
pub fn ddg_region_for_language(lang: &str) -> Option<&'static str> {
    match lang {
        "en" => Some("us-en"),
        "de" => Some("de-de"),
        "fr" => Some("fr-fr"),
        "es" => Some("es-es"),
        _ => None,
    }
}

/// Maps a `safe_search` level (`0`=off, `1`=moderate, `2`=strict — the same
/// three values `SearchRequest::try_from_params`/the MCP tool schema accept)
/// to DuckDuckGo's own `kp` safe-search parameter. This is what lets
/// DDG's own, far more capable classifier do the real filtering at the
/// source instead of relying solely on the local word-boundary blocklist in
/// `src/gateway/mod.rs`, which — being a fixed 18-word English list checked
/// only against `title`/`snippet` — is both trivially evaded (leetspeak,
/// non-English terms, a clean title over an explicit URL) and, at the
/// standalone-word entries like `"sex"`/`"adult"`/`"nude"`/`"webcam"`,
/// over-broad enough to filter legitimate results by default.
///
/// Values confirmed against DuckDuckGo's own documented URL parameters
/// (`kp=1` on/strict, `kp=-1` moderate, `kp=-2` off). DDG has three
/// distinct states, and `-1` is moderate, not off. Always returns a value
/// (never `None`/omits the param): the client intentionally carries no
/// cookie jar (see `DuckDuckGoAdapter::new`), so omitting `kp` would leave
/// every request at whatever
/// DDG's own unauthenticated default is rather than the caller's actual
/// requested level — sending it explicitly on every request, including
/// `off`, is what makes all three levels deterministic.
fn ddg_kp_for_safe_search(level: u8) -> &'static str {
    match level {
        0 => "-2",
        1 => "-1",
        // `2` is the only other value `SearchRequest::try_from_params`/the
        // MCP handler let through (both reject `> 2` before this is ever
        // called), so this arm also covers `2` explicitly; anything else is
        // defensive and treated as the strictest setting rather than
        // silently falling back to unfiltered.
        _ => "1",
    }
}

/// Build the DDG search URL for a query/page/language/safe_search
/// combination. A pure function (no network I/O) so the `kl`/`kp` parameters'
/// presence/value can be unit tested directly instead of only observable by
/// inspecting a live request.
fn build_search_url(
    base: &str,
    query: &str,
    offset: u32,
    language: Option<&str>,
    safe_search: u8,
) -> String {
    let encoded: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
    let mut url = format!("{}?q={}&s={}", base, encoded, offset);
    if let Some(region) = language.and_then(ddg_region_for_language) {
        url.push_str("&kl=");
        url.push_str(region);
    }
    url.push_str("&kp=");
    url.push_str(ddg_kp_for_safe_search(safe_search));
    url
}

/// DuckDuckGo adapter — queries the HTML endpoint, parses results, resolves redirect URLs.
pub struct DuckDuckGoAdapter {
    client: reqwest::Client,
    search_url: String,
}

impl Default for DuckDuckGoAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl DuckDuckGoAdapter {
    pub fn new() -> Self {
        Self::build("https://html.duckduckgo.com/html/".to_string(), true, None)
            .expect("Failed to build HTTP client")
    }

    /// Production constructor variant: same as `new()`, but routes search
    /// traffic through `proxy_url` (e.g. a corporate egress proxy) instead of
    /// dialling DuckDuckGo directly. Fallible — unlike `new()` — because a
    /// malformed `SERICA_PROXY__SEARCH_URL` is a startup-time configuration
    /// error the caller should report, not a `panic!`.
    ///
    /// Deliberately a separate constructor rather than a parameter on
    /// `new()`: this is the *search* egress path only. It has no bearing on
    /// `/api/v1/extract`'s SSRF/IP-pinning guarantee (see
    /// `util::ssrf::build_pinned_extract_client`, which takes its own,
    /// independently-configured proxy) — keeping them as distinct functions
    /// makes it structurally impossible to accidentally wire the extract
    /// proxy through here instead.
    pub fn with_proxy(proxy_url: &str) -> Result<Self, reqwest::Error> {
        Self::build(
            "https://html.duckduckgo.com/html/".to_string(),
            true,
            Some(proxy_url),
        )
    }

    /// Test-only constructor that points the adapter at an
    /// arbitrary base URL instead of the real DuckDuckGo endpoint — e.g. a
    /// `wiremock::MockServer`'s URI — so `search()`/`probe_upstream()` can be
    /// exercised against a fixture-backed local server instead of the live
    /// internet. Not `#[cfg(test)]`-gated: `tests/api_test.rs` and
    /// `src/gateway/http.rs`'s/`mcp.rs`'s own test modules all need it, and
    /// none of those are visible to a `#[cfg(test)]` item in this crate.
    ///
    /// Deliberately does *not* set `.https_only(true)` the way `new()` does:
    /// a `wiremock::MockServer` serves plain HTTP on `127.0.0.1`, and forcing
    /// HTTPS here would make this constructor unable to reach the exact
    /// thing it exists to reach. This is safe only because the base URL is a
    /// fixed value baked in by test code at construction time, never
    /// attacker- or request-influenced input — unlike the extract endpoint's
    /// caller-supplied URL, whose SSRF guard and HTTPS-only
    /// policy there are non-negotiable and have no equivalent escape hatch.
    /// Do not use this outside tests; `new()` is the production constructor.
    pub fn with_base_url(base: impl Into<String>) -> Self {
        Self::build(base.into(), false, None).expect("Failed to build HTTP client")
    }

    fn build(
        search_url: String,
        https_only: bool,
        proxy_url: Option<&str>,
    ) -> Result<Self, reqwest::Error> {
        let mut builder = reqwest::Client::builder()
            // No cookie store: reqwest's default already omits one, and we
            // want to keep it that way — a shared jar on this client would
            // replay DDG-set cookies (including safe-search preference
            // cookies) from one caller's search onto every other caller's
            // subsequent search, linking unrelated users' activity into what
            // looks like one continuous session.
            .https_only(https_only)
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:128.0) Gecko/20100101 Firefox/128.0",
            )
            .timeout(std::time::Duration::from_secs(10))
            // All search traffic targets a single host
            // (html.duckduckgo.com), so the idle pool only needs to be sized
            // for this process's own concurrency, not per-host fanout. The
            // codebase has no dedicated "max concurrent searches" config
            // Rate limiting (`server.rate_limit_per_minute`) bounds request
            // rate, not simultaneous connections, so the pool is sized for
            // headroom: enough that a burst of concurrent searches all reuse
            // warm connections instead of each paying a full TCP+TLS
            // handshake. Only applies when unproxied — see below.
            .pool_idle_timeout(Duration::from_secs(90));

        // Search egress has no IP-pinning guarantee to undermine (unlike
        // extract's `build_pinned_extract_client`), so a proxy is applied
        // directly with no additional caveats.
        //
        // Pooling is disabled (`pool_max_idle_per_host(0)`) whenever a proxy
        // is configured, trading away warm-connection reuse for
        // proxied traffic specifically: an HTTPS request through a forward
        // proxy is a `CONNECT`-tunneled TCP pipe, so a rotating proxy (one
        // that hands out a fresh exit IP per *request*) can only actually
        // rotate on a new
        // connection — reusing a pooled/keep-alive tunnel pins every request
        // sent over it to whatever exit IP that tunnel's `CONNECT` was
        // originally assigned. Confirmed live: 90s of connection reuse meant
        // 5 sequential searches all rode the same tunnel and hit the same
        // exit IP, and DuckDuckGo's anti-bot challenge tripped after the
        // 2nd. Forcing a fresh connection (and thus a fresh `CONNECT`, and a
        // fresh rotated IP) per request costs one extra TLS handshake per
        // search when proxied — a fine trade for actually getting the
        // rotation the proxy is configured for. Unproxied traffic is
        // unaffected and keeps the pooled behavior.
        builder = builder.pool_max_idle_per_host(if proxy_url.is_some() { 0 } else { 32 });
        if let Some(proxy_url) = proxy_url {
            builder = builder.proxy(reqwest::Proxy::all(proxy_url)?);
        }

        let client = builder.build()?;

        Ok(Self { client, search_url })
    }

    /// Execute a search and return results.
    ///
    /// Returns `Err` for outcomes the caller should treat as a real failure
    /// (blocked, non-2xx, transport error) or as a distinct observability
    /// signal (`ParseEmpty`). See `SearchError` for how callers are expected
    /// to map each variant — in particular, `ParseEmpty` must not become a
    /// client-facing error.
    ///
    /// `language`, if given, must already be a key `ddg_region_for_language`
    /// maps (callers reject anything else before this is ever called — see
    /// `SearchRequest::try_from_params`); an unmapped code here just falls
    /// back to an unconstrained search rather than panicking.
    ///
    /// `safe_search` (`0`/`1`/`2`) is forwarded to DuckDuckGo's own `kp`
    /// parameter (see `ddg_kp_for_safe_search`) so upstream filtering
    /// happens before this ever runs the local secondary-net blocklist in
    /// `src/gateway/mod.rs::apply_safe_search_filter`.
    pub async fn search(
        &self,
        query: &str,
        page: u32,
        language: Option<&str>,
        safe_search: u8,
    ) -> Result<Vec<SearchResult>, SearchError> {
        let offset = (page.saturating_sub(1)) * 20;
        let url = build_search_url(&self.search_url, query, offset, language, safe_search);

        let response = self
            .client
            .get(&url)
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("DNT", "1")
            .header("Upgrade-Insecure-Requests", "1")
            .header("Sec-Fetch-Dest", "document")
            .header("Sec-Fetch-Mode", "navigate")
            .header("Sec-Fetch-Site", "none")
            .header("Sec-Fetch-User", "?1")
            .header("Priority", "u=0, i")
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "DDG request failed");
                SearchError::Transport(e.to_string())
            })?;

        let status = response.status();
        if status == reqwest::StatusCode::FORBIDDEN {
            tracing::error!(
                "DDG returned 403 — likely blocked. Check User-Agent and rate limiting."
            );
            return Err(SearchError::Blocked);
        }
        if !status.is_success() {
            tracing::warn!(status = %status, "DDG returned non-success status");
            return Err(SearchError::Upstream(status));
        }

        let body = response.text().await.map_err(|e| {
            tracing::warn!(error = %e, "Failed to read DDG response body");
            SearchError::Transport(e.to_string())
        })?;

        // Checked before parsing (and matched on body content, not status
        // alone — see `is_challenge_page`): a 202 challenge page has no
        // `div.result__body` markup, so letting it fall through to the
        // parser would just produce the same zero-results outcome as a
        // genuinely empty query, indistinguishable from `ParseEmpty`. That
        // conflation is exactly what made this invisible before — DDG
        // returning its CAPTCHA page logged as "possible selector drift".
        if is_challenge_page(status, &body) {
            tracing::warn!("DDG returned 202 with an anti-bot challenge page instead of results");
            return Err(SearchError::Challenged);
        }

        // Parsing is synchronous, allocation-heavy, and CPU-bound:
        // running it inline on this Tokio worker thread would block it from
        // polling any other future — including unrelated health checks —
        // for the duration of the parse. Move it onto the blocking pool and
        // hand it an owned copy of the body, since `spawn_blocking` requires
        // `'static` ownership of the closure's captures.
        let results = tokio::task::spawn_blocking(move || parse_results(&body))
            .await
            .map_err(|e| {
                tracing::error!(error = %e, "DDG result parser task panicked");
                SearchError::ParsePanicked
            })?;
        if results.is_empty() {
            tracing::warn!(
                status = %status,
                "DDG returned a success status but parsed zero results (genuine empty query, or DDG markup changed)"
            );
            return Err(SearchError::ParseEmpty);
        }

        Ok(results)
    }

    /// Probe the DDG upstream directly. This performs live network I/O, so it
    /// must only ever be called from the fixed-interval background monitor
    /// (`spawn_health_monitor`) — never from a request handler. A
    /// request-triggered probe is what would let an unauthenticated caller
    /// drive unlimited outbound traffic at DuckDuckGo via `/api/v1/health`.
    ///
    /// Reads the body (not just the status) so a 202 challenge page — which
    /// is a 2xx and would otherwise read as "healthy" — is caught the same
    /// way `search()` catches it, rather than `/api/v1/health` reporting
    /// `engine_healthy: true` for a deployment that is fully challenged.
    pub async fn probe_upstream(&self) -> bool {
        match self
            .client
            .get(&self.search_url)
            .header("Accept", "text/html")
            .send()
            .await
        {
            Ok(r) => {
                let status = r.status();
                if !status.is_success() {
                    return false;
                }
                match r.text().await {
                    Ok(body) => !is_challenge_page(status, &body),
                    Err(_) => false,
                }
            }
            Err(_) => false,
        }
    }
}

// DDG result-page selectors. These are compile-time-constant
// strings, so the `Selector::parse` calls are hoisted into `LazyLock`
// statics and paid once per process rather than once per request: parsing
// happens the first time `parse_results` runs and is cached for every call
// after that. `.expect("valid selector")` is safe here because the input is
// a fixed, statically-known-valid string, not attacker- or request-derived
// data — there is no per-call fallibility to handle.
static CONTAINER_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("div.result__body").expect("valid selector"));
static TITLE_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.result__a").expect("valid selector"));
static SNIPPET_SEL: LazyLock<Selector> =
    LazyLock::new(|| Selector::parse("a.result__snippet").expect("valid selector"));

/// Parse the DDG HTML response into SearchResults.
///
/// A free function rather than a method: it's called from inside
/// `tokio::task::spawn_blocking` (since `Html::parse_document` and
/// selector matching are synchronous and CPU-bound), whose closure must own
/// everything it captures. Taking a borrowed `&self` would tie the closure's
/// lifetime to the adapter reference across the blocking-pool handoff, so
/// this takes only the owned `html` string it actually needs.
fn parse_results(html: &str) -> Vec<SearchResult> {
    let document = Html::parse_document(html);

    let mut results = Vec::new();

    for element in document.select(&CONTAINER_SEL) {
        // Extract title + URL from result__a link
        let (url, title) = element
            .select(&TITLE_SEL)
            .next()
            .and_then(|el| {
                let href = el.value().attr("href")?;
                let url = extract_uddg_url(href)?;
                let title = el.text().collect::<Vec<_>>().join(" ").trim().to_string();
                Some((url, title))
            })
            .unwrap_or_default();

        if url.is_empty() || title.is_empty() {
            continue;
        }

        // Extract snippet
        let snippet = element
            .select(&SNIPPET_SEL)
            .next()
            .map(|el| el.text().collect::<Vec<_>>().join(" ").trim().to_string())
            .unwrap_or_default();

        results.push(SearchResult {
            url,
            title,
            snippet,
        });
    }

    results
}

/// Cached liveness/readiness state for the DDG upstream. Populated by a
/// background task on a fixed interval rather than probed synchronously per
/// request: `/api/v1/health` is unauthenticated and deliberately
/// exempt from rate limiting, so a request-driven probe would let
/// any caller turn health polling into unbounded outbound traffic against
/// DuckDuckGo and risk an IP ban that breaks search for every user.
pub struct EngineHealth {
    healthy: AtomicBool,
    last_checked: AtomicU64,
}

impl EngineHealth {
    /// Starts optimistic (`healthy = true`) with `last_checked = 0` so a
    /// freshly started process — including in tests, which never spawn the
    /// background monitor — reports OK immediately instead of "degraded"
    /// for up to a full probe interval before any probe has run.
    pub fn new() -> Self {
        Self {
            healthy: AtomicBool::new(true),
            last_checked: AtomicU64::new(0),
        }
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Unix seconds of the last completed probe, or 0 if none has run yet.
    pub fn last_checked(&self) -> u64 {
        self.last_checked.load(Ordering::Relaxed)
    }

    fn record(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::Relaxed);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        self.last_checked.store(now, Ordering::Relaxed);
    }
}

impl Default for EngineHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn the background task that keeps `EngineHealth` fresh. The interval
/// is fixed at 60s and is never influenced by inbound request volume — that
/// decoupling is deliberate, since tying the probe to request
/// volume would turn `/api/v1/health` into a DDG amplification
/// vector.
///
/// `tokio::time::interval`'s first `tick()` resolves immediately rather than
/// after a full period, so the first probe runs right after this is called
/// instead of leaving the process reporting "unknown" for up to 60s after
/// startup.
pub fn spawn_health_monitor(engine: Arc<DuckDuckGoAdapter>, health: Arc<EngineHealth>) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            let healthy = engine.probe_upstream().await;
            health.record(healthy);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // `with_proxy` must actually apply the proxy setting rather than
    // silently building a bare client — proving that requires a URL
    // `reqwest::Proxy::all` accepts, not that traffic really flows through a
    // proxy (that's an integration concern, not this constructor's).
    #[test]
    fn with_proxy_accepts_valid_proxy_url() {
        assert!(DuckDuckGoAdapter::with_proxy("http://proxy.example.com:8080").is_ok());
    }

    #[test]
    fn with_proxy_rejects_malformed_proxy_url() {
        assert!(DuckDuckGoAdapter::with_proxy("not a url").is_err());
    }

    #[test]
    fn test_extract_uddg_url() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F&rut=abc123";
        let result = extract_uddg_url(href);
        assert_eq!(result, Some("https://rust-lang.org/".to_string()));
    }

    #[test]
    fn test_extract_uddg_url_absolute() {
        let href =
            "https://duckduckgo.com/l/?uddg=https%3A%2F%2Fen.wikipedia.org%2Fwiki%2FRust&rut=xyz";
        let result = extract_uddg_url(href);
        assert_eq!(
            result,
            Some("https://en.wikipedia.org/wiki/Rust".to_string())
        );
    }

    #[test]
    fn test_extract_uddg_url_rejects_javascript_scheme() {
        let href = "//duckduckgo.com/l/?uddg=javascript%3Aalert(1)&rut=abc123";
        let result = extract_uddg_url(href);
        assert_eq!(result, None);
    }

    #[test]
    fn test_extract_uddg_url_rejects_data_scheme() {
        let href =
            "//duckduckgo.com/l/?uddg=data%3Atext%2Fhtml%2C%3Cscript%3Ealert(1)%3C%2Fscript%3E&rut=abc123";
        let result = extract_uddg_url(href);
        assert_eq!(result, None);
    }

    // A supported language must add the matching `kl`
    // region parameter to the constructed DDG URL.
    #[test]
    fn build_search_url_includes_kl_for_supported_language() {
        let url = build_search_url(
            "https://html.duckduckgo.com/html/",
            "rust",
            0,
            Some("de"),
            1,
        );
        assert!(
            url.contains("&kl=de-de"),
            "expected de-de region param, got: {url}"
        );
    }

    #[test]
    fn build_search_url_maps_all_supported_languages() {
        for &lang in SUPPORTED_LANGUAGES {
            let region = ddg_region_for_language(lang).unwrap_or_else(|| {
                panic!("{lang} is listed in SUPPORTED_LANGUAGES but has no kl mapping")
            });
            let url = build_search_url(
                "https://html.duckduckgo.com/html/",
                "rust",
                0,
                Some(lang),
                1,
            );
            assert!(
                url.contains(&format!("&kl={region}")),
                "expected kl={region} for lang={lang}, got: {url}"
            );
        }
    }

    // No `lang` param at all must produce no `kl` param — an
    // unconstrained search, not a search for some default region.
    #[test]
    fn build_search_url_omits_kl_when_no_language_requested() {
        let url = build_search_url("https://html.duckduckgo.com/html/", "rust", 0, None, 1);
        assert!(!url.contains("kl="), "expected no kl param, got: {url}");
    }

    // `SearchRequest::try_from_params` rejects unmapped languages
    // before `search()` is ever called, but `build_search_url` itself must
    // still degrade gracefully (no bogus `kl` value) rather than trust the
    // caller.
    #[test]
    fn build_search_url_omits_kl_for_unmapped_language() {
        let url = build_search_url(
            "https://html.duckduckgo.com/html/",
            "rust",
            0,
            Some("ja"),
            1,
        );
        assert!(!url.contains("kl="), "expected no kl param, got: {url}");
    }

    #[test]
    fn ddg_region_for_language_rejects_unmapped_codes() {
        assert_eq!(ddg_region_for_language("ja"), None);
        assert_eq!(ddg_region_for_language("zz"), None);
    }

    // `safe_search` must be forwarded to DDG's own `kp` parameter so
    // upstream filtering (which the local blocklist only backs up) actually
    // runs. Each of the three levels the rest of the codebase accepts
    // (`SearchRequest::try_from_params`, the MCP tool schema) must map to a
    // distinct, DDG-documented `kp` value.
    #[test]
    fn build_search_url_maps_safe_search_off_to_kp_negative_2() {
        let url = build_search_url("https://html.duckduckgo.com/html/", "rust", 0, None, 0);
        assert!(url.contains("&kp=-2"), "expected kp=-2 for off, got: {url}");
    }

    #[test]
    fn build_search_url_maps_safe_search_moderate_to_kp_negative_1() {
        let url = build_search_url("https://html.duckduckgo.com/html/", "rust", 0, None, 1);
        assert!(
            url.contains("&kp=-1"),
            "expected kp=-1 for moderate, got: {url}"
        );
    }

    #[test]
    fn build_search_url_maps_safe_search_strict_to_kp_1() {
        let url = build_search_url("https://html.duckduckgo.com/html/", "rust", 0, None, 2);
        assert!(
            url.contains("&kp=1"),
            "expected kp=1 for strict, got: {url}"
        );
        // Sanity: must be exactly `kp=1`, not accidentally matching `kp=-1`
        // via a loose substring check.
        assert!(!url.contains("&kp=-1"));
    }

    // `kp` must always be present — the client deliberately carries
    // no cookie jar, so omitting it would leave every request at
    // DDG's own unauthenticated default instead of the caller's requested
    // level.
    #[test]
    fn build_search_url_always_includes_kp() {
        for level in 0..=2u8 {
            let url = build_search_url("https://html.duckduckgo.com/html/", "rust", 0, None, level);
            assert!(
                url.contains("&kp="),
                "expected a kp param for safe_search={level}, got: {url}"
            );
        }
    }

    #[test]
    fn test_parse_results_from_live_html() {
        let html = include_str!("../../tests/fixtures/ddg_rust_search.html");
        let results = parse_results(html);
        assert!(!results.is_empty(), "Should parse at least one result");
        // First result should be rust-lang.org
        let first = &results[0];
        assert!(
            first.url.contains("rust-lang.org"),
            "First result should be rust-lang.org, got: {}",
            first.url
        );
        assert!(!first.title.is_empty());
    }

    // `with_base_url` must actually be used by `search()` — i.e. the
    // adapter must dial the injected base, not the hardcoded
    // `html.duckduckgo.com` `new()` uses — so a `wiremock::MockServer` can
    // stand in for the live internet in tests. Also proves `search()`'s full
    // pipeline (URL construction -> HTTP GET -> spawn_blocking parse) works
    // end-to-end against a realistic fixture, not just `parse_results` in
    // isolation (`test_parse_results_from_live_html` above already covers
    // the parser alone, but never exercises the HTTP call around it).
    #[tokio::test]
    async fn with_base_url_search_hits_injected_server_not_real_ddg() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fixture = include_str!("../../tests/fixtures/ddg_rust_search.html");
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = DuckDuckGoAdapter::with_base_url(format!("{}/html/", server.uri()));
        let results = adapter
            .search("rust", 1, None, 1)
            .await
            .expect("mocked search must succeed");

        assert!(!results.is_empty());
        assert!(results[0].url.contains("rust-lang.org"));

        // `.expect(1)` above is verified when `server` drops, but assert
        // explicitly too so a failure here points straight at "the adapter
        // didn't call the mock" instead of a deferred panic-on-drop.
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    // `is_challenge_page` must key off body content, not status alone —
    // a 202 with no challenge markup should not be misclassified.
    #[test]
    fn is_challenge_page_requires_both_202_and_challenge_markup() {
        let challenge_html = include_str!("../../tests/fixtures/ddg_challenge_202.html");
        assert!(is_challenge_page(
            reqwest::StatusCode::ACCEPTED,
            challenge_html
        ));

        // Same status, unrelated body: must not false-positive on 202 alone.
        assert!(!is_challenge_page(
            reqwest::StatusCode::ACCEPTED,
            "<html>ok</html>"
        ));

        // Challenge markup present, but not actually a 202: status still gates it.
        assert!(!is_challenge_page(reqwest::StatusCode::OK, challenge_html));
    }

    // The core regression this test exists to prevent: DDG's
    // 202 challenge page must surface as `Challenged`, never silently
    // reparsed into `ParseEmpty` ("possible selector drift"), since that
    // conflation makes a fully-blocked deployment
    // indistinguishable from a genuinely empty query. Uses the real capture
    // at `tests/fixtures/ddg_challenge_202.html` (14,189 bytes, from a live
    // block) rather than a synthetic fixture.
    #[tokio::test]
    async fn search_returns_challenged_not_parse_empty_on_202_challenge_page() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fixture = include_str!("../../tests/fixtures/ddg_challenge_202.html");
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(202).set_body_string(fixture))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = DuckDuckGoAdapter::with_base_url(format!("{}/html/", server.uri()));
        let result = adapter.search("rust", 1, None, 1).await;

        assert!(
            matches!(result, Err(SearchError::Challenged)),
            "expected Err(SearchError::Challenged), got: {result:?}"
        );
    }

    // `gateway::http::tests::test_health_handlers_do_not_block_on_network`
    // explicitly avoids the real network path `probe_upstream` uses, so the
    // body-reading branch needs its own direct coverage: a bug there (e.g.
    // treating a read failure or a normal results page as unhealthy) would
    // silently flip `/api/v1/health` to false for every real,
    // non-challenged deployment.
    #[tokio::test]
    async fn probe_upstream_returns_true_on_healthy_response() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fixture = include_str!("../../tests/fixtures/ddg_rust_search.html");
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(fixture))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = DuckDuckGoAdapter::with_base_url(format!("{}/html/", server.uri()));
        assert!(adapter.probe_upstream().await);
    }

    // `probe_upstream` must not report healthy while challenged: reading
    // only `status().is_success()` would leave `/api/v1/health` at `true`
    // during a total upstream block.
    #[tokio::test]
    async fn probe_upstream_returns_false_on_202_challenge_page() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let fixture = include_str!("../../tests/fixtures/ddg_challenge_202.html");
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(202).set_body_string(fixture))
            .expect(1)
            .mount(&server)
            .await;

        let adapter = DuckDuckGoAdapter::with_base_url(format!("{}/html/", server.uri()));
        assert!(!adapter.probe_upstream().await);
    }
}

// Integration tests for the Serica HTTP API
// Exercises the full HTTP → handler → engine flow
//
// This suite runs offline: search-path tests are backed by `wiremock`,
// pointed at by injecting
// `DuckDuckGoAdapter::with_base_url` instead of the real DDG endpoint (see
// `test_app_with_engine` below). The two `/api/v1/extract` tests that still
// touch the network are the exception: the SSRF guard
// (`util::ssrf::validate_extract_target`) requires `https://` and rejects
// any host that resolves to a loopback/private address, which is exactly
// what a local `wiremock::MockServer` is — so they can't be mocked without
// bypassing that very protection. They are marked `#[ignore]`
// with an explanation rather than deleted; run them explicitly (`cargo test
// -- --ignored`) in a scheduled job with real network access, not in normal
// CI. The health tests are fully offline: the health
// handlers only ever read cached `EngineHealth` state and never perform I/O
// (see `test_health_handlers_do_not_block_on_network` in
// `src/gateway/http.rs`), so it was already fully offline before this pass.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serica_search::cache::noop::NoopCache;
use serica_search::cache::CacheLayer;
use serica_search::config::Config;
use serica_search::engines::duckduckgo::{DuckDuckGoAdapter, EngineHealth};
use serica_search::gateway::http::HttpGateway;
use serica_search::models::search::SearchResponse;
use serica_search::util::extract::ExtractLimiter;
use serica_search::util::metrics::MetricsCollector;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tower::ServiceExt;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Real DDG result-page HTML, captured for the `parse_results` fixture test
/// in `src/engines/duckduckgo.rs` and reused here so search tests exercise
/// the real parser against realistic markup instead of a synthetic
/// approximation. Contains 3 results: rust-lang.org, en.wikipedia.org, and
/// doc.rust-lang.org.
const DDG_PAGE_1_HTML: &str = include_str!("fixtures/ddg_rust_search.html");
/// Hand-built fixture standing in for DDG's *second* page of results for the
/// same query — see the file for why its result set is deliberately
/// disjoint from page 1's.
const DDG_PAGE_2_HTML: &str = include_str!("fixtures/ddg_rust_search_page2.html");

fn test_app() -> axum::Router {
    test_app_with_engine(DuckDuckGoAdapter::new())
}

fn test_app_with_engine(engine: DuckDuckGoAdapter) -> axum::Router {
    test_app_with_engine_and_cache(engine, Arc::new(NoopCache))
}

fn test_app_with_engine_and_cache(
    engine: DuckDuckGoAdapter,
    cache: Arc<dyn CacheLayer>,
) -> axum::Router {
    let config = Arc::new(Config::default());
    let engine = Arc::new(engine);
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
    gateway
        .router()
        .expect("test config produces a valid router")
}

/// Starts a `wiremock` server preloaded with the page-1 DDG fixture at
/// `/html/` (the same path segment `DuckDuckGoAdapter::new()`'s real
/// `search_url` ends in) and returns it alongside an adapter pointed at it
/// via `with_base_url`. The mock matches on path only (not query string), so
/// it answers any `q`/`kl`/`kp`/`s=0` combination — good enough for tests
/// that only care that *a* search succeeds, not the exact request shape.
async fn mock_ddg_single_page() -> (MockServer, DuckDuckGoAdapter) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/html/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DDG_PAGE_1_HTML))
        .mount(&server)
        .await;
    let engine = DuckDuckGoAdapter::with_base_url(format!("{}/html/", server.uri()));
    (server, engine)
}

/// Minimal in-memory `CacheLayer` for cache hit/miss tests. `NoopCache`
/// (used by every other test in this file) always misses by design, which
/// which is why this suite needs an explicit cache-hit test.
#[derive(Clone, Default)]
struct FakeCache {
    inner: Arc<Mutex<HashMap<String, SearchResponse>>>,
}

#[async_trait::async_trait]
impl CacheLayer for FakeCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        let response = self.inner.lock().unwrap().get(key).cloned()?;
        serde_json::to_vec(&response).ok()
    }

    async fn set(&self, key: &str, value: &[u8], _ttl: Duration) {
        if let Ok(response) = serde_json::from_slice::<SearchResponse>(value) {
            self.inner.lock().unwrap().insert(key.to_string(), response);
        }
    }
}

// The rate limiter's `PeerIpKeyExtractor` reads `ConnectInfo<SocketAddr>`
// from request extensions, which `into_make_service_with_connect_info` populates
// at serve time (see main.rs) but `Router::oneshot` does not. Without this,
// every request here would fail key extraction and 500 instead of exercising
// the handler under test.
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
async fn health_returns_200() {
    let app = test_app();
    let response = app.oneshot(get("/api/v1/health")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// /live and /ready must work without a running background probe —
// this test's gateway never spawns `spawn_health_monitor`, matching how a
// freshly started process behaves before its first probe tick completes.
#[tokio::test]
async fn health_live_returns_200() {
    let app = test_app();
    let response = app.oneshot(get("/api/v1/health/live")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn health_ready_returns_200() {
    let app = test_app();
    let response = app.oneshot(get("/api/v1/health/ready")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn search_valid_query_returns_200() {
    let (_server, engine) = mock_ddg_single_page().await;
    let app = test_app_with_engine(engine);
    let response = app.oneshot(get("/api/v1/search?q=rust")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn search_empty_query_returns_400() {
    let app = test_app();
    let response = app.oneshot(get("/api/v1/search?q=")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_missing_query_returns_400() {
    let app = test_app();
    let response = app.oneshot(get("/api/v1/search")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_invalid_page_returns_400() {
    let app = test_app();
    let response = app
        .oneshot(get("/api/v1/search?q=rust&page=51"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_invalid_safe_search_returns_400() {
    let app = test_app();
    let response = app
        .oneshot(get("/api/v1/search?q=rust&safe=3"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn stats_returns_200() {
    let app = test_app();
    let response = app.oneshot(get("/api/v1/stats")).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// Genuinely network-dependent: `/api/v1/extract` validates its target through
// `util::ssrf::validate_extract_target`, which requires `https://` and
// resolves + rejects loopback/private addresses before ever connecting — a
// local `wiremock::MockServer` would fail that check by construction, so
// this can't be converted the way the search tests were without bypassing
// the very SSRF guard it exists to exercise. Run explicitly via `cargo test --
// --ignored` (e.g. a scheduled workflow with real network access), not in
// normal offline/sandboxed CI.
#[tokio::test]
#[ignore = "reaches the live internet (example.com); see comment above"]
async fn extract_valid_url_returns_200() {
    let app = test_app();
    let response = app
        .oneshot(get("/api/v1/extract?url=https://example.com"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn extract_empty_url_returns_400() {
    let app = test_app();
    let response = app.oneshot(get("/api/v1/extract?url=")).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// The direct unit tests for the real decision function,
// `util::fetch::is_extractable_content_type`, live in `src/util/fetch.rs`
// (including the exact `image/png` case this test uses). This integration
// test adds the end-to-end check with a real, specific assertion, since
// `httpbin.org/image/png` reliably returns `image/png`.
#[tokio::test]
#[ignore = "reaches the live internet (httpbin.org); see comment above"]
async fn extract_non_html_content_type_returns_400() {
    let app = test_app();
    let response = app
        .oneshot(get("/api/v1/extract?url=https://httpbin.org/image/png"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "INVALID_QUERY");
}

#[tokio::test]
async fn extract_invalid_url_scheme_returns_400() {
    let app = test_app();
    let response = app
        .oneshot(get("/api/v1/extract?url=ftp://example.com"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn health_response_has_required_fields() {
    let app = test_app();
    let response = app.oneshot(get("/api/v1/health")).await.unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["success"], true);
    assert!(json["data"]["status"].is_string());
    assert!(json["data"]["version"].is_string());
    assert!(json["data"]["uptime_seconds"].is_number());
    assert!(json["data"]["engine_healthy"].is_boolean());
}

#[tokio::test]
async fn search_response_has_required_fields() {
    let (_server, engine) = mock_ddg_single_page().await;
    let app = test_app_with_engine(engine);
    let response = app
        .oneshot(get("/api/v1/search?q=rust+programming&page=1"))
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["success"], true);
    let results = json["data"]["results"]
        .as_array()
        .expect("results must be an array");
    // With the DDG fixture mocked, `results` is not
    // vacuously an empty array that a broken/blocked engine would also
    // produce — assert on the actual parsed content (the fixture's known
    // first result) rather than just the field's shape.
    assert!(
        !results.is_empty(),
        "expected the mocked DDG fixture to parse into at least one result"
    );
    assert!(results[0]["url"]
        .as_str()
        .unwrap()
        .contains("rust-lang.org"));
    assert!(!results[0]["title"].as_str().unwrap().is_empty());
    assert_eq!(json["data"]["total_results"], results.len());
    assert_eq!(json["data"]["page"], 1);
    assert!(json["data"]["timing_ms"].is_number());
    assert_eq!(json["data"]["cached"], false);
}

// Proves `lang=en` actually reaches DuckDuckGo as the `kl`
// region parameter (`build_search_url`/`ddg_region_for_language`), not just
// that the endpoint returns 200 for some unrelated reason. The mock only
// matches requests carrying `kl=us-en`; if the language were silently
// dropped anywhere between `try_from_params` and the outbound request, this
// mock would 404 and the search would fail closed instead of quietly
// passing.
#[tokio::test]
async fn search_with_language_filter_returns_200() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/html/"))
        .and(query_param("kl", "us-en"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DDG_PAGE_1_HTML))
        .expect(1)
        .mount(&server)
        .await;
    let engine = DuckDuckGoAdapter::with_base_url(format!("{}/html/", server.uri()));
    let app = test_app_with_engine(engine);
    let response = app
        .oneshot(get("/api/v1/search?q=rust&lang=en"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

// The double-pagination hazard: if the handler re-paginated an already
// page-scoped result set
// (`skip((page-1)*max_per_page)` on top of DDG's own `s=` offset), every
// `page >= 2` would silently return an empty array with `success: true`.
// The unit-level regression test (`finalize_results_page_2_does_not_double_paginate`
// in src/gateway/mod.rs) proves the pagination *math*, but it hands
// `finalize_results` a pre-built "as if DDG already offset this" Vec — it
// never exercises the actual offset construction in
// `DuckDuckGoAdapter::search`/`build_search_url` (the `s=` query param) or
// the HTTP round trip. This test does: two distinct DDG fixtures are mocked
// behind `s=0` (page 1) and `s=20` (page 2), and page 2's response must
// contain the page-2 fixture's results, not page 1's and not an empty set.
#[tokio::test]
async fn search_page_two_returns_results_distinct_from_page_one() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/html/"))
        .and(query_param("s", "0"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DDG_PAGE_1_HTML))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/html/"))
        .and(query_param("s", "20"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DDG_PAGE_2_HTML))
        .mount(&server)
        .await;
    let engine = DuckDuckGoAdapter::with_base_url(format!("{}/html/", server.uri()));
    let app = test_app_with_engine(engine);

    let page1 = app
        .clone()
        .oneshot(get("/api/v1/search?q=rust&page=1"))
        .await
        .unwrap();
    assert_eq!(page1.status(), StatusCode::OK);
    let page1_body = axum::body::to_bytes(page1.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let page1_json: serde_json::Value = serde_json::from_slice(&page1_body).unwrap();
    let page1_urls: Vec<String> = page1_json["data"]["results"]
        .as_array()
        .expect("page 1 results must be an array")
        .iter()
        .map(|r| r["url"].as_str().unwrap().to_string())
        .collect();
    assert!(
        !page1_urls.is_empty(),
        "page 1 must return the mocked fixture's results"
    );

    let page2 = app
        .clone()
        .oneshot(get("/api/v1/search?q=rust&page=2"))
        .await
        .unwrap();
    assert_eq!(page2.status(), StatusCode::OK);
    let page2_body = axum::body::to_bytes(page2.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let page2_json: serde_json::Value = serde_json::from_slice(&page2_body).unwrap();
    let page2_urls: Vec<String> = page2_json["data"]["results"]
        .as_array()
        .expect("page 2 results must be an array")
        .iter()
        .map(|r| r["url"].as_str().unwrap().to_string())
        .collect();

    // Page 2 must NOT be empty.
    assert!(
        !page2_urls.is_empty(),
        "page 2 must return real results, not an empty set (double-pagination would have produced one)"
    );
    assert_eq!(page2_json["data"]["page"], 2);
    // And it must be genuinely distinct content, not page 1 repeated.
    assert!(
        page1_urls.iter().all(|u| !page2_urls.contains(u)),
        "page 2's results must be disjoint from page 1's; page1={page1_urls:?} page2={page2_urls:?}"
    );
    assert!(page2_urls.iter().any(|u| u.contains("crates.io")));
}

// Every other router in this suite uses `NoopCache`, which always misses
// by construction, so this is the suite's only cache-hit coverage. It
// proves the search handler actually reads from the cache on a repeat
// request (not just writes to it): the mock DDG server only ever answers
// once (`.expect(1)`), so if the second identical request fell through to
// the engine again instead of the cache, the mock's expectation would fail
// when `server` drops at the end of the test.
#[tokio::test]
async fn search_second_identical_request_is_served_from_cache() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/html/"))
        .respond_with(ResponseTemplate::new(200).set_body_string(DDG_PAGE_1_HTML))
        .expect(1)
        .mount(&server)
        .await;
    let engine = DuckDuckGoAdapter::with_base_url(format!("{}/html/", server.uri()));
    let cache: Arc<dyn CacheLayer> = Arc::new(FakeCache::default());
    let app = test_app_with_engine_and_cache(engine, cache);

    let first = app
        .clone()
        .oneshot(get("/api/v1/search?q=rust"))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let first_body = axum::body::to_bytes(first.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let first_json: serde_json::Value = serde_json::from_slice(&first_body).unwrap();
    assert_eq!(
        first_json["data"]["cached"], false,
        "first request is a cache miss"
    );

    let second = app
        .clone()
        .oneshot(get("/api/v1/search?q=rust"))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::OK);
    let second_body = axum::body::to_bytes(second.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let second_json: serde_json::Value = serde_json::from_slice(&second_body).unwrap();
    assert_eq!(
        second_json["data"]["cached"], true,
        "second identical request must be served from cache"
    );
    // Results must be identical content, just relabeled as cached.
    assert_eq!(
        first_json["data"]["results"], second_json["data"]["results"],
        "cached response must return the same results as the original"
    );

    assert_eq!(
        server.received_requests().await.unwrap().len(),
        1,
        "cache hit must not re-query the upstream engine"
    );
}

#[tokio::test]
async fn search_invalid_language_returns_400() {
    let app = test_app();
    let response = app
        .oneshot(get("/api/v1/search?q=rust&lang=english"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// "ja" is a well-formed 2-letter ISO 639-1 code but this
// service has no DuckDuckGo `kl` region mapping for it (only en/de/fr/es
// do). It must be rejected rather than silently searching unconstrained.
#[tokio::test]
async fn search_unmapped_language_returns_400() {
    let app = test_app();
    let response = app
        .oneshot(get("/api/v1/search?q=rust&lang=ja"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn search_long_query_returns_400() {
    let app = test_app();
    let long_query = "a".repeat(501);
    let response = app
        .oneshot(get(&format!("/api/v1/search?q={}", long_query)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

// Configuration system: layered loading from defaults → TOML file → environment variables
use figment::providers::Format;
use serde::{Deserialize, Serialize};
use url::Url;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub search: SearchConfig,
    pub cache: CacheConfig,
    pub mcp: McpConfig,
    pub cors: CorsConfig,
    pub extract: ExtractConfig,
    pub proxy: ProxyConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub log_level: String,
    pub log_format: String,
    pub shutdown_drain_timeout_seconds: u64,
    pub rate_limit_per_minute: u32,
    /// Optional bearer/`X-API-Key` gate for `/api/v1/stats`
    /// (`gateway::http::check_api_key`). `None`/empty is the default and
    /// means "disabled" — the endpoint stays exactly as open as it is
    /// today — matching the `cache.redis_url`/`cors.origin` convention
    /// where an absent value means the feature is off rather than falling
    /// back to some other default-deny behavior that would break existing
    /// deployments on upgrade.
    pub api_key: Option<String>,
    /// Extract pipeline plan, Change 6: bounds how many extract requests
    /// (`/api/v1/extract` + `serica_extract`) may run their fetch+parse
    /// span concurrently. Read here, turned into the
    /// `util::extract::ExtractLimiter` semaphore constructed once in
    /// `main.rs`, and acquired by `run_extract` on both gateways. Default 2,
    /// validated `1..=16` in `Config::validate()`: 0 would mean nothing
    /// could ever acquire a permit, and above 16 is unreasonable against
    /// the per-permit RAM budget (`max_raw_bytes` buffered per concurrent
    /// extract).
    pub max_concurrent_extracts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    pub default_timeout_ms: u64,
    pub max_results_per_page: usize,
    pub safe_search_default: u8,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub ttl_seconds: u64,
    /// TTL for cached `/api/v1/extract`/`serica_extract` responses —
    /// intentionally separate from `ttl_seconds` (search's TTL), not
    /// inherited from it: docs/article pages a URL points at change far
    /// less often than search rankings do, so extract can afford (and
    /// benefits from) a much longer cache lifetime.
    pub extract_ttl_seconds: u64,
    pub redis_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpConfig {
    pub enabled: bool,
}

// `derive(Default)` gives `origin: None` — a deny-by-default posture:
// no CORS layer is attached unless an origin is explicitly configured —
// same-origin/server-to-server/MCP callers never need one), matching the
// `cache.redis_url` convention where an absent value means "disabled"
// rather than falling back to a wildcard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CorsConfig {
    pub origin: Option<String>,
}

// `max_content_chars`/`output_format` cap the size and shape of
// `rs_trafilatura`'s *output*, not the input-side fetch — that's
// `max_raw_bytes` below, which bounds the raw HTML byte count before
// extraction even runs. `MAX_EXTRACT_BYTES` in `util/fetch.rs` is this
// setting's default, kept as the single source of truth for that value.
// `max_content_chars` becomes `Options::max_extracted_len` and
// `output_format` decides whether `Options::output_markdown` is set, so
// both extract handlers stop calling the zero-config `rs_trafilatura::extract`
// entry point (which silently runs with the library's own 1,000,000-char
// default) and instead extract with a tuned, configurable `Options`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractConfig {
    pub max_content_chars: usize,
    /// `"text"` or `"markdown"` — validated in `Config::validate()`.
    pub output_format: String,
    /// Extract pipeline plan, Change 1: caps the raw HTML byte count
    /// `read_capped_body` will buffer before extraction runs (guards
    /// memory DoS / slow-drip responses, not the parse itself — that's a
    /// separate concern). Default 4MB, validated `64KB..=16MB` in
    /// `Config::validate()`: the floor keeps a too-small cap from
    /// rejecting essentially every real page, the ceiling keeps an
    /// operator from configuring an effectively-unbounded buffer (a
    /// self-DoS footgun).
    pub max_raw_bytes: usize,
    /// Extract pipeline plan, Change 1: wall-clock budget (milliseconds)
    /// for the full extract span — cache lookup, limiter wait, fetch, and
    /// parse — applied via `tokio::time::timeout` in `run_extract` and
    /// surfaced as `504 EXTRACT_TIMEOUT`. Default 30_000ms, validated
    /// `> 0` in `Config::validate()`: 0 would make every extract time out
    /// instantly.
    pub timeout_ms: u64,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        Self {
            max_content_chars: 50_000,
            output_format: "markdown".into(),
            // Single source of truth: this was a duplicated literal
            // `4 * 1024 * 1024` here, independent of
            // `util::fetch::DEFAULT_MAX_EXTRACT_BYTES` — exactly the kind of
            // drift-prone duplication `Config::validate()`'s own doc
            // comments elsewhere in this file call out avoiding.
            max_raw_bytes: crate::util::fetch::DEFAULT_MAX_EXTRACT_BYTES,
            timeout_ms: 30_000,
        }
    }
}

// Generic, vendor-neutral outbound proxy support for self-hosters behind
// corporate egress proxies. Deliberately two independent keys rather than one
// shared "proxy_url" — search and extract have very different trust models
// (see `extract_url`'s doc comment), and a self-hoster who only wants to
// proxy search traffic must not have to reason about extract's SSRF
// guarantees to do it. Both default to `None`/unproxied, matching the
// `cache.redis_url`/`cors.origin` convention where absence means "disabled"
// rather than falling back to some other implicit behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// Outbound proxy for DuckDuckGo search traffic
    /// (`DuckDuckGoAdapter::build`). Safe to enable freely — the search
    /// client has no IP-pinning guarantee for a proxy to undermine.
    pub search_url: Option<String>,
    /// Outbound proxy for the `/api/v1/extract`/`serica_extract` fetch path.
    /// ⚠️ Setting this **disables the DNS-rebinding protection**
    /// `util::ssrf::validate_extract_target`/`build_pinned_extract_client`
    /// rely on: `.resolve(host, addr)` pins a *direct* connection to the
    /// validated address, but a proxied request instead hands the hostname
    /// to the proxy in the CONNECT, which re-resolves it independently — so
    /// validation happens against one address and connection against
    /// another. Left unset (the default), extract stays unproxied
    /// regardless of `search_url`. See `docs/PROXY.md`.
    pub extract_url: Option<String>,
}

// `figment::Error` (a third-party type we don't control) is ~208
// bytes on its own, independent of `AppError` — `load`/`validate` return it
// directly rather than `AppError`, so boxing `AppError::Config`'s field
// doesn't help here. Reshaping
// these two signatures to return a boxed/wrapped error would ripple into
// every caller (`main.rs`, and the dozen or so `Config::load(...).expect(...)`
// call sites in this file's own tests) for a LOW-severity lint, so this is a
// deliberate, scoped `#[allow]` rather than a fix left half-done.
#[allow(clippy::result_large_err)]
impl Config {
    /// `config_path` is `None` for the default dev workflow (missing
    /// `serica.toml` is a no-op, defaults apply) but an explicit path from
    /// `--config`/`SERICA_CONFIG_FILE` that doesn't exist must fail fast —
    /// silently falling back to defaults there would mask a typo'd mount
    /// path in exactly the way this config layer is meant to prevent.
    ///
    /// An empty string is treated the same as `None`, matching the
    /// `cache.redis_url`/`cors.origin`/`server.api_key` convention where an
    /// unset-but-present env var (e.g. `SERICA_CONFIG_FILE=` in a `.env`
    /// template) means "disabled," not "use this literal path" — otherwise
    /// every `.env` shipped with a blank `SERICA_CONFIG_FILE=` placeholder
    /// line fails fast on startup instead of falling back to defaults.
    pub fn load(config_path: Option<&str>) -> Result<Self, figment::Error> {
        let config_path = config_path.filter(|p| !p.is_empty());
        if let Some(path) = config_path {
            if !std::path::Path::new(path).exists() {
                return Err(figment::Error::from(format!(
                    "config file '{path}' not found (from --config / SERICA_CONFIG_FILE)"
                )));
            }
        }
        let path = config_path.unwrap_or("serica.toml");
        figment::Figment::new()
            .merge(figment::providers::Serialized::defaults(Self::default()))
            .merge(figment::providers::Toml::file(path))
            .merge(figment::providers::Env::prefixed("SERICA_").split("__"))
            .extract()
    }

    /// Range/shape-check values that deserialize successfully but would
    /// otherwise silently misbehave or brick the service at runtime.
    /// Intended to be called once in `main`, immediately after `load()` and
    /// before any other startup work, so a bad `serica.toml` or env override
    /// fails fast with an actionable message instead of misbehaving deep
    /// inside a request handler or a background task.
    ///
    /// Two settings are deliberately *not* re-checked here, because they
    /// already fail safe elsewhere and
    /// duplicating the check would just create two sources of truth that
    /// could drift apart:
    /// - `cors.origin`: `build_cors_layer` (gateway/http.rs) already
    ///   rejects an unparseable origin at router-build time instead of
    ///   silently widening to `"*"`.
    /// - `server.rate_limit_per_minute == 0` is *still* checked below, even
    ///   though `build_governor_config` (gateway/http.rs) already
    ///   clamps it to a floor of 1/minute instead of bricking the service —
    ///   that clamp happens silently, deep inside router construction, so
    ///   validating here turns a silent behavior change into an explicit,
    ///   earlier, and more actionable startup error.
    pub fn validate(&self) -> Result<(), figment::Error> {
        if self.server.rate_limit_per_minute == 0 {
            return Err(figment::Error::from(
                "server.rate_limit_per_minute must be >= 1 (0 is silently clamped to a floor of \
                 1/minute by the rate limiter, which is almost certainly not what was intended); \
                 set SERICA_SERVER__RATE_LIMIT_PER_MINUTE or [server].rate_limit_per_minute"
                    .to_string(),
            ));
        }
        if self.server.port == 0 {
            return Err(figment::Error::from(
                "server.port must be 1..=65535 (0 asks the OS for an ephemeral port, but the \
                 container HEALTHCHECK and the startup log both target the *configured* port, \
                 so they'd end up targeting the wrong one); set SERICA_SERVER__PORT or \
                 [server].port"
                    .to_string(),
            ));
        }
        if !(1..=100).contains(&self.search.max_results_per_page) {
            return Err(figment::Error::from(format!(
                "search.max_results_per_page must be 1..=100, got {} (0 makes every search \
                 return zero results via `take(0)`); set SERICA_SEARCH__MAX_RESULTS_PER_PAGE or \
                 [search].max_results_per_page",
                self.search.max_results_per_page
            )));
        }
        if self.cache.ttl_seconds == 0 {
            return Err(figment::Error::from(
                "cache.ttl_seconds must be >= 1 (0 makes Redis reject every SETEX, so every \
                 cache write silently fails); set SERICA_CACHE__TTL_SECONDS or \
                 [cache].ttl_seconds"
                    .to_string(),
            ));
        }
        if self.cache.extract_ttl_seconds == 0 {
            return Err(figment::Error::from(
                "cache.extract_ttl_seconds must be >= 1 (0 makes Redis reject every SETEX, so \
                 every cache write silently fails); set SERICA_CACHE__EXTRACT_TTL_SECONDS or \
                 [cache].extract_ttl_seconds"
                    .to_string(),
            ));
        }
        if self.search.safe_search_default > 2 {
            return Err(figment::Error::from(format!(
                "search.safe_search_default must be 0, 1, or 2, got {}; set \
                 SERICA_SEARCH__SAFE_SEARCH_DEFAULT or [search].safe_search_default",
                self.search.safe_search_default
            )));
        }
        // Reuse the exact parse the real subscriber init performs
        // (util/tracing.rs's `init_logging`) so this check and the real
        // init can never disagree about what counts as valid — and so a
        // typo here becomes a clean validation error instead of a panic
        // inside `init_logging`, before any subscriber exists to record it.
        self.server
            .log_level
            .parse::<tracing_subscriber::filter::Directive>()
            .map_err(|e| {
                figment::Error::from(format!(
                    "invalid server.log_level '{}': {e} (set SERICA_SERVER__LOG_LEVEL or \
                     [server].log_level to a valid tracing directive, e.g. \"info\" or \"debug\")",
                    self.server.log_level
                ))
            })?;
        // `init_logging` only special-cases the literal "pretty" and falls
        // through to JSON for anything else, so a typo like "Pretty" or
        // "yaml" would silently change the output format rather than error.
        if !matches!(self.server.log_format.as_str(), "json" | "pretty") {
            return Err(figment::Error::from(format!(
                "server.log_format must be \"json\" or \"pretty\", got \"{}\" (any other value \
                 currently falls through to JSON silently, which is exactly the surprise this \
                 check exists to prevent); set SERICA_SERVER__LOG_FORMAT or [server].log_format",
                self.server.log_format
            )));
        }
        if self.extract.max_content_chars == 0 {
            return Err(figment::Error::from(
                "extract.max_content_chars must be >= 1 (0 would silently produce empty \
                 extractions for every request); set SERICA_EXTRACT__MAX_CONTENT_CHARS or \
                 [extract].max_content_chars"
                    .to_string(),
            ));
        }
        if !matches!(self.extract.output_format.as_str(), "text" | "markdown") {
            return Err(figment::Error::from(format!(
                "extract.output_format must be \"text\" or \"markdown\", got \"{}\"; set \
                 SERICA_EXTRACT__OUTPUT_FORMAT or [extract].output_format",
                self.extract.output_format
            )));
        }
        if !(64 * 1024..=16 * 1024 * 1024).contains(&self.extract.max_raw_bytes) {
            return Err(figment::Error::from(format!(
                "extract.max_raw_bytes must be 64KB..=16MB, got {} bytes (below the floor, \
                 accidentally rejects nearly every real page as too small to bother extracting; \
                 above the ceiling, lets an operator configure an effectively-unbounded raw-HTML \
                 buffer, a self-DoS footgun); set SERICA_EXTRACT__MAX_RAW_BYTES or \
                 [extract].max_raw_bytes",
                self.extract.max_raw_bytes
            )));
        }
        if self.extract.timeout_ms == 0 {
            return Err(figment::Error::from(
                "extract.timeout_ms must be >= 1 (0 would make every extract time out \
                 instantly); set SERICA_EXTRACT__TIMEOUT_MS or [extract].timeout_ms"
                    .to_string(),
            ));
        }
        if !(1..=16).contains(&self.server.max_concurrent_extracts) {
            return Err(figment::Error::from(format!(
                "server.max_concurrent_extracts must be 1..=16, got {} (0 would mean no extract \
                 request could ever acquire a permit and the service would hang forever; above \
                 16 is unreasonable for the concurrency's RAM budget); set \
                 SERICA_SERVER__MAX_CONCURRENT_EXTRACTS or \
                 [server].max_concurrent_extracts",
                self.server.max_concurrent_extracts
            )));
        }
        // Error messages deliberately omit the URL itself (unlike the checks
        // above): proxy URLs carry credentials
        // (`http://user:pass@host:port`), and echoing an invalid one back in
        // a config-validation error would put it in process output/logs.
        if let Some(url) = &self.proxy.search_url {
            Url::parse(url).map_err(|e| {
                figment::Error::from(format!(
                    "proxy.search_url is not a valid URL ({e}); set SERICA_PROXY__SEARCH_URL or \
                     [proxy].search_url"
                ))
            })?;
        }
        if let Some(url) = &self.proxy.extract_url {
            Url::parse(url).map_err(|e| {
                figment::Error::from(format!(
                    "proxy.extract_url is not a valid URL ({e}); set SERICA_PROXY__EXTRACT_URL \
                     or [proxy].extract_url"
                ))
            })?;
        }
        Ok(())
    }

    /// `serica mcp` must not silently start when the operator has
    /// disabled MCP via config — pulled out as its own method (rather than
    /// an inline check in `main`) so the rejection message is unit-testable
    /// without spinning up the cache/engine startup path that precedes the
    /// `Command::Mcp` dispatch arm.
    pub fn check_mcp_enabled(&self) -> Result<(), String> {
        if !self.mcp.enabled {
            return Err(
                "MCP server is disabled via config; set SERICA_MCP__ENABLED=true to enable it"
                    .to_string(),
            );
        }
        Ok(())
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 3000,
            log_level: "info".into(),
            log_format: "json".into(),
            shutdown_drain_timeout_seconds: 10,
            rate_limit_per_minute: 30,
            api_key: None,
            max_concurrent_extracts: 2,
        }
    }
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            default_timeout_ms: 4000,
            max_results_per_page: 20,
            safe_search_default: 1,
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            ttl_seconds: 300,
            // 1 hour: docs/article pages a URL points at are far more
            // stable than search result freshness (that's search's
            // 300s ttl_seconds above), so a longer default is a safe,
            // general-purpose choice for a self-hosted deployment —
            // long enough to meaningfully cut repeat-fetch cost for a
            // popular URL, short enough that a since-updated page
            // doesn't stay stale for the whole day.
            extract_ttl_seconds: 3600,
            redis_url: None,
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.server.port, 3000);
        assert_eq!(config.search.default_timeout_ms, 4000);
        assert_eq!(config.cache.ttl_seconds, 300);
        assert_eq!(config.cache.extract_ttl_seconds, 3600);
        assert_eq!(config.cache.redis_url, None);
        assert!(config.mcp.enabled);
        assert_eq!(config.cors.origin, None);
        assert_eq!(config.extract.max_content_chars, 50_000);
        assert_eq!(config.extract.output_format, "markdown");
        assert_eq!(config.extract.max_raw_bytes, 4 * 1024 * 1024);
        assert_eq!(config.extract.timeout_ms, 30_000);
        assert_eq!(config.server.max_concurrent_extracts, 2);
        assert_eq!(config.server.api_key, None);
        assert_eq!(config.proxy.search_url, None);
        assert_eq!(config.proxy.extract_url, None);
    }

    // `server.api_key` follows the same env-override path as every
    // other setting; unset must remain the default (checked above) so
    // `/api/v1/stats` stays open on upgrade for anyone not opting in.
    #[test]
    fn test_server_api_key_env_override() {
        temp_env::with_var("SERICA_SERVER__API_KEY", Some("s3cr3t"), || {
            let config = Config::load(None).expect("Failed to load config");
            assert_eq!(config.server.api_key.as_deref(), Some("s3cr3t"));
        });
    }

    #[test]
    fn test_cors_origin_env_override() {
        temp_env::with_var("SERICA_CORS__ORIGIN", Some("https://example.com"), || {
            let config = Config::load(None).expect("Failed to load config");
            assert_eq!(config.cors.origin.as_deref(), Some("https://example.com"));
        });
    }

    #[test]
    fn test_env_override() {
        temp_env::with_var("SERICA_SERVER__PORT", Some("9090"), || {
            let config = Config::load(None).expect("Failed to load config");
            assert_eq!(config.server.port, 9090);
        });
    }

    // These four tests call `Config::load`, which reads process-wide env
    // vars via figment — but assert against defaults/file values with no
    // `SERICA_*` override of their own. Without holding `temp_env`'s
    // internal lock, they can run concurrently with a test like
    // `test_env_override` that has `SERICA_SERVER__PORT=9090` live on
    // another thread, reading the leaked value instead of the expected one.
    // `with_var_unset` on a key nothing else touches acquires that same
    // lock without mutating any real var, serializing against every other
    // `temp_env`-based test here.
    #[test]
    fn test_load_none_missing_default_file_falls_back_to_defaults() {
        temp_env::with_var_unset("__SERICA_TEST_CONFIG_LOCK__", || {
            // No serica.toml is expected to exist in the test working
            // directory; absence of an explicit path must never be an error.
            let config = Config::load(None).expect("default load must not error");
            assert_eq!(config.server.port, 3000);
        });
    }

    #[test]
    fn test_load_explicit_missing_path_errors() {
        temp_env::with_var_unset("__SERICA_TEST_CONFIG_LOCK__", || {
            let result = Config::load(Some("/nonexistent/definitely-not-there/serica.toml"));
            assert!(result.is_err(), "explicit missing config path must error");
        });
    }

    #[test]
    fn test_load_empty_path_falls_back_to_defaults() {
        temp_env::with_var_unset("__SERICA_TEST_CONFIG_LOCK__", || {
            // A `.env` template shipping `SERICA_CONFIG_FILE=` (blank) must
            // be treated like the var is unset, not like an explicit path
            // that fails to exist — regression test for the empty-string
            // footgun.
            let config = Config::load(Some("")).expect("empty config path must not error");
            assert_eq!(config.server.port, 3000);
        });
    }

    #[test]
    fn test_load_explicit_path_applies_values() {
        temp_env::with_var_unset("__SERICA_TEST_CONFIG_LOCK__", || {
            let dir = std::env::temp_dir().join(format!(
                "serica-config-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).expect("create temp dir");
            let path = dir.join("custom.toml");
            std::fs::write(&path, "[server]\nport = 4242\n").expect("write temp config");

            let config = Config::load(Some(path.to_str().unwrap())).expect("load must succeed");
            assert_eq!(config.server.port, 4242);

            let _ = std::fs::remove_dir_all(&dir);
        });
    }

    // `validate()` must accept the untouched default config...
    #[test]
    fn test_validate_accepts_default_config() {
        assert!(Config::default().validate().is_ok());
    }

    // ...and reject each bad value the audit called out as brick-prone,
    // with a clear per-field error.
    #[test]
    fn test_validate_rejects_zero_rate_limit_per_minute() {
        let mut config = Config::default();
        config.server.rate_limit_per_minute = 0;
        let err = config
            .validate()
            .expect_err("0 rate limit must be rejected");
        assert!(err.to_string().contains("rate_limit_per_minute"));
    }

    #[test]
    fn test_validate_rejects_zero_port() {
        let mut config = Config::default();
        config.server.port = 0;
        let err = config.validate().expect_err("port 0 must be rejected");
        assert!(err.to_string().contains("server.port"));
    }

    #[test]
    fn test_validate_rejects_zero_max_results_per_page() {
        let mut config = Config::default();
        config.search.max_results_per_page = 0;
        let err = config
            .validate()
            .expect_err("0 max_results_per_page must be rejected");
        assert!(err.to_string().contains("max_results_per_page"));
    }

    #[test]
    fn test_validate_rejects_max_results_per_page_over_100() {
        let mut config = Config::default();
        config.search.max_results_per_page = 101;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_rejects_zero_ttl_seconds() {
        let mut config = Config::default();
        config.cache.ttl_seconds = 0;
        let err = config
            .validate()
            .expect_err("0 ttl_seconds must be rejected");
        assert!(err.to_string().contains("ttl_seconds"));
    }

    #[test]
    fn test_validate_rejects_zero_extract_ttl_seconds() {
        let mut config = Config::default();
        config.cache.extract_ttl_seconds = 0;
        let err = config
            .validate()
            .expect_err("0 extract_ttl_seconds must be rejected");
        assert!(err.to_string().contains("extract_ttl_seconds"));
    }

    #[test]
    fn test_validate_rejects_out_of_range_safe_search_default() {
        let mut config = Config::default();
        config.search.safe_search_default = 3;
        let err = config
            .validate()
            .expect_err("safe_search_default of 3 must be rejected");
        assert!(err.to_string().contains("safe_search_default"));
    }

    // `tracing_subscriber::filter::Directive`'s grammar is lenient: a bare
    // word like "not-a-real-level" parses successfully as a *module-path*
    // filter (not a level), so it is not a case that panics in
    // `init_logging` and correctly is not rejected here either — flagging
    // it would make `validate()` disagree with the real parser it's meant
    // to mirror. Malformed directive syntax (e.g. more than one `=`) is
    // the case that fails to parse and panics via `.expect(...)` today.
    #[test]
    fn test_validate_rejects_unparseable_log_level() {
        let mut config = Config::default();
        config.server.log_level = "a=b=c".into();
        let err = config
            .validate()
            .expect_err("malformed log_level directive must be rejected");
        assert!(err.to_string().contains("log_level"));
    }

    #[test]
    fn test_validate_accepts_log_level_that_is_a_bare_module_path() {
        // Not a typo-catching guarantee: `EnvFilter` treats a bare
        // identifier as a valid module-path directive, so this must parse
        // exactly as `init_logging`'s real call does.
        let mut config = Config::default();
        config.server.log_level = "not-a-real-level".into();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_rejects_unrecognized_log_format() {
        let mut config = Config::default();
        config.server.log_format = "yaml".into();
        let err = config
            .validate()
            .expect_err("unrecognized log_format must be rejected");
        assert!(err.to_string().contains("log_format"));
    }

    #[test]
    fn test_validate_accepts_pretty_log_format() {
        let mut config = Config::default();
        config.server.log_format = "pretty".into();
        assert!(config.validate().is_ok());
    }

    // `extract.max_content_chars == 0` would silently produce empty
    // extractions for every request; `extract.output_format` must be one of
    // the two values the handlers actually branch on.
    #[test]
    fn test_validate_rejects_zero_max_content_chars() {
        let mut config = Config::default();
        config.extract.max_content_chars = 0;
        let err = config
            .validate()
            .expect_err("0 max_content_chars must be rejected");
        assert!(err.to_string().contains("max_content_chars"));
    }

    #[test]
    fn test_validate_rejects_unrecognized_output_format() {
        let mut config = Config::default();
        config.extract.output_format = "html".into();
        let err = config
            .validate()
            .expect_err("unrecognized output_format must be rejected");
        assert!(err.to_string().contains("output_format"));
    }

    #[test]
    fn test_validate_accepts_text_output_format() {
        let mut config = Config::default();
        config.extract.output_format = "text".into();
        assert!(config.validate().is_ok());
    }

    // Extract pipeline plan, Change 1: `max_raw_bytes` floor/ceiling guard
    // against accidentally rejecting real pages (too low) or an
    // effectively-unbounded buffer (too high).
    #[test]
    fn test_validate_rejects_max_raw_bytes_below_floor() {
        let mut config = Config::default();
        config.extract.max_raw_bytes = 64 * 1024 - 1;
        let err = config
            .validate()
            .expect_err("max_raw_bytes below 64KB must be rejected");
        assert!(err.to_string().contains("max_raw_bytes"));
    }

    #[test]
    fn test_validate_rejects_max_raw_bytes_above_ceiling() {
        let mut config = Config::default();
        config.extract.max_raw_bytes = 16 * 1024 * 1024 + 1;
        let err = config
            .validate()
            .expect_err("max_raw_bytes above 16MB must be rejected");
        assert!(err.to_string().contains("max_raw_bytes"));
    }

    // Extract pipeline plan, Change 1: `timeout_ms == 0` would make every
    // extract time out instantly.
    #[test]
    fn test_validate_rejects_zero_timeout_ms() {
        let mut config = Config::default();
        config.extract.timeout_ms = 0;
        let err = config
            .validate()
            .expect_err("0 timeout_ms must be rejected");
        assert!(err.to_string().contains("timeout_ms"));
    }

    // `max_concurrent_extracts` must stay in 1..=16 — 0 would mean no
    // extract could ever acquire a permit, and above 16 exceeds a sane
    // RAM budget for the concurrency.
    #[test]
    fn test_validate_rejects_zero_max_concurrent_extracts() {
        let mut config = Config::default();
        config.server.max_concurrent_extracts = 0;
        let err = config
            .validate()
            .expect_err("0 max_concurrent_extracts must be rejected");
        assert!(err.to_string().contains("max_concurrent_extracts"));
    }

    #[test]
    fn test_validate_rejects_max_concurrent_extracts_over_16() {
        let mut config = Config::default();
        config.server.max_concurrent_extracts = 17;
        let err = config
            .validate()
            .expect_err("max_concurrent_extracts over 16 must be rejected");
        assert!(err.to_string().contains("max_concurrent_extracts"));
    }

    // Proxy URLs must be parseable, and — because they're not re-checked
    // anywhere downstream before being handed to `reqwest::Proxy::all` —
    // catching a typo here is the only place it happens before a confusing
    // runtime client-build failure.
    #[test]
    fn test_validate_rejects_unparseable_search_proxy_url() {
        let mut config = Config::default();
        config.proxy.search_url = Some("not a url".into());
        let err = config
            .validate()
            .expect_err("unparseable proxy.search_url must be rejected");
        assert!(err.to_string().contains("proxy.search_url"));
    }

    #[test]
    fn test_validate_rejects_unparseable_extract_proxy_url() {
        let mut config = Config::default();
        config.proxy.extract_url = Some("not a url".into());
        let err = config
            .validate()
            .expect_err("unparseable proxy.extract_url must be rejected");
        assert!(err.to_string().contains("proxy.extract_url"));
    }

    // The validation error must never echo the URL back — it may carry
    // `user:pass@` credentials, and a validation error is exactly the kind
    // of string that ends up in process output or a log aggregator.
    #[test]
    fn test_validate_error_does_not_echo_proxy_credentials() {
        let mut config = Config::default();
        config.proxy.search_url = Some("not a url but s3cr3t-t0ken shaped".into());
        let err = config.validate().expect_err("must be rejected");
        assert!(!err.to_string().contains("s3cr3t-t0ken"));
    }

    #[test]
    fn test_validate_accepts_valid_proxy_urls() {
        let mut config = Config::default();
        config.proxy.search_url = Some("http://proxy.example.com:8080".into());
        config.proxy.extract_url = Some("http://user:pass@proxy.example.com:8080".into());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_proxy_env_override() {
        temp_env::with_vars(
            vec![
                (
                    "SERICA_PROXY__SEARCH_URL",
                    Some("http://search-proxy.example.com:8080"),
                ),
                (
                    "SERICA_PROXY__EXTRACT_URL",
                    Some("http://extract-proxy.example.com:8080"),
                ),
            ],
            || {
                let config = Config::load(None).expect("Failed to load config");
                assert_eq!(
                    config.proxy.search_url.as_deref(),
                    Some("http://search-proxy.example.com:8080")
                );
                assert_eq!(
                    config.proxy.extract_url.as_deref(),
                    Some("http://extract-proxy.example.com:8080")
                );
            },
        );
    }

    // `serica mcp` must not silently start when `mcp.enabled` is
    // false — `check_mcp_enabled` is what `main` calls before dispatching
    // to the MCP subcommand.
    #[test]
    fn test_check_mcp_enabled_accepts_default_config() {
        assert!(Config::default().check_mcp_enabled().is_ok());
    }

    #[test]
    fn test_check_mcp_enabled_rejects_when_disabled() {
        let mut config = Config::default();
        config.mcp.enabled = false;
        let err = config
            .check_mcp_enabled()
            .expect_err("mcp.enabled = false must be rejected");
        assert!(err.contains("SERICA_MCP__ENABLED"));
    }
}

// Shared SSRF guard for the extract feature (src/gateway/http.rs and
// src/gateway/mcp.rs). Both gateways fetch a caller-supplied URL server-side;
// this module is the single place that decides whether the resolved
// destination is allowed, so the two gateways cannot drift out of sync.
//
// It also owns `normalize_extract_url`: not an SSRF check itself, but
// the same "single place the two gateways call into for the extract
// pre-fetch path" framing applies, and it must run before
// `validate_extract_target` so validation, the pinned client, and the fetch
// all agree on the same (normalized) URL.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use url::Url;

use crate::error::AppError;

const EXTRACT_USER_AGENT: &str = "Mozilla/5.0 (compatible; Serica/0.2)";

/// `pub(crate)` (rather than private) so `gateway::http`'s outer
/// `TimeoutLayer` can derive the `/api/v1/extract` route's timeout
/// budget from this single source of truth instead of a second hardcoded
/// duration that could drift out of sync with it.
pub(crate) const EXTRACT_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum number of redirect hops `fetch_extract_target` will follow
/// before failing closed. Hardcoded, like `MAX_EXTRACT_BYTES`/
/// `EXTRACT_TIMEOUT` above, rather than an operator-tunable config knob: an
/// unbounded (or merely very deep) redirect chain is a security/resource
/// bound, not a feature operators should be able to loosen.
pub(crate) const MAX_REDIRECT_HOPS: u8 = 5;

/// True if `ip` must never be dialled by the extract endpoint: loopback,
/// RFC1918 private space, link-local (including the 169.254.169.254 cloud
/// metadata address), broadcast, documentation, unspecified, and CGNAT
/// (100.64.0.0/10) for IPv4; loopback, unspecified, unique-local (fc00::/7),
/// link-local (fe80::/10), and IPv4-mapped addresses (checked recursively
/// against the IPv4 rules) for IPv6.
pub fn is_forbidden_target(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                || v6
                    .to_ipv4_mapped()
                    .map(|mapped| is_forbidden_v4(&mapped))
                    .unwrap_or(false)
        }
    }
}

fn is_forbidden_v4(v4: &Ipv4Addr) -> bool {
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_documentation()
        || v4.is_unspecified()
        || (v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]))
        || v4.octets() == [169, 254, 169, 254]
}

/// Resolves `url`'s host and validates every candidate address, returning the
/// one that must be dialled. The caller MUST connect to exactly this
/// `SocketAddr` (e.g. via `ClientBuilder::resolve`) rather than letting the
/// HTTP client re-resolve the hostname: resolving twice reopens a
/// DNS-rebinding TOCTOU window where the name that passed validation is
/// swapped for a forbidden one by the time the connection is actually made.
pub async fn validate_extract_target(url: &Url) -> Result<SocketAddr, AppError> {
    if url.scheme() != "https" {
        return Err(AppError::InvalidQuery(
            "Only https:// URLs may be extracted".into(),
        ));
    }

    let host = url
        .host_str()
        .ok_or_else(|| AppError::InvalidQuery("URL has no host".into()))?;
    let port = url.port_or_known_default().unwrap_or(443);

    let candidates = tokio::net::lookup_host((host, port))
        .await
        .map_err(|_| AppError::InvalidQuery("Host could not be resolved".into()))?;

    let mut resolved: Option<SocketAddr> = None;
    for addr in candidates {
        if is_forbidden_target(&addr.ip()) {
            // Deliberately generic: echoing the resolved IP or which check
            // tripped would turn this endpoint into a private-network scanner.
            return Err(AppError::InvalidQuery(
                "URL resolves to a non-public address".into(),
            ));
        }
        resolved.get_or_insert(addr);
    }

    resolved.ok_or_else(|| AppError::InvalidQuery("Host could not be resolved".into()))
}

/// Known cross-site tracking query parameters stripped by
/// `normalize_extract_url`. Deliberately a small, maintainable
/// allowlist rather than an exhaustive one.
const TRACKING_QUERY_PARAMS: &[&str] = &[
    "utm_source",
    "utm_medium",
    "utm_campaign",
    "utm_term",
    "utm_content",
    "fbclid",
    "gclid",
    "msclkid",
    "mc_cid",
    "mc_eid",
    "igshid",
    "ref",
    "ref_src",
    "_ga",
    "yclid",
];

/// Strips the fragment and any known tracking query parameters from
/// `url` before it is fetched. The extract endpoint is billed per byte
/// through the proxy, and a `#fragment` or `utm_*`/`fbclid`/`gclid`-style
/// tracking param on an otherwise-identical URL would otherwise be fetched
/// (and cached) as a distinct target even
/// though the content served is identical.
///
/// Every other query parameter, and their relative order, is preserved. If
/// the URL has no tracking parameters, the query string is left completely
/// untouched (not re-serialized) so its original percent-encoding survives
/// byte-for-byte; the query is only ever rebuilt when there is actually a
/// tracking parameter to remove, and dropping the last remaining parameter
/// removes the trailing `?` rather than leaving it empty.
///
/// Deliberately *not* a general-purpose URL normalizer (no scheme/host
/// case-folding, no default-port stripping, no path dot-segment resolution)
/// — just the two things that are pure waste for a per-byte-billed fetch.
pub fn normalize_extract_url(url: &Url) -> Url {
    let mut normalized = url.clone();
    normalized.set_fragment(None);

    let has_tracking_param = normalized
        .query_pairs()
        .any(|(k, _)| TRACKING_QUERY_PARAMS.contains(&k.as_ref()));
    if !has_tracking_param {
        return normalized;
    }

    let retained: Vec<(String, String)> = normalized
        .query_pairs()
        .filter(|(k, _)| !TRACKING_QUERY_PARAMS.contains(&k.as_ref()))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    if retained.is_empty() {
        normalized.set_query(None);
    } else {
        normalized.query_pairs_mut().clear().extend_pairs(&retained);
    }

    normalized
}

/// Builds a request-scoped client pinned to `addr` for `host`, so the
/// connection actually dialled is the exact address `validate_extract_target`
/// checked. `.resolve()` is a builder-time setting, so this must be built
/// per validated target rather than reused across requests. Redirects are
/// disabled: following them would let a validated URL bounce to an
/// unvalidated internal target.
///
/// `proxy_url` is `None` unless an operator has explicitly set
/// `SERICA_PROXY__EXTRACT_URL`. ⚠️ Passing `Some` here **defeats the
/// `.resolve()` pin above**: a proxied HTTPS request reaches the target via
/// an HTTP CONNECT naming the *hostname*, which the proxy — not this client —
/// resolves, so the DNS-rebinding protection `validate_extract_target`
/// exists to provide does not hold. Callers are responsible for having
/// already logged the startup-time warning this implies (see
/// `main.rs`); this function still honors the config because refusing would
/// silently downgrade an operator's explicit choice to route extract egress
/// through a corporate proxy into "ignored", which is worse than doing what
/// was asked while having warned loudly once at startup.
pub fn build_pinned_extract_client(
    host: &str,
    addr: SocketAddr,
    proxy_url: Option<&str>,
) -> Result<reqwest::Client, AppError> {
    let mut builder = reqwest::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .resolve(host, addr)
        .user_agent(EXTRACT_USER_AGENT)
        .timeout(EXTRACT_TIMEOUT)
        // reqwest's default `ClientBuilder` auto-detects `HTTP_PROXY`/
        // `HTTPS_PROXY`/`ALL_PROXY` from the process environment and honors
        // them with no code here opting in — so without this call, an
        // operator's ambient proxy env var (common in corporate
        // deployments, and often set for completely unrelated tools) would
        // silently defeat the `.resolve()` pin above even when
        // `proxy.extract_url` is unset. `.no_proxy()` only disables that
        // auto-detection; it does not prevent the explicit `.proxy()` call
        // below from applying `proxy_url` when an operator opts in on
        // purpose.
        .no_proxy();

    if let Some(proxy_url) = proxy_url {
        let proxy = reqwest::Proxy::all(proxy_url)
            .map_err(|e| AppError::Internal(format!("Failed to build extract proxy: {e}")))?;
        builder = builder.proxy(proxy);
    }

    builder
        .build()
        .map_err(|e| AppError::Internal(format!("Failed to build extract client: {e}")))
}

/// Resolves a redirect response's `Location` header against the URL that
/// produced it, then applies `normalize_extract_url` to the result —
/// a redirect to a URL carrying `?utm_source=...`/`#fragment` gets exactly
/// the same treatment the entry URL already gets. `Url::join` handles both
/// relative (`/path`, `path`) and protocol-relative (`//host/path`)
/// `Location` values per RFC 7231 §7.1.2; anything that fails to parse even
/// against `current` as a base is treated as a bad hop, same as a hop that
/// fails SSRF validation — there's nothing safe to fall back to.
fn resolve_redirect_target(current: &Url, location: &str) -> Result<Url, AppError> {
    let next = current.join(location).map_err(|_| {
        AppError::InvalidQuery("Redirect target could not be parsed as a URL".into())
    })?;
    Ok(normalize_extract_url(&next))
}

/// Fetches `url` on behalf of the extract endpoint, following up to
/// `MAX_REDIRECT_HOPS` redirects.
///
/// Redirects are a classic SSRF bypass (a validated public URL 3xx-ing to an
/// internal address, or to a non-`https` scheme), so every hop — including
/// the entry URL — gets exactly the same treatment: `normalize_extract_url`
/// `normalize_extract_url`, then the complete `validate_extract_target` check (https-only,
/// DNS resolution, every candidate address checked against
/// `is_forbidden_target`), before anything is dialled. A fresh
/// `build_pinned_extract_client` is built for each hop's validated
/// `(host, addr)` pair and never reused across hops: a redirect can target a
/// different host than the one that was just validated, and `.resolve()`
/// pins a single host:addr per client.
///
/// Fails the whole request closed the moment any hop doesn't validate, or if
/// the chain exceeds `MAX_REDIRECT_HOPS` redirects — a 3xx response carries
/// no content worth returning as a partial result, so there is nothing to
/// gracefully degrade to.
///
/// Returns the first non-redirect response together with the exact `Url`
/// that produced it, so callers can make `ExtractResponse.url` (and their
/// logging) reflect what was actually fetched rather than the caller's
/// original input — consistent with how the entry URL is already treated.
///
/// `proxy_url` (`config.proxy.extract_url`) is forwarded unchanged to
/// every hop's `build_pinned_extract_client` call; see that function's doc
/// comment for why a configured proxy defeats the per-hop DNS-rebinding pin,
/// and why that's accepted as an operator's explicit, once-warned-at-startup
/// choice rather than refused here.
pub async fn fetch_extract_target(
    url: &Url,
    proxy_url: Option<&str>,
) -> Result<(reqwest::Response, Url), AppError> {
    let mut current = normalize_extract_url(url);
    let mut redirects_followed: u8 = 0;

    loop {
        let validated_addr = validate_extract_target(&current).await?;
        let host = current
            .host_str()
            .ok_or_else(|| AppError::InvalidQuery("URL has no host".into()))?
            .to_string();
        let client = build_pinned_extract_client(&host, validated_addr, proxy_url)?;

        let response = client
            .get(current.clone())
            .header("Accept", "text/html,application/xhtml+xml;q=0.9")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("DNT", "1")
            .send()
            .await
            .map_err(|e| {
                tracing::warn!(url = %current, error = %e, "Extract fetch failed");
                // Opaque on purpose, for the same reason the gateways treat
                // config/bind/redis errors opaquely: the raw
                // reqwest error can carry resolved IPs, DNS failure detail,
                // and TLS certificate subjects a caller could use to map the
                // internal network by diffing error strings.
                AppError::ServiceUnavailable("Failed to fetch URL".into())
            })?;

        if !response.status().is_redirection() {
            return Ok((response, current));
        }

        if redirects_followed >= MAX_REDIRECT_HOPS {
            return Err(AppError::InvalidQuery(format!(
                "Too many redirects (exceeded {MAX_REDIRECT_HOPS})"
            )));
        }

        // Not every 3xx carries a `Location` (300 Multiple Choices isn't
        // required to; a 304 never does, though this client never sends the
        // conditional headers that would provoke one). Treat a Location-less
        // redirect as a bad hop, same as any other: fail closed rather than
        // returning the redirect envelope's body as if it were content.
        let location = response
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| {
                AppError::InvalidQuery(format!(
                    "Redirect response ({}) had no usable Location header",
                    response.status()
                ))
            })?
            .to_string();

        let next = resolve_redirect_target(&current, &location)?;
        redirects_followed += 1;
        tracing::debug!(
            from = %current,
            to = %next,
            hop = redirects_followed,
            "Following extract redirect"
        );
        current = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_loopback_v4() {
        assert!(is_forbidden_target(&"127.0.0.1".parse().unwrap()));
    }

    #[test]
    fn blocks_metadata_ip() {
        assert!(is_forbidden_target(&"169.254.169.254".parse().unwrap()));
    }

    #[test]
    fn blocks_private_ranges() {
        assert!(is_forbidden_target(&"10.0.0.5".parse().unwrap()));
        assert!(is_forbidden_target(&"172.16.0.1".parse().unwrap()));
        assert!(is_forbidden_target(&"192.168.1.1".parse().unwrap()));
    }

    #[test]
    fn blocks_cgnat() {
        assert!(is_forbidden_target(&"100.64.0.1".parse().unwrap()));
        assert!(!is_forbidden_target(&"100.63.255.255".parse().unwrap()));
        assert!(!is_forbidden_target(&"100.128.0.0".parse().unwrap()));
    }

    #[test]
    fn blocks_unspecified_and_broadcast() {
        assert!(is_forbidden_target(&"0.0.0.0".parse().unwrap()));
        assert!(is_forbidden_target(&"255.255.255.255".parse().unwrap()));
    }

    #[test]
    fn blocks_ipv6_loopback_and_unique_local() {
        assert!(is_forbidden_target(&"::1".parse().unwrap()));
        assert!(is_forbidden_target(&"fc00::1".parse().unwrap()));
        assert!(is_forbidden_target(&"fe80::1".parse().unwrap()));
    }

    #[test]
    fn blocks_ipv4_mapped_private() {
        assert!(is_forbidden_target(&"::ffff:10.0.0.1".parse().unwrap()));
    }

    #[test]
    fn allows_public_v4_and_v6() {
        assert!(!is_forbidden_target(&"1.1.1.1".parse().unwrap()));
        assert!(!is_forbidden_target(
            &"2606:4700:4700::1111".parse().unwrap()
        ));
    }

    #[tokio::test]
    async fn rejects_non_https_scheme() {
        let url = Url::parse("http://example.com").unwrap();
        let err = validate_extract_target(&url).await.unwrap_err();
        assert!(matches!(err, AppError::InvalidQuery(_)));
    }

    #[tokio::test]
    async fn rejects_loopback_host() {
        let url = Url::parse("https://127.0.0.1/").unwrap();
        let err = validate_extract_target(&url).await.unwrap_err();
        assert!(matches!(err, AppError::InvalidQuery(_)));
    }

    #[test]
    fn build_pinned_extract_client_default_has_no_proxy_url() {
        let addr: SocketAddr = "1.2.3.4:443".parse().unwrap();
        assert!(build_pinned_extract_client("example.com", addr, None).is_ok());
    }

    #[test]
    fn build_pinned_extract_client_rejects_malformed_proxy_url() {
        let addr: SocketAddr = "1.2.3.4:443".parse().unwrap();
        let err = build_pinned_extract_client("example.com", addr, Some("not a url")).unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
    }

    // reqwest's `ClientBuilder` auto-detects `HTTP_PROXY`/`HTTPS_PROXY`
    // from the process environment by default, entirely independent of
    // anything this crate configures. Left unguarded, that means an
    // operator's ambient proxy env var — set for some unrelated tool,
    // common in corporate environments — would silently route extract
    // traffic through it, defeating the `.resolve()` IP pin exactly the way
    // an explicit `proxy.extract_url` misconfiguration would. This proves
    // `build_pinned_extract_client`'s unconditional `.no_proxy()` call
    // actually holds: with `HTTPS_PROXY` pointed at a listener that is
    // never passed in as `addr`, a request must still land on the pinned
    // origin directly, and the env-advertised "proxy" must never see a
    // connection at all.
    //
    // Uses raw env mutation rather than the `temp_env` crate used
    // elsewhere in this codebase: `temp_env` only wraps synchronous
    // closures, and this test needs `tokio::net::TcpListener`, which
    // requires staying on the Tokio reactor.
    #[tokio::test]
    async fn extract_client_ignores_ambient_https_proxy_env_by_default() {
        let origin_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind origin listener");
        let origin_addr = origin_listener.local_addr().unwrap();

        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake-proxy listener");
        let proxy_addr = proxy_listener.local_addr().unwrap();

        let previous_https_proxy = std::env::var("HTTPS_PROXY").ok();
        // SAFETY: this test owns `HTTPS_PROXY` for its short duration and
        // restores the prior value unconditionally before returning.
        unsafe {
            std::env::set_var("HTTPS_PROXY", format!("http://{proxy_addr}"));
        }

        // Host and pinned addr's ports must match: `.resolve(host, addr)`
        // overrides DNS for that exact host:port pair, so the request below
        // targets `origin_addr`'s port explicitly rather than the https
        // default of 443 (which `origin_listener`, bound to an ephemeral
        // port, isn't listening on).
        let client = build_pinned_extract_client("example.invalid", origin_addr, None)
            .expect("client must build");
        let request_url = format!("https://example.invalid:{}/", origin_addr.port());
        // Expected to fail: `origin_listener` is a bare TCP socket, not a
        // TLS server, so the handshake never completes. Only whether a
        // *connection attempt* reached each listener matters here.
        let _ = tokio::time::timeout(Duration::from_secs(2), client.get(&request_url).send()).await;

        // SAFETY: see above.
        unsafe {
            match &previous_https_proxy {
                Some(v) => std::env::set_var("HTTPS_PROXY", v),
                None => std::env::remove_var("HTTPS_PROXY"),
            }
        }

        let origin_dialled =
            tokio::time::timeout(Duration::from_millis(1000), origin_listener.accept())
                .await
                .is_ok();
        assert!(
            origin_dialled,
            "expected the extract client to dial the pinned origin address directly"
        );

        let proxy_dialled =
            tokio::time::timeout(Duration::from_millis(300), proxy_listener.accept())
                .await
                .is_ok();
        assert!(
            !proxy_dialled,
            "extract client must not honor an ambient HTTPS_PROXY by default"
        );
    }

    #[test]
    fn normalize_strips_fragment() {
        let url = Url::parse("https://example.com/docs#section-2").unwrap();
        assert_eq!(
            normalize_extract_url(&url).as_str(),
            "https://example.com/docs"
        );
    }

    #[test]
    fn normalize_strips_tracking_params() {
        let url = Url::parse(
            "https://example.com/post?utm_source=twitter&utm_medium=social&fbclid=abc123",
        )
        .unwrap();
        assert_eq!(
            normalize_extract_url(&url).as_str(),
            "https://example.com/post"
        );
    }

    #[test]
    fn normalize_preserves_non_tracking_params_and_order() {
        let url = Url::parse("https://example.com/search?q=rust&page=2&sort=asc").unwrap();
        assert_eq!(
            normalize_extract_url(&url).as_str(),
            "https://example.com/search?q=rust&page=2&sort=asc"
        );
    }

    #[test]
    fn normalize_is_noop_when_nothing_to_strip() {
        let url = Url::parse("https://example.com/docs/guide?version=2").unwrap();
        assert_eq!(
            normalize_extract_url(&url).as_str(),
            "https://example.com/docs/guide?version=2"
        );
    }

    #[test]
    fn normalize_drops_trailing_question_mark_when_all_params_were_tracking() {
        let url =
            Url::parse("https://example.com/article?utm_source=newsletter&gclid=xyz").unwrap();
        let normalized = normalize_extract_url(&url);
        assert_eq!(normalized.as_str(), "https://example.com/article");
        assert!(normalized.query().is_none());
    }

    #[test]
    fn normalize_mixed_tracking_and_non_tracking_params_preserves_order() {
        let url = Url::parse(
            "https://example.com/item?id=42&utm_source=fb&color=blue&fbclid=xyz&size=large",
        )
        .unwrap();
        assert_eq!(
            normalize_extract_url(&url).as_str(),
            "https://example.com/item?id=42&color=blue&size=large"
        );
    }

    #[test]
    fn normalize_strips_fragment_and_tracking_params_together() {
        let url = Url::parse("https://example.com/page?q=test&utm_campaign=spring#top").unwrap();
        assert_eq!(
            normalize_extract_url(&url).as_str(),
            "https://example.com/page?q=test"
        );
    }

    // === redirect-following ===
    //
    // `fetch_extract_target` always resolves the target host via real
    // `tokio::net::lookup_host` DNS before dialling anything (that's the
    // whole point of per-hop re-validation). That makes a fully end-to-end
    // test — real HTTP origin, serving a 3xx chain, driven entirely through
    // `fetch_extract_target` — impractical to write as a reliable, offline
    // unit test for two independent reasons that both have to hold at once:
    //   1. Every hop's *host* must resolve to a real, currently-routable
    //      public address for `validate_extract_target` to accept it — an
    //      IP literal like `1.1.1.1` resolves without any real network I/O
    //      (see `redirect_target_with_public_ip_literal_passes_validation`
    //      below) and is used elsewhere in this file for exactly that
    //      reason, but this repo doesn't control what a public IP serves,
    //      so it can't be made to return a scripted redirect chain.
    //   2. A local `tokio::net::TcpListener` (the trick the ambient-proxy
    //      test above uses to script exact responses) is bound to
    //      `127.0.0.1`, which `is_forbidden_target` correctly rejects — by
    //      design, since loopback is exactly the kind of address SSRF
    //      validation exists to block. The ambient-proxy test above only
    //      gets away with a loopback listener because it calls
    //      `build_pinned_extract_client` directly and never goes through
    //      `validate_extract_target` at all.
    // So a real "single hop followed to a public target succeeds" or
    // "chain exceeding MAX_REDIRECT_HOPS fails" test would have to either
    // depend on a live third-party server behaving in a specific way
    // (flaky, and not this crate's to control) or bypass the exact
    // validation logic these tests exist to prove is applied. Instead, the
    // tests below exercise the two units `fetch_extract_target`'s loop is
    // built from — hop-target resolution (`resolve_redirect_target`) and
    // per-hop validation (`validate_extract_target`, already covered above)
    // — both directly and composed together, which covers the same
    // decision logic without a network dependency. The `MAX_REDIRECT_HOPS`
    // hop cap and the full follow-loop are additionally covered by manual
    // verification against a real redirecting URL (arxiv).

    #[test]
    fn max_redirect_hops_is_five() {
        // Pins the hardcoded hop cap so a future change to it is a
        // deliberate, reviewed edit to this test rather than a silent bump.
        assert_eq!(MAX_REDIRECT_HOPS, 5);
    }

    #[test]
    fn resolve_redirect_target_handles_relative_path() {
        let current = Url::parse("https://example.com/a/b").unwrap();
        let next = resolve_redirect_target(&current, "/c").unwrap();
        assert_eq!(next.as_str(), "https://example.com/c");
    }

    #[test]
    fn resolve_redirect_target_handles_absolute_url() {
        let current = Url::parse("https://example.com/a").unwrap();
        let next = resolve_redirect_target(&current, "https://other.example/b").unwrap();
        assert_eq!(next.as_str(), "https://other.example/b");
    }

    #[test]
    fn resolve_redirect_target_handles_protocol_relative_location() {
        let current = Url::parse("https://example.com/a").unwrap();
        let next = resolve_redirect_target(&current, "//cdn.example.com/b").unwrap();
        assert_eq!(next.as_str(), "https://cdn.example.com/b");
    }

    #[test]
    fn resolve_redirect_target_normalizes_like_the_entry_url() {
        // Normalization must apply to every hop, not just the entry
        // URL: a redirect to a tracking-param/fragment-laden target should
        // come out clean.
        let current = Url::parse("https://example.com/a").unwrap();
        let next =
            resolve_redirect_target(&current, "/b?utm_source=newsletter&id=1#section").unwrap();
        assert_eq!(next.as_str(), "https://example.com/b?id=1");
    }

    #[test]
    fn resolve_redirect_target_rejects_unparseable_location() {
        // A relative-looking string almost always resolves against `current`
        // as a base (percent-encoding whatever doesn't fit) rather than
        // failing to parse, so this needs a `Location` that's unambiguously
        // malformed even as an absolute URL: an unterminated/invalid IPv6
        // host literal, which `url` rejects outright rather than treating as
        // a path segment.
        let current = Url::parse("https://example.com/a").unwrap();
        let err = resolve_redirect_target(&current, "https://[not-a-valid-ipv6/path").unwrap_err();
        assert!(matches!(err, AppError::InvalidQuery(_)));
    }

    #[tokio::test]
    async fn redirect_target_with_public_ip_literal_passes_validation() {
        // Numeric IP literals resolve without performing real DNS network
        // I/O (the OS resolver short-circuits straight to parsing), so this
        // is safe and deterministic to run offline — it exercises the same
        // `validate_extract_target` call `fetch_extract_target` makes for
        // every hop, proving a "public-looking" redirect target is let
        // through.
        let current = Url::parse("https://example.com/start").unwrap();
        let next = resolve_redirect_target(&current, "https://1.1.1.1/next").unwrap();
        let addr = validate_extract_target(&next).await.unwrap();
        assert_eq!(addr.ip(), "1.1.1.1".parse::<IpAddr>().unwrap());
    }

    #[tokio::test]
    async fn redirect_target_pointing_at_forbidden_ip_fails_closed() {
        // Simulates "the origin hop was fine, but it redirects somewhere
        // forbidden": `current` stands in for an already-validated hop, and
        // its `Location` points at loopback. The resolved+normalized target
        // must fail exactly the same `validate_extract_target` check
        // `fetch_extract_target` runs on every hop, regardless of how
        // trustworthy the hop that produced the redirect looked.
        let current = Url::parse("https://example.com/start").unwrap();
        let next = resolve_redirect_target(&current, "https://127.0.0.1/admin").unwrap();
        let err = validate_extract_target(&next).await.unwrap_err();
        assert!(matches!(err, AppError::InvalidQuery(_)));
    }

    #[tokio::test]
    async fn redirect_target_downgrading_to_http_fails_closed() {
        // A redirect to a non-https scheme is exactly the SSRF bypass this
        // item's scope decisions call out by name — must fail the same
        // https-only check the entry URL gets.
        let current = Url::parse("https://example.com/start").unwrap();
        let next = resolve_redirect_target(&current, "http://example.com/insecure").unwrap();
        let err = validate_extract_target(&next).await.unwrap_err();
        assert!(matches!(err, AppError::InvalidQuery(_)));
    }
}

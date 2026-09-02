// Gateway module: HTTP and MCP server implementations
pub mod http;
pub mod mcp;

use std::collections::hash_map::DefaultHasher;
use std::collections::HashSet;
use std::hash::{Hash, Hasher};
use std::sync::LazyLock;

use crate::models::search::{SearchRequest, SearchResponse, SearchResult};
use url::Url;

// === Cache key ===
//
// HTTP and MCP must derive the cache key identically — they share one
// `Arc<dyn CacheLayer>` (see main.rs), so any divergence here reopens the
// cross-gateway cache-poisoning issue this module exists to close (a prior
// version had two copies of this format!() that quietly drifted).
//
// The key is a hash of the structured fields rather than a delimited string:
// the query is user-controlled and could otherwise smuggle the `|`/`:`
// delimiters to collide with an unrelated request's key. `DefaultHasher` is
// not stable across Rust releases, which is fine for a ~300s TTL cache but
// would be the wrong choice for anything persisted long-term. The `v1`
// prefix lets a future incompatible change to this function invalidate old
// entries instead of silently misinterpreting them.
pub(crate) fn cache_key(req: &SearchRequest) -> String {
    let mut hasher = DefaultHasher::new();
    req.query.hash(&mut hasher);
    req.page.hash(&mut hasher);
    req.safe_search.hash(&mut hasher);
    req.language.hash(&mut hasher);
    format!("serica:v1:search:{:016x}", hasher.finish())
}

// The extract cache key is a hash of the *normalized*
// URL's string form — not the raw caller-supplied URL, and not the post-
// redirect final URL. `normalize_extract_url` runs first, so
// building on the normalized URL means `?utm_source=...`/
// `#fragment` variants of the same target share one cache entry for free.
// It must be computed from the *entry* URL, before `fetch_extract_target`
// runs — that's what lets a cache hit skip the fetch (and therefore SSRF
// validation) entirely, which is the whole point of checking cache first;
// keying on the final (post-redirect) URL would require doing the fetch
// before the cache could ever be consulted.
//
// Distinct `"serica:v1:extract:"` namespace from `cache_key`'s
// `"serica:v1:search:"` above so the two response types — which now share
// one `CacheLayer`/Redis instance (see this module's `cache_key` doc
// comment) — can never collide even by coincidence of hash value.
pub(crate) fn extract_cache_key(url: &Url) -> String {
    let mut hasher = DefaultHasher::new();
    url.as_str().hash(&mut hasher);
    format!("serica:v1:extract:{:016x}", hasher.finish())
}

// === Filters ===
//
// This local blocklist is a *secondary net*, not the primary
// defense. The primary safe-search filtering happens upstream, at
// DuckDuckGo itself, via the `kp` request parameter
// (`engines::duckduckgo::ddg_kp_for_safe_search`/`build_search_url`) — the
// same design choice as language filtering, and for the same
// reasons: DDG's own classifier is far more capable than a fixed English
// word list (it isn't defeated by leetspeak, plurals, or non-English terms,
// none of which this list can ever cover), and filtering
// before pagination is committed avoids a local post-filter shrinking an
// already-paginated result set. What remains here is best-effort
// defense-in-depth for whatever slips past upstream filtering (e.g. DDG
// itself misclassifying a result) — see the README's "Safe search" section
// for the explicit best-effort framing.

/// Blocklist applied at `safe_search=2` (strict). Every entry here is
/// unambiguous when matched as a whole word — porn-industry-specific terms
/// with no common non-adult usage.
const SAFE_SEARCH_BLOCKLIST_STRICT: &[&str] = &[
    "porn",
    "xxx",
    "adult",
    "nsfw",
    "sex",
    "pornography",
    "explicit",
    "nude",
    "nudity",
    "erotic",
    "fetish",
    "hentai",
    "onlyfans",
    "escort",
    "camgirl",
    "webcam",
    "stripper",
    "stripclub",
];

/// Blocklist applied at `safe_search=1` (moderate, the service default). A
/// narrower subset of the strict list: it drops the four standalone words
/// the audit specifically flagged as over-broad at the default level —
/// `"adult"` ("adult education"), `"sex"` ("sex differences in clinical
/// trials"), `"nude"` ("nude descriptive color"), and `"webcam"` ("webcam
/// conferencing") — while keeping every term that has no comparable
/// legitimate reading. Since upstream `kp=-1` filtering (see the module doc
/// comment above) already does the real moderate-level filtering at the
/// source, this list only needs to catch upstream misses without
/// reintroducing false positives on legitimate queries.
const SAFE_SEARCH_BLOCKLIST_MODERATE: &[&str] = &[
    "porn",
    "xxx",
    "nsfw",
    "pornography",
    "explicit",
    "nudity",
    "erotic",
    "fetish",
    "hentai",
    "onlyfans",
    "escort",
    "camgirl",
    "stripper",
    "stripclub",
];

/// `SAFE_SEARCH_BLOCKLIST_STRICT`/`_MODERATE` as `HashSet`s for O(1)
/// membership testing, built once on first use rather than re-derived per
/// request.
static SAFE_SEARCH_BLOCKLIST_STRICT_SET: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| SAFE_SEARCH_BLOCKLIST_STRICT.iter().copied().collect());
static SAFE_SEARCH_BLOCKLIST_MODERATE_SET: LazyLock<HashSet<&'static str>> =
    LazyLock::new(|| SAFE_SEARCH_BLOCKLIST_MODERATE.iter().copied().collect());

/// Apply the local secondary-net safe_search filter (0=off, 1=moderate,
/// 2=strict). Real filtering happens upstream via DDG's `kp` parameter
/// (see the module doc comment above) before results ever reach
/// here; this only backstops whatever gets through that.
pub(crate) fn apply_safe_search_filter(
    mut results: Vec<SearchResult>,
    level: u8,
) -> Vec<SearchResult> {
    let blocklist: &HashSet<&'static str> = match level {
        0 => return results,
        1 => &SAFE_SEARCH_BLOCKLIST_MODERATE_SET,
        // Anything >= 2 (in practice only ever 2, since
        // `SearchRequest::try_from_params`/the MCP handler reject anything
        // higher) uses the fuller strict list.
        _ => &SAFE_SEARCH_BLOCKLIST_STRICT_SET,
    };
    results.retain(|r| {
        // Include the result URL's host alongside title+snippet —
        // a filter that looked only at title/snippet would miss a
        // result with a clean title/snippet over an explicit host (or vice
        // versa) passed straight through either direction. Parsed fresh per
        // result since `SearchResult` doesn't carry a pre-parsed host; this
        // runs over a bounded, already-paginated batch (~20-40 results), not
        // a corpus, so the added parse cost here is negligible next to the
        // network round-trip that already dominates a search request.
        let host = url::Url::parse(&r.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();

        // Single tokenization pass over title+snippet+host (word-boundary
        // split), checked against the blocklist HashSet with early exit on
        // first match. A per-term re-tokenization of a freshly allocated,
        // lowercased combined string would rebuild the same string once per
        // blocklist term — this does the whole job in one pass.
        !r.title
            .split(|c: char| !c.is_alphanumeric())
            .chain(r.snippet.split(|c: char| !c.is_alphanumeric()))
            .chain(host.split(|c: char| !c.is_alphanumeric()))
            .any(|word| blocklist.contains(word.to_ascii_lowercase().as_str()))
    });
    results
}

// === Shared finalize step ===
//
// The one place safe_search filtering + pagination happen, called by both
// gateways before anything is cached. Both gateways run every result
// through this step, so the cache never holds engine output that wasn't
// filtered under a key claiming it was.
//
// Language filtering deliberately does NOT happen here. A local "does this
// text contain enough common words" heuristic — `text.contains(word)` against
// a short per-language word list — is close to a no-op for the languages it
// covers (English "is"/"in"/"it"/"the" match inside "th-IS",
// "informat-ION", "it-EM", "o-THE-r") and actively wrong for the rest
// (German "in"/"die" matching inside English "in-side"/"die-t"). Language
// filtering happens upstream instead, via the `kl` region
// parameter DuckDuckGo itself understands (`ddg_region_for_language` in
// src/engines/duckduckgo.rs). `SearchRequest::try_from_params`
// (src/gateway/http.rs) and the MCP `serica_search` handler
// (src/gateway/mcp.rs) both reject a requested `lang`/`language` outright if
// it isn't in that mapping. Filtering at
// the source is strictly better than a local post-filter here: it's accurate
// by construction, and it avoids a class of bug where a post-filter
// shrinks a page's result count after DDG has already committed pagination
// to a different (unfiltered) count.
//
// Pagination happens EXACTLY ONCE, here, and it must not re-skip by
// `req.page`. The DuckDuckGo adapter (src/engines/duckduckgo.rs) already
// requests the correct page from DDG itself via an offset query param —
// `results` arriving here is already the page-scoped slice (~20 items for
// whatever page was asked for), not the full corpus. Re-applying
// `skip((page - 1) * max_per_page)` on top of that skips past an
// already-small, already-offset vector, so every page >= 2 would come back
// empty even though DDG returned real results. Only `take(max_per_page)` is
// needed here, to enforce the page-size cap after filtering may have
// shrunk the set.
pub(crate) fn finalize_results(
    results: Vec<SearchResult>,
    req: &SearchRequest,
    max_per_page: usize,
    timing_ms: u64,
) -> SearchResponse {
    let filtered = apply_safe_search_filter(results, req.safe_search);

    // NOTE: this is the size of the current (already page-scoped) filtered
    // batch, not a corpus-wide total — see the doc comment on
    // `SearchResponse::total_results`. DDG's HTML scrape doesn't expose a
    // reliable overall hit count, so a truthful corpus total isn't
    // available here without a different upstream.
    let total = filtered.len();
    let paginated: Vec<_> = filtered.into_iter().take(max_per_page).collect();

    SearchResponse {
        results: paginated,
        total_results: total,
        page: req.page,
        timing_ms,
        cached: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(title: &str, snippet: &str) -> SearchResult {
        SearchResult {
            url: "https://example.com".into(),
            title: title.into(),
            snippet: snippet.into(),
        }
    }

    // MCP calls this same function, so proving it filters here proves
    // MCP's safe_search parameter is a real filter, not a no-op — and that
    // the cache never holds unfiltered engine output under a key claiming
    // it was filtered.
    #[test]
    fn finalize_results_applies_safe_search_filter() {
        let req = SearchRequest {
            query: "test".into(),
            page: 1,
            safe_search: 2,
            language: None,
        };
        let results = vec![
            result("Rust programming guide", "Learn Rust"),
            result("Escort service listings", "Adult content here"),
        ];

        let response = finalize_results(results, &req, 20, 5);

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].title, "Rust programming guide");
    }

    // The DuckDuckGo adapter already scopes
    // `results` to the requested page via an offset query param before
    // `finalize_results` ever sees them (see src/engines/duckduckgo.rs
    // `search()`). This simulates that: a 25-element Vec stands in for
    // what DDG already returned as "page 2" (already offset,
    // not the full corpus). Re-applying
    // `skip((page - 1) * max_per_page)` = `skip(20)` on top of that would
    // run past the end of this already-small vector and produce zero
    // results for every page >= 2 despite the engine returning real
    // data. So this must not skip by page at all — only cap to
    // `max_per_page`.
    #[test]
    fn finalize_results_page_2_does_not_double_paginate() {
        let req = SearchRequest {
            query: "test".into(),
            page: 2,
            safe_search: 0,
            language: None,
        };
        let results: Vec<SearchResult> = (0..25)
            .map(|i| result(&format!("Result {i}"), "snippet"))
            .collect();

        let response = finalize_results(results, &req, 20, 5);

        assert_eq!(
            response.results.len(),
            20,
            "page 2 must return the (already page-scoped) results DDG sent, capped at max_per_page — not an empty set"
        );
        assert_eq!(
            response.results[0].title, "Result 0",
            "handler must not re-skip by page — DDG already offset these results"
        );
        assert_eq!(response.results[19].title, "Result 19");
    }

    #[test]
    fn finalize_results_off_keeps_all_results() {
        let req = SearchRequest {
            query: "test".into(),
            page: 1,
            safe_search: 0,
            language: None,
        };
        let results = vec![result("Escort service listings", "Adult content here")];

        let response = finalize_results(results, &req, 20, 5);

        assert_eq!(response.results.len(), 1);
    }

    fn result_with_url(url: &str, title: &str, snippet: &str) -> SearchResult {
        SearchResult {
            url: url.into(),
            title: title.into(),
            snippet: snippet.into(),
        }
    }

    // "adult education" must not be blocked at the *default*
    // safe_search=1 (moderate) level, since "adult" was a standalone entry
    // in the one-size-fits-all list every prior level shared.
    #[test]
    fn apply_safe_search_filter_moderate_does_not_block_adult_education() {
        let results = vec![result(
            "Adult education programs",
            "Community college courses",
        )];
        let filtered = apply_safe_search_filter(results, 1);
        assert_eq!(
            filtered.len(),
            1,
            "moderate (safe_search=1) must not block 'adult' as a standalone word"
        );
    }

    // The same query must still be blocked at
    // safe_search=2 (strict) — the moderate list is a narrower subset of
    // strict's, not a wholesale replacement, so strict retains every
    // standalone entry moderate dropped.
    #[test]
    fn apply_safe_search_filter_strict_still_blocks_adult() {
        let results = vec![result(
            "Adult education programs",
            "Community college courses",
        )];
        let filtered = apply_safe_search_filter(results, 2);
        assert_eq!(
            filtered.len(),
            0,
            "strict (safe_search=2) must still block 'adult' as a standalone word"
        );
    }

    // Same idea for the other three over-broad words —
    // "sex differences in clinical trials", "nude descriptive
    // color", and "webcam conferencing" must all survive moderate filtering.
    #[test]
    fn apply_safe_search_filter_moderate_does_not_block_named_false_positives() {
        let results = vec![
            result(
                "Sex differences in clinical trials",
                "A review of enrollment patterns",
            ),
            result("Nude descriptive color", "Paint and design terminology"),
            result("Webcam conferencing setup", "Office equipment guide"),
        ];
        let filtered = apply_safe_search_filter(results, 1);
        assert_eq!(
            filtered.len(),
            3,
            "moderate must not block any of these legitimate results"
        );
    }

    // Moderate and strict must actually differ in
    // practice, not just in name — this pins the behavioral distinction
    // between the two lists.
    #[test]
    fn apply_safe_search_filter_moderate_and_strict_are_not_identical() {
        let results = vec![result(
            "Adult education programs",
            "Community college courses",
        )];
        let moderate = apply_safe_search_filter(results.clone(), 1);
        let strict = apply_safe_search_filter(results, 2);
        assert_ne!(
            moderate.len(),
            strict.len(),
            "moderate and strict must filter this case differently"
        );
    }

    // The filter must also inspect the result URL's
    // host, not just title/snippet — a blocklisted term hidden
    // only in the host (with a clean title/snippet) would otherwise pass
    // straight through.
    // Uses "xxx" (an exact blocklist entry) as its own subdomain label so
    // the word-boundary host split — same alphanumeric-boundary splitting
    // used for title/snippet, not a substring search — actually isolates it
    // as a whole word; a host like "pornhub.example.com" would NOT match
    // ("porn" is on the list, but "pornhub" is a different whole word).
    #[test]
    fn apply_safe_search_filter_strict_blocks_on_url_host() {
        let results = vec![result_with_url(
            "https://xxx.example.com/page",
            "Weekly newsletter",
            "Sign up for updates",
        )];
        let filtered = apply_safe_search_filter(results, 2);
        assert_eq!(
            filtered.len(),
            0,
            "a blocklisted term in the URL host must be caught even with a clean title/snippet"
        );
    }

    // A clean host with a blocklisted title/snippet must
    // still be caught too — the host check is additive, not a replacement
    // for the existing title/snippet check.
    #[test]
    fn apply_safe_search_filter_still_blocks_on_title_when_host_is_clean() {
        let results = vec![result_with_url(
            "https://example.com/page",
            "Escort service listings",
            "Adult content here",
        )];
        let filtered = apply_safe_search_filter(results, 2);
        assert_eq!(filtered.len(), 0);
    }

    // cache_key must be a pure function of the structured request fields —
    // both gateways rely on this to land on the same cache entry for the
    // same logical query (see http.rs / mcp.rs).
    #[test]
    fn cache_key_is_deterministic_and_namespaced() {
        let req = SearchRequest {
            query: "cats".into(),
            page: 1,
            safe_search: 2,
            language: Some("en".into()),
        };
        let key_a = cache_key(&req);
        let key_b = cache_key(&req);
        assert_eq!(key_a, key_b);
        assert!(key_a.starts_with("serica:v1:search:"));
    }

    #[test]
    fn cache_key_does_not_collide_across_delimiter_injection() {
        // Old delimited-string format let a query containing the key's own
        // delimiters (`|`, `:`) collide with a differently-paged/filtered
        // request. Hashing structured fields closes this off.
        let req_a = SearchRequest {
            query: "cats|p:1|s:0|l:".into(),
            page: 1,
            safe_search: 2,
            language: None,
        };
        let req_b = SearchRequest {
            query: "cats".into(),
            page: 1,
            safe_search: 0,
            language: None,
        };
        assert_ne!(cache_key(&req_a), cache_key(&req_b));
    }

    // extract_cache_key must be a pure function of the normalized URL's
    // string form — both gateways rely on this to land on the same cache
    // entry for the same logical target (see http.rs / mcp.rs), and it must
    // never collide with the search cache's namespace now that both share
    // one `CacheLayer`/Redis instance.
    #[test]
    fn extract_cache_key_is_deterministic_and_namespaced() {
        let url = Url::parse("https://example.com/article").unwrap();
        let key_a = extract_cache_key(&url);
        let key_b = extract_cache_key(&url);
        assert_eq!(key_a, key_b);
        assert!(key_a.starts_with("serica:v1:extract:"));
    }

    #[test]
    fn extract_cache_key_differs_across_distinct_urls() {
        let url_a = Url::parse("https://example.com/article-one").unwrap();
        let url_b = Url::parse("https://example.com/article-two").unwrap();
        assert_ne!(extract_cache_key(&url_a), extract_cache_key(&url_b));
    }

    #[test]
    fn extract_cache_key_namespace_never_collides_with_search_key() {
        // Even in the (astronomically unlikely) case the underlying hash
        // values ever coincided, the "serica:v1:search:" vs.
        // "serica:v1:extract:" prefixes guarantee the full keys differ.
        let url = Url::parse("https://example.com/").unwrap();
        let extract_key = extract_cache_key(&url);
        assert!(!extract_key.starts_with("serica:v1:search:"));
    }
}

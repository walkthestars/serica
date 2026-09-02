//! Pure, I/O-free extraction core (extract pipeline plan, Changes 4 and 5):
//! [`extract_from_html`] runs the full patch-and-extract pipeline on
//! already-fetched HTML and applies the fallback cascade that fixes two real,
//! option-immune bugs in `rs-trafilatura` 0.2.2. No I/O, no `async` in that
//! function.
//!
//! The two bugs, both diagnosed by calling `rs_trafilatura::extract_with_options`
//! directly on the literal captured fixtures in `tests/fixtures/`, bypassing
//! this pipeline entirely:
//!
//! - **AUP-class** (`ddg_aup.html`): `content_markdown` silently comes back
//!   empty while `content_text` on the same document succeeds, so the response
//!   carried `content: null` outright.
//! - **terms-class** (`ddg_terms.html`): trafilatura's 30% content-coverage
//!   check rejects long prose split across many sibling `<div>`s, so a
//!   105KB page yielded only an 807-character hero snippet — 9.6% of the
//!   8,409-character whole-document text dump.
//!
//! Neither is reachable through any exposed `Options` knob (`favor_precision`
//! included — 0.2.2's `Default` already sets it `false`, so both pages failed
//! with precision off). The unit tests at the bottom of this file assert the
//! fixed behavior against those same two fixtures.
//!
//! A third failure class lives in *this* cascade's own gating rather than in
//! rs-trafilatura: on very large pages the absolute-size pre-filter floors
//! let a gutted selection through unaudited (RFC-class — one section of a
//! giant single-page spec: fluent, on-topic, and ~1% of the document). The
//! large-page plausibility rule in [`extract_from_html_impl`] (see
//! [`LARGE_PAGE_RAW_BYTES`]) audits those; the `rfc9110_subset.html` fixture
//! pins it.
//!
//! Pipeline order: [`crate::util::prune::prune_scripts_styles`] →
//! [`crate::util::codespan_fixup::preserve_whitespace_only_spans`] →
//! [`crate::util::codeblock_linebreaks::preserve_code_block_line_breaks`] →
//! `rs_trafilatura::extract_with_options` → the fallback cascade below.
//!
//! [`ExtractLimiter`] and [`run_extract`] (extract pipeline plan, Changes 4
//! and 6) are the async layer built on top of the above: cache lookup,
//! SSRF-validated fetch (via `util::ssrf::fetch_extract_target`), a
//! `tokio::time::timeout`-wrapped, semaphore-gated call into
//! `extract_from_html` on the blocking pool, and cache-on-success. This is
//! the single implementation both `gateway::http` and `gateway::mcp` call
//! into.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use url::Url;

use crate::cache::CacheLayer;
use crate::config::{Config, ExtractConfig};
use crate::error::AppError;
// `gateway::extract_cache_key` is `pub(crate)`, so it's reachable crate-wide
// from here despite `util` normally sitting "below" `gateway` — Rust has no
// enforced module layering, and reusing the one cache-key function (rather
// than a second copy) matters more than keeping that direction tidy.
use crate::gateway::extract_cache_key;
use crate::models::extract::{extract_options_from_config, ExtractResponse};
use crate::util::codeblock_linebreaks::preserve_code_block_line_breaks;
use crate::util::codespan_fixup::{
    preserve_whitespace_only_spans, restore_whitespace_placeholders,
};
use crate::util::fetch::{is_extractable_content_type_header, read_capped_body};
use crate::util::prune::{prune_scripts_styles, whole_doc_text};
use crate::util::ssrf::{fetch_extract_target, normalize_extract_url};

/// Below this `chosen` length (chars), the ratio check (step 4 of the
/// cascade) runs at all. At or above it, the extraction is assumed healthy
/// and the expensive whole-doc dump is skipped entirely.
const WHOLE_DOC_CHECK_FLOOR: usize = 3_000;

/// A healthy non-empty markdown extraction (`ExtractMethod::Markdown`)
/// between this floor and [`WHOLE_DOC_CHECK_FLOOR`] is trusted as-is: the
/// step-4 ratio check is skipped for it. The ratio check exists to catch the
/// terms-class failure — trafilatura's node selection gutting a page and
/// returning a small hero snippet — but its denominator is the *whole-page*
/// text, so on a nav-heavy institutional page it also fires on a short,
/// complete article: for `tests/fixtures/crowcog.html` the entire real
/// article is 2,245 chars of perfectly good markdown inside ~19.8k chars of
/// site-wide nav/footer chrome, ratio 0.114 — nearly identical to the terms
/// fixture's 0.096, so no ratio threshold can separate the two. Length can:
/// the known-gutted markdown extract is 807 chars, the known-good one 2,245,
/// a 2.8x gap with nothing observed between, and this floor sits mid-gap.
/// A gutted-but-non-empty markdown extract landing inside that gap would now
/// be served as-is instead of rescued — a consciously traded false-rescue
/// class, to be revisited on real distribution data (n=3 calibration, same
/// caveat as [`RATIO_THRESHOLD`]).
const HEALTHY_MARKDOWN_FLOOR: usize = 1_500;

/// Absolute floor on the whole-doc dump itself: below this many chars, even
/// the full-page text has essentially no real content, so no fallback can
/// rescue the request — content ends up `None`, same as today.
const MIN_FALLBACK_CHARS: usize = 200;

/// `chosen.len() as f64 / whole_doc.len() as f64` below this triggers the
/// whole-doc fallback. Calibrated from two fixtures (terms ≈9.6%, AUP ≈86%)
/// — a wide margin, but n=2; logged per-request by the caller so it can be
/// revisited on real distribution data. The check's whole-page denominator
/// makes it unable to distinguish a correctly-selected short article on a
/// nav-heavy page from a gutted extract (both land ≈0.1) — that case is
/// handled by exempting in-band healthy markdown from this check entirely;
/// see [`HEALTHY_MARKDOWN_FLOOR`].
const RATIO_THRESHOLD: f64 = 0.30;

/// Raw fetched-HTML byte count above which the extract gets a plausibility
/// audit regardless of its absolute size. On giant single-page documents
/// (RFC full-texts, `print.html` book exports) the selector can keep one
/// fluent, on-topic fragment that sails past every absolute floor while
/// representing a few percent of the document — the RFC-class gutting that
/// motivated [`LARGE_PAGE_RATIO_THRESHOLD`].
///
/// Large pages take a two-step rule in [`extract_from_html_impl`]:
///
/// 1. A selection that reached `max_content_chars` is trusted outright —
///    trafilatura was still capturing content when the cap cut it off, so a
///    cap-saturated extract is healthy by construction. This is what keeps
///    whole-book `print.html` extracts (which cap out long before they run
///    dry) from being misread by any ratio against a million-char document.
/// 2. Everything else on a large page is held to
///    [`LARGE_PAGE_RATIO_THRESHOLD`] — deliberately not the band exemption
///    small pages get ([`HEALTHY_MARKDOWN_FLOOR`]): that band protects a
///    short *complete* article buried in site chrome, and a ≥512KB document
///    whose complete payload is ≤3k chars is not a shape that occurs in
///    practice.
///
/// The audit's false-rescue class — a genuinely complete, small-but-over-3k
/// extract on a chrome-heavy ≥512KB page — is consciously traded, narrower
/// than the pre-fix behavior (which audited nothing ≥3k chars at all), and
/// to be revisited on real distribution data. Calibrated on two points:
/// rfc9110_subset.html (5,666 chosen / ~240k whole-doc ≈ 0.024, gutted) and
/// a healthy uncapped large-article extract (50k / ~560k ≈ 0.089); the
/// threshold sits mid-gap. n=2 — revisit on real data.
const LARGE_PAGE_RAW_BYTES: usize = 512 * 1024;

/// Large-page variant of [`RATIO_THRESHOLD`] — see [`LARGE_PAGE_RAW_BYTES`]
/// for the two-step rule and the calibration points either side of it.
const LARGE_PAGE_RATIO_THRESHOLD: f64 = 0.05;

/// Which stage of the fallback cascade ultimately supplied `content`.
///
/// Internal only — not part of the `ExtractResponse` API schema (may become
/// an additive `extraction_method` field later, but that's out of scope for
/// v1). **Only meaningful when `ExtractOutcome::content` is `Some`** — when
/// the cascade ends with `content: None`, `method` carries no information
/// and callers should not interpret it (in particular, the `Err(_)` arm of
/// the cascade sets a placeholder value here).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtractMethod {
    /// `content_markdown` came back healthy from trafilatura.
    Markdown,
    /// Either text mode was requested, or markdown mode was requested but
    /// `content_markdown` came back empty/`None` while `content_text`
    /// succeeded (the AUP-class bug) — both converge here because there is
    /// genuinely no other content to have chosen from in either case.
    TextFallback,
    /// Trafilatura's own content selection recovered too little of the
    /// page (ratio below [`RATIO_THRESHOLD`]); served the whole-document
    /// text dump instead (the terms-class bug).
    WholeDocFallback,
}

/// Result of running [`extract_from_html`]. Carries the same metadata shape
/// `ExtractResponse` needs (title/author/formatted-date/sitename/page_type/
/// excerpt) so a later session can map this directly instead of duplicating
/// the mapping logic per-handler.
#[derive(Debug, Clone)]
pub struct ExtractOutcome {
    /// The final extracted content, already cascade-resolved and capped at
    /// `config.max_content_chars`. `None` only when every stage of the
    /// cascade came up empty (or extraction itself errored).
    pub content: Option<String>,
    /// See the enum's doc comment: only meaningful when `content` is `Some`.
    pub method: ExtractMethod,
    pub title: Option<String>,
    pub author: Option<String>,
    /// Already formatted `"%Y-%m-%d"`, matching `ExtractResponse::date`.
    pub date: Option<String>,
    pub sitename: Option<String>,
    pub page_type: Option<String>,
    /// From `metadata.description`, matching `ExtractResponse::excerpt`.
    pub excerpt: Option<String>,
    /// `chosen.len() / whole_doc.len()`, only `Some` when the ratio check
    /// (cascade step 4) actually ran. This pure function has no URL to log
    /// with — the caller (the async pipeline below) logs this
    /// alongside the URL.
    pub ratio: Option<f64>,
}

struct Metadata {
    title: Option<String>,
    author: Option<String>,
    date: Option<String>,
    sitename: Option<String>,
    page_type: Option<String>,
    excerpt: Option<String>,
}

impl Metadata {
    fn none() -> Self {
        Metadata {
            title: None,
            author: None,
            date: None,
            sitename: None,
            page_type: None,
            excerpt: None,
        }
    }

    fn from_result(extract: &rs_trafilatura::ExtractResult) -> Self {
        Metadata {
            title: extract.metadata.title.clone(),
            author: extract.metadata.author.clone(),
            date: extract
                .metadata
                .date
                .map(|d| d.format("%Y-%m-%d").to_string()),
            sitename: extract.metadata.sitename.clone(),
            page_type: extract.metadata.page_type.clone(),
            excerpt: extract.metadata.description.clone(),
        }
    }
}

/// Truncates `s` to at most `max_chars` bytes, at a valid `char` boundary.
/// `rs_trafilatura`'s own `max_extracted_len` enforcement does a raw
/// byte-index `String::truncate` with no boundary check (`extract.rs:1114-
/// 1120` in the vendored crate) — that's a bug in their code, not ours to
/// fix, but we must not repeat it here: a mid-multi-byte-character
/// byte-index truncate panics.
fn truncate_at_char_boundary(s: String, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s;
    }
    let boundary = s
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|&end| end <= max_bytes)
        .last()
        .unwrap_or(0);
    let mut s = s;
    s.truncate(boundary);
    s
}

/// Runs the full patch-and-extract pipeline plus the fallback cascade on
/// already-fetched HTML. Pure and synchronous — no I/O, no `async`; the
/// caller (a later session) is responsible for fetching `html` and for
/// running this off the async executor (`spawn_blocking`), since trafilatura
/// extraction is CPU-bound.
pub fn extract_from_html(
    html: &str,
    options: &rs_trafilatura::Options,
    config: &ExtractConfig,
) -> ExtractOutcome {
    extract_from_html_impl(html, html.len(), options, config, true)
}

fn extract_from_html_impl(
    html: &str,
    raw_bytes: usize,
    options: &rs_trafilatura::Options,
    config: &ExtractConfig,
    prune: bool,
) -> ExtractOutcome {
    let pruned = if prune {
        prune_scripts_styles(html)
    } else {
        html.to_string()
    };
    let patched = preserve_code_block_line_breaks(&preserve_whitespace_only_spans(&pruned));

    // Step 2: run extraction, resolve (chosen, method, metadata) from the
    // Result per the cascade's exact branching.
    let (mut chosen, mut method, metadata) =
        match rs_trafilatura::extract_with_options(&patched, options) {
            Ok(extract) => {
                let is_healthy_markdown = options.output_markdown
                    && extract
                        .content_markdown
                        .as_deref()
                        .map(|md| !md.trim().is_empty())
                        .unwrap_or(false);
                if is_healthy_markdown {
                    let md = extract.content_markdown.as_deref().unwrap_or_default();
                    let content = restore_whitespace_placeholders(md);
                    let metadata = Metadata::from_result(&extract);
                    (Some(content), ExtractMethod::Markdown, metadata)
                } else {
                    // Either output_format=text (content_markdown is always None
                    // — expected, not the AUP bug), or markdown was requested
                    // but content_markdown came back empty/None while
                    // content_text succeeded (the AUP-class bug). Both converge
                    // on TextFallback: there is genuinely no other content to
                    // have chosen from.
                    let text = restore_whitespace_placeholders(&extract.content_text);
                    let chosen = if text.trim().is_empty() {
                        None
                    } else {
                        Some(text)
                    };
                    let metadata = Metadata::from_result(&extract);
                    (chosen, ExtractMethod::TextFallback, metadata)
                }
            }
            Err(_) => {
                // No ExtractResult to pull metadata from. `method` here is a
                // placeholder — content is None, so per the doc comment on
                // ExtractOutcome, callers must not interpret it.
                (None, ExtractMethod::TextFallback, Metadata::none())
            }
        };

    let mut ratio: Option<f64> = None;

    // Step 3 (cheap pre-filter) + Step 4 (ratio check). The check runs only
    // for `chosen` under [`WHOLE_DOC_CHECK_FLOOR`], and never for a healthy
    // markdown extraction at or above [`HEALTHY_MARKDOWN_FLOOR`]: within
    // that band the markdown output is the extractor's confident, complete
    // selection of a short article, and the whole-page-text denominator of
    // the ratio check would misread a nav-heavy page's site chrome as
    // missing content (see the constant's doc comment for the measured
    // crowcog/terms case).
    //
    // Large pages (raw bytes >= [`LARGE_PAGE_RAW_BYTES`]) get a plausibility
    // audit *in addition*, regardless of chosen length: on giant single-page
    // documents the selector can keep one fluent, on-topic fragment that
    // clears every absolute floor while representing a few percent of the
    // page (the RFC-class gutting — see the rfc9110_subset fixture test).
    // Two carve-outs keep that audit from harming healthy large pages: a
    // cap-saturated extract is trusted outright (trafilatura was still
    // capturing when the cap cut it off — see [`LARGE_PAGE_RAW_BYTES`]),
    // and the audit threshold on large pages is the more lenient
    // [`LARGE_PAGE_RATIO_THRESHOLD`], not [`RATIO_THRESHOLD`].
    let chosen_len = chosen.as_deref().map(str::len).unwrap_or(0);
    let healthy_markdown_in_band =
        method == ExtractMethod::Markdown && chosen_len >= HEALTHY_MARKDOWN_FLOOR;
    let is_large_page = raw_bytes >= LARGE_PAGE_RAW_BYTES;
    let large_page_audit =
        is_large_page && chosen.is_some() && chosen_len < config.max_content_chars;
    let audit_needed =
        (chosen_len < WHOLE_DOC_CHECK_FLOOR && !healthy_markdown_in_band) || large_page_audit;
    if audit_needed {
        let whole_doc = whole_doc_text(html);
        if !whole_doc.is_empty() {
            let computed_ratio = chosen_len as f64 / whole_doc.len() as f64;
            ratio = Some(computed_ratio);
            let threshold = if is_large_page {
                LARGE_PAGE_RATIO_THRESHOLD
            } else {
                RATIO_THRESHOLD
            };
            if computed_ratio < threshold {
                if whole_doc.len() >= MIN_FALLBACK_CHARS {
                    chosen = Some(whole_doc);
                    method = ExtractMethod::WholeDocFallback;
                } else {
                    chosen = None;
                }
            }
            // else: ratio >= threshold — keep `chosen`/`method` as-is even
            // though it's under the floor (the AUP fixture's case).
        }
        // else: whole_doc empty — leave ratio as None, chosen/method
        // untouched.
    }

    // Step 5: cap at config.max_content_chars, at a valid char boundary.
    // No-op in practice for Markdown/TextFallback (already capped by the
    // library's own max_extracted_len, which max_content_chars sets via
    // extract_options_from_config) — this is what makes WholeDocFallback
    // safe without a separate special-case.
    let chosen = chosen.map(|c| truncate_at_char_boundary(c, config.max_content_chars));

    ExtractOutcome {
        content: chosen,
        method,
        title: metadata.title,
        author: metadata.author,
        date: metadata.date,
        sitename: metadata.sitename,
        page_type: metadata.page_type,
        excerpt: metadata.excerpt,
        ratio,
    }
}

/// Bounds how many extract requests may run their fetch+parse span
/// concurrently (extract pipeline plan, Change 6). Constructed once in
/// `main.rs` with `config.server.max_concurrent_extracts` permits, then
/// cloned into both `HttpGateway`/`AppState` and `McpGateway` — cheap, since
/// this is just an `Arc<Semaphore>` clone.
#[derive(Clone)]
pub struct ExtractLimiter(Arc<Semaphore>);

impl ExtractLimiter {
    pub fn new(max_concurrent: usize) -> Self {
        Self(Arc::new(Semaphore::new(max_concurrent)))
    }
}

/// Shared async extract pipeline: cache
/// lookup, SSRF-validated fetch, capped body read, the pure
/// [`extract_from_html`] above, and cache-on-success — the single
/// implementation both `gateway::http` and `gateway::mcp` call into, so
/// there is exactly one copy of that ~120-line handler flow, and one real
/// timer behind `timing_ms` for both gateways.
///
/// Takes `url: &Url`, not `&str`, and has no `client: &reqwest::Client`
/// parameter — both deliberate design choices:
/// - URL parsing stays in each handler because the two gateways report a
///   parse failure through genuinely different error shapes (an HTTP 400 vs.
///   a JSON-RPC `-32602` with a `{"field": "url", ...}` payload), so
///   `run_extract` only ever receives an already-validated `Url`.
/// - There is no shared `reqwest::Client` parameter because
///   `fetch_extract_target` builds a fresh, SSRF-pinned client per redirect
///   hop (`.resolve(host, addr)` pins one client to one validated address) —
///   a single client handed in from outside couldn't work across hops that
///   resolve to different addresses, so `run_extract` calls
///   `fetch_extract_target` exactly as both handlers already did.
pub async fn run_extract(
    config: &Config,
    cache: &Arc<dyn CacheLayer>,
    limiter: &ExtractLimiter,
    url: &Url,
) -> Result<ExtractResponse, AppError> {
    let normalized = normalize_extract_url(url);
    let key = extract_cache_key(&normalized);

    // Cache-hit latency is measured from here: `timing_ms` must report THIS
    // request's cost, not replay the original fetch's timing that was
    // frozen into the cached payload. A hit
    // does no I/O, so this lands at single-digit milliseconds.
    let start = std::time::Instant::now();

    // Cache lookup happens BEFORE the timeout wrapper below — a cache hit
    // does no I/O and has nothing to time out on.
    if let Some(cached_bytes) = cache.get(&key).await {
        match serde_json::from_slice::<ExtractResponse>(&cached_bytes) {
            Ok(mut response) => {
                response.cached = true;
                response.timing_ms = start.elapsed().as_millis() as u64;
                return Ok(response);
            }
            Err(e) => {
                tracing::error!(key = %key, error = %e, "Failed to deserialize cached extract response");
            }
        }
    }

    let timeout_ms = config.extract.timeout_ms;

    // The permit is acquired INSIDE the timeout, not outside:
    // a request queued on the semaphore must
    // count against its own timeout budget, not get a free pass while
    // waiting for a slot. On the MCP path specifically, an extract now
    // passes through two nested gates by the time it gets here: this
    // `ExtractLimiter` and the pre-existing, coarser `mcp.rs`
    // `Semaphore::new(32)` that bounds all in-flight MCP tool calls (see
    // `McpGateway::run_stdio`) — the outer one bounds total concurrent tool
    // calls of any kind, this inner one specifically bounds extract's
    // fetch+parse span.
    let attempt = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let _permit = limiter
            .0
            .acquire()
            .await
            .expect("semaphore is never closed");

        let (response, final_url) =
            fetch_extract_target(url, config.proxy.extract_url.as_deref()).await?;
        let url_string = final_url.to_string();

        let status = response.status();
        if !status.is_success() {
            return Err(AppError::ServiceUnavailable(format!(
                "URL returned HTTP {status}"
            )));
        }

        let content_type_header = response.headers().get("content-type");
        if !is_extractable_content_type_header(content_type_header) {
            let ct_display = content_type_header
                .and_then(|ct| ct.to_str().ok())
                .unwrap_or("<missing or unparseable>");
            return Err(AppError::InvalidQuery(format!(
                "URL returned Content-Type '{ct_display}', expected text/html"
            )));
        }

        let body = read_capped_body(response, config.extract.max_raw_bytes)
            .await
            .map_err(|e| {
                tracing::warn!(url = %url_string, error = %e, "Failed to read response body");
                e
            })?;

        let options = extract_options_from_config(config);
        let extract_config = config.extract.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            extract_from_html(&body, &options, &extract_config)
        })
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "Extraction task panicked");
            AppError::Internal("Extraction task panicked".into())
        })?;

        Ok::<_, AppError>((url_string, outcome))
    })
    .await;

    let (url_string, outcome) = match attempt {
        Ok(inner) => inner?,
        Err(_elapsed) => {
            return Err(AppError::ExtractTimeout(format!(
                "extract exceeded {timeout_ms}ms"
            )));
        }
    };

    // Change 5 step 5: the pure `extract_from_html` core has no URL to log
    // ratio against — this async layer is the one caller with both in hand.
    if let Some(ratio) = outcome.ratio {
        tracing::debug!(
            url = %url_string,
            method = ?outcome.method,
            ratio,
            "extract fallback-cascade ratio check"
        );
    }

    let timing_ms = start.elapsed().as_millis() as u64;
    let response = ExtractResponse {
        url: url_string,
        title: outcome.title,
        author: outcome.author,
        date: outcome.date,
        sitename: outcome.sitename,
        page_type: outcome.page_type,
        content: outcome.content,
        excerpt: outcome.excerpt,
        timing_ms,
        cached: false,
    };

    // Only successful extracts are cached — errors (including
    // `ContentTooLarge`/`ExtractTimeout`) return early above via `?` and
    // never reach this line, so this single code path already satisfies the
    // "timeouts and content-too-large follow the same non-cache path as
    // every other error" rule without any extra special-casing.
    let ttl = Duration::from_secs(config.cache.extract_ttl_seconds);
    match serde_json::to_vec(&response) {
        Ok(bytes) => cache.set(&key, &bytes, ttl).await,
        Err(e) => {
            tracing::error!(key = %key, error = %e, "Failed to serialize extract response for caching");
        }
    }

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::models::extract::extract_options_from_config;

    const TERMS: &str = include_str!("../../tests/fixtures/ddg_terms.html");
    const AUP: &str = include_str!("../../tests/fixtures/ddg_aup.html");
    const CROWCOG: &str = include_str!("../../tests/fixtures/crowcog.html");

    // First 540KB of the live RFC 9110 single-page HTML,
    // truncated at a tag boundary and re-closed. Large enough to trip the
    // large-page rule; the vendored selector keeps only section 6.4 (~5.7k
    // chars out of ~240k chars of page text) — the RFC-class gutting
    // signature, captured deterministically.
    const RFC_SUBSET: &str = include_str!("../../tests/fixtures/rfc9110_subset.html");

    /// Test-only twin of `extract_from_html` with pruning disabled, used
    /// solely by the equivalence test below to prove `prune_scripts_styles`
    /// doesn't change trafilatura's node-selection output on real pages
    /// (extract pipeline plan, Change 3: a size/perf optimization, NOT a
    /// correctness fix — trafilatura already ignores script/style/noscript
    /// content for content candidates).
    #[cfg(test)]
    fn extract_from_html_no_prune_for_equivalence_test(
        html: &str,
        options: &rs_trafilatura::Options,
        config: &ExtractConfig,
    ) -> ExtractOutcome {
        extract_from_html_impl(html, html.len(), options, config, false)
    }

    // These assert the *fixed* behavior. The pre-fix numbers they are measured
    // against (807 chars of 8,409 for terms; empty markdown for AUP) are in
    // this module's doc comment, reproducible at any time by calling
    // `rs_trafilatura::extract_with_options` directly on the same two
    // fixtures. The throwaway harness that originally produced them was
    // deleted once the fix landed — the fixtures are the durable evidence,
    // not the harness.

    #[test]
    fn terms_fixture_recovers_via_whole_doc_fallback() {
        let config = Config::default();
        let options = extract_options_from_config(&config);
        let outcome = extract_from_html(TERMS, &options, &config.extract);

        let content = outcome.content.expect("terms fixture must recover content");
        assert!(
            content.contains("warrant"),
            "expected terms body content, got: {}",
            &content[..content.len().min(300)]
        );
        assert!(
            content.len() > 2_000,
            "expected recovered terms body to exceed 2,000 chars, got {}",
            content.len()
        );
        assert_eq!(outcome.method, ExtractMethod::WholeDocFallback);
    }

    #[test]
    fn aup_fixture_stays_on_text_fallback_not_downgraded() {
        let config = Config::default();
        let options = extract_options_from_config(&config); // default output_format = markdown
        let outcome = extract_from_html(AUP, &options, &config.extract);

        let content = outcome.content.expect("AUP fixture must recover content");
        assert!(
            content.contains("Sell or resell"),
            "expected AUP body content, got: {}",
            &content[..content.len().min(300)]
        );
        assert_eq!(
            outcome.method,
            ExtractMethod::TextFallback,
            "AUP fixture's ~86% ratio must keep it on TextFallback, not downgrade to WholeDocFallback"
        );
    }

    #[test]
    fn equivalence_prune_on_vs_off_is_byte_identical() {
        let config = Config::default();
        let options = extract_options_from_config(&config);

        for (name, html) in [("terms", TERMS), ("aup", AUP)] {
            let pruned = extract_from_html(html, &options, &config.extract);
            let unpruned =
                extract_from_html_no_prune_for_equivalence_test(html, &options, &config.extract);
            assert_eq!(
                pruned.content, unpruned.content,
                "prune on vs off diverged on {name} fixture — this is a prune bug"
            );
            assert_eq!(
                pruned.method, unpruned.method,
                "method diverged on {name} fixture"
            );
        }
    }

    #[test]
    fn short_document_with_no_rescue_yields_none() {
        // A page with no article-like content at all (just a nav — no
        // <article>/<p> structure trafilatura's node selection treats as
        // main content). content_text ends up empty (chosen = None after
        // step 2), and the whole-doc dump of the same tiny document is also
        // under MIN_FALLBACK_CHARS — no fallback can rescue it, content must
        // end up None (same as today).
        let html = "<html><body><nav>Home About</nav></body></html>";
        let config = Config::default();
        let options = extract_options_from_config(&config);
        let outcome = extract_from_html(html, &options, &config.extract);
        assert!(
            outcome.content.is_none(),
            "expected None for a document with no rescuable content, got: {:?}",
            outcome.content
        );
    }

    #[test]
    fn ratio_at_or_above_threshold_is_not_downgraded() {
        // Synthetic sibling to the AUP-fixture check: `chosen` is under the
        // 3,000-char pre-filter floor, but its ratio against the whole-doc
        // text is deliberately kept >= 30% so it must NOT be downgraded to
        // WholeDocFallback.
        //
        // Use text mode so `chosen` == content_text directly and the
        // synthetic ratio is easy to control: the article body below is the
        // near-entirety of the visible text, so chosen/whole_doc is high.
        let mut config = Config::default();
        config.extract.output_format = "text".into();
        let options = extract_options_from_config(&config);

        let body = "Short article body with enough real prose to read naturally. ".repeat(15); // ~1,900 chars, under the 3,000 floor
        let html = format!(
            "<html><head><title>T</title></head><body><article><h1>Title</h1><p>{body}</p></article></body></html>"
        );

        let outcome = extract_from_html(&html, &options, &config.extract);
        let content = outcome.content.expect("healthy short article must extract");
        assert!(
            content.len() < WHOLE_DOC_CHECK_FLOOR,
            "test setup invariant: chosen content must be under the pre-filter floor, got {}",
            content.len()
        );
        let ratio = outcome
            .ratio
            .expect("ratio check must have run for content under the floor");
        assert!(
            ratio >= RATIO_THRESHOLD,
            "test setup invariant: ratio must be >= threshold, got {ratio}"
        );
        assert_eq!(
            outcome.method,
            ExtractMethod::TextFallback,
            "high-ratio short content under the floor must not be downgraded to WholeDocFallback"
        );
    }

    #[test]
    fn text_output_format_healthy_article_skips_whole_doc_fallback() {
        // output_format=text: content_markdown is always None because it
        // was never requested — must NOT be mistaken for the AUP bug. A
        // healthy long article should land on TextFallback cheaply, never
        // touching the whole-doc-fallback path.
        let mut config = Config::default();
        config.extract.output_format = "text".into();
        let options = extract_options_from_config(&config);

        let paragraph = "word ".repeat(700); // well over the 3,000-char floor
        let html = format!(
            "<html><head><title>Long Article</title></head><body><article><h1>Hello</h1><p>{paragraph}</p></article></body></html>"
        );

        let outcome = extract_from_html(&html, &options, &config.extract);
        let content = outcome.content.expect("healthy long article must extract");
        assert!(
            content.len() > 1_000,
            "expected substantial content, got {}",
            content.len()
        );
        assert_eq!(outcome.method, ExtractMethod::TextFallback);
        assert!(
            outcome.ratio.is_none(),
            "healthy content over WHOLE_DOC_CHECK_FLOOR must skip the ratio check entirely, got ratio={:?}",
            outcome.ratio
        );
    }

    #[test]
    fn markdown_healthy_article_skips_whole_doc_fallback() {
        // Sibling to the text-mode test above, confirming the same
        // pre-filter skip holds for markdown mode's healthy path.
        let config = Config::default(); // default output_format = markdown
        let options = extract_options_from_config(&config);

        let paragraph = "word ".repeat(700);
        let html = format!(
            "<html><head><title>Long Article</title></head><body><article><h1>Hello</h1><p>{paragraph}</p></article></body></html>"
        );

        let outcome = extract_from_html(&html, &options, &config.extract);
        assert!(outcome.content.is_some());
        assert_eq!(outcome.method, ExtractMethod::Markdown);
        assert!(outcome.ratio.is_none());
    }

    #[test]
    fn healthy_short_markdown_not_downgraded_on_nav_heavy_page() {
        // The third calibration shape, after ddg_terms (gutted-but-non-empty
        // extract on a mostly-article page) and ddg_aup (empty markdown,
        // healthy text). A nav-heavy institutional page (Max Planck
        // CrowCoG): the *entire* real article is 2,245 chars of healthy
        // markdown inside ~19.8k chars of site-wide nav/footer chrome, so
        // its chosen/whole-doc ratio (0.114) is nearly identical to terms'
        // (0.096) — no ratio threshold can separate a correctly-selected
        // short article from a gutted one. What separates them is length:
        // terms' known-gutted extract is 807 chars, this page's known-good
        // one is 2,245. The `HEALTHY_MARKDOWN_FLOOR` band sits mid-gap, so
        // this page's healthy short markdown must be served as-is — kept on
        // `ExtractMethod::Markdown`, no ratio check run, no whole-doc
        // boilerplate downgrade (which would produce a 19.7k-char
        // nav/footer text dump for this exact fixture).
        let config = Config::default(); // default output_format = markdown
        let options = extract_options_from_config(&config);

        let outcome = extract_from_html(CROWCOG, &options, &config.extract);

        let content = outcome
            .content
            .expect("crowcog's healthy short markdown extract must survive the cascade");
        assert!(
            content.contains("Crow Cognition Group (CrowCoG)"),
            "expected the real article body, got: {}",
            &content[..content.len().min(300)]
        );
        assert!(
            !content.contains("Jump directly to main navigation"),
            "whole-doc boilerplate leaked into content — downgrade happened"
        );
        assert_eq!(
            outcome.method,
            ExtractMethod::Markdown,
            "healthy short markdown must not be downgraded to WholeDocFallback"
        );
        assert!(
            outcome.ratio.is_none(),
            "markdown in the HEALTHY_MARKDOWN_FLOOR band must skip the ratio check, got {:?}",
            outcome.ratio
        );
        assert!(
            content.len() >= HEALTHY_MARKDOWN_FLOOR && content.len() < WHOLE_DOC_CHECK_FLOOR,
            "test setup invariant: extracted markdown must fall inside the trusted band, \
             got {} chars (floor {HEALTHY_MARKDOWN_FLOOR}, check floor {WHOLE_DOC_CHECK_FLOOR})",
            content.len()
        );
    }

    #[test]
    fn truncate_at_char_boundary_does_not_panic_on_multibyte() {
        // A multi-byte char ('é', 2 bytes in UTF-8) straddling the byte cap
        // must not panic — the naive String::truncate(N) would.
        let s = "a".repeat(9) + "é"; // 9 ascii bytes + 2-byte char = 11 bytes total
        let truncated = truncate_at_char_boundary(s, 10); // cap lands mid-char
        assert_eq!(truncated, "a".repeat(9));
    }

    // Change 6: `ExtractLimiter` itself has no network dependency to test
    // against — `run_extract`'s live-fetch behavior is exercised by the
    // gateway tests — but
    // the semaphore-wrapping itself is cheaply, deterministically testable
    // without any I/O: a single-permit limiter must have no permit left
    // while one is held, and must have one available again once released.
    #[test]
    fn extract_limiter_single_permit_is_exclusive() {
        let limiter = ExtractLimiter::new(1);
        let first = limiter
            .0
            .try_acquire()
            .expect("a fresh single-permit limiter must have a permit available");
        assert!(
            limiter.0.try_acquire().is_err(),
            "a second try_acquire must fail while the only permit is held"
        );
        drop(first);
        assert!(
            limiter.0.try_acquire().is_ok(),
            "releasing the held permit must make one available again"
        );
    }

    #[test]
    fn extract_limiter_allows_configured_concurrency() {
        let limiter = ExtractLimiter::new(2);
        let _first = limiter.0.try_acquire().expect("permit 1 of 2");
        let _second = limiter.0.try_acquire().expect("permit 2 of 2");
        assert!(
            limiter.0.try_acquire().is_err(),
            "a third try_acquire must fail once both configured permits are held"
        );
    }

    #[test]
    fn large_gutted_page_is_rescued_by_whole_doc_fallback() {
        // RFC-class gutting on a large page: the selector keeps one fluent,
        // on-topic section (~5.7k chars) out of ~240k chars of document text.
        // Above every absolute floor, so the large-page plausibility rule is
        // the only thing standing between callers and a confident 1% extract.
        let config = Config::default();
        let options = extract_options_from_config(&config);
        let outcome = extract_from_html(RFC_SUBSET, &options, &config.extract);

        let content = outcome
            .content
            .expect("large gutted page must be rescued by the whole-doc fallback");
        // The rescue serves the whole-document text, which the final
        // max_content_chars cap then truncates — filling the cap is the
        // observable proof that the bulk of the document, not the gutted
        // 5.7k selection, is being served.
        assert_eq!(
            content.len(),
            config.extract.max_content_chars,
            "rescued content must fill the output cap with whole-doc text, got {} chars",
            content.len()
        );
        assert!(
            content.contains("Hypertext Transfer Protocol"),
            "the rescued whole-doc dump must include text beyond the section \
             the gutted selection kept"
        );
        assert_eq!(outcome.method, ExtractMethod::WholeDocFallback);
        assert!(
            outcome.ratio.is_some(),
            "the ratio check must have run for the large gutted page"
        );
    }

    #[test]
    fn large_page_with_healthy_proportional_extract_is_not_downgraded() {
        // print.html-class: a genuinely large, healthy selection. ~560k bytes
        // of real article content (70 paragraphs of ~8k chars each) inside
        // modest chrome, so the chosen/whole-doc ratio stays high and the
        // extract must be trusted: Markdown method, no ratio check, no
        // boilerplate downgrade.
        let paragraph = format!("<p>{}</p>", "word ".repeat(1_600)); // ~8k chars
        let body = paragraph.repeat(70); // ~560k chars of article content
        let html = format!(
            "<html><head><title>Big Doc</title></head><body>\
             <nav><ul><li>Home</li><li>About</li></ul></nav>\
             <article><h1>Big Document</h1>{body}</article>\
             </body></html>"
        );
        assert!(
            html.len() > LARGE_PAGE_RAW_BYTES,
            "test setup invariant: fixture must exceed the large-page raw-byte threshold"
        );

        let config = Config::default();
        let options = extract_options_from_config(&config);
        let outcome = extract_from_html(&html, &options, &config.extract);

        let content = outcome
            .content
            .expect("healthy large-page extract must survive");
        assert_eq!(
            outcome.method,
            ExtractMethod::Markdown,
            "proportional extract on a large page must not be downgraded"
        );
        assert!(
            outcome.ratio.is_none(),
            "proportional extract must skip the ratio check entirely, got {:?}",
            outcome.ratio
        );
        assert!(
            content.len() >= 40_000,
            "expected the bulk of the healthy article (capped at {}), got {} chars",
            config.extract.max_content_chars,
            content.len()
        );
    }

    // An extract cache hit must report THIS
    // request's latency in `timing_ms`, not replay the timing frozen into
    // the cached payload when the page was first fetched. The canned entry
    // carries an absurd 1_000_000 ms so replaying it verbatim cannot pass
    // by luck of a fast machine. The http:// URL is
    // the same trick `test_tools_call_extract_cache_hit_skips_fetch` (see
    // the gateway tests) uses: if the cache lookup ever missed, SSRF
    // validation would reject the scheme loudly instead of a cached success
    // appearing, so this cannot silently pass through the fetch path.
    #[tokio::test]
    async fn extract_cache_hit_remeasures_timing_ms() {
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
            async fn set(&self, _key: &str, _value: &[u8], _ttl: Duration) {}
        }

        let url = url::Url::parse("http://example.com/cached-timing").unwrap();
        let key = extract_cache_key(&normalize_extract_url(&url));

        let canned = ExtractResponse {
            url: url.as_str().to_string(),
            title: Some("Cached Title".into()),
            author: None,
            date: None,
            sitename: None,
            page_type: None,
            content: Some("Cached content".into()),
            excerpt: None,
            timing_ms: 1_000_000,
            cached: false,
        };
        let bytes = serde_json::to_vec(&canned).unwrap();
        let cache: Arc<dyn CacheLayer> = Arc::new(CannedExtractCache { key, bytes });

        let config = Config::default();
        let limiter = ExtractLimiter::new(2);

        let response = run_extract(&config, &cache, &limiter, &url)
            .await
            .expect("cache hit must succeed without any network I/O");
        assert!(response.cached, "handler must report this as a cache hit");
        assert!(
            response.timing_ms < 1_000_000,
            "cache hit must re-measure timing_ms, not replay the canned 1_000_000 ms (got {})",
            response.timing_ms
        );
        assert!(
            response.timing_ms < 5_000,
            "a no-I/O cache hit should complete in well under five seconds even on a loaded CI runner, got {} ms",
            response.timing_ms
        );
    }
}

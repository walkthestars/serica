// Shared response-reading helpers for the extract feature (src/gateway/http.rs
// and src/gateway/mcp.rs). Both gateways fetch a caller-supplied URL and need
// the same content-type allowlist and size cap so they cannot drift out of
// sync (see util::ssrf for the same rationale applied to target validation).

use futures_util::StreamExt;
use reqwest::header::HeaderValue;
use reqwest::Response;

use crate::error::AppError;

/// Security limit, not a parse-correctness limit.
/// `read_capped_body` buffers the entire response body in RAM
/// before extraction, and the *caller* controls the multiplier simply by
/// choosing large URLs — an uncapped (or loosely capped) limit is a
/// memory-exhaustion DoS surface (slow-drip responses included, since the
/// running per-chunk check below is what actually bounds a stream that never
/// ends). Its size is not meant to keep `rs-trafilatura`'s parse cost down —
/// `src/util/prune.rs`'s `prune_scripts_styles` strips script/style/
/// noscript content before extraction runs, so a large real page (multi-MB
/// Next.js/SPA markup) is cheap to parse regardless of raw byte count. This
/// is purely a buffer-size ceiling; the actual operative value is
/// `config.extract.max_raw_bytes` (default 4MB, validated 64KB..=16MB — see
/// `Config::validate()`), with this constant kept only as that default's
/// single source of truth (see `ExtractConfig::default()` in `config.rs`).
pub const DEFAULT_MAX_EXTRACT_BYTES: usize = 4 * 1024 * 1024;

/// True if `content_type` is safe to buffer and hand to the HTML extractor.
///
/// Deliberately narrow: only actual HTML. `text/plain` and `application/xml`
/// are not accepted, because both can carry large non-article payloads
/// (logs, sitemaps, RSS dumps) the HTML extractor has no business
/// processing.
pub fn is_extractable_content_type(content_type: &str) -> bool {
    let ct_lower = content_type.to_lowercase();
    ct_lower.contains("text/html") || ct_lower.contains("application/xhtml")
}

/// Same check as [`is_extractable_content_type`], applied directly to a
/// response's raw `Content-Type` header value (or its absence).
///
/// Fails closed: a missing header, or one that isn't valid
/// `to_str()`-able text, is rejected rather than let through. Both gateways
/// call this single function so the fail-closed behavior can't drift out of
/// sync between them (see module doc).
pub fn is_extractable_content_type_header(content_type: Option<&HeaderValue>) -> bool {
    content_type
        .and_then(|ct| ct.to_str().ok())
        .map(is_extractable_content_type)
        .unwrap_or(false)
}

/// Reads `response`'s body, aborting as soon as more than `max_bytes` have
/// been buffered. Content-Length is checked first as a cheap pre-check, but
/// it cannot be relied on alone: it's absent for chunked responses and, if
/// the client ever decompresses transparently, it reflects the compressed
/// size on the wire rather than what actually lands in `buf`. The running
/// total is checked after every chunk — not after the stream ends — so a
/// stream that never ends (or only ends after gigabytes) is still bounded.
pub async fn read_capped_body(response: Response, max_bytes: usize) -> Result<String, AppError> {
    if let Some(len) = response.content_length() {
        if len as usize > max_bytes {
            return Err(AppError::ContentTooLarge(format!(
                "Response exceeds maximum size of {max_bytes} bytes"
            )));
        }
    }

    let mut buf: Vec<u8> = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|e| AppError::Internal(format!("Failed to read response body: {e}")))?;
        buf.extend_from_slice(&chunk);
        if buf.len() > max_bytes {
            return Err(AppError::ContentTooLarge(format!(
                "Response exceeds maximum size of {max_bytes} bytes"
            )));
        }
    }

    Ok(String::from_utf8_lossy(&buf).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // `tests/api_test.rs::extract_non_html_content_type_returns_400` is an
    // end-to-end check against `httpbin.org`'s fixed `image/png` path; it
    // exercises the real HTTP flow but depends on a third-party server's
    // behavior. These tests exercise the decision function directly, with
    // no network involved. (The end-to-end HTTP-level version of this check
    // can't be converted to a `wiremock` mock server the way the search
    // engine tests were: `/api/v1/extract` validates its target through
    // `util::ssrf::validate_extract_target`, which requires `https://` and
    // rejects any host that resolves to a loopback/private address — exactly
    // what a local `wiremock::MockServer` is. See that test's `#[ignore]`
    // comment.)
    #[test]
    fn accepts_text_html() {
        assert!(is_extractable_content_type("text/html"));
    }

    #[test]
    fn accepts_text_html_with_charset_param() {
        // The real-world common case: servers almost always send a charset.
        assert!(is_extractable_content_type("text/html; charset=utf-8"));
    }

    // text/plain can carry large non-article payloads (logs, dumps)
    // the HTML extractor has no business processing, so it is rejected.
    #[test]
    fn rejects_text_plain() {
        assert!(!is_extractable_content_type("text/plain; charset=utf-8"));
    }

    #[test]
    fn accepts_application_xhtml() {
        assert!(is_extractable_content_type(
            "application/xhtml+xml; charset=utf-8"
        ));
    }

    // application/xml can carry large non-article payloads (sitemaps,
    // RSS dumps) the HTML extractor has no business processing; only
    // text/html and application/xhtml+xml are accepted.
    #[test]
    fn rejects_application_xml() {
        assert!(!is_extractable_content_type("application/xml"));
    }

    #[test]
    fn is_case_insensitive() {
        assert!(is_extractable_content_type("TEXT/HTML; Charset=UTF-8"));
    }

    // This is the exact case the tautological integration test was trying
    // (and failing) to exercise: a binary image response must be rejected.
    #[test]
    fn rejects_image_png() {
        assert!(!is_extractable_content_type("image/png"));
    }

    #[test]
    fn rejects_application_json() {
        assert!(!is_extractable_content_type("application/json"));
    }

    #[test]
    fn rejects_application_octet_stream() {
        assert!(!is_extractable_content_type("application/octet-stream"));
    }

    #[test]
    fn rejects_application_pdf() {
        assert!(!is_extractable_content_type("application/pdf"));
    }

    // A response with no Content-Type header at all must be rejected,
    // not waved through. This is the shared logic both gateways call.
    #[test]
    fn rejects_missing_content_type() {
        assert!(!is_extractable_content_type_header(None));
    }

    // A header value that can't be interpreted as text (e.g. opaque
    // non-ASCII bytes) must also be rejected, not silently skipped.
    #[test]
    fn rejects_unparseable_content_type() {
        let raw =
            HeaderValue::from_bytes(&[0xFF, 0xFE]).expect("opaque bytes are a valid HeaderValue");
        assert!(!is_extractable_content_type_header(Some(&raw)));
    }

    #[test]
    fn header_variant_accepts_text_html() {
        let raw = HeaderValue::from_static("text/html; charset=utf-8");
        assert!(is_extractable_content_type_header(Some(&raw)));
    }

    // reqwest's "gzip"/"brotli" features make decompression transparent,
    // which flips `Content-Length` from a
    // reliable pre-check into something that can't even be read most of the
    // time (see the assertion below) — the running per-chunk check in the
    // loop is what actually has to hold the line. This proves it does, using
    // a classic decompression-bomb shape: a few KB of gzip-compressed
    // repeated bytes that unpack to several MB, served with a
    // `Content-Encoding: gzip` header and no way for the pre-check to see
    // the true size coming.
    #[tokio::test]
    async fn read_capped_body_rejects_decompression_bomb() {
        use std::io::Write;
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let decompressed_size = DEFAULT_MAX_EXTRACT_BYTES * 10;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder
            .write_all(&vec![b'a'; decompressed_size])
            .expect("writing to an in-memory GzEncoder cannot fail");
        let compressed = encoder
            .finish()
            .expect("finishing an in-memory gzip stream cannot fail");
        assert!(
            compressed.len() < DEFAULT_MAX_EXTRACT_BYTES,
            "the compressed payload must itself be small — that's the whole point of a \
             decompression-bomb shape; got {} bytes",
            compressed.len()
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Encoding", "gzip")
                    .set_body_raw(compressed, "text/html"),
            )
            .mount(&server)
            .await;

        // Plain client: reqwest's `gzip` Cargo feature makes transparent
        // decompression the default with no opt-in needed here, matching the
        // real extract client built by `util::ssrf::build_pinned_extract_client`,
        // which never calls `.no_gzip()`/`.no_brotli()`.
        let client = reqwest::Client::new();
        let response = client
            .get(server.uri())
            .send()
            .await
            .expect("request to local mock server should succeed");

        // Confirms the premise: once the client is transparently decoding,
        // reqwest cannot report an exact content length at all (see
        // `Response::content_length`'s own doc comment — "the response is
        // gzipped and automatically decoded, thus changing the actual
        // decoded length"). So `read_capped_body`'s pre-check is skipped
        // entirely here; if the cap held, it's the per-chunk check that did it.
        assert!(
            response.content_length().is_none(),
            "content_length() should be unknown once transparent gzip decoding is in play"
        );

        let result = read_capped_body(response, DEFAULT_MAX_EXTRACT_BYTES).await;
        assert!(
            result.is_err(),
            "a decompression bomb (tiny compressed body, {}x DEFAULT_MAX_EXTRACT_BYTES \
             decompressed) must still be rejected by the running per-chunk check",
            decompressed_size / DEFAULT_MAX_EXTRACT_BYTES
        );
        // Extract pipeline plan, Change 1: an oversized body must be
        // rejected as `ContentTooLarge` (413), not `InvalidQuery` (400) —
        // "your request produced too much content" is a distinct failure
        // mode from "your request was malformed".
        assert!(matches!(
            result.unwrap_err(),
            AppError::ContentTooLarge(msg) if msg.contains("maximum size")
        ));
    }
}

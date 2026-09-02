// lol_html pre-prune pass (extract pipeline plan, Change 3): a single
// streaming tokenizer pass over raw HTML that strips `<script>`, `<style>`
// and `<noscript>` before the html5ever-based rs-trafilatura parse runs.
// lol_html is a tolerant streaming tokenizer (the same engine behind
// Cloudflare's HTMLRewriter) — malformed input cannot corrupt its output,
// so this is fail-safe by construction, not by careful handling here.
//
// Also home to `whole_doc_text`, the document-level text dump used by the
// WholeDocFallback path (Change 5): prune, then collect every remaining
// text node, entity-decode it, and coalesce whitespace.

/// Removes every `<script>`, `<style>` and `<noscript>` element (tags and
/// their content) from `html` in a single streaming pass. Never panics: a
/// tolerant tokenizer has no "malformed input" error case to propagate.
pub fn prune_scripts_styles(html: &str) -> String {
    lol_html::rewrite_str(
        html,
        lol_html::RewriteStrSettings::new().append_element_content_handler(lol_html::element!(
            "script, style, noscript",
            |el| {
                el.remove();
                Ok(())
            }
        )),
    )
    .expect("lol_html rewrite of malformed HTML cannot fail")
}

/// Dumps every remaining text node after [`prune_scripts_styles`], decodes
/// HTML entities (lol_html's text handler hands back raw text, so
/// `DuckDuckGo&#x27;s` would otherwise stay literal), and coalesces
/// whitespace down to single spaces. This is the exact content the
/// WholeDocFallback path serves when trafilatura's node selection rejects
/// too much of a page (extract pipeline plan, Change 5).
pub fn whole_doc_text(html: &str) -> String {
    let pruned = prune_scripts_styles(html);
    let mut out = String::new();
    lol_html::rewrite_str(
        &pruned,
        lol_html::RewriteStrSettings::new().append_document_content_handler(
            lol_html::DocumentContentHandlers::default().text(
                |t: &mut lol_html::html_content::TextChunk| {
                    out.push_str(t.as_str());
                    out.push(' ');
                    Ok(())
                },
            ),
        ),
    )
    .expect("lol_html rewrite of malformed HTML cannot fail");
    let decoded = html_escape::decode_html_entities(&out);
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn script_closing_tag_lookalike_in_string_ends_element_cleanly() {
        // Per the HTML5 spec, `<script>` is a "raw text" element: the
        // tokenizer looks for the literal byte sequence `</script`
        // regardless of JS syntax context, so a `</script>` inside a JS
        // string literal DOES end the element early — this is universal
        // HTML-parser behavior (the reason real-world JS must escape it as
        // `<\/script>`), not a bug specific to a naive/regex-based remover.
        // What this test actually guards: lol_html handles that early close
        // *cleanly* — no broken/partial tag fragments leak into the output,
        // and a real tag structurally outside any script (`<p>kept</p>`)
        // survives intact and unaffected.
        let html = r#"<html><body><script>var x = "</script> not real";
        console.log("still inside the real script");</script><p>kept</p></body></html>"#;
        let out = prune_scripts_styles(html);
        // The part of the script *before* the lookalike close is gone.
        assert!(!out.contains("var x ="));
        // No broken/partial opening-tag fragment leaked into the text.
        assert!(!out.contains("<scr"));
        // Content structurally outside the script survives untouched.
        assert!(out.contains("<p>kept</p>"));
    }

    #[test]
    fn removes_uppercase_script_and_style_tags() {
        let html = "<HTML><HEAD><STYLE>body{color:red}</STYLE></HEAD><BODY><SCRIPT>alert(1)</SCRIPT><P>text</P></BODY></HTML>";
        let out = prune_scripts_styles(html);
        assert!(!out.contains("alert"));
        assert!(!out.contains("color:red"));
        assert!(out.contains("text"));
    }

    #[test]
    fn handles_unclosed_script_tag_without_panicking() {
        let html = "<html><body><p>before</p><script>var x = 1; console.log(x);";
        // Must not panic; an unclosed script at EOF is swallowed along with
        // whatever content follows it inside the same element context.
        let out = prune_scripts_styles(html);
        assert!(out.contains("before"));
        assert!(!out.contains("console.log"));
    }

    #[test]
    fn removes_attribute_laden_script_tags() {
        let html = r#"<html><body><script type="text/javascript" src="https://example.com/a.js" async defer crossorigin="anonymous"></script><p>kept</p></body></html>"#;
        let out = prune_scripts_styles(html);
        assert!(!out.contains("example.com"));
        assert!(out.contains("<p>kept</p>"));
    }

    #[test]
    fn removes_noscript_including_fallback_content() {
        // "script, style, noscript" is a selector *list* — noscript's
        // fallback markup (often a real <img>/<p> visible to non-JS
        // clients) must be removed wholesale along with the tag itself,
        // not unwrapped and kept.
        let html = r#"<html><body><noscript><p>Please enable JavaScript to view this page.</p><img src="tracker.gif"></noscript><p>kept</p></body></html>"#;
        let out = prune_scripts_styles(html);
        assert!(!out.contains("Please enable JavaScript"));
        assert!(!out.contains("tracker.gif"));
        assert!(out.contains("<p>kept</p>"));
    }

    #[test]
    fn malformed_garbage_input_does_not_panic() {
        let inputs = [
            "",
            "<<<>>>not even close to html",
            "<html><body><div><span><p>unclosed everything",
            "\u{0}\u{1}\u{2} binary garbage <script>",
            "<script><script><script></script>",
            "plain text, no tags at all",
        ];
        for html in inputs {
            let _ = prune_scripts_styles(html);
            let _ = whole_doc_text(html);
        }
    }

    #[test]
    fn whole_doc_text_decodes_html_entities() {
        let html = "<html><body><p>DuckDuckGo&#x27;s terms &amp; conditions</p></body></html>";
        let out = whole_doc_text(html);
        assert!(
            out.contains("DuckDuckGo's"),
            "expected decoded apostrophe in: {out}"
        );
        assert!(
            out.contains('&'),
            "named entity &amp; should decode to a literal &: {out}"
        );
    }

    #[test]
    fn whole_doc_text_excludes_pruned_script_content() {
        let html = r#"<html><body><script>var secret = "should not appear";</script><style>.x{color:red}</style><p>Hello world</p></body></html>"#;
        let out = whole_doc_text(html);
        assert!(out.contains("Hello world"));
        assert!(!out.contains("secret"));
        assert!(!out.contains("color:red"));
    }

    // --- Benchmark. Generated
    // in-test, not committed as a fixture — same pattern as the
    // decompression-bomb payload in `util::fetch::tests`
    // (`read_capped_body_rejects_decompression_bomb`, fetch.rs:187).
    //
    // There is no memory-profiling crate in this project's dependency
    // graph, so instead of pulling one in just to sample RSS, this test
    // measures and reports the byte-size reduction (pruned length vs. raw
    // length) as a documented *proxy* for the memory-shrink effect —
    // explicitly logged as a proxy, not literal process RSS: an honest
    // number rather than a precise-looking but unavailable one.

    /// Builds a ~2MB synthetic "Next.js-style" HTML page: several large
    /// `<script type="application/json">` blobs simulating `__NEXT_DATA__`,
    /// a handful of ordinary `<script>`/`<style>` tags, and ~20KB of real
    /// prose spread across `<p>` tags.
    fn build_nextjs_style_fixture() -> String {
        let mut html = String::with_capacity(2 * 1024 * 1024 + 64 * 1024);
        html.push_str("<!DOCTYPE html><html><head><title>Bench Fixture</title>");
        html.push_str("<style>body{font-family:sans-serif;margin:0}</style>");
        html.push_str("</head><body>");

        // Real prose, ~20KB, spread across <p> tags — the content a
        // whole-doc dump / extractor actually cares about preserving.
        let sentence = "The quick brown fox jumps over the lazy dog near the riverbank while \
             the sun sets slowly behind the distant hills, painting the sky in shades of \
             orange and violet. ";
        let mut prose_bytes = 0usize;
        while prose_bytes < 20 * 1024 {
            html.push_str("<p>");
            html.push_str(sentence);
            html.push_str("</p>");
            prose_bytes += sentence.len();
        }

        // Simulated __NEXT_DATA__ style JSON blobs — the bulk of the 2MB,
        // mimicking Next.js's habit of embedding large serialized page
        // props/state directly in <script type="application/json"> tags.
        let json_chunk = r#"{"id":123456,"name":"widget","tags":["a","b","c"],"nested":{"x":1,"y":2,"z":[1,2,3,4,5]},"desc":"lorem ipsum dolor sit amet consectetur adipiscing elit"},"#;
        let mut json_bytes = 0usize;
        // Leave headroom so total document lands close to but not wildly
        // over 2MB once combined with prose + boilerplate script/style tags.
        let target_json_bytes = 2 * 1024 * 1024 - 40 * 1024;
        html.push_str(r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"items":["#);
        while json_bytes < target_json_bytes {
            html.push_str(json_chunk);
            json_bytes += json_chunk.len();
        }
        html.push_str(r#"]}},"page":"/"}</script>"#);

        // A scattering of ordinary script/style tags, as a real page would
        // also carry alongside the data blob.
        for i in 0..25 {
            html.push_str(&format!(
                "<script>console.log('chunk {i}'); (function(){{ var a = {i}; return a * 2; }})();</script>"
            ));
            html.push_str(&format!(
                "<style>.chunk-{i} {{ display: none; padding: {i}px; }}</style>"
            ));
        }
        html.push_str(
            r#"<noscript><p>Enable JavaScript to view this bench fixture.</p></noscript>"#,
        );

        html.push_str("</body></html>");
        html
    }

    #[test]
    fn prune_benchmark_2mb_nextjs_style_fixture() {
        let html = build_nextjs_style_fixture();
        let raw_len = html.len();
        assert!(
            raw_len > 1_800_000,
            "fixture should be roughly 2MB to match the benchmark target, got {raw_len} bytes"
        );

        // A few iterations, report the minimum (least noise from scheduler
        // jitter / thermal throttling on a shared CI box) for both passes.
        const ITERS: usize = 5;

        let mut prune_times = Vec::with_capacity(ITERS);
        let mut pruned_len = 0usize;
        for _ in 0..ITERS {
            let start = Instant::now();
            let pruned = prune_scripts_styles(&html);
            prune_times.push(start.elapsed());
            pruned_len = pruned.len();
        }

        let mut whole_doc_times = Vec::with_capacity(ITERS);
        let mut whole_doc_len = 0usize;
        for _ in 0..ITERS {
            let start = Instant::now();
            let text = whole_doc_text(&html);
            whole_doc_times.push(start.elapsed());
            whole_doc_len = text.len();
        }

        let min_prune = prune_times.iter().min().copied().unwrap_or(Duration::ZERO);
        let min_whole_doc = whole_doc_times
            .iter()
            .min()
            .copied()
            .unwrap_or(Duration::ZERO);
        let median = |mut v: Vec<Duration>| {
            v.sort();
            v[v.len() / 2]
        };
        let median_prune = median(prune_times);
        let median_whole_doc = median(whole_doc_times);

        let reduction_ratio = pruned_len as f64 / raw_len as f64;

        println!(
            "[prune_benchmark_2mb] raw_bytes={raw_len} pruned_bytes={pruned_len} \
             size_reduction_ratio={reduction_ratio:.4} ({:.1}% of original) \
             whole_doc_text_bytes={whole_doc_len}",
            reduction_ratio * 100.0
        );
        println!(
            "[prune_benchmark_2mb] prune_scripts_styles: min={min_prune:?} median={median_prune:?} \
             over {ITERS} iterations"
        );
        println!(
            "[prune_benchmark_2mb] whole_doc_text (prune + text dump + entity-decode): \
             min={min_whole_doc:?} median={median_whole_doc:?} over {ITERS} iterations"
        );
        println!(
            "[prune_benchmark_2mb] NOTE: no memory-profiling crate is in this project's \
             dependency graph, so peak RSS was not sampled. The byte-size reduction above \
             (pruned vs. raw length) is logged as an explicit PROXY for the memory-shrink \
             effect, not measured process RSS."
        );

        // Sanity: the JSON/script-heavy synthetic page should shrink
        // substantially once scripts/styles/noscript are stripped — this is
        // the whole point of the pre-prune pass.
        assert!(
            pruned_len < raw_len / 2,
            "expected pruning to remove at least half the bytes from a script/JSON-heavy \
             fixture; raw={raw_len} pruned={pruned_len}"
        );
        // The real prose must survive the prune.
        assert!(
            whole_doc_len > 15_000,
            "expected ~20KB of prose to survive the prune and text dump, got {whole_doc_len} bytes"
        );
    }
}

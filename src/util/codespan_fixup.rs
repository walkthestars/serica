//! Local patch for a `rs-trafilatura` 0.2.2 bug that mangles syntax-highlighted
//! code blocks.
//!
//! **Symptom:** extracting a page with Shiki/Prism-style syntax-highlighted
//! code (many `<span class="token ...">` elements wrapping individual tokens,
//! separated by whitespace-only `<span>...(a single space)...</span>` spacer
//! spans) collapses the spacing between tokens. For example, TypeScript
//! handbook / MDN code samples like:
//!
//! ```text
//! function fn(x) { return x.flip(); }
//! ```
//!
//! come back (in `content_markdown`) as:
//!
//! ```text
//! functionfn(x){returnx.flip();}
//! ```
//!
//! **Root cause:** `rs-trafilatura` 0.2.2's `prune_html` (`html_processing.rs`,
//! lines 323-346) deletes any element whose tag is in its `EMPTY_TAGS_TO_REMOVE`
//! list (which includes `"span"`) when `text.trim().is_empty()`. A spacer span
//! containing only a literal `" "` counts as "empty" by that check, so the
//! whole span — tag and whitespace text alike — is deleted from the DOM
//! before the content node's HTML/markdown is ever serialized. The two text
//! nodes on either side of the spacer span end up directly
//! adjacent with nothing between them, gluing the tokens together. This
//! specifically hits `content_markdown` (rendered from the pruned
//! `content_html` via `quick_html2md`, which does no space-insertion of its
//! own); `content_text` happens to escape it because its independent
//! text-node-join path (`extract_filtered_text_inner`) appends a trailing
//! space after every text node regardless of DOM structure. There is no
//! `Options` flag (see that crate's `options.rs`) to disable this pruning or
//! to exempt whitespace-only spans, so the fix has to intercept the HTML
//! from serica's side rather than through `rs_trafilatura::Options`.
//!
//! **Fix:** before handing raw HTML to `rs_trafilatura::extract_with_options`,
//! [`preserve_whitespace_only_spans`] rewrites the text content of any
//! whitespace-only `<span>...</span>` into a single sentinel character (see
//! below) that `str::trim()` does *not* consider whitespace, so
//! `prune_html`'s `text.trim().is_empty()` check now sees non-empty text and
//! leaves the span alone. The span (now non-empty) survives pruning, and a
//! later unwrap-but-keep-text pass (`extract_filtered_html_inner`) carries
//! the sentinel through to `content_html`/`content_markdown` in place of the
//! original whitespace. After extraction, [`restore_whitespace_placeholders`]
//! does one unconditional pass over the returned content replacing every
//! sentinel back to a normal space `' '`.
//!
//! Pipeline order (`util::extract::extract_from_html`): this
//! patch now runs *after* [`crate::util::prune::prune_scripts_styles`], which
//! only removes `script`/`style`/`noscript` elements and never touches
//! `<span>` content, so the two passes don't interact.
//!
//! **Sentinel character — corrected from the original design during
//! implementation.** The design this module was built from called for
//! U+00A0 (NO-BREAK SPACE), on the assumption that Rust's
//! `char::is_whitespace()` / `str::trim()` don't treat U+00A0 as whitespace.
//! That assumption is **false** and was disproved empirically while wiring
//! this up: `'\u{00A0}'.is_whitespace()` returns `true` in Rust, because
//! U+00A0 *does* carry the Unicode `White_Space` property (`PropList.txt`
//! lists `00A0 ; White_Space`). Using NBSP as the sentinel made
//! `prune_html`'s `text.trim().is_empty()` check still see the span as
//! empty, so it kept getting deleted — verified by a probe test that fed a
//! literal NBSP straight through `extract_with_options` and watched it
//! vanish from `content_html` entirely, same as a plain space would.
//!
//! This module instead uses **U+E000**, the first code point of the Unicode
//! Private Use Area (category `Co`, not `Zs`): confirmed via
//! `'\u{E000}'.is_whitespace() == false`, so it survives `str::trim()` and
//! every other whitespace-collapsing step in the pipeline intact, and is
//! restored to a plain space on the way out. Being outside any assigned
//! Unicode block, it also has effectively zero chance of colliding with
//! real page content, unlike NBSP (which does show up legitimately as
//! `&nbsp;` in real HTML).
//!
//! [`restore_whitespace_placeholders`] normalizes *every* occurrence of the
//! sentinel back to a plain space, unconditionally, on whatever
//! `rs_trafilatura::extract_with_options` returns — whether or not that
//! particular request's HTML went through the preservation pass. This is
//! deliberately unconditional rather than scoped to only the spans this
//! module actually substituted, and that has one known, accepted edge case:
//! if a source page's own text legitimately contains a literal U+E000
//! character (some icon-font/legacy-CMS setups encode glyphs as literal
//! Private Use Area characters in a text node, rather than via a CSS
//! `::before`/webfont ligature), that character is silently collapsed to a
//! plain space too, with no signal to the caller that it happened. This is
//! judged acceptably low-probability against a docs/article extraction
//! target — nothing in *this* pipeline emits U+E000, so it is a pure
//! restore for the vast majority of pages — but it is not a zero-downside
//! choice the way a truly novel/unrepresentable sentinel would be. If this
//! ever needs tightening, the fix is to scope the restore pass to spans
//! `preserve_whitespace_only_spans` actually touched (e.g. by tracking
//! offsets) rather than a blanket `replace`.
//!
//! **This module was written against `rs-trafilatura` 0.2.2.** Delete this
//! module and its two call sites once upstream fixes span-boundary whitespace
//! preservation and the pinned `rs-trafilatura` version is bumped past 0.2.2.

/// The sentinel used in place of whitespace-only `<span>` content while the
/// HTML is inside `rs_trafilatura::extract_with_options`. See the module doc
/// comment for why this is U+E000 (Private Use Area) rather than U+00A0
/// (NBSP, which Rust's `char::is_whitespace()` does treat as whitespace).
const WHITESPACE_SENTINEL: char = '\u{E000}';

/// Rewrites whitespace-only `<span>...</span>` elements in `html` so their
/// text content becomes a single [`WHITESPACE_SENTINEL`] instead of ordinary
/// whitespace, so `rs-trafilatura`'s `prune_html` does not treat them as
/// empty and delete them. See the module doc comment for the full story.
///
/// This is a plain manual byte/string scanner — no regex, no HTML parser —
/// deliberately, to avoid adding a new dependency (see the MSRV comment at
/// the top of `Cargo.toml`). It runs in a single left-to-right pass with no
/// backtracking, so it stays linear in `html.len()` even though this runs on
/// every extract request.
///
/// Algorithm, per span candidate found (case-insensitive `<span` at a proper
/// tag-name boundary — whitespace, `>`, or `/` immediately follows, so
/// `<spanner>`-style non-tags don't match):
/// 1. Find the `>` that closes the opening tag, correctly skipping over `>`
///    inside single- or double-quoted attribute values.
/// 2. From just after that `>`, look for the next `<`.
///    - If it starts a case-insensitive `</span>` *and* everything between
///      the opening tag and that `</span>` is non-empty and entirely
///      whitespace (space/tab/`\n`/`\r`), substitute that whitespace run
///      with a single sentinel character and continue scanning after the
///      `</span>`.
///    - Otherwise (nested/other tag, unclosed span, or non-whitespace /
///      empty content) leave everything from the opening tag onward
///      untouched and resume scanning right after the opening tag's `>`.
pub fn preserve_whitespace_only_spans(html: &str) -> String {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;

    while i < len {
        if bytes[i] == b'<' {
            if is_span_open_at(bytes, i) {
                if let Some(tag_end) = find_tag_end(bytes, i) {
                    let open_tag_end = tag_end + 1;

                    // Find the next '<' after the opening tag closes.
                    let mut j = open_tag_end;
                    while j < len && bytes[j] != b'<' {
                        j += 1;
                    }

                    if j < len {
                        if let Some(close_len) = span_close_len(bytes, j) {
                            let between = &html[open_tag_end..j];
                            let ws_only = !between.is_empty()
                                && between
                                    .chars()
                                    .all(|c| c == ' ' || c == '\t' || c == '\n' || c == '\r');
                            if ws_only {
                                out.push_str(&html[i..open_tag_end]);
                                out.push(WHITESPACE_SENTINEL);
                                out.push_str(&html[j..j + close_len]);
                                i = j + close_len;
                                continue;
                            }
                        }
                    }

                    // Nested tag, unclosed span, or non-whitespace/empty
                    // content: leave it untouched, resume after the '>'.
                    out.push_str(&html[i..open_tag_end]);
                    i = open_tag_end;
                    continue;
                }
                // "<span" with no closing '>' anywhere in the document:
                // not a real tag we can act on, treat the '<' as plain text.
                out.push('<');
                i += 1;
                continue;
            }
            out.push('<');
            i += 1;
            continue;
        }

        // Batch-copy the plain-text/other-tag run up to the next '<'.
        let next_lt = match html[i..].find('<') {
            Some(p) => i + p,
            None => len,
        };
        out.push_str(&html[i..next_lt]);
        i = next_lt;
    }

    out
}

/// Reverses [`preserve_whitespace_only_spans`]'s placeholder substitution:
/// every [`WHITESPACE_SENTINEL`] in `content` becomes an ordinary space.
/// Applied unconditionally to whatever `rs_trafilatura::extract_with_options`
/// returns (`content_text` / `content_markdown`), whether or not it actually
/// went through the preservation pass (see module doc comment for why that's
/// safe for this particular sentinel).
///
/// This also **collapses a single space already adjacent to the sentinel**,
/// on either side, into the one space the sentinel restores — rather than a
/// blind `replace`. That matters for `content_text`: unlike
/// `content_markdown` (rendered via `sel.text()`, which inserts no space of
/// its own around a surviving spacer-span text node), `content_text` is
/// built by a different join path (`extract_filtered_text_inner`) that
/// unconditionally appends a trailing space after *every* text node. The
/// surviving spacer span therefore carries a per-node trailing space on top
/// of the one already following the token before it:
/// `"function" + " " + SENTINEL + " " + "fn"`. A blind
/// `replace(SENTINEL, " ")` would yield `"function   fn"` (three spaces)
/// where `content_text` needs one. Collapsing one pre-existing adjacent space
/// into the restored sentinel space keeps the output single-spaced
/// without touching legitimate multi-space runs elsewhere (e.g. code
/// indentation in `content_markdown`), since those never sit next to a
/// sentinel in the first place.
pub fn restore_whitespace_placeholders(content: &str) -> String {
    let mut out = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(c) = chars.next() {
        if c == WHITESPACE_SENTINEL {
            if chars.peek() == Some(&' ') {
                chars.next();
            }
            if out.ends_with(' ') {
                out.pop();
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

/// True if `bytes[i..]` starts with a case-insensitive `<span` tag-name
/// boundary (i.e. the next character is whitespace, `>`, or `/`, not part of
/// a longer tag name like `<spanner>`).
fn is_span_open_at(bytes: &[u8], i: usize) -> bool {
    const TAG: &[u8] = b"<span";
    if i + TAG.len() > bytes.len() {
        return false;
    }
    for (k, &tc) in TAG.iter().enumerate() {
        if !bytes[i + k].eq_ignore_ascii_case(&tc) {
            return false;
        }
    }
    match bytes.get(i + TAG.len()) {
        Some(b) => b.is_ascii_whitespace() || *b == b'>' || *b == b'/',
        None => false,
    }
}

/// Scans forward from `start` (the index of the tag's opening `<`) for the
/// `>` that closes the tag, tracking single/double-quoted attribute values
/// so a `>` inside a quoted value doesn't end the tag prematurely. Returns
/// the index of the closing `>`, or `None` if the tag is never closed.
fn find_tag_end(bytes: &[u8], start: usize) -> Option<usize> {
    let len = bytes.len();
    let mut k = start;
    let mut in_squote = false;
    let mut in_dquote = false;
    while k < len {
        let b = bytes[k];
        if in_squote {
            if b == b'\'' {
                in_squote = false;
            }
        } else if in_dquote {
            if b == b'"' {
                in_dquote = false;
            }
        } else {
            match b {
                b'\'' => in_squote = true,
                b'"' => in_dquote = true,
                b'>' => return Some(k),
                _ => {}
            }
        }
        k += 1;
    }
    None
}

/// If `bytes[j..]` starts with a case-insensitive `</span` followed by
/// optional whitespace and then `>`, returns the byte length of that closing
/// tag (from `j` up to and including the `>`). Otherwise `None` — this is
/// also the boundary check that keeps something like `</spanx>` from being
/// mistaken for a span close.
fn span_close_len(bytes: &[u8], j: usize) -> Option<usize> {
    const TAG: &[u8] = b"</span";
    let len = bytes.len();
    if j + TAG.len() > len {
        return None;
    }
    for (k, &tc) in TAG.iter().enumerate() {
        if !bytes[j + k].eq_ignore_ascii_case(&tc) {
            return None;
        }
    }
    let mut k = j + TAG.len();
    while k < len && bytes[k].is_ascii_whitespace() {
        k += 1;
    }
    if k < len && bytes[k] == b'>' {
        Some(k + 1 - j)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- preserve_whitespace_only_spans: unit cases ----

    #[test]
    fn whitespace_only_span_becomes_sentinel() {
        let html = r#"<span class="token plain"> </span>"#;
        let out = preserve_whitespace_only_spans(html);
        assert_eq!(
            out,
            format!("<span class=\"token plain\">{WHITESPACE_SENTINEL}</span>")
        );
    }

    #[test]
    fn span_with_non_whitespace_content_is_untouched() {
        let html = r#"<span class="token keyword">function</span>"#;
        let out = preserve_whitespace_only_spans(html);
        assert_eq!(out, html);
    }

    #[test]
    fn span_with_nested_tag_is_left_untouched() {
        let html = "<span> <b>x</b> </span>";
        let out = preserve_whitespace_only_spans(html);
        assert_eq!(out, html);
    }

    #[test]
    fn empty_span_is_untouched() {
        let html = "<span></span>";
        let out = preserve_whitespace_only_spans(html);
        assert_eq!(out, html);
    }

    #[test]
    fn multiple_spans_in_sequence_all_handled() {
        let html = r#"<span class="token keyword">function</span><span class="token plain"> </span><span class="token function">fn</span>"#;
        let out = preserve_whitespace_only_spans(html);
        assert_eq!(
            out,
            format!(
                "<span class=\"token keyword\">function</span><span class=\"token plain\">{WHITESPACE_SENTINEL}</span><span class=\"token function\">fn</span>"
            )
        );
    }

    #[test]
    fn span_with_attributes_works() {
        let html = r#"<span class="token plain" data-x="a>b">   </span>"#;
        let out = preserve_whitespace_only_spans(html);
        assert_eq!(
            out,
            format!("<span class=\"token plain\" data-x=\"a>b\">{WHITESPACE_SENTINEL}</span>")
        );
    }

    #[test]
    fn tag_name_boundary_not_fooled_by_spanx() {
        let html = "<spanx> </spanx>";
        let out = preserve_whitespace_only_spans(html);
        // Not treated as a span open at all, left completely untouched.
        assert_eq!(out, html);
    }

    #[test]
    fn self_closing_or_unclosed_span_without_gt_is_left_as_text() {
        let html = "<span class=\"unterminated";
        let out = preserve_whitespace_only_spans(html);
        assert_eq!(out, html);
    }

    #[test]
    fn uppercase_span_tags_are_matched_case_insensitively() {
        let html = "<SPAN> </SPAN>";
        let out = preserve_whitespace_only_spans(html);
        assert_eq!(out, format!("<SPAN>{WHITESPACE_SENTINEL}</SPAN>"));
    }

    #[test]
    fn tab_and_newline_only_span_is_substituted() {
        let html = "<span>\t\n\r </span>";
        let out = preserve_whitespace_only_spans(html);
        assert_eq!(out, format!("<span>{WHITESPACE_SENTINEL}</span>"));
    }

    // ---- sentinel choice sanity check ----
    //
    // Pins the empirical discovery documented in the module doc comment: the
    // original design's sentinel (U+00A0 NBSP) IS treated as whitespace by
    // Rust, so it would not have worked. If this ever starts failing, it
    // means something changed in Rust's Unicode tables (extremely unlikely)
    // — not a reason to "fix" this file by reverting to NBSP.
    #[test]
    fn nbsp_is_whitespace_per_rust_contrary_to_common_assumption() {
        assert!('\u{00A0}'.is_whitespace());
        assert!("\u{00A0}".trim().is_empty());
    }

    #[test]
    fn sentinel_is_not_whitespace() {
        assert!(!WHITESPACE_SENTINEL.is_whitespace());
        assert!(!WHITESPACE_SENTINEL.to_string().trim().is_empty());
    }

    // ---- restore_whitespace_placeholders ----

    #[test]
    fn restore_whitespace_placeholders_round_trips_single_sentinel() {
        assert_eq!(
            restore_whitespace_placeholders(&format!("a{WHITESPACE_SENTINEL}b")),
            "a b"
        );
    }

    #[test]
    fn restore_whitespace_placeholders_handles_multiple_sentinels() {
        assert_eq!(
            restore_whitespace_placeholders(&format!(
                "function{WHITESPACE_SENTINEL}fn(x){WHITESPACE_SENTINEL}{{ return{WHITESPACE_SENTINEL}x; }}"
            )),
            "function fn(x) { return x; }"
        );
    }

    #[test]
    fn restore_whitespace_placeholders_is_noop_without_sentinel() {
        let content = "plain text with no placeholders";
        assert_eq!(restore_whitespace_placeholders(content), content);
    }

    // Regression: a sentinel adjacent to a pre-existing space on either (or
    // both) sides — the exact shape `content_text`'s per-text-node
    // trailing-space join produces around a surviving spacer-span node —
    // must collapse to one space, not two or three. See the function doc
    // comment for why this happens.
    #[test]
    fn restore_whitespace_placeholders_collapses_space_before_sentinel() {
        assert_eq!(
            restore_whitespace_placeholders(&format!("function {WHITESPACE_SENTINEL}fn")),
            "function fn"
        );
    }

    #[test]
    fn restore_whitespace_placeholders_collapses_space_after_sentinel() {
        assert_eq!(
            restore_whitespace_placeholders(&format!("function{WHITESPACE_SENTINEL} fn")),
            "function fn"
        );
    }

    #[test]
    fn restore_whitespace_placeholders_collapses_space_on_both_sides() {
        assert_eq!(
            restore_whitespace_placeholders(&format!("function {WHITESPACE_SENTINEL} fn")),
            "function fn"
        );
    }

    // ---- End-to-end regression pinning the rs-trafilatura 0.2.2 bug ----

    /// A syntax-highlighted code sample wrapped in enough real prose to
    /// clear rs-trafilatura's default extraction thresholds
    /// (`min_extracted_len` / `min_output_size` in that crate's
    /// `Options::default()`), inside an `<article>` so it's unambiguously
    /// selected as the main content node.
    fn highlighted_code_fixture() -> String {
        format!(
            r#"<html>
              <head><title>Example Docs Page</title></head>
              <body>
                <article>
                  <h1>Working With Functions</h1>
                  <p>This guide walks through a small example function and explains how it
                  is used elsewhere in the codebase, with enough surrounding prose that the
                  content extractor's scoring heuristics clearly select this article as the
                  main body of the page rather than falling back to boilerplate.</p>
                  <pre><code>{code}</code></pre>
                  <p>As shown above, the highlighted snippet defines a function and then
                  calls a method on its argument. The rest of this paragraph is here purely
                  to pad the extracted word count comfortably past the extractor's minimum
                  output thresholds so the fixture is realistic and reliably selected.</p>
                </article>
              </body>
            </html>"#,
            code = concat!(
                "<span class=\"token keyword\">function</span>",
                "<span class=\"token plain\"> </span>",
                "<span class=\"token function\">fn</span>",
                "<span class=\"token punctuation\">(</span>",
                "<span class=\"token parameter\">x</span>",
                "<span class=\"token punctuation\">)</span>",
                "<span class=\"token plain\"> </span>",
                "<span class=\"token punctuation\">{</span>",
                "<span class=\"token plain\"> </span>",
                "<span class=\"token keyword\">return</span>",
                "<span class=\"token plain\"> </span>",
                "<span class=\"token parameter\">x</span>",
                "<span class=\"token punctuation\">.</span>",
                "<span class=\"token function\">flip</span>",
                "<span class=\"token punctuation\">()</span>",
                "<span class=\"token punctuation\">;</span>",
                "<span class=\"token plain\"> </span>",
                "<span class=\"token punctuation\">}</span>",
            )
        )
    }

    /// Matches how both extract handlers actually call the crate (see
    /// `extract_options_from_config` in `src/models/extract.rs`): markdown
    /// output enabled. The glued-token bug lives in `content_markdown`
    /// (rendered straight off the pruned `content_html` via
    /// `quick_html2md`); `content_text`'s generic text-node join
    /// unconditionally appends a trailing space after every text node,
    /// which happens to paper over this particular bug in that one field
    /// (see module doc comment).
    fn markdown_options() -> rs_trafilatura::Options {
        rs_trafilatura::Options {
            output_markdown: true,
            ..rs_trafilatura::Options::default()
        }
    }

    #[test]
    fn upstream_bug_still_mangles_raw_html_unpatched() {
        // Pins the current (0.2.2) upstream behavior. If a future
        // rs-trafilatura bump fixes span-boundary whitespace preservation,
        // this assertion starts failing — that failure is the signal to
        // delete this module and its two call sites (see module doc
        // comment), not to "fix" this test.
        let html = highlighted_code_fixture();
        let result = rs_trafilatura::extract_with_options(&html, &markdown_options())
            .expect("fixture should extract successfully");
        let content = result
            .content_markdown
            .expect("markdown output should be populated");
        assert!(
            content.contains("functionfn"),
            "expected the known upstream mangling (\"functionfn\") in: {content:?}"
        );
    }

    #[test]
    fn preprocessing_and_restore_fix_the_spacing() {
        let html = highlighted_code_fixture();
        let preprocessed = preserve_whitespace_only_spans(&html);
        let result = rs_trafilatura::extract_with_options(&preprocessed, &markdown_options())
            .expect("fixture should extract successfully");
        let content = restore_whitespace_placeholders(
            &result
                .content_markdown
                .expect("markdown output should be populated"),
        );
        assert!(
            content.contains("function fn"),
            "expected correctly spaced tokens in: {content:?}"
        );
        assert!(
            !content.contains("functionfn"),
            "tokens must not be glued together: {content:?}"
        );
    }

    /// `content_text` output (the server's other configured mode, see
    /// `extract_options_from_config` in `src/models/extract.rs`). Unlike
    /// `content_markdown`, this field was never mangled by the upstream bug
    /// in the first place (its own text-node join already adds a separator)
    /// — but a naive fix that blindly replaced the sentinel with a space
    /// introduced a *new* regression here: the surviving spacer-span node
    /// now picks up its own auto-trailing-space on top of the one already
    /// following the preceding token, producing `"function   fn"` (three
    /// spaces) instead of the correct `"function fn"` (one). Pins that this
    /// stays fixed — see `restore_whitespace_placeholders`'s doc comment.
    #[test]
    fn text_output_is_not_regressed_to_extra_spaces() {
        let html = highlighted_code_fixture();
        let preprocessed = preserve_whitespace_only_spans(&html);
        let text_options = rs_trafilatura::Options {
            output_markdown: false,
            ..rs_trafilatura::Options::default()
        };
        let result = rs_trafilatura::extract_with_options(&preprocessed, &text_options)
            .expect("fixture should extract successfully");
        let content = restore_whitespace_placeholders(&result.content_text);

        assert!(
            content.contains("function fn"),
            "expected single-spaced tokens in: {content:?}"
        );
        assert!(
            !content.contains("function  fn") && !content.contains("function   fn"),
            "content_text must not gain extra spaces around a restored \
             sentinel (regression check): {content:?}"
        );
    }
}

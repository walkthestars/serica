//! Local patch for a second, structurally distinct `rs-trafilatura` 0.2.2 /
//! `quick_html2md` 0.2.1 bug that mangles syntax-highlighted code blocks
//! — a sibling of the bug [`crate::util::codespan_fixup`] patches: same
//! user-facing symptom class, "code-block whitespace/structure collapse", different root
//! cause than [`crate::util::codespan_fixup`]).
//!
//! **Symptom:** many syntax highlighters (Shiki, VitePress's built-in
//! highlighter, and similar tools) render each source line as its own
//! sibling block element inside `<pre><code>` — typically
//! `<div class="line">...</div>` per line, sometimes preceded by a
//! `<div class="language-js">js</div>`-style language tag — and rely on CSS
//! (`display: block` / `white-space: pre`) to produce the visual line
//! breaks. The HTML itself carries **zero literal whitespace** between these
//! sibling elements: `<div class="language-js">js</div><div class="line">function fn(x) {</div><div class="line">  return x.flip();</div>...`.
//! Extracting a page built this way collapses every line onto one, and glues
//! the language tag onto the first token:
//!
//! ```text
//! jsfunction fn(x) {  return x.flip();}
//! ```
//!
//! instead of
//!
//! ```text
//! js
//! function fn(x) {
//!   return x.flip();
//! }
//! ```
//!
//! **Root cause:** `quick_html2md` 0.2.1's code-block conversion
//! (`elements/code.rs`, `convert_code_block`, around lines 56-72) renders a
//! `<pre>` element's contents with a single call to
//! `dom_query::Selection::text()` (`sel.text()`). That method
//! (`dom_query` 0.24.0, `selection.rs:205`, delegating to
//! `TreeNodeOps::text_of` in `dom_tree/ops.rs:30`) does a raw
//! concatenation of every descendant text node in document order with *no*
//! separator inserted at element boundaries — it has no concept of
//! block-level vs. inline elements at all. (`dom_query::Selection` also
//! exposes a `formatted_text()` that *does* insert breaks after block
//! elements, but `quick_html2md`'s `convert_code_block` doesn't use it —
//! and `formatted_text()` also collapses internal whitespace to single
//! spaces, which would destroy code indentation, so it isn't a drop-in fix
//! even if `quick_html2md` adopted it.) This is a different bug from the one
//! `codespan_fixup` patches: that one is about `rs-trafilatura`'s
//! `prune_html` deleting whitespace-only `<span>` elements before
//! serialization; this one is about `quick_html2md`'s `<pre>` rendering
//! never having inserted separators between block siblings in the first
//! place, regardless of what survives pruning. There is no `MarkdownOptions`
//! flag (see that crate's `options.rs`) to change how code blocks are
//! rendered, so — as with `codespan_fixup` — the fix has to intercept the
//! HTML from serica's side.
//!
//! **Fix:** unlike `codespan_fixup`, this one does *not* need a sentinel
//! placeholder. A literal `\n` inserted between the divs survives untouched
//! all the way through `rs_trafilatura::extract_with_options` — confirmed
//! empirically against `rs-trafilatura` 0.2.2 with a throwaway repro crate:
//! `prune_html` (`html_processing.rs:323-346`) only ever removes *elements*
//! whose tag is in `EMPTY_TAGS_TO_REMOVE_SET`, and `div`/`p`/`tr`/`li` are
//! not in that set, so the check that deletes whitespace-only `<span>`s
//! (and would delete a lone `\n` text node the same way `codespan_fixup`'s
//! doc comment describes) never runs on this text at all — it's a sibling
//! text node next to elements that were never candidates for removal, not
//! text trapped inside one. So [`preserve_code_block_line_breaks`] just
//! inserts a real `\n` directly into the raw HTML before extraction, and
//! there is nothing to restore afterward.
//!
//! **This module was written against `rs-trafilatura` 0.2.2** (which pins
//! `quick_html2md = "0.2"`, resolved to 0.2.1, and `dom_query = "0.24"`,
//! resolved to 0.24.0). Delete this module and its two call sites once
//! upstream fixes block-sibling separation inside `<pre>` conversion and the
//! pinned `rs-trafilatura` version is bumped past whatever introduces that
//! fix.
//!
//! Pipeline order (`util::extract::extract_from_html`): this
//! patch now runs *after* [`crate::util::prune::prune_scripts_styles`],
//! which only removes `script`/`style`/`noscript` elements — never a `<pre>`
//! region's `div`/`p`/`tr`/`li` structure — so the two passes don't interact.

/// Closing tags treated as "line boundaries" inside a `<pre>` region: when
/// one of these is immediately followed (zero intervening whitespace) by
/// more content, a real line break is missing from the source HTML and
/// [`preserve_code_block_line_breaks`] inserts one. `div` is the tag every
/// known Shiki/VitePress-style highlighter actually uses for this
/// (`<div class="line">`/`<div class="language-*">`); `p`, `tr`, and `li`
/// are included defensively for the rarer table- or list-based line
/// renderers, without going so far as to treat *every* element as a line
/// boundary (that would also catch inline token `<span>`s, which must stay
/// glued to their neighbors on the same line — that's a different, already
/// correct, part of the pipeline).
const LINE_BOUNDARY_CLOSE_TAGS: [&[u8]; 4] = [b"div", b"p", b"tr", b"li"];

/// Rewrites `html` so that, inside every `<pre>...</pre>` region, a real
/// `\n` is inserted immediately after any [`LINE_BOUNDARY_CLOSE_TAGS`]
/// closing tag that is directly followed by more non-whitespace content
/// (another opening tag, another closing tag's content, or plain text) with
/// no separator in the source markup. See the module doc comment for the
/// full story and why, unlike [`crate::util::codespan_fixup`], no sentinel
/// placeholder or restore pass is needed.
///
/// This is a plain manual byte/string scanner — no regex, no HTML parser —
/// deliberately, to avoid adding a new dependency (see the MSRV comment at
/// the top of `Cargo.toml`), matching `codespan_fixup`'s approach. It runs
/// in a single left-to-right pass with no backtracking, so it stays linear
/// in `html.len()`.
///
/// Scoping rules, applied only while inside a `<pre>...</pre>` region
/// (`<pre>` cannot nest in valid HTML, so this tracks a simple boolean, not
/// a depth counter). **Known, accepted tradeoff:** if a `<pre>` is never
/// closed (malformed/truncated HTML), that boolean never resets, so every
/// `div`/`p`/`tr`/`li` boundary for the *rest of the document* is treated as
/// a line boundary too — worst case, unrelated later content gains spurious
/// `\n`s. This is judged low-probability (well-formed docs-generator output
/// is the overwhelmingly common case this module targets) and low-blast-
/// radius (extra newlines, not corrupted/lost content, and nothing panics)
/// against the complexity of tracking a real "container end" boundary from
/// outside a full HTML parser; `unclosed_pre_leaks_in_pre_state_to_rest_of_document`
/// below pins the current behavior so a future change to this tradeoff is a
/// deliberate, visible decision rather than an accidental one.
///
/// - A candidate closing tag must land on a proper tag-name boundary
///   (`</div>`, `</div  >`, etc. — not `</divider>`).
/// - No break is inserted if the very next byte is ASCII whitespace (a
///   separator is already present) or end of input.
/// - No break is inserted if the next thing is itself a closing tag
///   (`</...`) — e.g. `<div class="line">}</div></code></pre>` should not
///   grow a trailing blank line before the code fence closes.
/// - Otherwise, a break is missing and a single `\n` is inserted right
///   after the closing tag.
pub fn preserve_code_block_line_breaks(html: &str) -> String {
    let bytes = html.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(html.len());
    let mut i = 0usize;
    let mut in_pre = false;

    while i < len {
        if bytes[i] == b'<' {
            if !in_pre && is_tag_open_at(bytes, i, b"pre") {
                if let Some(tag_end) = find_tag_end(bytes, i) {
                    out.push_str(&html[i..=tag_end]);
                    i = tag_end + 1;
                    in_pre = true;
                    continue;
                }
            }

            if in_pre {
                if let Some(close_len) = closing_tag_len(bytes, i, b"pre") {
                    out.push_str(&html[i..i + close_len]);
                    i += close_len;
                    in_pre = false;
                    continue;
                }

                let matched_line_boundary = LINE_BOUNDARY_CLOSE_TAGS
                    .iter()
                    .find_map(|tag| closing_tag_len(bytes, i, tag));
                if let Some(close_len) = matched_line_boundary {
                    out.push_str(&html[i..i + close_len]);
                    i += close_len;

                    let needs_break = match bytes.get(i) {
                        None => false,
                        Some(b) if b.is_ascii_whitespace() => false,
                        Some(b'<') if bytes.get(i + 1) == Some(&b'/') => false,
                        Some(_) => true,
                    };
                    if needs_break {
                        out.push('\n');
                    }
                    continue;
                }
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

/// True if `bytes[i..]` starts with a case-insensitive `<{name}` tag-name
/// boundary (i.e. the next character is whitespace, `>`, or `/`, not part
/// of a longer tag name).
fn is_tag_open_at(bytes: &[u8], i: usize, name: &[u8]) -> bool {
    let prefix_len = 1 + name.len(); // '<' + name
    if i + prefix_len > bytes.len() || bytes[i] != b'<' {
        return false;
    }
    for (k, &tc) in name.iter().enumerate() {
        if !bytes[i + 1 + k].eq_ignore_ascii_case(&tc) {
            return false;
        }
    }
    match bytes.get(i + prefix_len) {
        Some(b) => b.is_ascii_whitespace() || *b == b'>' || *b == b'/',
        None => false,
    }
}

/// Scans forward from `start` (the index of a tag's opening `<`) for the
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

/// If `bytes[j..]` starts with a case-insensitive `</{name}` followed by
/// optional whitespace and then `>`, returns the byte length of that
/// closing tag (from `j` up to and including the `>`). Otherwise `None` —
/// this is also the boundary check that keeps something like `</divider>`
/// from being mistaken for a `</div>` close.
fn closing_tag_len(bytes: &[u8], j: usize, name: &[u8]) -> Option<usize> {
    let prefix_len = 2 + name.len(); // "</" + name
    let len = bytes.len();
    if j + prefix_len > len || bytes[j] != b'<' || bytes[j + 1] != b'/' {
        return None;
    }
    for (k, &tc) in name.iter().enumerate() {
        if !bytes[j + 2 + k].eq_ignore_ascii_case(&tc) {
            return None;
        }
    }
    let mut k = j + prefix_len;
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

    // ---- preserve_code_block_line_breaks: unit cases ----

    #[test]
    fn adjacent_line_divs_get_a_break_inserted() {
        let html = r#"<pre><code><div class="line">a</div><div class="line">b</div></code></pre>"#;
        let out = preserve_code_block_line_breaks(html);
        assert_eq!(
            out,
            "<pre><code><div class=\"line\">a</div>\n<div class=\"line\">b</div></code></pre>"
        );
    }

    #[test]
    fn language_tag_div_followed_by_line_div_gets_a_break() {
        let html = r#"<pre><code><div class="language-js">js</div><div class="line">function fn(x) {</div></code></pre>"#;
        let out = preserve_code_block_line_breaks(html);
        assert_eq!(
            out,
            "<pre><code><div class=\"language-js\">js</div>\n<div class=\"line\">function fn(x) {</div></code></pre>"
        );
    }

    #[test]
    fn language_tag_div_followed_directly_by_token_span_gets_a_break() {
        // No line-wrapper divs at all: the language tag glues straight onto
        // the first highlighter token span.
        let html = r#"<pre><code><div class="language-js">js</div><span class="token keyword">function</span></code></pre>"#;
        let out = preserve_code_block_line_breaks(html);
        assert_eq!(
            out,
            "<pre><code><div class=\"language-js\">js</div>\n<span class=\"token keyword\">function</span></code></pre>"
        );
    }

    #[test]
    fn already_whitespace_separated_divs_are_left_alone() {
        let html = "<pre><code><div>a</div>\n<div>b</div></code></pre>";
        let out = preserve_code_block_line_breaks(html);
        assert_eq!(out, html);
    }

    #[test]
    fn no_double_break_before_closing_code_and_pre_tags() {
        let html = r#"<pre><code><div class="line">}</div></code></pre>"#;
        let out = preserve_code_block_line_breaks(html);
        assert_eq!(
            out, html,
            "no trailing newline should be inserted before </code></pre>"
        );
    }

    #[test]
    fn nested_divs_do_not_get_a_break_before_their_own_closing_wrapper() {
        let html = r#"<pre><code><div class="line"><div class="inner">a</div></div><div class="line">b</div></code></pre>"#;
        let out = preserve_code_block_line_breaks(html);
        assert_eq!(
            out,
            "<pre><code><div class=\"line\"><div class=\"inner\">a</div></div>\n<div class=\"line\">b</div></code></pre>"
        );
    }

    #[test]
    fn tag_name_boundary_not_fooled_by_divider() {
        let html = "<pre><code><div>a</divider><div>b</div></code></pre>";
        let out = preserve_code_block_line_breaks(html);
        // "</divider>" doesn't match the </div> boundary check, so it's
        // left untouched and no break is inserted around it — but the real
        // </div> before </code> still isn't followed by anything, so
        // nothing else changes either.
        assert_eq!(out, html);
    }

    #[test]
    fn content_outside_pre_is_never_touched() {
        let html = r#"<div class="card">a</div><div class="card">b</div>"#;
        let out = preserve_code_block_line_breaks(html);
        assert_eq!(out, html);
    }

    /// Pins the known, accepted tradeoff documented on
    /// [`preserve_code_block_line_breaks`]: an unclosed `<pre>` leaks
    /// `in_pre = true` for the rest of the document, so later, unrelated
    /// `div` boundaries also get a `\n` inserted. If this test starts
    /// failing because someone tightened the scanner to track real
    /// container boundaries, that's an improvement — update this test to
    /// match, don't treat the failure as a regression.
    #[test]
    fn unclosed_pre_leaks_in_pre_state_to_rest_of_document() {
        let html = r#"<pre><code><div class="line">a</div><div class="unrelated1">x</div><div class="unrelated2">y</div>"#;
        let out = preserve_code_block_line_breaks(html);
        assert_eq!(
            out,
            r#"<pre><code><div class="line">a</div>
<div class="unrelated1">x</div>
<div class="unrelated2">y</div>"#,
            "documents the leak: unrelated divs after an unclosed <pre> \
             still get a break inserted between them"
        );
    }

    #[test]
    fn p_tr_li_boundaries_are_also_handled() {
        let html = "<pre><code><p>a</p><p>b</p></code></pre>";
        let out = preserve_code_block_line_breaks(html);
        assert_eq!(out, "<pre><code><p>a</p>\n<p>b</p></code></pre>");
    }

    #[test]
    fn uppercase_tags_are_matched_case_insensitively() {
        let html = "<PRE><CODE><DIV>a</DIV><DIV>b</DIV></CODE></PRE>";
        let out = preserve_code_block_line_breaks(html);
        assert_eq!(out, "<PRE><CODE><DIV>a</DIV>\n<DIV>b</DIV></CODE></PRE>");
    }

    // ---- End-to-end regression pinning the current upstream behavior ----

    /// Mirrors `codespan_fixup`'s `highlighted_code_fixture()` padding/
    /// structure approach (enough surrounding prose to clear
    /// rs-trafilatura's default extraction thresholds, inside an
    /// `<article>` so it's unambiguously selected as the main content
    /// node), but models a Shiki/VitePress-style line-wrapped code block
    /// instead of bare token spans.
    fn line_wrapped_code_fixture() -> String {
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
                  <pre class="shiki"><code>{code}</code></pre>
                  <p>As shown above, the highlighted snippet defines a function across
                  several lines and then calls a method on its argument. The rest of this
                  paragraph is here purely to pad the extracted word count comfortably past
                  the extractor's minimum output thresholds so the fixture is realistic and
                  reliably selected.</p>
                </article>
              </body>
            </html>"#,
            code = concat!(
                r#"<div class="language-js">js</div>"#,
                r#"<div class="line">function fn(x) {</div>"#,
                r#"<div class="line">  return x.flip();</div>"#,
                r#"<div class="line">}</div>"#,
            )
        )
    }

    fn markdown_options() -> rs_trafilatura::Options {
        rs_trafilatura::Options {
            output_markdown: true,
            ..rs_trafilatura::Options::default()
        }
    }

    #[test]
    fn upstream_bug_still_collapses_lines_unpatched() {
        // Pins the current (rs-trafilatura 0.2.2 / quick_html2md 0.2.1)
        // upstream behavior. If a future bump fixes block-sibling
        // separation inside <pre> conversion, this assertion starts
        // failing — that failure is the signal to delete this module and
        // its two call sites (see module doc comment), not to "fix" this
        // test.
        let html = line_wrapped_code_fixture();
        let result = rs_trafilatura::extract_with_options(&html, &markdown_options())
            .expect("fixture should extract successfully");
        let content = result
            .content_markdown
            .expect("markdown output should be populated");
        assert!(
            content.contains("jsfunction fn(x)"),
            "expected the known upstream mangling (language tag glued to \
             the first token, lines collapsed) in: {content:?}"
        );
    }

    #[test]
    fn preprocessing_fixes_the_line_breaks() {
        let html = line_wrapped_code_fixture();
        let preprocessed = preserve_code_block_line_breaks(&html);
        let result = rs_trafilatura::extract_with_options(&preprocessed, &markdown_options())
            .expect("fixture should extract successfully");
        let content = result
            .content_markdown
            .expect("markdown output should be populated");

        assert!(
            !content.contains("jsfunction"),
            "language tag must not be glued to the first token: {content:?}"
        );
        assert!(
            content.contains("js\nfunction fn(x) {"),
            "expected the language tag on its own line, followed by the \
             first code line: {content:?}"
        );
        assert!(
            content.contains("function fn(x) {\n  return x.flip();\n}"),
            "expected each source line to keep its own line break: {content:?}"
        );
    }
}

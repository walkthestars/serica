// Utility module: tracing, metrics, SSRF guard, capped body reads, code-span
// whitespace fixup (local patch for a rs-trafilatura 0.2.2 bug, see
// codespan_fixup's module doc comment) and code-block line-break fixup
// (local patch for a related but distinct rs-trafilatura 0.2.2 /
// quick_html2md 0.2.1 bug, see codeblock_linebreaks's module doc comment)
pub mod codeblock_linebreaks;
pub mod codespan_fixup;
pub mod extract;
pub mod fetch;
pub mod metrics;
pub mod prune;
pub mod ssrf;
pub mod tracing;

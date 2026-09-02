// Extract request/response models for the content extraction endpoint

use crate::config::Config;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct ExtractRequest {
    pub url: String,
}

/// Builds the `rs_trafilatura::Options` both extract handlers pass to
/// `extract_with_options`, from `config.extract` rather than the
/// crate's zero-config `Options::default()`.
///
/// `max_extracted_len` is an OUTPUT-side cap on the extracted content —
/// distinct from, and applied after, the `MAX_EXTRACT_BYTES` cap on the
/// raw fetched HTML body. `output_markdown` is derived from
/// `config.extract.output_format`; `Config::validate()` guarantees it is
/// always `"text"` or `"markdown"` by the time a handler runs, so anything
/// other than the literal `"markdown"` is treated as `"text"`.
/// `include_images`/`include_links` are hard-set to `false` — text-only
/// output unless a future per-request parameter opts in, which is out of
/// scope here.
pub fn extract_options_from_config(config: &Config) -> rs_trafilatura::Options {
    rs_trafilatura::Options {
        max_extracted_len: config.extract.max_content_chars,
        output_markdown: config.extract.output_format == "markdown",
        include_images: false,
        include_links: false,
        ..rs_trafilatura::Options::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_options_from_config_maps_max_content_chars() {
        let mut config = Config::default();
        config.extract.max_content_chars = 12_345;
        let options = extract_options_from_config(&config);
        assert_eq!(options.max_extracted_len, 12_345);
    }

    #[test]
    fn test_extract_options_from_config_maps_markdown_output_format() {
        let mut config = Config::default();
        config.extract.output_format = "markdown".into();
        assert!(extract_options_from_config(&config).output_markdown);
    }

    #[test]
    fn test_extract_options_from_config_maps_text_output_format() {
        let mut config = Config::default();
        config.extract.output_format = "text".into();
        assert!(!extract_options_from_config(&config).output_markdown);
    }

    #[test]
    fn test_extract_options_from_config_disables_images_and_links() {
        // Text-only output unless a caller opts in — no per-request
        // parameter exists yet, so these are always false.
        let options = extract_options_from_config(&Config::default());
        assert!(!options.include_images);
        assert!(!options.include_links);
    }

    // `max_content_chars` must actually cap the content that comes
    // back out of `rs_trafilatura::extract_with_options`, not just get
    // threaded through unused — build a document with far more content
    // than the configured cap and confirm the returned text respects it.
    #[test]
    fn test_extract_with_options_respects_max_content_chars() {
        let mut config = Config::default();
        config.extract.max_content_chars = 200;
        config.extract.output_format = "text".into();
        let options = extract_options_from_config(&config);

        let paragraph = "word ".repeat(500); // ~2500 chars, well over the 200-char cap
        let html = format!(
            "<html><head><title>Long Article</title></head><body><article><p>{paragraph}</p></article></body></html>"
        );

        let result = rs_trafilatura::extract_with_options(&html, &options)
            .expect("extraction of a well-formed document must succeed");

        assert!(
            result.content_text.len() <= 200,
            "content_text.len() = {} exceeds configured max_content_chars = 200",
            result.content_text.len()
        );
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractResponse {
    pub url: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub date: Option<String>,
    pub sitename: Option<String>,
    pub page_type: Option<String>,
    /// The extracted content, already capped to
    /// `config.extract.max_content_chars` and already in whichever shape
    /// `config.extract.output_format` selected: GitHub-Flavored
    /// Markdown (`rs_trafilatura::ExtractResult::content_markdown`) when
    /// `output_format = "markdown"`, or flattened plain text
    /// (`content_text`) when `output_format = "text"`. Kept as a single
    /// `Option<String>` field — rather than adding a second
    /// `content_markdown` field alongside it — so this shared response
    /// shape (serialized to both the HTTP JSON body and the MCP tool-call
    /// result) doesn't grow a schema branch that most callers would have to
    /// ignore; the server picks one shape per its own configuration.
    pub content: Option<String>,
    pub excerpt: Option<String>,
    pub timing_ms: u64,
    /// `true` when this response was served from cache rather than
    /// freshly fetched and extracted.
    pub cached: bool,
}

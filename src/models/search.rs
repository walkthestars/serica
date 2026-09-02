// Search request, result, and response data models
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchRequest {
    pub query: String,
    pub page: u32,
    pub safe_search: u8,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub url: String,
    pub title: String,
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    /// Count of results in the current page's filtered batch (i.e.
    /// `results.len()` before the page-size cap is applied), NOT a
    /// corpus-wide total across all pages. DuckDuckGo's HTML scrape doesn't
    /// reliably expose an overall hit count, so this field can't promise
    /// more than "how many survived filtering for this page's request."
    pub total_results: usize,
    pub page: u32,
    pub timing_ms: u64,
    pub cached: bool,
}

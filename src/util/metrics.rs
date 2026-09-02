// Metrics collector: in-memory counters for search stats
// Exposes atomic counters and a snapshot method for /api/v1/stats

use std::sync::atomic::{AtomicU64, Ordering};

/// In-memory metrics collector for search system observability.
pub struct MetricsCollector {
    total_searches: AtomicU64,
    total_latency_ms: AtomicU64,
    // These counters exist so an upstream outage or a DDG markup change (which
    // silently degrades every query to zero results) shows up somewhere
    // other than a client's empty result array. `total_empty_parses` in
    // particular is the selector-drift canary — a 200 OK response containing
    // no parseable results is not a client-facing error (a query CAN
    // legitimately have zero real-world matches), but a sustained rate of
    // them is the earliest signal that the DDG scraper broke.
    total_errors: AtomicU64,
    total_blocked: AtomicU64,
    // DDG's interactive anti-bot challenge (202 + anomaly-modal),
    // tracked separately from `total_blocked` (403) — this is the number
    // that should actually drive proxy/exit-IP rotation decisions, since
    // it's the only counter that reflects "DDG served its CAPTCHA page"
    // rather than an outright 403 or a transport-level failure.
    total_challenged: AtomicU64,
    total_empty_parses: AtomicU64,
}

/// Snapshot of metrics at a point in time.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetricsSnapshot {
    pub total_searches: u64,
    pub avg_latency_ms: f64,
    pub total_errors: u64,
    pub total_blocked: u64,
    pub total_challenged: u64,
    pub total_empty_parses: u64,
}

impl MetricsCollector {
    pub fn new() -> Self {
        Self {
            total_searches: AtomicU64::new(0),
            total_latency_ms: AtomicU64::new(0),
            total_errors: AtomicU64::new(0),
            total_blocked: AtomicU64::new(0),
            total_challenged: AtomicU64::new(0),
            total_empty_parses: AtomicU64::new(0),
        }
    }

    pub fn record_search(&self, timing_ms: u64) {
        self.total_searches.fetch_add(1, Ordering::Relaxed);
        self.total_latency_ms
            .fetch_add(timing_ms, Ordering::Relaxed);
    }

    /// Upstream returned a non-2xx status (excluding 403, tracked separately
    /// via `record_blocked`) or the request failed at the transport level.
    pub fn record_error(&self) {
        self.total_errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Upstream returned 403 — DDG is actively blocking this deployment.
    pub fn record_blocked(&self) {
        self.total_blocked.fetch_add(1, Ordering::Relaxed);
    }

    /// Upstream returned its anti-bot challenge page (202 + anomaly-modal)
    /// instead of results.
    pub fn record_challenged(&self) {
        self.total_challenged.fetch_add(1, Ordering::Relaxed);
    }

    /// HTTP 200 from DDG but the HTML parser matched zero result containers.
    pub fn record_empty_parse(&self) {
        self.total_empty_parses.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> MetricsSnapshot {
        let total = self.total_searches.load(Ordering::Relaxed);
        let total_latency = self.total_latency_ms.load(Ordering::Relaxed);

        let avg_latency_ms = if total > 0 {
            total_latency as f64 / total as f64
        } else {
            0.0
        };

        MetricsSnapshot {
            total_searches: total,
            avg_latency_ms,
            total_errors: self.total_errors.load(Ordering::Relaxed),
            total_blocked: self.total_blocked.load(Ordering::Relaxed),
            total_challenged: self.total_challenged.load(Ordering::Relaxed),
            total_empty_parses: self.total_empty_parses.load(Ordering::Relaxed),
        }
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_collector_snapshots_all_zero() {
        let metrics = MetricsCollector::new();
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.total_searches, 0);
        assert_eq!(snapshot.total_errors, 0);
        assert_eq!(snapshot.total_blocked, 0);
        assert_eq!(snapshot.total_challenged, 0);
        assert_eq!(snapshot.total_empty_parses, 0);
        assert_eq!(snapshot.avg_latency_ms, 0.0);
    }

    // These counters are what make an outage, a challenge, or a
    // selector-drift regression observable at all, since the client-facing
    // response for "genuinely no results" and "engine broken" must stay
    // identical.
    #[test]
    fn error_blocked_challenged_and_empty_parse_counters_are_independent() {
        let metrics = MetricsCollector::new();
        metrics.record_error();
        metrics.record_error();
        metrics.record_blocked();
        metrics.record_challenged();
        metrics.record_challenged();
        metrics.record_empty_parse();
        metrics.record_empty_parse();
        metrics.record_empty_parse();

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.total_errors, 2);
        assert_eq!(snapshot.total_challenged, 2);
        assert_eq!(snapshot.total_blocked, 1);
        assert_eq!(snapshot.total_empty_parses, 3);
        assert_eq!(
            snapshot.total_searches, 0,
            "error/blocked/empty-parse counters must not affect total_searches"
        );
    }
}

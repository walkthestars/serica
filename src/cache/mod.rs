// Cache layer: trait definition and implementations
pub mod noop;
#[cfg(feature = "redis-cache")]
pub mod redis;

use async_trait::async_trait;
use std::time::Duration;

/// CacheLayer trait — defines the interface for byte-blob caching.
///
/// Response-type-agnostic: callers are responsible for serializing to/from
/// their own response type (e.g. `SearchResponse`, `ExtractResponse`) around
/// these calls. This lets one trait/impl pair serve multiple cached response
/// types instead of a parallel trait+impl per type.
#[async_trait]
pub trait CacheLayer: Send + Sync {
    /// Get cached bytes by key. Returns None on miss or error.
    async fn get(&self, key: &str) -> Option<Vec<u8>>;

    /// Set a cache entry with TTL. Best-effort — errors are logged, not propagated.
    async fn set(&self, key: &str, value: &[u8], ttl: Duration);
}

// NoopCache: pass-through cache when Redis is not configured
// Always returns None on get, no-op on set

use crate::cache::CacheLayer;
use async_trait::async_trait;
use std::time::Duration;

#[derive(Clone)]
pub struct NoopCache;

#[async_trait]
impl CacheLayer for NoopCache {
    async fn get(&self, _key: &str) -> Option<Vec<u8>> {
        None
    }

    async fn set(&self, _key: &str, _value: &[u8], _ttl: Duration) {
        // No-op
    }
}

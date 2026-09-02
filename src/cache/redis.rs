// RedisCache: Redis-backed LRU cache with connection pooling
// Uses redis-rs async ConnectionManager for connection pooling

use async_trait::async_trait;
use std::time::Duration;

use crate::cache::CacheLayer;

pub struct RedisCache {
    conn: redis::aio::ConnectionManager,
    key_prefix: String,
    // Retained on the struct (rather than dropped at the end of `new`) so a future
    // fallback-TTL code path for `set` calls without an explicit ttl has it on hand.
    #[allow(dead_code)]
    default_ttl: Duration,
}

impl RedisCache {
    /// Connect to Redis at startup. If connection fails, return Err.
    /// The caller (main) MUST handle this error by falling back to NoopCache.
    pub async fn new(
        redis_url: &str,
        default_ttl: Duration,
    ) -> Result<Self, crate::error::AppError> {
        let client = redis::Client::open(redis_url).map_err(|e| {
            crate::error::AppError::Redis(format!("Failed to create Redis client: {}", e))
        })?;

        let conn = client.get_connection_manager().await.map_err(|e| {
            crate::error::AppError::Redis(format!("Failed to connect to Redis: {}", e))
        })?;

        tracing::info!("Connected to Redis at {}", redis_url);

        Ok(Self {
            conn,
            key_prefix: "serica:".to_string(),
            default_ttl,
        })
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}{}", self.key_prefix, key)
    }
}

#[async_trait]
impl CacheLayer for RedisCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        let full_key = self.full_key(key);
        let mut conn = self.conn.clone();

        match redis::cmd("GET")
            .arg(&full_key)
            .query_async::<Option<Vec<u8>>>(&mut conn)
            .await
        {
            Ok(Some(data)) => {
                tracing::debug!(key = %full_key, "Redis cache hit");
                Some(data)
            }
            Ok(None) => {
                tracing::debug!(key = %full_key, "Redis cache miss");
                None
            }
            Err(e) => {
                tracing::error!(key = %full_key, error = %e, "Redis GET error");
                None
            }
        }
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Duration) {
        let full_key = self.full_key(key);
        let ttl_secs = ttl.as_secs();
        let mut conn = self.conn.clone();

        match redis::cmd("SETEX")
            .arg(&full_key)
            .arg(ttl_secs)
            .arg(value)
            .query_async::<()>(&mut conn)
            .await
        {
            Ok(_) => tracing::debug!(key = %full_key, ttl = %ttl_secs, "Redis cache set"),
            Err(e) => {
                tracing::error!(key = %full_key, error = %e, "Redis SETEX error");
            }
        }
    }
}

// RedisCache: Redis-backed LRU cache with connection pooling
// Uses redis-rs async ConnectionManager for connection pooling

use async_trait::async_trait;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

use crate::cache::CacheLayer;

/// Bound on a single Redis connection attempt. Without this,
/// `get_connection_manager()` can hang indefinitely when the Redis host
/// accepts TCP but never answers (e.g. Docker engine down behind a stale
/// WSL2 port-forward on :6380) — which stalls the MCP stdio handshake and
/// gets the server dropped from the host's tool catalog.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Minimum interval between reconnect attempts, so a dead Redis degrades
/// cache ops to Noop-speed instead of adding a timeout latency to each one.
const RECONNECT_MIN_INTERVAL: Duration = Duration::from_secs(10);

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

struct LazyState {
    conn: Option<redis::aio::ConnectionManager>,
    last_attempt: Option<Instant>,
}

/// Lazy, self-healing Redis cache.
///
/// Unlike [`RedisCache`] (which connects eagerly at startup and can stall
/// the server handshake when Redis is down), this connects in the
/// background on first use: construction only parses the URL and performs
/// no I/O, so it can never block startup. While disconnected, `get` is a
/// miss and `set` is a no-op — identical observable behavior to
/// `NoopCache` — and reconnects are throttled to [`RECONNECT_MIN_INTERVAL`]
/// so a dead Redis costs cache ops nothing. Once Redis answers, the
/// [`redis::aio::ConnectionManager`] itself auto-reconnects on later drops,
/// so caching resumes on its own when Redis comes back.
pub struct LazyRedisCache {
    client: redis::Client,
    redis_url_for_log: String,
    key_prefix: String,
    #[allow(dead_code)]
    default_ttl: Duration,
    state: RwLock<LazyState>,
}

impl LazyRedisCache {
    /// Parse the Redis URL. Performs no I/O — safe to call at startup.
    /// Returns Err only if the URL itself is invalid (caller falls back
    /// to NoopCache, same as before).
    pub fn new(
        redis_url: &str,
        default_ttl: Duration,
    ) -> Result<Self, crate::error::AppError> {
        let client = redis::Client::open(redis_url).map_err(|e| {
            crate::error::AppError::Redis(format!("Failed to create Redis client: {}", e))
        })?;

        Ok(Self {
            client,
            redis_url_for_log: redis_url.to_string(),
            key_prefix: "serica:".to_string(),
            default_ttl,
            state: RwLock::new(LazyState {
                conn: None,
                last_attempt: None,
            }),
        })
    }

    fn full_key(&self, key: &str) -> String {
        format!("{}{}", self.key_prefix, key)
    }

    /// Return a live connection, connecting (with timeout) if needed.
    /// Returns None when Redis is unreachable — callers degrade to Noop.
    async fn ensure_conn(&self) -> Option<redis::aio::ConnectionManager> {
        // Fast path: already connected.
        {
            let state = self.state.read().await;
            if let Some(conn) = state.conn.clone() {
                return Some(conn);
            }
            // Throttle reconnect attempts while Redis is down.
            if let Some(last) = state.last_attempt {
                if last.elapsed() < RECONNECT_MIN_INTERVAL {
                    return None;
                }
            }
        }

        let mut state = self.state.write().await;
        // Re-check under the write lock — another task may have connected.
        if let Some(conn) = state.conn.clone() {
            return Some(conn);
        }
        if let Some(last) = state.last_attempt {
            if last.elapsed() < RECONNECT_MIN_INTERVAL {
                return None;
            }
        }
        state.last_attempt = Some(Instant::now());

        let client = self.client.clone();
        match tokio::time::timeout(CONNECT_TIMEOUT, client.get_connection_manager()).await {
            Ok(Ok(conn)) => {
                tracing::info!("Connected to Redis at {}", self.redis_url_for_log);
                state.conn = Some(conn.clone());
                Some(conn)
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Redis connect failed, serving uncached; will retry");
                None
            }
            Err(_) => {
                tracing::warn!(
                    "Redis connect timed out after {:?}, serving uncached; will retry",
                    CONNECT_TIMEOUT
                );
                None
            }
        }
    }

    /// Drop a dead connection so the next op triggers a fresh attempt
    /// (subject to throttle). Called when an op fails on a live handle —
    /// e.g. Redis restarted mid-session and the pooled connection broke.
    async fn invalidate(&self) {
        self.state.write().await.conn = None;
    }
}

#[async_trait]
impl CacheLayer for LazyRedisCache {
    async fn get(&self, key: &str) -> Option<Vec<u8>> {
        let full_key = self.full_key(key);
        let mut conn = self.ensure_conn().await?;

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
                tracing::warn!(key = %full_key, error = %e, "Redis GET error, dropping connection");
                self.invalidate().await;
                None
            }
        }
    }

    async fn set(&self, key: &str, value: &[u8], ttl: Duration) {
        let full_key = self.full_key(key);
        let Some(mut conn) = self.ensure_conn().await else {
            return;
        };
        let ttl_secs = ttl.as_secs();

        match redis::cmd("SETEX")
            .arg(&full_key)
            .arg(ttl_secs)
            .arg(value)
            .query_async::<()>(&mut conn)
            .await
        {
            Ok(_) => tracing::debug!(key = %full_key, ttl = %ttl_secs, "Redis cache set"),
            Err(e) => {
                tracing::warn!(key = %full_key, error = %e, "Redis SETEX error, dropping connection");
                self.invalidate().await;
            }
        }
    }
}

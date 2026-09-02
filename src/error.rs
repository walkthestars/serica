// Global error types and error handling infrastructure
// Uses thiserror for typed error enums with IntoResponse for axum

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    // === Client errors (4xx) ===
    #[error("Invalid query: {0}")]
    InvalidQuery(String),

    #[error("Invalid page: {0}")]
    InvalidPage(String),

    #[error("Invalid safe search value: {0}")]
    InvalidSafeSearch(u8),

    #[error("Invalid language code: {0}")]
    InvalidLanguage(String),

    #[error("Rate limited")]
    RateLimited,

    // Returned by `gateway::http::check_api_key` when
    // `server.api_key` is configured and the request's `Authorization:
    // Bearer <key>` / `X-API-Key` header is missing or doesn't match. The
    // message is safe to return as-is (it never echoes the configured or
    // provided key), unlike the opaque-on-purpose Config/Bind/Redis arms
    // below.
    #[error("Unauthorized: {0}")]
    Unauthorized(String),

    // Extract pipeline plan, Change 1: returned when the raw fetched HTML
    // exceeds `config.extract.max_raw_bytes` (Content-Length pre-check or
    // per-chunk check in `read_capped_body`). The message is safe to
    // return as-is — it's a caller-facing "your request produced too much
    // content" detail, not internal detail like the opaque-on-purpose
    // Config/Bind/Redis arms below.
    #[error("Content too large: {0}")]
    ContentTooLarge(String),

    // === Server errors (5xx) ===
    #[error("Internal error: {0}")]
    Internal(String),

    // === Service degradation ===
    #[error("Service unavailable: {0}")]
    ServiceUnavailable(String),

    // Extract pipeline plan, Change 1: returned when the full extract span
    // (fetch through parse) exceeds `config.extract.timeout_ms` — Serica is
    // acting as a gateway fetching an upstream resource, so 504 is the
    // RFC 9110-correct code. The message is safe to return as-is, same
    // reasoning as `ContentTooLarge` above.
    #[error("Extract timeout: {0}")]
    ExtractTimeout(String),

    // === Startup errors ===
    // Boxed: `figment::Error` alone is ~208 bytes, which would make this
    // variant dominate `AppError`'s size and trip clippy::result_large_err
    // on every function returning `Result<_, AppError>` (e.g.
    // `SearchRequest::try_from_params`). `#[from]` can't be kept alongside
    // the box since the field type is `Box<figment::Error>` — the `From`
    // impl below replaces it so `?` still works unchanged at every call
    // site.
    #[error("Config error: {0}")]
    Config(Box<figment::Error>),

    #[error("Port bind error: {0}")]
    Bind(#[from] std::io::Error),

    #[error("Redis error: {0}")]
    Redis(String),
}

impl From<figment::Error> for AppError {
    fn from(e: figment::Error) -> Self {
        AppError::Config(Box::new(e))
    }
}

impl AppError {
    pub fn to_http_parts(&self) -> (StatusCode, &'static str, String) {
        match self {
            AppError::InvalidQuery(msg) => (StatusCode::BAD_REQUEST, "INVALID_QUERY", msg.clone()),
            AppError::InvalidPage(msg) => (StatusCode::BAD_REQUEST, "INVALID_PAGE", msg.clone()),
            AppError::InvalidSafeSearch(val) => (
                StatusCode::BAD_REQUEST,
                "INVALID_SAFE_SEARCH",
                format!("Invalid safe search value: {}", val),
            ),
            AppError::InvalidLanguage(msg) => {
                (StatusCode::BAD_REQUEST, "INVALID_LANGUAGE", msg.clone())
            }
            AppError::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "RATE_LIMITED",
                "Rate limit exceeded. Try again later.".into(),
            ),
            AppError::Unauthorized(msg) => (StatusCode::UNAUTHORIZED, "UNAUTHORIZED", msg.clone()),
            AppError::ContentTooLarge(msg) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "CONTENT_TOO_LARGE",
                msg.clone(),
            ),
            AppError::Internal(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "INTERNAL_ERROR",
                msg.clone(),
            ),
            AppError::ServiceUnavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "SERVICE_UNAVAILABLE",
                msg.clone(),
            ),
            AppError::ExtractTimeout(msg) => {
                (StatusCode::GATEWAY_TIMEOUT, "EXTRACT_TIMEOUT", msg.clone())
            }
            AppError::Config(e) => {
                // Opaque on purpose: figment errors can echo back file paths,
                // env var names/values, and other host-local detail.
                tracing::warn!(error = %e, "Config error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "CONFIG_ERROR",
                    "Configuration error".to_string(),
                )
            }
            AppError::Bind(e) => {
                // Opaque on purpose: io::Error can reveal the bind address/port
                // and OS-level detail about the host's network stack.
                tracing::warn!(error = %e, "Bind error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "BIND_ERROR",
                    "Server failed to bind".to_string(),
                )
            }
            AppError::Redis(msg) => {
                // Opaque on purpose: the Redis error can include the connection
                // string, which may carry credentials.
                tracing::warn!(error = %msg, "Redis error");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "REDIS_ERROR",
                    "Cache backend error".to_string(),
                )
            }
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, code, message) = self.to_http_parts();
        let body = serde_json::json!({
            "success": false,
            "error": {
                "code": code,
                "message": message,
            }
        });
        (status, axum::Json(body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::StatusCode;

    #[test]
    fn test_invalid_query_returns_400() {
        let err = AppError::InvalidQuery("query is empty".into());
        let (status, code, _) = err.to_http_parts();
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(code, "INVALID_QUERY");
    }

    #[test]
    fn test_rate_limited_returns_429() {
        let err = AppError::RateLimited;
        let (status, code, _) = err.to_http_parts();
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(code, "RATE_LIMITED");
    }

    // The API-key gate on /api/v1/stats returns this variant, and it
    // must use the standard JSON envelope (not a bare 401) like every other
    // rejection path in this codebase.
    #[test]
    fn test_unauthorized_returns_401() {
        let err = AppError::Unauthorized("Missing or invalid API key".into());
        let (status, code, msg) = err.to_http_parts();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(code, "UNAUTHORIZED");
        assert_eq!(msg, "Missing or invalid API key");
    }

    // Extract pipeline plan, Change 1: `ContentTooLarge`/`ExtractTimeout`
    // are caller-facing detail (unlike Redis/Bind/Config's opaque arms
    // above), so the message must pass through unchanged.
    #[test]
    fn test_content_too_large_returns_413() {
        let err = AppError::ContentTooLarge("raw HTML exceeds 4194304 bytes".into());
        let (status, code, msg) = err.to_http_parts();
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(code, "CONTENT_TOO_LARGE");
        assert_eq!(msg, "raw HTML exceeds 4194304 bytes");
    }

    #[test]
    fn test_extract_timeout_returns_504() {
        let err = AppError::ExtractTimeout("extract exceeded 30000ms".into());
        let (status, code, msg) = err.to_http_parts();
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(code, "EXTRACT_TIMEOUT");
        assert_eq!(msg, "extract exceeded 30000ms");
    }

    #[test]
    fn test_internal_error_returns_500() {
        let err = AppError::Internal("something broke".into());
        let (status, code, msg) = err.to_http_parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "INTERNAL_ERROR");
        assert_eq!(msg, "something broke");
    }

    #[test]
    fn test_error_produces_json_envelope() {
        let err = AppError::InvalidQuery("empty".into());
        let response = err.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // Bind/Config/Redis internal error detail (paths, env values, Redis
    // connection strings that may carry credentials) must never reach the
    // client-facing body — only the opaque code/message pair should.
    #[test]
    fn test_redis_error_body_is_opaque() {
        let err = AppError::Redis("redis://user:hunter2@10.0.0.5:6379 refused".into());
        let (status, code, msg) = err.to_http_parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "REDIS_ERROR");
        assert_eq!(msg, "Cache backend error");
        assert!(!msg.contains("hunter2"));
        assert!(!msg.contains("10.0.0.5"));
    }

    #[test]
    fn test_bind_error_body_is_opaque() {
        let io_err =
            std::io::Error::new(std::io::ErrorKind::AddrInUse, "address in use 0.0.0.0:3000");
        let err = AppError::Bind(io_err);
        let (status, code, msg) = err.to_http_parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "BIND_ERROR");
        assert_eq!(msg, "Server failed to bind");
        assert!(!msg.contains("3000"));
    }

    #[test]
    fn test_config_error_body_is_opaque() {
        let fig_err =
            figment::Error::from("secret file '/etc/serica/prod.toml' not found".to_string());
        let err = AppError::Config(Box::new(fig_err));
        let (status, code, msg) = err.to_http_parts();
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(code, "CONFIG_ERROR");
        assert_eq!(msg, "Configuration error");
        assert!(!msg.contains("/etc/serica"));
    }
}

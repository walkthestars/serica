// Serica — Single-binary search engine (DuckDuckGo)
// Entry point: CLI parsing, config loading, server startup

use clap::Parser;
use serica_search::cache::noop::NoopCache;
use serica_search::cache::CacheLayer;
use serica_search::engines::duckduckgo::{spawn_health_monitor, DuckDuckGoAdapter, EngineHealth};
use serica_search::gateway::http::HttpGateway;
use serica_search::gateway::mcp::McpGateway;
use serica_search::util::metrics::MetricsCollector;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser, Debug)]
#[command(
    name = "serica",
    version,
    about = "Single-binary search engine via DuckDuckGo"
)]
struct Cli {
    /// Configuration file path
    #[arg(short, long, env = "SERICA_CONFIG_FILE")]
    config: Option<String>,

    /// Subcommand (default: server)
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Parser, Debug)]
enum Command {
    /// Start HTTP server (default)
    Server,
    /// Start MCP stdio server
    Mcp,
    /// Probe the local HTTP server's liveness endpoint and exit 0/1
    /// (replaces the `curl` dependency in the container HEALTHCHECK)
    Healthcheck,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Captured before config load / cache connect / anything else, so
    // `/api/v1/health`'s reported uptime reflects true process age instead
    // of only the time since the router was built.
    let started_at = Instant::now();

    // Must run before Cli::parse(): the --config flag is env-backed
    // (SERICA_CONFIG_FILE), so a value set in .env has to land in the
    // process environment before clap reads it.
    dotenvy::dotenv().ok();

    let cli = Cli::parse();

    // Load configuration
    let config = serica_search::config::Config::load(cli.config.as_deref())?;
    // Catch brick-prone values (e.g. rate_limit_per_minute/port/
    // ttl_seconds == 0, an unparseable log_level, an out-of-range
    // safe_search_default) here, before logging/cache/engine setup and
    // before the `serica healthcheck` short-circuit below — both paths
    // depend on a sane config, and failing here gives one clear,
    // actionable error instead of a startup panic or silent misbehavior
    // discovered later inside a handler or background task.
    config.validate()?;
    let config = Arc::new(config);

    // Healthcheck is intentionally handled before logging/cache/engine setup:
    // it runs every 30s from the container HEALTHCHECK and must reflect only
    // whether the HTTP server is accepting connections, not whether Redis
    // happens to be reachable at that instant.
    if matches!(cli.command, Some(Command::Healthcheck)) {
        return run_healthcheck(config.server.port).await;
    }

    // `serica mcp` must not silently start when the operator has
    // disabled MCP via config — checked here, before logging/cache/engine
    // setup, so a misconfigured `mcp.enabled = false` fails fast with one
    // clear error instead of the stdio server coming up anyway.
    if matches!(cli.command, Some(Command::Mcp)) {
        config.check_mcp_enabled()?;
    }

    // Initialize logging. MCP mode must send logs to stderr, not stdout —
    // stdout is reserved exclusively for JSON-RPC messages, and an
    // interleaved log line breaks any client parsing stdout strictly one
    // JSON-RPC value per line (e.g. Cline). The HTTP server keeps logging to
    // stdout, matching the container HEALTHCHECK/docker-compose.yml
    // expectation that logs land there.
    let is_mcp = matches!(cli.command, Some(Command::Mcp));
    serica_search::util::tracing::init_logging(&config, is_mcp);

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        log_level = %config.server.log_level,
        log_format = %config.server.log_format,
        "Serica starting"
    );

    // An extract proxy is a deliberate, fail-closed-by-visibility
    // tradeoff, not a silent one — `build_pinned_extract_client` honors
    // `proxy.extract_url` when set, but doing so defeats the `.resolve()`
    // DNS-rebinding pin `util::ssrf::validate_extract_target` exists to
    // enforce (the proxy, not this client, resolves the target hostname).
    // Logged once here at startup, unconditionally at WARN regardless of
    // `server.log_level`, so it can't be missed the way a per-request log
    // line would be.
    if config.proxy.extract_url.is_some() {
        tracing::warn!(
            "SERICA_PROXY__EXTRACT_URL is set: /api/v1/extract and serica_extract requests will \
             be routed through a proxy. This DISABLES the DNS-rebinding / SSRF IP-pinning \
             protection in util::ssrf — the proxy resolves the target hostname itself, so the \
             address validate_extract_target checked is not guaranteed to be the address \
             actually dialled. Private-range protection depends entirely on the proxy's own \
             network egress policy. Do not set this unless the proxy itself enforces equivalent \
             SSRF protections."
        );
    }

    // Build cache layer: Redis if configured + feature enabled, otherwise Noop
    let cache: Arc<dyn CacheLayer> = {
        let redis_url = config.cache.redis_url.as_deref().unwrap_or("");
        if !redis_url.is_empty() {
            #[cfg(feature = "redis-cache")]
            {
                match serica_search::cache::redis::RedisCache::new(
                    redis_url,
                    Duration::from_secs(config.cache.ttl_seconds),
                )
                .await
                {
                    Ok(redis_cache) => {
                        tracing::info!("Redis cache enabled at {}", redis_url);
                        Arc::new(redis_cache)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "Redis unavailable, falling back to noop cache");
                        Arc::new(NoopCache)
                    }
                }
            }
            #[cfg(not(feature = "redis-cache"))]
            {
                // A configured redis_url with the feature off means caching silently
                // never happens in production — fail fast instead of masking it with a WARN.
                return Err(format!(
                    "cache.redis_url is set to '{}' but this binary was built without the \
                     redis-cache feature; rebuild with --features redis-cache or unset the \
                     Redis URL to run uncached",
                    redis_url
                )
                .into());
            }
        } else {
            tracing::debug!("No Redis URL configured; using noop cache");
            Arc::new(NoopCache)
        }
    };

    // Build DuckDuckGo adapter (routed through a proxy when configured)
    let engine = Arc::new(match config.proxy.search_url.as_deref() {
        Some(proxy_url) => DuckDuckGoAdapter::with_proxy(proxy_url)
            .map_err(|e| format!("proxy.search_url is set to an invalid proxy URL: {e}"))?,
        None => DuckDuckGoAdapter::new(),
    });
    let metrics = Arc::new(MetricsCollector::new());

    // Extract pipeline plan, Change 6: constructed once here, before command
    // dispatch, and passed into whichever gateway constructor the matched
    // arm below calls — only one arm ever runs per process invocation, so a
    // direct move (rather than an extra `.clone()`) is fine.
    let extract_limiter =
        serica_search::util::extract::ExtractLimiter::new(config.server.max_concurrent_extracts);

    match &cli.command {
        Some(Command::Mcp) => {
            tracing::info!("Starting MCP stdio server");
            let gateway = McpGateway::new(config.clone(), engine, cache, metrics, extract_limiter);
            gateway.run_stdio().await?;
        }
        Some(Command::Healthcheck) => unreachable!("handled above, before cache/engine setup"),
        None | Some(Command::Server) => {
            tracing::info!(
                host = %config.server.host,
                port = %config.server.port,
                "Starting HTTP server"
            );

            // Spawned only for the HTTP server: it exists to back the
            // `/api/v1/health/*` routes, which the MCP stdio server doesn't
            // expose, so starting it there would just be an unrequested,
            // unbounded-lifetime source of outbound DDG traffic.
            let engine_health = Arc::new(EngineHealth::new());
            spawn_health_monitor(engine.clone(), engine_health.clone());

            // Owned here (not inside `router()`) so the same flag can
            // be shared with the `with_graceful_shutdown` future below —
            // flipping it during drain is what lets `/api/v1/health/*`
            // report "shutting_down" / 503 while the listener is closing.
            let is_shutting_down = Arc::new(AtomicBool::new(false));

            let gateway = HttpGateway::new(
                config.clone(),
                engine,
                engine_health,
                metrics,
                cache,
                is_shutting_down.clone(),
                started_at,
                extract_limiter,
            );
            let router = gateway.router()?;
            let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
                .parse()
                .expect("Invalid server address");

            let listener = tokio::net::TcpListener::bind(addr).await?;

            tracing::info!("Listening on {}", addr);

            // `with_connect_info` populates `ConnectInfo<SocketAddr>` on every
            // request, which the rate limiter's `PeerIpKeyExtractor` (see
            // gateway/http.rs) needs to identify per-client peers —
            // without it every request would fail key extraction.
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(async move {
                shutdown_signal().await;

                // Flip the shared flag first so `/api/v1/health/*`
                // (reading it from `AppState`) starts reporting
                // "shutting_down" / 503 immediately, then hold the listener
                // open for `shutdown_drain_timeout_seconds` before letting
                // this future resolve — giving the load balancer time to
                // observe the state change and stop routing new traffic here
                // before axum actually closes the listener and this future's
                // resolution lets in-flight requests finish draining. This
                // gives the config value a real effect without needing an
                // axum version upgrade.
                is_shutting_down.store(true, Ordering::SeqCst);
                tracing::info!(
                    drain_timeout_seconds = config.server.shutdown_drain_timeout_seconds,
                    "Marked not-ready; draining before listener close"
                );
                tokio::time::sleep(Duration::from_secs(
                    config.server.shutdown_drain_timeout_seconds,
                ))
                .await;
            })
            .await?;

            tracing::info!("Server shutdown complete");
        }
    }

    Ok(())
}

/// Backs `serica healthcheck`, invoked by the container `HEALTHCHECK` instead
/// of `curl` — avoids shipping a network-capable binary in the
/// runtime image purely to poll ourselves. Targets loopback directly rather
/// than `config.server.host`, since the health probe always runs inside the
/// same network namespace as the server regardless of its configured bind
/// address (e.g. `0.0.0.0`).
async fn run_healthcheck(port: u16) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://127.0.0.1:{port}/api/v1/health/live");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()?;

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => Ok(()),
        Ok(resp) => {
            eprintln!("healthcheck failed: {} returned {}", url, resp.status());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("healthcheck failed: {e}");
            std::process::exit(1);
        }
    }
}

/// Graceful shutdown handler — waits for SIGTERM or SIGINT
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("Received Ctrl+C, starting shutdown"),
        _ = terminate => tracing::info!("Received SIGTERM, starting shutdown"),
    }
}

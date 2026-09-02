// Structured logging setup with tracing and tracing-subscriber
// Supports JSON format (default) and pretty format

use crate::config::Config;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::EnvFilter;

/// Initialize the global tracing/logging subscriber.
/// Must be called once at startup, before any other logging.
/// Panics if called more than once (guaranteed by tracing_subscriber).
///
/// `use_stderr` must be `true` for the MCP stdio server: stdout there
/// carries JSON-RPC messages exclusively (one value per line), and any log
/// line written to it — even a well-formed JSON one, which is what the
/// default `json` log format produces — desyncs a client parsing stdout
/// strictly as JSON-RPC. The HTTP server has no such constraint and keeps
/// logging to stdout, matching the container HEALTHCHECK/docker-compose.yml
/// expectation that logs land there.
pub fn init_logging(config: &Config, use_stderr: bool) {
    let filter = EnvFilter::builder()
        .with_default_directive(
            config
                .server
                .log_level
                .parse()
                .expect("Invalid log level in config"),
        )
        .from_env_lossy();

    let writer = if use_stderr {
        BoxMakeWriter::new(std::io::stderr)
    } else {
        BoxMakeWriter::new(std::io::stdout)
    };

    if config.server.log_format == "pretty" {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_target(true)
            .with_thread_ids(true)
            .with_file(true)
            .with_line_number(true)
            .with_writer(writer)
            .init();
    } else {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter)
            .with_current_span(true)
            .with_span_list(true)
            .with_target(true)
            .with_thread_ids(false)
            .with_writer(writer)
            .init();
    }
}

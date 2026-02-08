use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry, fmt};

/// Guard that must be held alive for non-blocking writer flush on shutdown.
/// When dropped, buffered JSONL lines are flushed to disk.
pub(crate) struct TracingGuard {
    _guards: Vec<WorkerGuard>,
}

/// Initialize the layered tracing subscriber.
///
/// Layers:
/// 1. Console — enabled only when `console` is true (daemon mode), filtered by `RUST_LOG`
/// 2. JSONL file — activated by `COOP_TRACE_FILE` env var, filtered at `debug`
///
/// TUI commands (`chat`, `attach`) pass `console: false` to avoid polluting the terminal.
/// Returns a guard that must be held in `main()` to ensure buffered writes flush.
pub(crate) fn init(console: bool) -> TracingGuard {
    let mut guards = Vec::new();

    let console_layer = if console {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,libsignal_service=warn,libsignal_protocol=warn")
        });
        Some(fmt::layer().with_target(false).with_filter(filter))
    } else {
        None
    };

    let jsonl_layer = if let Ok(trace_file) = std::env::var("COOP_TRACE_FILE") {
        let path = std::path::PathBuf::from(&trace_file);
        let dir = path
            .parent()
            .unwrap_or(std::path::Path::new("."))
            .to_path_buf();
        let filename = path.file_name().map_or_else(
            || "traces.jsonl".to_string(),
            |f| f.to_string_lossy().into_owned(),
        );

        let file_appender = tracing_appender::rolling::never(dir, filename);
        let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);
        guards.push(guard);

        let jsonl_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("debug,libsignal_service=warn,libsignal_protocol=warn,presage=info")
        });

        Some(
            fmt::layer()
                .json()
                .with_writer(non_blocking)
                .with_span_list(true)
                .with_file(true)
                .with_line_number(true)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                .with_filter(jsonl_filter),
        )
    } else {
        None
    };

    Registry::default()
        .with(console_layer)
        .with(jsonl_layer)
        .init();

    TracingGuard { _guards: guards }
}

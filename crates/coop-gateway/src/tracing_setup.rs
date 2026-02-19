use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{EnvFilter, Registry, fmt};

/// Guard that must be held alive for non-blocking writer flush on shutdown.
/// When dropped, buffered JSONL lines are flushed to disk.
pub(crate) struct TracingGuard {
    _guards: Vec<WorkerGuard>,
}

/// Rotating appender that always writes to a stable path (e.g. `traces.jsonl`).
///
/// On rotation the current file is renamed to a dated archive
/// (`traces.2026-02-09.jsonl`, `traces.2026-02-09.001.jsonl`, …) and a fresh
/// file is opened at the original path. Rotation triggers when:
///
/// * the calendar date (UTC) changes, or
/// * the current file would exceed `max_bytes`.
struct RotatingAppender {
    state: Arc<Mutex<RotatingState>>,
}

struct RotatingState {
    /// The path we always write to (e.g. `./traces.jsonl`).
    current_path: PathBuf,
    /// Prefix used when building archive names (e.g. `traces`).
    prefix: String,
    /// Directory that holds all trace files.
    dir: PathBuf,
    /// Size limit per file.
    max_bytes: u64,
    /// Open handle to `current_path`.
    writer: Option<File>,
    /// Bytes written since last rotation.
    written: u64,
    /// UTC date string (`YYYY-MM-DD`) when the current file was opened.
    opened_date: String,
}

impl RotatingAppender {
    fn new(path: PathBuf, max_bytes: u64) -> Self {
        let dir = path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();

        let prefix = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("traces.jsonl")
            .strip_suffix(".jsonl")
            .unwrap_or("traces")
            .to_owned();

        Self {
            state: Arc::new(Mutex::new(RotatingState {
                current_path: path,
                prefix,
                dir,
                max_bytes,
                writer: None,
                written: 0,
                opened_date: String::new(),
            })),
        }
    }
}

fn today() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

/// Pick the next free archive name for the given date.
///
/// Returns e.g. `traces.2026-02-09.jsonl` or `traces.2026-02-09.001.jsonl`.
fn archive_name(dir: &std::path::Path, prefix: &str, date: &str) -> PathBuf {
    let base = dir.join(format!("{prefix}.{date}.jsonl"));
    if !base.exists() {
        return base;
    }
    let mut seq: u32 = 1;
    loop {
        let candidate = dir.join(format!("{prefix}.{date}.{seq:03}.jsonl"));
        if !candidate.exists() {
            return candidate;
        }
        seq += 1;
    }
}

/// Rotate the current file out and open a fresh one.
///
/// 1. Close the current writer (implicitly, by dropping).
/// 2. Rename `current_path` → archive name.
/// 3. Open a new file at `current_path`.
fn rotate(s: &mut RotatingState) -> io::Result<()> {
    // Drop the old writer so the file handle is released.
    s.writer.take();

    // Only rename if the file exists and is non-empty.
    if s.current_path.exists() {
        let meta = fs::metadata(&s.current_path)?;
        if meta.len() > 0 {
            let dest = archive_name(&s.dir, &s.prefix, &s.opened_date);
            fs::rename(&s.current_path, dest)?;
        }
    }

    open_fresh(s)
}

/// Open (or create) the current path for appending and sync bookkeeping.
fn open_fresh(s: &mut RotatingState) -> io::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&s.current_path)?;
    let len = file.metadata()?.len();
    s.writer = Some(file);
    s.written = len;
    s.opened_date = today();
    Ok(())
}

fn needs_rotation(s: &RotatingState, incoming: usize) -> bool {
    if s.writer.is_none() {
        return false; // will be handled by the "first write" path
    }
    // Date changed → rotate.
    if s.opened_date != today() {
        return true;
    }
    // Size limit would be exceeded → rotate.
    s.written + incoming as u64 > s.max_bytes
}

impl Write for RotatingAppender {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut s = self.state.lock().expect("tracing lock poisoned");

        // First write ever – just open the file (reuse existing if present).
        if s.writer.is_none() {
            // If the file already exists from a previous run, check whether we
            // need to archive it (different date or already too large).
            if s.current_path.exists() {
                let meta = fs::metadata(&s.current_path)?;
                // Guess the date the file was last modified.
                let file_date = meta
                    .modified()
                    .ok()
                    .map(|t| {
                        let dt: chrono::DateTime<chrono::Utc> = t.into();
                        dt.format("%Y-%m-%d").to_string()
                    })
                    .unwrap_or_default();

                if (!file_date.is_empty() && file_date != today())
                    || meta.len() + buf.len() as u64 > s.max_bytes
                {
                    // Archive the leftover file under its original date.
                    if meta.len() > 0 {
                        s.opened_date = file_date;
                        rotate(&mut s)?;
                    } else {
                        open_fresh(&mut s)?;
                    }
                } else {
                    open_fresh(&mut s)?;
                }
            } else {
                open_fresh(&mut s)?;
            }
        } else if needs_rotation(&s, buf.len()) {
            rotate(&mut s)?;
        }

        let writer = s.writer.as_mut().expect("writer should be open");
        let n = writer.write(buf)?;
        s.written += n as u64;
        drop(s);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut s = self.state.lock().expect("tracing lock poisoned");
        if let Some(ref mut w) = s.writer {
            w.flush()
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Subscriber initialisation
// ---------------------------------------------------------------------------

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

    let console_layer = console.then(|| {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,libsignal_service=info,libsignal_service::sender=debug,libsignal_service::websocket=info,libsignal_service::websocket::sender=info,libsignal_protocol=warn,presage=info,presage::manager::registered=debug,presage_store_sqlite=warn,sqlx=warn,hyper_util=warn,reqwest_websocket=warn")
        });
        fmt::layer()
            .compact()
            .without_time()
            .with_target(false)
            .with_filter(filter)
    });

    let jsonl_layer = if let Ok(trace_file) = std::env::var("COOP_TRACE_FILE") {
        let path = PathBuf::from(&trace_file);

        let max_bytes = std::env::var("COOP_TRACE_MAX_SIZE")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(50 * 1024 * 1024);

        let appender = RotatingAppender::new(path, max_bytes);
        let (non_blocking, guard) = tracing_appender::non_blocking(appender);
        guards.push(guard);

        let jsonl_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("debug,libsignal_service=info,libsignal_service::sender=debug,libsignal_service::websocket=info,libsignal_service::websocket::sender=debug,libsignal_protocol=warn,presage=info,presage::manager::registered=debug,presage_store_sqlite=warn,sqlx=warn,hyper_util=warn,reqwest_websocket=warn")
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

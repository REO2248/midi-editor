//! Rolling structured logs + one-click diagnostics bundle (issue #6).
//!
//! - `tracing` writes to a daily-rotating file under `%APPDATA%/midi-editor/
//!   logs/`, retained for `LOG_KEEP` files, via a `non_blocking` writer so
//!   log calls never stall the UI/audio path (the writer thread absorbs
//!   flush latency).
//! - `install_panic_hook` routes panic location + captured backtrace into
//!   the same log (and keeps the stderr print).
//! - `export_bundle` gathers environment/host facts plus the tail of each
//!   retained log into a single text file a user can attach to a bug
//!   report. It deliberately contains NO MIDI file content and redacts
//!   credential-shaped values (`token=`, `Bearer`, `Authorization`,
//!   `MIDI_MCP_TOKEN` values) regardless of what upstream logged.
//!
//! Privacy: logs contain file paths, destination/plugin names, and error
//! text — never note/event payload bytes. Nothing leaves the machine until
//! the user sends the bundle themselves.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_subscriber::prelude::*;

/// Rolling file retention — daily rotation, keep this many files.
pub(crate) const LOG_KEEP: usize = 7;
/// Bytes of each log file folded into a bundle (from the tail).
const BUNDLE_TAIL_BYTES: u64 = 256 * 1024;

static LOG_GUARD: OnceLock<tracing_appender::non_blocking::WorkerGuard> = OnceLock::new();

/// The active document, registered by the app at startup so the panic hook
/// can write an emergency recovery snapshot (#200). A weak reference: the
/// hook never keeps a closed document alive.
static CRASH_DOC: std::sync::Mutex<Option<std::sync::Weak<std::sync::Mutex<mcp_server::Shared>>>> =
    std::sync::Mutex::new(None);

/// Register the active shared document for the panic hook (#200). Call once
/// after the doc is created; reopening re-registers over the old handle.
pub(crate) fn register_crash_doc(shared: &mcp_server::SharedDoc) {
    if let Ok(mut slot) = CRASH_DOC.lock() {
        *slot = Some(std::sync::Arc::downgrade(shared));
    }
}

/// Append one panic record to `panic.log` next to the rolling logs —
/// synchronous, no buffering: the non-blocking tracing writer's queued
/// records are routinely lost when the process dies mid-panic, so the
/// on-disk panic reason and backtrace cannot depend on it (#200).
fn append_panic_log(loc: &str, msg: &str, bt: &str) {
    let path = log_dir().join("panic.log");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("panic at {ts}s {loc}: {msg}\n{bt}\n");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = f.write_all(line.as_bytes());
    }
}

pub(crate) fn app_data_dir() -> PathBuf {
    std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir())
        .join("midi-editor")
}

pub(crate) fn log_dir() -> PathBuf {
    app_data_dir().join("logs")
}

/// Initialize rolling-file + stderr tracing. Idempotent.
/// Returns the log directory (created if needed).
pub(crate) fn init_logging() -> PathBuf {
    let dir = log_dir();
    let _ = std::fs::create_dir_all(&dir);
    let appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("midi-editor")
        // files land as `midi-editor.<date>.log` — the suffix makes them
        // recognizable to humans AND to export_bundle's filter below
        .filename_suffix("log")
        .max_log_files(LOG_KEEP)
        .build(&dir)
        .unwrap_or_else(|_| {
            // can't create the appender (readonly app-data) — still run, just
            // without the file layer
            tracing_appender::rolling::never(dir.parent().unwrap_or(&dir), "unreachable")
        });
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let _ = LOG_GUARD.set(guard);
    let _ = tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false),
        )
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false),
        )
        .with(tracing_subscriber::filter::LevelFilter::INFO)
        .try_init();
    dir
}

/// Panic location + backtrace land in the rolling log AND in a dedicated
/// `panic.log` written synchronously (the buffered tracing writer can lose
/// its tail when the process dies). Before returning, the hook writes a
/// best-effort emergency recovery snapshot of the active document (#200) —
/// at most one panic of work is lost instead of everything since the last
/// autosave tick. Every step is individually infallible: a failing write
/// must never panic inside the hook.
pub(crate) fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".into());
        let bt = std::backtrace::Backtrace::capture();
        // the emergency snapshot first — it is the unsaved work
        if let Ok(slot) = CRASH_DOC.lock() {
            if let Some(shared) = slot.as_ref().and_then(|w| w.upgrade()) {
                if let Some(path) = crate::recovery::write_emergency_snapshot(
                    &shared,
                    &crate::recovery::recovery_dir(),
                ) {
                    eprintln!("emergency snapshot written: {}", path.display());
                }
            }
        }
        tracing::error!(target: "panic", location = %loc, "{info}\n{bt}");
        append_panic_log(&loc, &format!("{info}"), &bt.to_string());
        eprintln!("panic at {loc}: {info}\n{bt}");
    }));
}

/// Startup facts worth having when reading a post-mortem log.
pub(crate) fn log_boot() {
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        profile = if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
        "midi-editor starting"
    );
}

/// Reveal `dir` in Explorer (fire-and-forget; ignore launch failures).
pub(crate) fn open_in_explorer(path: &Path) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}

/// Reveal `file` selected in Explorer.
pub(crate) fn reveal_file(path: &Path) {
    let _ = std::process::Command::new("explorer")
        .arg(format!("/select,{}", path.display()))
        .spawn();
}

/// Scrub credential-shaped values out of a log line destined for the
/// bundle. Conservative: anything after a token-ish keyword gets masked.
fn redact_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let lower = line.to_ascii_lowercase();
    let mut i = 0;
    while i < line.len() {
        // find the next keyword among several secret shapes
        let rest = &lower[i..];
        let hit = [
            "token",
            "bearer",
            "authorization",
            "api_key",
            "apikey",
            "secret",
        ]
        .iter()
        .filter_map(|k| rest.find(k).map(|p| (p + i, k.len())))
        .min_by_key(|(p, _)| *p);
        match hit {
            None => {
                out.push_str(&line[i..]);
                break;
            }
            Some((pos, klen)) => {
                out.push_str(&line[i..pos + klen]);
                i = pos + klen;
                // echo the separator (=, :, whitespace, quotes) verbatim,
                // then mask the value run up to the next delimiter
                while i < line.len()
                    && matches!(line.as_bytes()[i], b'=' | b':' | b' ' | b'"' | b'\'')
                {
                    out.push(line.as_bytes()[i] as char);
                    i += 1;
                }
                let start = i;
                while i < line.len()
                    && !matches!(line.as_bytes()[i], b' ' | b',' | b'"' | b'\'' | b';')
                {
                    i += 1;
                }
                if i > start {
                    out.push_str("[REDACTED]");
                }
            }
        }
    }
    out
}

/// Last `n` bytes of `path`, snapped to a UTF-8 boundary.
fn file_tail(path: &Path, n: u64) -> io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path)?;
    let len = f.metadata()?.len();
    let from = len.saturating_sub(n);
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    let mut start = 0usize;
    while start < buf.len() && (buf[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1; // skip continuation byte of a split multibyte char
    }
    Ok(String::from_utf8_lossy(&buf[start..]).into_owned())
}

/// Write `midi-editor-diagnostics-<unix-ts>.txt` into `dir` containing
/// environment facts, `host_lines` (caller-supplied system info), and the
/// redacted tail of every retained log file. Returns the bundle path.
pub(crate) fn export_bundle(dir: &Path, host_lines: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = dir.join(format!("midi-editor-diagnostics-{ts}.txt"));
    let mut out = String::new();
    out.push_str(&format!(
        "midi-editor diagnostics bundle\nversion={} profile={} os={} arch={} generated_unix={ts}\n\n",
        env!("CARGO_PKG_VERSION"),
        if cfg!(debug_assertions) { "debug" } else { "release" },
        std::env::consts::OS,
        std::env::consts::ARCH,
    ));
    out.push_str("=== host ===\n");
    for line in host_lines.lines() {
        out.push_str(&redact_line(line));
        out.push('\n');
    }
    out.push_str("\n=== logs ===\n");
    let mut logs: Vec<PathBuf> = std::fs::read_dir(log_dir())
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_type().map(|t| t.is_file()).unwrap_or(false) && {
                        let n = e.file_name().to_string_lossy().into_owned();
                        n.starts_with("midi-editor.") && n.ends_with(".log")
                    }
                })
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default();
    // newest first by mtime
    logs.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
    logs.reverse();
    for lf in logs.iter().take(LOG_KEEP) {
        out.push_str(&format!(
            "\n--- {} ---\n",
            lf.file_name().unwrap_or_default().to_string_lossy()
        ));
        match file_tail(lf, BUNDLE_TAIL_BYTES) {
            Ok(tail) => {
                for line in tail.lines() {
                    out.push_str(&redact_line(line));
                    out.push('\n');
                }
            }
            Err(e) => out.push_str(&format!("<unreadable: {e}>\n")),
        }
    }
    std::fs::write(&path, &out)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_masks_token_and_bearer() {
        assert_eq!(
            redact_line(r#"auth token=abc123 other=x Bearer "zzz""#),
            "auth token=[REDACTED] other=x Bearer \"[REDACTED]\""
        );
        assert_eq!(
            redact_line("Authorization: ghp_secretvalue more"),
            "Authorization: [REDACTED] more"
        );
        // nothing to redact — passthrough
        assert_eq!(redact_line("opened file foo.mid"), "opened file foo.mid");
    }

    #[test]
    fn file_tail_reads_tail_at_utf8_boundary() {
        let d = std::env::temp_dir().join(format!("diag-tail-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let f = d.join("x.log");
        let long = format!("{}{}", "a".repeat(5000), "tail-日本語-end");
        std::fs::write(&f, &long).unwrap();
        let tail = file_tail(&f, 64).unwrap();
        assert!(tail.ends_with("tail-日本語-end"));
        assert!(tail.len() <= 64 + 8);
        // small file: whole file
        let small = file_tail(&f, 10_000_000).unwrap();
        assert_eq!(small, long);
    }

    #[test]
    fn bundle_includes_header_and_redacted_logs() {
        let d = std::env::temp_dir().join(format!("diag-bundle-{}", std::process::id()));
        let p = export_bundle(&d, "audio_device=ok token=hunter2").unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("version="));
        assert!(body.contains("=== host ==="));
        assert!(body.contains("audio_device=ok"));
        // host_lines are run through redact_line — secrets must not survive
        assert!(!body.contains("hunter2"));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn bundle_finds_rotated_log_files() {
        // regression: tracing-appender names files `midi-editor.<date>.log`
        // — the bundle filter must match that shape, not `midi-editor.log*`
        let dir = log_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("midi-editor.2099-01-01.log");
        std::fs::write(&fake, "marker-line-rotation-shape").unwrap();
        let d = std::env::temp_dir().join(format!("diag-bundle2-{}", std::process::id()));
        let p = export_bundle(&d, "").unwrap();
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.contains("marker-line-rotation-shape"));
        std::fs::remove_file(&fake).ok();
        std::fs::remove_dir_all(&d).ok();
    }
}

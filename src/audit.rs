//! Audit log: one JSON line per tool call or rejected request, in `~/Library/Logs/msgmcp/`
//! by default (shown by Console.app under Log Reports). It records what was asked, never
//! what was returned: tool arguments (including text sent with send_message) and outcomes.
//! The dashboard reads it back.

use anyhow::Result;
use serde_json::{Value, json};
use std::{
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

const MAX_BYTES: u64 = 10 * 1024 * 1024;
const KEPT_FILES: usize = 5;

struct Log {
    path: PathBuf,
    transport: &'static str,
    lock: Mutex<()>,
}

static LOG: OnceLock<Log> = OnceLock::new();

/// Start logging to `path` for this process; `transport` is "socket" or "gateway".
pub fn init(path: PathBuf, transport: &'static str) -> Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)?;
    }
    let _ = LOG.set(Log {
        path,
        transport,
        lock: Mutex::new(()),
    });
    Ok(())
}

// Keep audit.log plus audit.1.log … audit.5.log, newest first.
fn rotate(path: &Path) {
    let numbered = |n: usize| path.with_extension(format!("{n}.log"));
    let _ = std::fs::remove_file(numbered(KEPT_FILES));
    for n in (1..KEPT_FILES).rev() {
        let _ = std::fs::rename(numbered(n), numbered(n + 1));
    }
    let _ = std::fs::rename(path, numbered(1));
}

/// Where this process logs, if logging was started.
pub fn path() -> Option<&'static Path> {
    LOG.get().map(|log| log.path.as_path())
}

/// The newest `limit` events, newest first, across audit.log and its rotated files, and
/// whether older ones were left out. Lines that aren't JSON (a write in progress) are skipped.
pub fn recent(limit: usize) -> (Vec<Value>, bool) {
    let Some(path) = path() else {
        return (Vec::new(), false);
    };
    let files = std::iter::once(path.to_owned())
        .chain((1..=KEPT_FILES).map(|n| path.with_extension(format!("{n}.log"))));
    let mut events = Vec::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines().rev() {
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if events.len() == limit {
                return (events, true);
            }
            events.push(event);
        }
    }
    (events, false)
}

/// Append one event. Logging failures go to stderr and never fail the request.
pub fn record(event: &str, mut fields: Value) {
    let Some(log) = LOG.get() else { return };
    if let Value::Object(map) = &mut fields {
        map.insert("time".into(), json!(chrono::Utc::now().to_rfc3339()));
        map.insert("transport".into(), json!(log.transport));
        map.insert("event".into(), json!(event));
    }
    let _guard = log.lock.lock().unwrap_or_else(|e| e.into_inner());
    if std::fs::metadata(&log.path).is_ok_and(|m| m.len() > MAX_BYTES) {
        rotate(&log.path);
    }
    let written = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log.path)
        .and_then(|mut file| writeln!(file, "{fields}"));
    if let Err(error) = written {
        eprintln!("Audit log write failed: {error}");
    }
}

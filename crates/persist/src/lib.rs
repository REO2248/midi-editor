//! Persistence core shared by the editor frontends: durable atomic writes
//! and recovery hooks. The document lifecycle service built on top of this
//! (`mcp-server::service`) is what GUI, MCP, and stdio saves all route
//! through, so durability and revision semantics cannot diverge.

mod atomic;
pub mod json;

pub use atomic::{write_atomic, write_atomic_opts, AtomicOptions, PersistError, Phase};

use std::path::{Path, PathBuf};

/// Prefix that `write_atomic` gives temp files: `.{name}.sav-*`.
fn temp_prefix(stem: &str) -> String {
    format!(".{stem}.sav")
}

/// Temp files left beside `path` by saves that never completed (crashes,
/// failed replaces). Callers may surface them as a recovery hint.
pub fn temp_siblings(path: &Path) -> Vec<PathBuf> {
    let Some(dir) = path.parent() else {
        return Vec::new();
    };
    let stem = path.file_name().map(|n| n.to_string_lossy());
    let Some(stem) = stem else { return Vec::new() };
    let prefix = temp_prefix(&stem);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy().starts_with(&prefix))
                .unwrap_or(false)
        })
        .collect()
}

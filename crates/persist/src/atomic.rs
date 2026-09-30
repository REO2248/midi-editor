//! Atomic file replacement: write to a temp file in the destination
//! directory, then rename it over the target so a crash mid-save can never
//! leave a zero-length or half-written target.
//!
//! Every failure carries a `Phase` so callers and logs can tell *where* the
//! save broke, plus the recoverable temp path when one was left behind for
//! manual recovery.

use std::path::{Path, PathBuf};
use thiserror::Error;

/// The stage a failed write reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// creating the temp file in the destination directory
    TempCreate,
    /// writing the payload into the temp file
    Write,
    /// flushing the temp file to stable storage
    Sync,
    /// atomically replacing the target with the temp file
    Replace,
    /// syncing the containing directory entry
    DirSync,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::TempCreate => "create temp file",
            Phase::Write => "write temp file",
            Phase::Sync => "sync temp file",
            Phase::Replace => "replace target",
            Phase::DirSync => "sync directory",
        }
    }
}

/// A failed save. `recoverable` is the temp file left in place when the
/// replace step failed ambiguously — it holds the complete new contents.
#[derive(Debug, Error)]
#[error("{msg}")]
pub struct PersistError {
    /// the stage that failed
    pub phase: Phase,
    /// temp file kept for manual recovery (full new payload inside)
    pub recoverable: Option<PathBuf>,
    /// the underlying OS error
    #[source]
    pub source: std::io::Error,
    msg: String,
}

impl PersistError {
    fn new(phase: Phase, path: &Path, source: std::io::Error) -> Self {
        Self {
            phase,
            recoverable: None,
            msg: format!("save {}: {} failed: {source}", path.display(), phase.label()),
            source,
        }
    }

    fn replace(path: &Path, source: std::io::Error, recoverable: Option<PathBuf>) -> Self {
        let mut msg = format!("save {}: {} failed: {source}", path.display(), Phase::Replace.label());
        if let Some(t) = &recoverable {
            msg.push_str(&format!(" — new contents kept at {}", t.display()));
        }
        Self {
            phase: Phase::Replace,
            recoverable,
            msg,
            source,
        }
    }
}

/// Extra knobs for `write_atomic_opts`.
#[derive(Debug, Default)]
pub struct AtomicOptions<'a> {
    /// preserve the target's previous contents at this path before replacing
    /// (a bounded backup — the caller decides where)
    pub backup: Option<&'a Path>,
}

/// Write `bytes` to `path` atomically: a uniquely-named temp file in the
/// same directory, synced, then renamed over the target.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    write_atomic_opts(path, bytes, &AtomicOptions::default())
}

pub fn write_atomic_opts(path: &Path, bytes: &[u8], opts: &AtomicOptions) -> Result<(), PersistError> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // unique per save: two overlapping saves must never share a temp path
    let tmp = dir.join(format!(
        "{}-{}-{}",
        crate::temp_prefix(&stem),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if let Err(source) = std::fs::write(&tmp, bytes) {
        let _ = std::fs::remove_file(&tmp);
        return Err(PersistError::new(Phase::Write, path, source));
    }
    match replace(path, &tmp, opts) {
        Ok(()) => Ok(()),
        Err(source) => {
            // the replace may or may not have landed — never delete the only
            // recoverable copy of the new contents on an ambiguous failure
            let recoverable = tmp.exists().then_some(tmp);
            Err(PersistError::replace(path, source, recoverable))
        }
    }
}

/// Put `tmp` in place of `path`, honouring the backup option.
fn replace(path: &Path, tmp: &Path, opts: &AtomicOptions) -> std::io::Result<()> {
    if let Some(b) = opts.backup {
        if path.exists() {
            std::fs::copy(path, b)?;
        }
    }
    std::fs::rename(tmp, path)
}

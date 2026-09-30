//! Durable atomic file replacement, shared by every save path.
//!
//! Sequence: allocate a unique temp file in the destination directory →
//! write + fsync the payload → preserve the target's permissions on the
//! temp (unix; Windows keeps them via `ReplaceFileW`) → replace → fsync
//! the containing directory so the rename itself survives a crash.
//!
//! Every failure carries a `Phase` so callers and logs can tell *where*
//! the save broke. On an ambiguous replace failure the temp file is left
//! in place and reported in `PersistError::recoverable` — it holds the
//! complete new contents and is the only recovery artifact.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
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
    pub(crate) fn new(phase: Phase, path: &Path, source: std::io::Error) -> Self {
        Self {
            phase,
            recoverable: None,
            msg: format!(
                "save {}: {} failed: {source}",
                path.display(),
                phase.label()
            ),
            source,
        }
    }

    fn replace(path: &Path, source: std::io::Error, recoverable: Option<PathBuf>) -> Self {
        let mut msg = format!(
            "save {}: {} failed: {source}",
            path.display(),
            Phase::Replace.label()
        );
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
    /// preserve the target's previous contents at this path while replacing
    /// (a bounded backup — the caller decides where). On Windows this is
    /// atomic with the replace via `ReplaceFileW`.
    pub backup: Option<&'a Path>,
}

// ---- test-only fault injection -----------------------------------------

#[cfg(test)]
static FAIL_PHASE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// fail the next write at `phase` once (unit tests only)
#[cfg(test)]
fn set_fail_phase(phase: Option<Phase>) {
    FAIL_PHASE.store(phase.map(|p| p as u8 + 1).unwrap_or(0), Ordering::SeqCst);
}

#[inline]
fn inject(phase: Phase) -> std::io::Result<()> {
    #[cfg(test)]
    {
        let want = phase as u8 + 1;
        // clear only on a hit — a pending injection for a later stage must
        // survive checks at earlier stages
        if FAIL_PHASE.load(Ordering::SeqCst) == want {
            FAIL_PHASE.store(0, Ordering::SeqCst);
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "fault-injected",
            ));
        }
    }
    let _ = phase;
    Ok(())
}

// ---- the write path -----------------------------------------------------

/// Write `bytes` to `path` atomically and durably.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), PersistError> {
    write_atomic_opts(path, bytes, &AtomicOptions::default())
}

pub fn write_atomic_opts(
    path: &Path,
    bytes: &[u8],
    opts: &AtomicOptions,
) -> Result<(), PersistError> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let (mut file, tmp) =
        create_temp(dir, &stem).map_err(|e| PersistError::new(Phase::TempCreate, path, e))?;
    // write + fsync in one stage-tracked block so failures carry the right
    // phase; on error the weakly-durable temp is removed so nothing later
    // can mistake it for a recoverable artifact
    let mut phase = Phase::Write;
    let res = (|| -> std::io::Result<()> {
        inject(phase)?;
        file.write_all(bytes)?;
        phase = Phase::Sync;
        inject(phase)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(source) = res {
        let _ = std::fs::remove_file(&tmp);
        return Err(PersistError::new(phase, path, source));
    }
    drop(file);

    preserve_permissions(path, &tmp);

    inject(Phase::Replace)
        .and_then(|()| replace(path, &tmp, opts.backup))
        .map_err(|source| {
            // the replace may or may not have landed — never delete the
            // only recoverable copy of the new contents on ambiguity
            let recoverable = tmp.exists().then_some(tmp.clone());
            PersistError::replace(path, source, recoverable)
        })?;

    inject(Phase::DirSync)
        .and_then(|()| sync_dir(dir))
        .map_err(|e| PersistError::new(Phase::DirSync, path, e))?;
    Ok(())
}

/// unique temp name per save: pid + counter + nanos means two overlapping
/// saves (same thread or cross-process) never collide on a temp path.
static TEMP_CTR: AtomicU64 = AtomicU64::new(0);

fn unique_temp(dir: &Path, stem: &str) -> PathBuf {
    let ctr = TEMP_CTR.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(
        "{}-{}-{}-{}",
        crate::temp_prefix(&stem),
        std::process::id(),
        ctr,
        nanos
    ))
}

fn create_temp(dir: &Path, stem: &str) -> std::io::Result<(std::fs::File, PathBuf)> {
    for _ in 0..16 {
        inject(Phase::TempCreate)?;
        let tmp = unique_temp(dir, stem);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => return Ok((f, tmp)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "could not allocate a unique temp name",
    ))
}

/// Put `tmp` in place of `path`. Windows prefers `ReplaceFileW`, which
/// keeps the replaced file's security descriptor/attributes (metadata-safe)
/// and can write the previous contents to `backup` atomically; targets
/// that don't exist yet or filesystems where ReplaceFileW is unsupported
/// fall back to `MoveFileExW` with write-through. Concurrent replaces of
/// the same target return transient sharing errors — the whole replace
/// step retries briefly before giving up.
#[cfg(windows)]
fn replace(path: &Path, tmp: &Path, backup: Option<&Path>) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{GetLastError, ERROR_CALL_NOT_IMPLEMENTED};
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
        REPLACEFILE_WRITE_THROUGH,
    };

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    let try_once = || -> std::io::Result<()> {
        if path.exists() {
            let backup_w = backup.map(wide);
            let backup_ptr = backup_w
                .as_ref()
                .map(|v| v.as_ptr())
                .unwrap_or(std::ptr::null());
            // SAFETY: all pointers reference valid null-terminated wide
            // strings living at least as long as the call; lpExclude and
            // lpReserved are reserved and stay null.
            let ok = unsafe {
                ReplaceFileW(
                    wide(path).as_ptr(),
                    wide(tmp).as_ptr(),
                    backup_ptr,
                    REPLACEFILE_WRITE_THROUGH,
                    std::ptr::null(),
                    std::ptr::null(),
                )
            };
            if ok != 0 {
                return Ok(());
            }
            // SAFETY: reads only the thread's last-error value.
            let err = unsafe { GetLastError() };
            // filesystems without ReplaceFileW support (non-NTFS, some
            // network drives) report these — fall through to MoveFileExW
            const ERROR_NOT_SUPPORTED: u32 = 50;
            const ERROR_INVALID_FUNCTION: u32 = 1;
            if !matches!(
                err,
                ERROR_CALL_NOT_IMPLEMENTED | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION
            ) {
                return Err(std::io::Error::from_raw_os_error(err as i32));
            }
        }
        // SAFETY: valid null-terminated wide strings; write-through waits
        // for the rename to hit stable storage before returning.
        let ok = unsafe {
            MoveFileExW(
                wide(tmp).as_ptr(),
                wide(path).as_ptr(),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    };

    retry_transient(try_once)
}

/// transient sharing/racing errors worth a short retry — overlapping saves
/// to the same target hit these (ReplaceFileW's UNABLE_TO_MOVE family,
/// sharing and lock violations, ACCESS_DENIED on an open handle)
#[cfg(windows)]
fn is_transient(e: &std::io::Error) -> bool {
    // 2 = target vanished between the exists() check and the call;
    // 5/32/33 = sharing/lock violations; 1175-1178 = ReplaceFileW's
    // UNABLE_TO_MOVE/UNABLE_TO_REMOVE family on a target being
    // concurrently replaced
    matches!(
        e.raw_os_error(),
        Some(2 | 5 | 32 | 33 | 1175 | 1176 | 1177 | 1178)
    )
}

/// portable fallback: atomic rename (POSIX rename(2) replaces atomically)
#[cfg(not(windows))]
fn replace(path: &Path, tmp: &Path, backup: Option<&Path>) -> std::io::Result<()> {
    if let Some(b) = backup {
        if path.exists() {
            std::fs::copy(path, b)?;
        }
    }
    std::fs::rename(tmp, path)
}

#[cfg(not(windows))]
fn is_transient(_e: &std::io::Error) -> bool {
    false
}

/// retry a replace briefly while it keeps losing races to concurrent saves
fn retry_transient(mut f: impl FnMut() -> std::io::Result<()>) -> std::io::Result<()> {
    let mut attempt = 0;
    loop {
        match f() {
            Err(e) if is_transient(&e) && attempt < 10 => {
                attempt += 1;
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            other => return other,
        }
    }
}

/// copy the target's permission bits onto the temp where rename carries
/// them over (unix). Windows doesn't need this — `ReplaceFileW` retains
/// the replaced file's ACL and attributes.
#[cfg(unix)]
fn preserve_permissions(path: &Path, tmp: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(m) = path.metadata() {
        let _ =
            std::fs::set_permissions(tmp, std::fs::Permissions::from_mode(m.permissions().mode()));
    }
}

#[cfg(not(unix))]
fn preserve_permissions(_path: &Path, _tmp: &Path) {}

/// fsync the containing directory so the rename's directory entry is
/// durable too.
#[cfg(windows)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::windows::fs::OpenOptionsExt;
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    // BACKUP_SEMANTICS is what lets CreateFileW produce a directory
    // handle; File::sync_all then calls FlushFileBuffers on it, which
    // requires GENERIC_WRITE (FILE_ADD_FILE) — GENERIC_READ is denied
    std::fs::OpenOptions::new()
        .access_mode(0x4000_0000) // GENERIC_WRITE
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(dir)
        .and_then(|d| d.sync_all())
}

#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir).and_then(|d| d.sync_all())
}

#[cfg(not(any(windows, unix)))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

// fault injection is one process-global flag — every test in this crate
// (atomic + json) takes the guard so no test can consume another's armed phase
#[cfg(test)]
pub(crate) static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;

    fn testdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("midi-editor-persist-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn leftover_temps(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().contains(".sav-"))
            .collect()
    }

    #[test]
    fn unique_temps_no_collision_under_concurrency() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("unique_temps_no_collision_under_concurrency");
        let p = dir.join("song.mid");
        std::fs::write(&p, b"original").unwrap();
        let mut handles = Vec::new();
        for i in 0..8 {
            let p = p.clone();
            handles.push(std::thread::spawn(move || {
                let payload = format!("payload-from-thread-{i}").into_bytes();
                write_atomic(&p, &payload).unwrap();
                payload
            }));
        }
        let payloads: Vec<Vec<u8>> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let final_bytes = std::fs::read(&p).unwrap();
        // the winner is a complete payload — never a torn mix of two
        assert!(payloads.iter().any(|pl| *pl == final_bytes));
        assert!(
            leftover_temps(&dir).is_empty(),
            "no temp litter after success"
        );
    }

    #[test]
    fn write_failure_leaves_target_intact() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("write_failure_leaves_target_intact");
        let p = dir.join("song.mid");
        std::fs::write(&p, b"old").unwrap();
        set_fail_phase(Some(Phase::Write));
        let e = write_atomic(&p, b"new").unwrap_err();
        assert_eq!(e.phase, Phase::Write);
        assert!(e.recoverable.is_none());
        assert_eq!(std::fs::read(&p).unwrap(), b"old");
        assert!(leftover_temps(&dir).is_empty());
    }

    #[test]
    fn sync_failure_leaves_target_intact() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("sync_failure_leaves_target_intact");
        let p = dir.join("song.mid");
        std::fs::write(&p, b"old").unwrap();
        set_fail_phase(Some(Phase::Sync));
        let e = write_atomic(&p, b"new").unwrap_err();
        assert_eq!(e.phase, Phase::Sync);
        assert_eq!(std::fs::read(&p).unwrap(), b"old");
        assert!(leftover_temps(&dir).is_empty());
    }

    #[test]
    fn replace_failure_keeps_recoverable_temp() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("replace_failure_keeps_recoverable_temp");
        let p = dir.join("song.mid");
        std::fs::write(&p, b"old").unwrap();
        set_fail_phase(Some(Phase::Replace));
        let e = write_atomic(&p, b"new").unwrap_err();
        assert_eq!(e.phase, Phase::Replace);
        assert_eq!(std::fs::read(&p).unwrap(), b"old");
        // the only copy of the new contents survives for recovery
        let tmp = e.recoverable.expect("recoverable temp path");
        assert_eq!(std::fs::read(&tmp).unwrap(), b"new");
        assert!(tmp.exists());
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn dirsync_failure_reports_phase_after_replace() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("dirsync_failure_reports_phase_after_replace");
        let p = dir.join("song.mid");
        std::fs::write(&p, b"old").unwrap();
        set_fail_phase(Some(Phase::DirSync));
        let e = write_atomic(&p, b"new").unwrap_err();
        assert_eq!(e.phase, Phase::DirSync);
        // the replace already landed — the error is honest about where it
        // stopped and nothing recoverable is hidden
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert!(e.recoverable.is_none());
    }

    #[test]
    fn backup_holds_previous_contents() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("backup_holds_previous_contents");
        let p = dir.join("song.mid");
        let bak = dir.join("song.mid.bak");
        std::fs::write(&p, b"old").unwrap();
        write_atomic_opts(
            &p,
            b"new",
            &AtomicOptions {
                backup: Some(bak.as_path()),
            },
        )
        .unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new");
        assert_eq!(std::fs::read(&bak).unwrap(), b"old");
    }

    #[test]
    fn write_into_missing_target_creates_it() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("write_into_missing_target_creates_it");
        let p = dir.join("fresh.mid");
        write_atomic(&p, b"data").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"data");
    }

    #[test]
    fn temp_create_failure_is_typed() {
        let _g = TEST_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        let dir = testdir("temp_create_failure_is_typed");
        let p = dir.join("song.mid");
        set_fail_phase(Some(Phase::TempCreate));
        let e = write_atomic(&p, b"new").unwrap_err();
        assert_eq!(e.phase, Phase::TempCreate);
        assert_eq!(e.source.kind(), ErrorKind::Other);
    }
}

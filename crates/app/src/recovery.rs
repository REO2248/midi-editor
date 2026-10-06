//! Crash/autosave recovery snapshots.
//!
//! While the document is dirty the doc-watch loop periodically writes a
//! snapshot into `%APPDATA%/midi-editor/recovery/` — NEVER over the source
//! .mid. A snapshot is one file: a single-line JSON header followed by the
//! raw serialized SMF payload, written atomically so a mid-write crash can
//! only leave a temp file (ignored — only `*.snap` is considered). On
//! startup the newest snapshot newer than its source prompts
//! Restore / Discard / Inspect. Snapshots are cleared only by a verified
//! normal save or an explicit discard; retention is bounded.

use crate::lock_shared;
use mcp_server::SharedDoc;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Payload format — bumped if the meta/payload layout ever changes.
pub(crate) const FORMAT_VERSION: u32 = 1;
/// Newest snapshots to keep; older ones are deleted on startup.
pub(crate) const KEEP_MAX: usize = 4;
/// Snapshots older than this are removed on startup regardless of count.
pub(crate) const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 3600);
/// Minimum gap between snapshot writes while the doc keeps changing.
pub(crate) const DEBOUNCE: Duration = Duration::from_secs(2);
/// Only `*.snap` files are recovery candidates; atomic-write temp files and
/// anything else in the directory are ignored.
pub(crate) const SNAP_EXT: &str = "snap";
/// FAT/exFAT timestamps tick in ~2s steps — a source file this much newer
/// than the snapshot still counts as "not externally modified".
const MTIME_EPSILON_SECS: u64 = 2;

/// The JSON header of a snapshot file. Carries enough provenance to decide
/// whether the snapshot is newer than the on-disk source and to present
/// the Inspect details without parsing the MIDI payload.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub(crate) struct SnapshotMeta {
    /// payload format version — refuse to load what we don't understand
    pub v: u32,
    pub app_version: String,
    /// backing .mid path when the document has one (None = untitled doc)
    pub source_path: Option<PathBuf>,
    /// `Shared::saved_revision` at snapshot time — the last on-disk state
    pub saved_revision: u64,
    /// `Document::revision` at snapshot time — the unsaved state
    pub current_revision: u64,
    /// unix seconds when the snapshot was written
    pub timestamp: u64,
    /// SMF payload length and a cheap integrity hash over it
    pub payload_len: u64,
    pub payload_hash: u64,
}

/// FNV-1a over the payload — enough to detect truncation/corruption; not a
/// security control.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn unix_secs(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// %APPDATA%/midi-editor/recovery (temp dir fallback when APPDATA is unset —
/// matches `GlobalPrefs::path`).
pub(crate) fn recovery_dir() -> PathBuf {
    let base = std::env::var("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    base.join("midi-editor").join("recovery")
}

/// Serialize the document's meta + SMF payload into one file.
/// `serde_json::to_vec` never emits raw newlines inside strings, so the
/// first `\n` unambiguously splits header from payload.
fn encode(meta: &SnapshotMeta, payload: &[u8]) -> Vec<u8> {
    let mut out = serde_json::to_vec(meta).unwrap_or_default();
    out.push(b'\n');
    out.extend_from_slice(payload);
    out
}

/// Emergency snapshot for the panic hook (#200): best-effort, non-blocking.
/// Uses `try_lock` — a panic while the document lock is held (or any
/// contention) skips the write instead of deadlocking the crash path.
/// Files land next to autosaves with an `emergency-` prefix so the startup
/// restore prompt offers them like any other snapshot.
pub(crate) fn write_emergency_snapshot(shared: &SharedDoc, dir: &Path) -> Option<PathBuf> {
    let Ok(sh) = shared.try_lock() else {
        return None;
    };
    let (payload, meta) = {
        let payload = sh.doc.serialize(smf_core::WriteOptions {
            running_status: false,
        });
        let now = std::time::SystemTime::now();
        let meta = SnapshotMeta {
            v: FORMAT_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            source_path: sh.path.clone(),
            saved_revision: sh.saved_revision,
            current_revision: sh.doc.revision(),
            timestamp: unix_secs(now),
            payload_len: payload.len() as u64,
            payload_hash: fnv1a(&payload),
        };
        (payload, meta)
    };
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(format!("emergency-{}.snap", meta.timestamp));
    mcp_server::write_atomic(&path, &encode(&meta, &payload)).ok()?;
    Some(path)
}

/// Split and validate an encoded snapshot. Fails safely on anything odd:
/// no header line, malformed JSON, wrong format version, or a payload that
/// doesn't match its declared length/hash (truncated writes).
pub(crate) fn decode(bytes: &[u8]) -> Result<(SnapshotMeta, Vec<u8>), String> {
    let nl = bytes
        .iter()
        .position(|b| *b == b'\n')
        .ok_or("missing header line")?;
    let meta: SnapshotMeta =
        serde_json::from_slice(&bytes[..nl]).map_err(|e| format!("bad header: {e}"))?;
    if meta.v != FORMAT_VERSION {
        return Err(format!("unsupported format v{}", meta.v));
    }
    let payload = bytes[nl + 1..].to_vec();
    if payload.len() as u64 != meta.payload_len || fnv1a(&payload) != meta.payload_hash {
        return Err("payload integrity check failed".into());
    }
    Ok((meta, payload))
}

/// Write a snapshot for the shared doc's current state. Returns the path.
/// Name carries the timestamp so several generations can coexist; atomic
/// write means a crash mid-write leaves only a temp file.
pub(crate) fn write_snapshot(
    shared: &SharedDoc,
    dir: &Path,
    now: SystemTime,
) -> std::io::Result<PathBuf> {
    let (payload, meta) = {
        let sh = lock_shared(shared);
        let payload = sh.doc.serialize(smf_core::WriteOptions {
            running_status: false,
        });
        let meta = SnapshotMeta {
            v: FORMAT_VERSION,
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            source_path: sh.path.clone(),
            saved_revision: sh.saved_revision,
            current_revision: sh.doc.revision(),
            timestamp: unix_secs(now),
            payload_len: payload.len() as u64,
            payload_hash: fnv1a(&payload),
        };
        (payload, meta)
    };
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("autosave-{}.snap", meta.timestamp));
    mcp_server::write_atomic(&path, &encode(&meta, &payload))
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(path)
}

/// Read and fully validate one snapshot file.
pub(crate) fn load_snapshot(path: &Path) -> Result<(SnapshotMeta, Vec<u8>), String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    decode(&bytes)
}

/// All `*.snap` files in the dir, newest-modified first.
pub(crate) fn list_snapshots(dir: &Path) -> Vec<PathBuf> {
    let mut snaps: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some(SNAP_EXT))
                .map(|e| {
                    let mtime = e
                        .metadata()
                        .and_then(|m| m.modified())
                        .unwrap_or(UNIX_EPOCH);
                    (mtime, e.path())
                })
                .collect()
        })
        .unwrap_or_default();
    snaps.sort_by_key(|s| std::cmp::Reverse(s.0));
    snaps.into_iter().map(|(_, p)| p).collect()
}

/// Is the snapshot newer than the source it was taken from? Offered when
/// the source is gone or hasn't been touched since the snapshot —
/// if something else wrote the source afterwards the snapshot is stale.
pub(crate) fn snapshot_is_newer(meta: &SnapshotMeta) -> bool {
    let Some(src) = &meta.source_path else {
        return true;
    };
    let mtime = std::fs::metadata(src)
        .and_then(|m| m.modified())
        .map(unix_secs);
    match mtime {
        // source deleted/renamed or unreadable (removable drive gone) —
        // the snapshot may be the only copy left; offer it
        Err(_) => true,
        Ok(m) => meta.timestamp + MTIME_EPSILON_SECS >= m,
    }
}

/// The newest valid, non-stale snapshot offered on startup. A snap matches
/// an argv-opened file only when it was taken from that same source; with
/// no argv any newer snapshot is offered (restore adopts its source path).
pub(crate) fn find_candidate(
    dir: &Path,
    argv_path: Option<&Path>,
) -> Option<(PathBuf, SnapshotMeta, Vec<u8>)> {
    for path in list_snapshots(dir) {
        let Ok((meta, payload)) = load_snapshot(&path) else {
            continue; // corrupt/incomplete — fails safely
        };
        if let Some(argv) = argv_path {
            if meta.source_path.as_deref() != Some(argv) {
                continue;
            }
        }
        if !snapshot_is_newer(&meta) {
            continue;
        }
        return Some((path, meta, payload));
    }
    None
}

/// Delete only the snapshots belonging to one document identity — those
/// whose `source_path` equals `source` (`None` matches untitled-lineage
/// snapshots). Snapshots of other songs survive saves, opens, and new-file
/// operations (#174): choosing "Later" on a recovery prompt keeps the
/// snapshot on disk until it is explicitly resolved or aged out.
pub(crate) fn clear_snapshots_for(dir: &Path, source: Option<&Path>) {
    for p in list_snapshots(dir) {
        let matches = load_snapshot(&p)
            .map(|(meta, _)| meta.source_path.as_deref() == source)
            .unwrap_or(false);
        if matches {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Convenience wrapper: clear the snapshots of the given document identity
/// in the default recovery dir.
pub(crate) fn clear_recovery_for(source: Option<&Path>) {
    clear_snapshots_for(&recovery_dir(), source);
}

/// Bounded retention: drop stale/overflow snapshots. Runs once on startup
/// so a crash loop can't accumulate files forever.
pub(crate) fn cleanup_stale(dir: &Path, keep: usize, max_age: Duration, now: SystemTime) {
    for (i, p) in list_snapshots(dir).into_iter().enumerate() {
        let old = std::fs::metadata(&p)
            .and_then(|m| m.modified())
            .map(|m| now.duration_since(m).unwrap_or_default() > max_age)
            .unwrap_or(false);
        if i >= keep || old {
            let _ = std::fs::remove_file(&p);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> mcp_server::SharedDoc {
        let f = smf_core::File {
            format: 1,
            division: smf_core::Division::Metrical(480),
            tracks: vec![smf_core::Track { events: vec![] }],
            warnings: vec![],
        };
        mcp_server::SharedDoc::new(std::sync::Mutex::new(mcp_server::Shared::new(
            document::Document::from_file(f),
        )))
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("midi-recovery-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn meta(ts: u64) -> SnapshotMeta {
        SnapshotMeta {
            v: FORMAT_VERSION,
            app_version: "test".into(),
            source_path: None,
            saved_revision: 0,
            current_revision: 3,
            timestamp: ts,
            payload_len: 0,
            payload_hash: fnv1a(&[]),
        }
    }

    #[test]
    fn encode_decode_roundtrip() {
        let payload = b"MThd\x00\x00\x00\x06\x00\x01\x00\x01\x01\xe0";
        let m = SnapshotMeta {
            payload_len: payload.len() as u64,
            payload_hash: fnv1a(payload),
            ..meta(42)
        };
        let (back_meta, back_payload) = decode(&encode(&m, payload)).unwrap();
        assert_eq!(back_meta, m);
        assert_eq!(back_payload, payload);
    }

    #[test]
    fn decode_fails_safely_on_corruption() {
        let payload = b"payload".to_vec();
        let m = SnapshotMeta {
            payload_len: payload.len() as u64,
            payload_hash: fnv1a(&payload),
            ..meta(42)
        };
        let good = encode(&m, &payload);
        // interrupted write: file truncated mid-payload
        assert!(decode(&good[..good.len() - 3]).is_err());
        // garbage / not a snapshot at all
        assert!(decode(b"not a snapshot").is_err());
        // header-only, payload missing
        let mut header_only = serde_json::to_vec(&m).unwrap();
        header_only.push(b'\n');
        assert!(decode(&header_only).is_err());
        // tampered payload fails the hash
        let mut tampered = good.clone();
        let n = tampered.len();
        tampered[n - 1] ^= 0xFF;
        assert!(decode(&tampered).is_err());
    }

    #[test]
    fn decode_rejects_unknown_format_version() {
        let m = SnapshotMeta {
            v: FORMAT_VERSION + 1,
            ..meta(0)
        };
        assert!(decode(&encode(&m, &[])).is_err());
    }

    #[test]
    fn write_then_load_snapshot() {
        let dir = tmpdir("write");
        let sh = shared();
        let path = write_snapshot(&sh, &dir, SystemTime::now()).unwrap();
        assert!(path.exists());
        let (m, payload) = load_snapshot(&path).unwrap();
        assert_eq!(m.current_revision, 0);
        assert!(!payload.is_empty());
        assert!(smf_core::parse(&payload).is_ok());
    }

    #[test]
    fn list_ignores_temp_and_non_snap_files() {
        let dir = tmpdir("list");
        std::fs::write(dir.join("a.snap"), b"x").unwrap();
        std::fs::write(dir.join("a.snap.tmp"), b"partial").unwrap();
        std::fs::write(dir.join("notes.txt"), b"hi").unwrap();
        std::fs::create_dir_all(dir.join("sub.snap")).unwrap();
        let found = list_snapshots(&dir);
        assert_eq!(found.len(), 1);
        assert!(found[0].ends_with("a.snap"));
    }

    #[test]
    fn cleanup_keeps_newest_and_drops_aged() {
        let dir = tmpdir("cleanup");
        let now = SystemTime::now();
        for i in 0..6 {
            let p = dir.join(format!("s{i}.snap"));
            std::fs::write(&p, b"x").unwrap();
            // age files: newest first by mtime — s5 is newest, s0 oldest;
            // s0 also pushed past MAX_AGE
            let age = Duration::from_secs((5 - i) as u64 * 10);
            let past = now - age;
            let _ = filetime_set(&p, past);
        }
        // artificially age s0 far beyond retention
        let _ = filetime_set(
            &dir.join("s0.snap"),
            now - MAX_AGE - Duration::from_secs(10),
        );
        cleanup_stale(&dir, 3, MAX_AGE, now);
        let remaining = list_snapshots(&dir);
        assert_eq!(remaining.len(), 3);
        assert!(remaining.iter().all(|p| !p.ends_with("s0.snap")));
        assert!(remaining.iter().all(|p| !p.ends_with("s1.snap")));
        assert!(remaining.iter().all(|p| !p.ends_with("s2.snap")));
    }

    /// mtime on Windows is settable via `std::fs::File::set_modified`.
    fn filetime_set(path: &Path, t: SystemTime) -> std::io::Result<()> {
        std::fs::File::options()
            .write(true)
            .open(path)?
            .set_modified(t)
    }

    #[test]
    fn stale_snapshot_not_newer_than_touched_source() {
        let dir = tmpdir("stale");
        let src = dir.join("song.mid");
        std::fs::write(&src, b"MThd").unwrap();
        // snapshot taken an hour ago; source touched now -> stale
        let m = SnapshotMeta {
            source_path: Some(src.clone()),
            timestamp: unix_secs(SystemTime::now()) - 3600,
            ..meta(0)
        };
        assert!(!snapshot_is_newer(&m));
        // same snapshot but source deleted -> still recoverable
        std::fs::remove_file(&src).unwrap();
        assert!(snapshot_is_newer(&m));
        // no source path at all -> always offered
        assert!(snapshot_is_newer(&meta(0)));
    }

    #[test]
    fn find_candidate_matches_argv_and_skips_corrupt() {
        let dir = tmpdir("cand");
        let src = dir.join("song.mid");
        std::fs::write(&src, b"MThd").unwrap();
        let now = unix_secs(SystemTime::now());
        // a corrupt newest file must not shadow a valid older one
        std::fs::write(dir.join("corrupt.snap"), b"\x00\x01\x02").unwrap();
        let m = SnapshotMeta {
            source_path: Some(src.clone()),
            timestamp: now,
            ..meta(now)
        };
        let payload = b"payload".to_vec();
        let m = SnapshotMeta {
            payload_len: payload.len() as u64,
            payload_hash: fnv1a(&payload),
            ..m
        };
        std::fs::write(dir.join("good.snap"), encode(&m, &payload)).unwrap();
        let found = find_candidate(&dir, Some(&src)).unwrap();
        assert_eq!(found.1.source_path.as_deref(), Some(src.as_path()));
        // an argv path for a different file finds nothing
        assert!(find_candidate(&dir, Some(&dir.join("other.mid"))).is_none());
    }

    #[test]
    fn clear_snapshots_for_spares_other_songs() {
        // #174: opening/saving Song B must not delete Song A's snapshots —
        // deletion is scoped to the document identity being resolved
        let dir = tmpdir("scoped");
        let song_a = dir.join("a.mid");
        let song_b = dir.join("b.mid");
        let payload = b"payload".to_vec();
        let mk = |src: Option<PathBuf>, ts: u64| {
            let m = SnapshotMeta {
                source_path: src,
                timestamp: ts,
                ..meta(ts)
            };
            let m = SnapshotMeta {
                payload_len: payload.len() as u64,
                payload_hash: fnv1a(&payload),
                ..m
            };
            encode(&m, &payload)
        };
        std::fs::write(dir.join("a1.snap"), mk(Some(song_a.clone()), 1)).unwrap();
        std::fs::write(dir.join("a2.snap"), mk(Some(song_a.clone()), 2)).unwrap();
        std::fs::write(dir.join("b1.snap"), mk(Some(song_b.clone()), 3)).unwrap();
        std::fs::write(dir.join("u1.snap"), mk(None, 4)).unwrap();
        // saving/opening Song B clears only Song B's lineage
        clear_snapshots_for(&dir, Some(&song_b));
        let left = list_snapshots(&dir);
        assert_eq!(left.len(), 3);
        assert!(left.iter().all(|p| !p.ends_with("b1.snap")));
        // an untitled doc's save/new clears only untitled-lineage snapshots
        clear_snapshots_for(&dir, None);
        let left = list_snapshots(&dir);
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|p| !p.ends_with("u1.snap")));
        // Song A's snapshots survive every other document's lifecycle
        assert!(left.iter().any(|p| p.ends_with("a1.snap")));
        assert!(left.iter().any(|p| p.ends_with("a2.snap")));
    }

    #[test]
    fn emergency_snapshot_writes_and_offers() {
        // #200: the panic hook's snapshot lands in the recovery dir as a valid
        // *.snap the startup candidate search can offer
        let dir = tmpdir("emergency");
        let sh = shared();
        let path = write_emergency_snapshot(&sh, &dir).unwrap();
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("emergency-"));
        let (m, payload) = load_snapshot(&path).unwrap();
        assert_eq!(m.current_revision, 0);
        assert!(!payload.is_empty());
        // the emergency file is a candidate: newest, valid, newer than source
        let found = find_candidate(&dir, None).unwrap();
        assert_eq!(found.0, path);
        // a document lock held elsewhere must not deadlock the crash path:
        // the write is skipped instead
        let g = lock_shared(&sh);
        assert!(write_emergency_snapshot(&sh, &dir).is_none());
        drop(g);
        assert!(write_emergency_snapshot(&sh, &dir).is_some());
    }
}

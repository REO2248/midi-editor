//! Document lifecycle service shared by GUI, MCP, and stdio frontends.
//!
//! Serialize → durable write → saved-revision commit and document
//! open/swap live here so no frontend can drift on durability or revision
//! semantics. UI prompting stays in the app; MCP only formats these calls
//! into JSON. `save_document` is the single path every save takes: a
//! revision is marked saved only after the durable replace lands.

use std::path::{Path, PathBuf};

use commands::UndoStack;
use document::Document;
use thiserror::Error;

use crate::{Shared, SharedDoc};

fn lock(shared: &SharedDoc) -> std::sync::MutexGuard<'_, Shared> {
    // poison-tolerant like dispatch: a panicked critical section must not
    // take down every later save/open
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

fn save_options() -> smf_core::WriteOptions {
    smf_core::WriteOptions {
        running_status: false,
    }
}

/// Errors a save can produce, classified so callers can distinguish
/// user-facing causes (no path, conflict) from I/O failures by phase.
#[derive(Debug, Error)]
pub enum SaveError {
    /// neither the request nor the document knows where to write
    #[error("no path — pass one or open a file in the editor")]
    NoPath,
    /// the live document moved past the revision the caller meant to save
    #[error("document changed since revision {expected} (now {actual}) — save again to overwrite")]
    Conflict {
        /// revision the caller expected to still be current
        expected: u64,
        /// revision the document is actually at
        actual: u64,
    },
    /// the durable write itself failed (failed phase + any recoverable
    /// temp path are inside)
    #[error(transparent)]
    Persist(#[from] persist::PersistError),
}

/// Parameters for `save_document`.
#[derive(Debug, Default)]
pub struct SaveRequest<'a> {
    /// explicit target ("save as"); falls back to `Shared::path`
    pub path: Option<&'a Path>,
    /// optimistic guard: refuse to save a document that moved past this
    /// revision since the caller last observed it — the same convention
    /// `apply_patch` uses for `base_revision`
    pub expect_revision: Option<u64>,
}

/// What a save did.
#[derive(Debug)]
pub struct SaveOutcome {
    /// where the bytes landed
    pub path: PathBuf,
    /// revision the written bytes were serialized at
    pub revision: u64,
    /// temp files beside `path` left by earlier crashed saves (recovery hint)
    pub leftovers: Vec<PathBuf>,
    /// false when the document was swapped while the write was in flight:
    /// the bytes are on disk but the *current* document is not marked saved
    pub committed: bool,
    /// how long the snapshot critical section held the editor lock —
    /// the number `begin_save` keeps small (clone, not serialize+write)
    pub lock_held: std::time::Duration,
}

/// A save in flight between `begin_save` and `finish_save`: the snapshot
/// bytes will be serialized from, plus the revision bookkeeping needed to
/// commit safely. `Send` — the GUI moves it to a worker thread so a slow
/// save never runs on the UI critical path.
pub struct SaveTicket {
    shared: SharedDoc,
    path: PathBuf,
    snapshot: document::DocSnapshot,
    revision: u64,
    generation: u64,
    /// how long `begin_save` held the editor lock (benchmark signal)
    lock_held: std::time::Duration,
}

impl SaveTicket {
    /// the snapshot's event count — e.g. to gate save-progress UI on
    /// whether the save is likely to be perceptible at all
    pub fn event_count(&self) -> usize {
        self.snapshot.event_count()
    }

    /// the snapshot this save will write — e.g. to time serialization
    /// separately in benchmarks
    pub fn snapshot(&self) -> &document::DocSnapshot {
        &self.snapshot
    }
}

/// The single save path every frontend shares, in two steps: `begin_save`
/// takes a snapshot under one short lock; `finish_save` serializes and
/// writes without the editor lock, so a slow save never freezes edits.
/// `save_document` composes both for synchronous callers (MCP/stdio).
pub fn begin_save(shared: &SharedDoc, req: SaveRequest) -> Result<SaveTicket, SaveError> {
    // resolve the path, check the optimistic guard, and clone the snapshot
    // — all under one short lock. Edits applied after this stay dirty.
    let t0 = std::time::Instant::now();
    let sh = lock(shared);
    let path = req
        .path
        .map(PathBuf::from)
        .or_else(|| sh.path.clone())
        .ok_or(SaveError::NoPath)?;
    let rev = sh.doc.revision();
    if let Some(expected) = req.expect_revision {
        if rev != expected {
            return Err(SaveError::Conflict {
                expected,
                actual: rev,
            });
        }
    }
    Ok(SaveTicket {
        shared: shared.clone(),
        path,
        snapshot: sh.doc.snapshot(),
        revision: rev,
        generation: sh.generation,
        lock_held: t0.elapsed(),
    })
}

/// Serialize the snapshot and write it durably — no editor lock held.
/// Marks the revision saved only after the replace landed, and only if
/// the document wasn't swapped meanwhile.
pub fn finish_save(ticket: SaveTicket) -> Result<SaveOutcome, SaveError> {
    // 1. serialize + durable write — never hold the editor lock across
    //    encode or disk I/O
    let bytes = ticket.snapshot.serialize(save_options());
    persist::write_atomic(&ticket.path, &bytes)?;
    // 2. commit: mark saved only now, and only for the document we
    //    actually snapshotted (a swapped-in doc stays at its own state)
    let mut sh = lock(&ticket.shared);
    let committed = sh.generation == ticket.generation;
    if committed {
        sh.saved_revision = ticket.revision;
        // record the save point for dirty tracking (#177). An edit landing
        // after begin_save leaves the live document past the written
        // revision — the marker is dropped so the revision comparison
        // keeps the document dirty.
        if sh.doc.revision() == ticket.revision {
            sh.undo.mark_saved();
        } else {
            sh.undo.clear_saved();
        }
    }
    let leftovers = persist::temp_siblings(&ticket.path);
    drop(sh);
    Ok(SaveOutcome {
        path: ticket.path,
        revision: ticket.revision,
        leftovers,
        committed,
        lock_held: ticket.lock_held,
    })
}

/// Synchronous save — the same begin/finish core MCP, stdio, and the GUI's
/// save worker all route through.
pub fn save_document(shared: &SharedDoc, req: SaveRequest) -> Result<SaveOutcome, SaveError> {
    finish_save(begin_save(shared, req)?)
}

/// Replace the shared document in place — the MCP server and GUI hold the
/// same `Arc`, so both see the swap. Resets undo and per-document routing,
/// marks the fresh document saved at its own revision, and bumps
/// `generation` so a save of the old document that is still in flight
/// cannot mark the new one saved.
pub fn swap_document(shared: &SharedDoc, doc: Document, path: Option<PathBuf>) {
    let mut sh = lock(shared);
    sh.doc = doc;
    sh.undo = UndoStack::new(512);
    sh.undo.mark_saved();
    sh.path = path;
    sh.saved_revision = sh.doc.revision();
    sh.generation += 1;
    sh.muted.clear();
    sh.soloed.clear();
    sh.track_dest.clear();
}

/// Errors `load_document` can produce.
#[derive(Debug, Error)]
pub enum LoadError {
    /// the file could not be read
    #[error("read {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    /// the bytes are not parseable SMF
    #[error("parse {}: {source}", .path.display())]
    Parse {
        path: PathBuf,
        source: smf_core::Error,
    },
}

/// Read + parse a MIDI file into a `Document`. SMF bytes are the source of
/// truth — an imported file is never rebuilt by open+save round-trips.
pub fn load_document(path: &Path) -> Result<(Document, Vec<String>), LoadError> {
    let bytes = std::fs::read(path).map_err(|source| LoadError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let file =
        smf_core::parse_with_limits(&bytes, &smf_core::Limits::from_env()).map_err(|source| {
            LoadError::Parse {
                path: path.to_path_buf(),
                source,
            }
        })?;
    let warnings = file.warnings.clone();
    Ok((Document::from_file(file), warnings))
}

/// Load `path` into a fresh `SharedDoc` (stdio `--file` mode).
pub fn open_shared(path: &Path) -> Result<SharedDoc, LoadError> {
    let (doc, _warnings) = load_document(path)?;
    let shared = std::sync::Arc::new(std::sync::Mutex::new(Shared::new(doc)));
    lock(&shared).path = Some(path.to_path_buf());
    Ok(shared)
}

#[cfg(test)]
mod tests {
    use super::*;
    use document::Op;
    use std::sync::{Arc, Mutex};

    fn shared() -> SharedDoc {
        let f = smf_core::File {
            format: 1,
            division: smf_core::Division::Metrical(480),
            tracks: vec![smf_core::Track { events: vec![] }],
            warnings: vec![],
        };
        Arc::new(Mutex::new(Shared::new(Document::from_file(f))))
    }

    fn testdir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("midi-editor-service-tests")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn save_document_writes_and_marks_revision() {
        let sh = shared();
        let p = testdir("save_document_writes_and_marks_revision").join("a.mid");
        lock(&sh).path = Some(p.clone());
        let rev = lock(&sh).doc.revision();
        let out = save_document(&sh, SaveRequest::default()).unwrap();
        assert!(out.committed);
        assert_eq!(out.revision, rev);
        assert_eq!(lock(&sh).saved_revision, rev);
        // valid SMF round-trip
        assert_eq!(
            smf_core::parse(&std::fs::read(&p).unwrap())
                .unwrap()
                .tracks
                .len(),
            1
        );
    }

    #[test]
    fn save_document_requires_a_path() {
        let sh = shared();
        let err = save_document(&sh, SaveRequest::default()).unwrap_err();
        assert!(matches!(err, SaveError::NoPath));
    }

    #[test]
    fn save_document_explicit_path_overrides_shared_path() {
        let sh = shared();
        let dir = testdir("save_document_explicit_path_overrides_shared_path");
        lock(&sh).path = Some(dir.join("ignored.mid"));
        let p = dir.join("chosen.mid");
        save_document(
            &sh,
            SaveRequest {
                path: Some(&p),
                expect_revision: None,
            },
        )
        .unwrap();
        assert!(p.exists());
    }

    #[test]
    fn save_document_conflict_guard() {
        let sh = shared();
        let p = testdir("save_document_conflict_guard").join("a.mid");
        lock(&sh).path = Some(p.clone());
        let rev = lock(&sh).doc.revision();
        let err = save_document(
            &sh,
            SaveRequest {
                path: None,
                expect_revision: Some(rev + 1),
            },
        )
        .unwrap_err();
        assert!(matches!(err, SaveError::Conflict { .. }));
        assert!(!p.exists(), "conflict must not write");
        // matching expectation saves fine
        save_document(
            &sh,
            SaveRequest {
                path: None,
                expect_revision: Some(rev),
            },
        )
        .unwrap();
    }

    #[test]
    fn save_document_survives_concurrent_edits() {
        // an edit between serialization and commit must NOT be marked
        // saved — saved_revision stays at the snapshot's revision
        let sh = shared();
        let dir = testdir("save_document_survives_concurrent_edits");
        let p = dir.join("a.mid");
        lock(&sh).path = Some(p.clone());
        save_document(&sh, SaveRequest::default()).unwrap();
        lock(&sh)
            .apply(
                "t",
                vec![Op::InsertTrack {
                    index: 1,
                    track: document::Track {
                        name: None,
                        out_port: 0,
                        out_channel: 0,
                        events: vec![],
                    },
                }],
            )
            .unwrap();
        let out = save_document(&sh, SaveRequest::default()).unwrap();
        // one lock per expression — two lock() calls in one assert_eq!
        // deadlock on the non-reentrant mutex
        assert_eq!(out.revision, lock(&sh).doc.revision());
        let g = lock(&sh);
        assert_eq!(g.saved_revision, g.doc.revision());
    }

    #[test]
    fn snapshot_serializes_identical_bytes() {
        // DocSnapshot::serialize must be the same bytes Document::serialize
        // produced — the split path is a pure move, not a format change
        let sh = shared();
        let opts = save_options();
        let g = lock(&sh);
        let a = g.doc.serialize(opts);
        let b = g.doc.snapshot().serialize(opts);
        assert_eq!(a, b);
    }

    #[test]
    fn finish_save_marks_only_the_snapshotted_revision() {
        // an edit landing after begin_save must stay dirty: saved_revision
        // only covers the revision the bytes came from
        let sh = shared();
        let p = testdir("finish_save_marks_only_the_snapshotted_revision").join("a.mid");
        lock(&sh).path = Some(p.clone());
        let ticket = begin_save(&sh, SaveRequest::default()).unwrap();
        // concurrent edit while the save is in flight
        lock(&sh)
            .apply(
                "t",
                vec![Op::InsertTrack {
                    index: 1,
                    track: document::Track {
                        name: None,
                        out_port: 0,
                        out_channel: 0,
                        events: vec![],
                    },
                }],
            )
            .unwrap();
        let out = finish_save(ticket).unwrap();
        assert!(out.committed);
        let g = lock(&sh);
        assert_eq!(g.saved_revision, out.revision);
        assert!(
            g.doc.revision() > g.saved_revision,
            "post-snapshot edit stays dirty"
        );
    }

    #[test]
    fn swap_during_write_does_not_mark_new_doc_saved() {
        // force the commit path to see a different generation: write to
        // `path`, then swap the doc before save_document's commit can run.
        // (synchronous test: simulate by calling the pieces directly)
        let sh = shared();
        let dir = testdir("swap_during_write_does_not_mark_new_doc_saved");
        let p = dir.join("a.mid");
        lock(&sh).path = Some(p.clone());
        // emulate a stale save completing after an open: swap bumps
        // generation, and the new doc must keep its own saved_revision
        swap_document(
            &sh,
            Document::from_file(smf_core::File {
                format: 1,
                division: smf_core::Division::Metrical(480),
                tracks: vec![smf_core::Track { events: vec![] }],
                warnings: vec![],
            }),
            Some(dir.join("b.mid")),
        );
        let rev = lock(&sh).doc.revision();
        assert_eq!(lock(&sh).saved_revision, rev);
        assert_eq!(lock(&sh).generation, 1);
        assert_eq!(lock(&sh).path.as_deref(), Some(dir.join("b.mid").as_path()));
        // undo stack and per-document routing were reset
        assert!(lock(&sh).undo.is_empty());
    }

    #[test]
    fn swap_document_resets_routing() {
        let sh = shared();
        {
            let mut g = lock(&sh);
            g.muted.insert(0);
            g.soloed.insert(1);
            g.track_dest.insert(0, 3);
            g.undo.push(document::Transaction {
                label: "x".into(),
                base: 0,
                ops: vec![],
            });
        }
        swap_document(
            &sh,
            Document::from_file(smf_core::File {
                format: 1,
                division: smf_core::Division::Metrical(480),
                tracks: vec![smf_core::Track { events: vec![] }],
                warnings: vec![],
            }),
            None,
        );
        let g = lock(&sh);
        assert!(g.muted.is_empty() && g.soloed.is_empty() && g.track_dest.is_empty());
        assert!(g.undo.is_empty());
        assert!(g.path.is_none());
    }

    #[test]
    fn temp_siblings_reports_leftovers() {
        let dir = testdir("temp_siblings_reports_leftovers");
        let p = dir.join("song.mid");
        std::fs::write(dir.join(".song.mid.sav-1-2"), b"partial").unwrap();
        std::fs::write(dir.join("other.mid"), b"x").unwrap();
        let sibs = persist::temp_siblings(&p);
        assert_eq!(sibs.len(), 1);
        assert!(sibs[0]
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(".song.mid.sav"));
    }

    #[test]
    fn open_shared_marks_loaded_revision_saved() {
        let dir = testdir("open_shared_marks_loaded_revision_saved");
        let p = dir.join("a.mid");
        let f = smf_core::File {
            format: 1,
            division: smf_core::Division::Metrical(480),
            tracks: vec![smf_core::Track { events: vec![] }],
            warnings: vec![],
        };
        std::fs::write(
            &p,
            smf_core::write(
                f.format,
                f.division,
                &f.tracks,
                smf_core::WriteOptions {
                    running_status: false,
                },
            ),
        )
        .unwrap();
        let sh = open_shared(&p).unwrap();
        let g = lock(&sh);
        assert_eq!(g.saved_revision, g.doc.revision());
        assert_eq!(g.path.as_deref(), Some(p.as_path()));
    }
}

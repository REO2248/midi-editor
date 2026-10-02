//! Shared document state + transaction bookkeeping owned by the MCP service.
//!
//! `Shared` is the single authority the GUI and MCP transports both see —
//! one `SharedDoc` (`Arc<Mutex<Shared>>`) guards the `Document`, the undo
//! stack, destination routing, and the bounded agent-facing history.
//! Named batches (`Batch`) stage agent edits on a private document copy
//! until commit; the real document is never locked against GUI edits.
//! Everything in this module runs under the `SharedDoc` lock — no other
//! locking discipline is permitted here.

use super::*;

pub struct Shared {
    pub doc: Document,
    pub undo: UndoStack,
    pub path: Option<PathBuf>,
    pub saved_revision: u64,
    /// bumped on every `service::swap_document` — a save that serialized the
    /// old document must not mark the swapped-in one saved
    pub generation: u64,
    pub gui_notify: Arc<AtomicU64>,
    /// destination catalog: (display label, stable identity). Index into this
    /// vec is what `default_dest`/`track_dest` reference — identities, never
    /// midir indexes.
    pub dests: Vec<(String, Destination)>,
    pub default_dest: usize,
    /// track index -> index into `dests`
    pub track_dest: HashMap<usize, usize>,
    pub muted: HashSet<usize>,
    pub soloed: HashSet<usize>,
    pub metronome: bool,
    /// explicit metronome click destination — index into `dests`, `None` =
    /// follow `default_dest`. Never auto-picks a port (#137).
    pub met_dest: Option<usize>,
    pub loop_enabled: bool,
    /// explicit loop locators in ticks — `None` = unset (#130). `Some` +
    /// `loop_enabled` wraps playback at the right locator back to the left;
    /// both unset falls back to the legacy play-start→end wrap.
    pub loop_start: Option<u64>,
    pub loop_end: Option<u64>,
    /// opt-in: also chase the last complete SysEx message on play/loop wrap
    /// (a chased GM/GS/XG reset can wipe the channel-state chase)
    pub chase_sysex: bool,
    /// the GUI drains `transport_req` and repaints on `gui_notify`; false in
    /// standalone `mcp-bridge --file` mode (feature-detected via editor_info)
    pub gui_attached: bool,
    /// how long SysEx dumps leave a MIDI port sink during playback
    /// (serialize / background lane / skip) — `midi_io::SysexPolicy`
    pub sysex_policy: midi_io::SysexPolicy,
    /// (port_name, ord) pairs the last MIDI output enumeration reported —
    /// refreshed by the GUI watcher; a MidiPort dest absent from this set is
    /// currently offline but keeps its identity and assignment
    pub port_present: std::collections::HashSet<(String, usize)>,
    /// drained by the GUI watcher
    pub transport_req: Vec<TransportReq>,
    /// effective security posture of the MCP transport serving this doc
    /// (stdio by default; the HTTP server overwrites it at startup).
    /// Never carries the credential itself, only its provenance.
    pub mcp_security: SecurityReport,
    /// open named transaction (begin_transaction) — staged edits live here
    /// until commit/rollback; never blocks GUI edits on the real document
    pub batch: Option<Batch>,
    /// bounded committed-transaction log (oldest evicted past TX_HISTORY_CAP)
    pub history: std::collections::VecDeque<TxRecord>,
    /// last agent-originated committed transaction — the GUI watches this to
    /// show "MCP: name" in the status bar
    pub last_mcp_tx: Option<TxRecord>,
    /// file-write scope for `save` — stamped by the transport entry point;
    /// defaults to the stricter HTTP policy
    pub fs_scope: FsScope,
}

/// A named edit checkpoint. While open, every edit tool stages its ops on
/// `staging` — a private copy of the document taken at `begin` — and reads
/// see the staged state (read-your-writes inside a transaction). The real
/// document is untouched until `commit_transaction`, so rollback or an
/// abandoned batch leaves it byte-for-byte identical. GUI edits are never
/// locked out: they land on the real document and turn the commit into a
/// stale-revision conflict instead of clobbering anyone.
pub struct Batch {
    /// transaction label — becomes the single undo step's label on commit
    pub label: String,
    /// `doc.revision()` at begin; commit refuses when it no longer matches
    pub base: u64,
    pub staging: Document,
    /// ops accepted so far, in call order — merged into one `Transaction`
    pub ops: Vec<Op>,
    pub last_activity: Instant,
}

/// Idle time after which an open batch is rolled back automatically — an
/// abandoned agent session must not pin a document clone forever.
pub const BATCH_TTL: Duration = Duration::from_secs(300);

/// Result of routing an edit through `apply_or_stage`.
pub enum StageOutcome {
    /// committed on the real document (no batch open)
    Committed {
        revision: u64,
        summary: ChangeSummary,
    },
    /// staged into the open batch — `summary` covers this call's ops
    Staged {
        pending_ops: usize,
        staged_revision: u64,
        summary: ChangeSummary,
    },
}

/// Who committed a transaction — recorded in `history`/`last_mcp_tx` so
/// agent-originated edits are attributable (and surfaced in the GUI status).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOrigin {
    Gui,
    Mcp,
}

/// What a history entry did to the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxKind {
    Commit,
    Undo,
    Redo,
}

/// Structured change summary derived from a transaction's committed ops —
/// never re-scanned from the document, so it cannot drift from what apply()
/// actually did.
#[derive(Debug, Clone, Default)]
pub struct ChangeSummary {
    pub ops: usize,
    pub inserted: usize,
    pub removed: usize,
    pub updated: usize,
    pub notes_inserted: usize,
    pub notes_removed: usize,
    pub notes_moved: usize,
    pub cc_changes: usize,
    pub meta_changes: usize,
    pub other_events: usize,
    pub tracks_touched: Vec<usize>,
    pub tick_range: Option<(u64, u64)>,
    /// (before, after) when the transaction converted the SMF format
    pub format_change: Option<(u16, u16)>,
}

/// One entry in the bounded agent-facing transaction log.
#[derive(Debug, Clone)]
pub struct TxRecord {
    /// document revision before this entry (coverage cursor for
    /// `changes_since_revision`)
    pub base: u64,
    /// document revision after this entry
    pub revision: u64,
    pub label: String,
    pub origin: TxOrigin,
    pub kind: TxKind,
    pub summary: ChangeSummary,
}

/// Bounded transaction history — `transaction_history`/`changes_since_revision`
/// reads never grow past this.
pub const TX_HISTORY_CAP: usize = 64;

pub type SharedDoc = Arc<Mutex<Shared>>;

impl Shared {
    pub fn new(doc: Document) -> Self {
        // a freshly opened document is saved at whatever revision it
        // starts on — identical bookkeeping for GUI, MCP, and stdio opens
        let saved_revision = doc.revision();
        Self {
            doc,
            undo: UndoStack::new(512),
            path: None,
            saved_revision,
            generation: 0,
            gui_notify: Arc::new(AtomicU64::new(0)),
            dests: Vec::new(),
            default_dest: 0,
            track_dest: HashMap::new(),
            muted: HashSet::new(),
            soloed: HashSet::new(),
            metronome: false,
            met_dest: None,
            loop_enabled: false,
            loop_start: None,
            loop_end: None,
            chase_sysex: false,
            gui_attached: false,
            sysex_policy: midi_io::SysexPolicy::Serialize,
            port_present: std::collections::HashSet::new(),
            transport_req: Vec::new(),
            mcp_security: SecurityReport::stdio(),
            batch: None,
            history: std::collections::VecDeque::new(),
            last_mcp_tx: None,
            fs_scope: FsScope::Http,
        }
    }

    /// Index into `dests` for `dest`, appending a fresh entry when absent.
    /// Missing MIDI ports keep their identity — `open_named` fails at play
    /// time, which surfaces a readable error instead of a wrong port. Dedup
    /// is by routing identity (`same_identity`), not struct equality, so a
    /// plugin arriving with different metadata (moved path, fresh vendor
    /// string) doesn't fork the catalog into duplicate entries.
    pub fn ensure_dest(&mut self, label: &str, dest: Destination) -> usize {
        if let Some(i) = self.dests.iter().position(|(_, d)| d.same_identity(&dest)) {
            return i;
        }
        self.dests.push((label.to_string(), dest));
        self.dests.len() - 1
    }

    /// The destination index a track resolves to (per-track override else default).
    pub fn dest_of(&self, track: usize) -> usize {
        self.track_dest
            .get(&track)
            .copied()
            .unwrap_or(self.default_dest)
    }

    /// Apply a transaction and push it onto the shared undo stack — the
    /// GUI's entry point (origin Gui). Returns the new revision.
    pub fn apply(&mut self, label: &str, ops: Vec<Op>) -> Result<u64, ApplyError> {
        self.apply_origin(TxOrigin::Gui, label, ops)
    }

    /// `apply` with an explicit origin — the summary is computed from the
    /// committed ops and recorded in `history`; MCP-originated commits also
    /// update `last_mcp_tx` for the GUI status surface.
    pub fn apply_origin(
        &mut self,
        origin: TxOrigin,
        label: &str,
        ops: Vec<Op>,
    ) -> Result<u64, ApplyError> {
        let tx = Transaction {
            label: label.into(),
            base: self.doc.revision(),
            ops,
        };
        let base = self.doc.revision();
        let applied = self.doc.apply(tx)?;
        let rev = applied.revision;
        let summary = change_summary(&applied.tx.ops);
        // undo replays the *effective* transaction (caller's ops plus any
        // synthesized normalization) so pre-edit bytes restore exactly
        self.undo.push(applied.tx);
        self.record_history(TxRecord {
            base,
            revision: rev,
            label: label.to_string(),
            origin,
            kind: TxKind::Commit,
            summary,
        });
        self.gui_notify.fetch_add(1, Ordering::Relaxed);
        Ok(rev)
    }

    /// Append to the bounded history; agent-originated entries also update
    /// `last_mcp_tx` (the GUI status surface watches that field).
    pub fn record_history(&mut self, rec: TxRecord) {
        if rec.origin == TxOrigin::Mcp {
            self.last_mcp_tx = Some(rec.clone());
        }
        if self.history.len() == TX_HISTORY_CAP {
            self.history.pop_front();
        }
        self.history.push_back(rec);
    }

    /// The document edit tools and reads see: the staged copy while a batch
    /// is open (read-your-writes), else the committed document.
    pub fn view(&self) -> &Document {
        self.batch.as_ref().map(|b| &b.staging).unwrap_or(&self.doc)
    }

    pub fn view_mut(&mut self) -> &mut Document {
        if let Some(b) = &mut self.batch {
            &mut b.staging
        } else {
            &mut self.doc
        }
    }

    /// Drop a batch that went idle — called once per dispatch so abandoned
    /// sessions need no timer thread.
    pub fn expire_batch(&mut self) {
        if self
            .batch
            .as_ref()
            .is_some_and(|b| b.last_activity.elapsed() > BATCH_TTL)
        {
            self.batch = None;
        }
    }

    /// Open a named transaction. One at a time — a second begin is an error
    /// naming the open checkpoint (a caller cannot silently hijack it).
    // the Err arm is the wire-level JSON-RPC error payload — it is the
    // value being returned, not overhead, so boxing it buys nothing
    #[allow(clippy::result_large_err)]
    pub fn begin_batch(&mut self, label: String) -> Result<u64, CallToolResponse> {
        if let Some(b) = &self.batch {
            return Err(err_json(
                serde_json::json!({
                    "error": "batch_open",
                    "open_label": b.label,
                    "staged_ops": b.ops.len(),
                    "hint": "commit_transaction or rollback_transaction first",
                })
                .to_string(),
            ));
        }
        let base = self.doc.revision();
        self.batch = Some(Batch {
            label,
            base,
            staging: self.doc.clone(),
            ops: Vec::new(),
            last_activity: Instant::now(),
        });
        Ok(base)
    }

    /// Route an edit: stage into the open batch, or commit as one undo step.
    /// A staged apply is still atomic per call — a failing call cannot
    /// corrupt the checkpoint.
    pub fn apply_or_stage(
        &mut self,
        label: &str,
        ops: Vec<Op>,
    ) -> Result<StageOutcome, ApplyError> {
        if let Some(b) = &mut self.batch {
            let summary = change_summary(&ops);
            let tx = Transaction {
                label: label.into(),
                base: b.staging.revision(),
                ops: ops.clone(),
            };
            let rev = b.staging.apply(tx)?.revision;
            // batch accumulates the caller's ops; normalization is
            // re-derived when the merged transaction commits
            b.ops.extend(ops);
            b.last_activity = Instant::now();
            return Ok(StageOutcome::Staged {
                pending_ops: b.ops.len(),
                staged_revision: rev,
                summary,
            });
        }
        let summary = change_summary(&ops);
        Ok(StageOutcome::Committed {
            revision: self.apply_origin(TxOrigin::Mcp, label, ops)?,
            summary,
        })
    }

    /// Commit the staged ops as ONE transaction on the real document — one
    /// undo step labelled after the checkpoint. `dry_run` validates the
    /// merged ops against a clone of the committed document and keeps the
    /// batch open. A document changed since begin yields a stale-revision
    /// conflict; the batch stays open so the caller can inspect and decide.
    // the Err arm is the wire-level JSON-RPC error payload — it is the
    // value being returned, not overhead, so boxing it buys nothing
    #[allow(clippy::result_large_err)]
    pub fn commit_batch(&mut self, dry_run: bool) -> Result<serde_json::Value, CallToolResponse> {
        let Some(b) = self.batch.take() else {
            return Err(err_json("no open transaction"));
        };
        let changes = change_summary(&b.ops);
        let n_ops = b.ops.len();
        let label = b.label.clone();
        let cur = self.doc.revision();
        if cur != b.base {
            let resp = err_json(
                serde_json::json!({
                    "error": "stale_base",
                    "batch_base_revision": b.base,
                    "current_revision": cur,
                    "hint": "the document changed since begin_transaction (concurrent edit); re-read it, then re-plan — or rollback_transaction",
                })
                .to_string(),
            );
            self.batch = Some(b);
            return Err(resp);
        }
        if dry_run {
            let mut check = self.doc.clone();
            let result = check.apply(Transaction {
                label: label.clone(),
                base: cur,
                ops: b.ops.clone(),
            });
            self.batch = Some(b);
            return match result {
                Ok(applied) => Ok(serde_json::json!({
                    "dry_run": true,
                    "valid": true,
                    "label": label,
                    "ops": n_ops,
                    "summary": change_summary_json(&changes),
                    "would_be_revision": applied.revision,
                })),
                Err(e) => Err(err_json(format!("dry_run failed: {e}"))),
            };
        }
        match self.apply_origin(TxOrigin::Mcp, &label, b.ops.clone()) {
            Ok(rev) => Ok(serde_json::json!({
                "committed": true,
                "label": label,
                "ops": n_ops,
                "summary": change_summary_json(&changes),
                "revision": rev,
            })),
            Err(e) => {
                let resp = err_json(e.to_string());
                self.batch = Some(b);
                Err(resp)
            }
        }
    }
}

// note-on = status 0x9x with nonzero velocity
fn is_note_on(e: &Event) -> bool {
    matches!(&e.kind, EventKind::Channel { status, data, .. } if status & 0xF0 == 0x90 && data[1] > 0)
}

// 0 = note-on, 1 = controller, 2 = meta, 3 = other (note-off, pitch bend, sysex...)
fn classify(e: &Event) -> u8 {
    match &e.kind {
        EventKind::Channel { status, .. } if status & 0xF0 == 0xB0 => 1,
        EventKind::Channel { .. } if is_note_on(e) => 0,
        EventKind::Meta { .. } => 2,
        _ => 3,
    }
}

fn tick_extend(s: &mut ChangeSummary, t: u64) {
    s.tick_range = Some(match s.tick_range {
        None => (t, t),
        Some((lo, hi)) => (lo.min(t), hi.max(t)),
    });
}

fn class_count(s: &mut ChangeSummary, class: u8, ins: bool) {
    match (class, ins) {
        (0, true) => s.notes_inserted += 1,
        (0, false) => s.notes_removed += 1,
        (1, _) => s.cc_changes += 1,
        (2, _) => s.meta_changes += 1,
        _ => s.other_events += 1,
    }
}

/// Structured change summary of an op list — computed from the ops actually
/// committed (or staged), so the report cannot drift from what apply() did.
pub fn change_summary(ops: &[Op]) -> ChangeSummary {
    let mut s = ChangeSummary::default();
    let mut tracks = std::collections::BTreeSet::new();
    for op in ops {
        s.ops += 1;
        match op {
            Op::InsertEvents { track, events } => {
                tracks.insert(*track);
                for e in events {
                    s.inserted += 1;
                    tick_extend(&mut s, e.tick);
                    class_count(&mut s, classify(e), true);
                }
            }
            Op::RemoveEvents { track, removed } => {
                tracks.insert(*track);
                for (_, e) in removed {
                    s.removed += 1;
                    tick_extend(&mut s, e.tick);
                    class_count(&mut s, classify(e), false);
                }
            }
            Op::UpdateEvent {
                track,
                before,
                after,
                ..
            } => {
                tracks.insert(*track);
                s.updated += 1;
                tick_extend(&mut s, before.tick.min(after.tick));
                tick_extend(&mut s, before.tick.max(after.tick));
                // a note-on whose tick or key changed = a moved note
                let key = |e: &Event| match &e.kind {
                    EventKind::Channel { data, .. } => data[0],
                    _ => 0,
                };
                if is_note_on(before) && (before.tick != after.tick || key(before) != key(after)) {
                    s.notes_moved += 1;
                } else {
                    match classify(after) {
                        1 => s.cc_changes += 1,
                        2 => s.meta_changes += 1,
                        _ => {}
                    }
                }
            }
            Op::InsertTrack { index, track } | Op::RemoveTrack { index, track } => {
                let ins = matches!(op, Op::InsertTrack { .. });
                tracks.insert(*index);
                for e in &track.events {
                    if ins {
                        s.inserted += 1;
                    } else {
                        s.removed += 1;
                    }
                    tick_extend(&mut s, e.tick);
                    class_count(&mut s, classify(e), ins);
                }
            }
            Op::UpdateTrack { index, .. } => {
                tracks.insert(*index);
                s.updated += 1;
                s.meta_changes += 1; // the only UpdateTrack field is the name meta
            }
            Op::SetFormat { before, after } => {
                s.format_change = Some((*before, *after));
            }
        }
    }
    s.tracks_touched = tracks.into_iter().collect();
    s
}

pub(crate) fn merge_summary(a: &mut ChangeSummary, b: &ChangeSummary) {
    a.ops += b.ops;
    a.inserted += b.inserted;
    a.removed += b.removed;
    a.updated += b.updated;
    a.notes_inserted += b.notes_inserted;
    a.notes_removed += b.notes_removed;
    a.notes_moved += b.notes_moved;
    a.cc_changes += b.cc_changes;
    a.meta_changes += b.meta_changes;
    a.other_events += b.other_events;
    if b.format_change.is_some() {
        a.format_change = b.format_change;
    }
    for t in &b.tracks_touched {
        if !a.tracks_touched.contains(t) {
            a.tracks_touched.push(*t);
        }
    }
    a.tracks_touched.sort_unstable();
    if let Some((lo, hi)) = b.tick_range {
        a.tick_range = Some(match a.tick_range {
            None => (lo, hi),
            Some((l, h)) => (l.min(lo), h.max(hi)),
        });
    }
}

pub(crate) fn change_summary_json(s: &ChangeSummary) -> serde_json::Value {
    serde_json::json!({
        "ops": s.ops,
        "inserted": s.inserted,
        "removed": s.removed,
        "updated": s.updated,
        "notes": {
            "inserted": s.notes_inserted,
            "removed": s.notes_removed,
            "moved": s.notes_moved,
        },
        "cc_changes": s.cc_changes,
        "meta_changes": s.meta_changes,
        "other_events": s.other_events,
        "tracks_touched": s.tracks_touched,
        "tick_range": s.tick_range.map(|(lo, hi)| vec![lo, hi]),
        "format_change": s.format_change.map(|(b, a)| serde_json::json!({"before": b, "after": a})),
    })
}

pub(crate) fn tx_record_json(r: &TxRecord) -> serde_json::Value {
    serde_json::json!({
        "base_revision": r.base,
        "revision": r.revision,
        "label": r.label,
        "origin": match r.origin {
            TxOrigin::Gui => "gui",
            TxOrigin::Mcp => "mcp",
        },
        "kind": match r.kind {
            TxKind::Commit => "commit",
            TxKind::Undo => "undo",
            TxKind::Redo => "redo",
        },
        "summary": change_summary_json(&r.summary),
    })
}

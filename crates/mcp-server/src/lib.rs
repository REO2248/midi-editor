//! Embedded MCP server.
//!
//! Topology:
//!   - the GUI app hosts Streamable-HTTP on 127.0.0.1 (optional Bearer token
//!     via `MIDI_MCP_TOKEN`), sharing one `SharedDoc` with the editor.
//!   - `mcp-bridge` connects to that endpoint as an rmcp client and re-serves
//!     it over stdio so stdio-only clients (Claude Desktop etc.) can reach it.
//!     With `--file`, it can also serve a standalone document without the app.
//!
//! Every mutation goes through `Document::apply(Transaction)` on the shared
//! doc — the exact same path GUI edits take — so undo is unified.

use bytes::Bytes;
use commands::UndoStack;
use document::{ApplyError, Document, Event, EventId, Op, Transaction};
use midi_io::Destination;
use smf_core::EventKind;
use std::collections::{HashMap, HashSet};
use rmcp::model::*;
use rmcp::service::{RequestContext, ServiceExt};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A transport action the MCP side requests and the GUI poller drains —
/// playback itself lives in the app process (owns sinks/audio), MCP just asks.
#[derive(Debug, Clone, PartialEq)]
pub enum TransportReq {
    Play,
    Stop,
    Seek { tick: u64 },
}

/// Shared editor state. The GUI owns one `Arc`; MCP handlers hold clones and
/// lock briefly per request. `gui_notify` is bumped on every MCP-side edit so
/// the UI can poll and repaint (the GUI has no push channel into views).
///
/// The destination/track state lives here too (not in the view) so MCP tools
/// can see and route the same destinations the GUI offers, and so the GUI's
/// sidecar persist() covers MCP-originated routing changes.
pub struct Shared {
    pub doc: Document,
    pub undo: UndoStack,
    pub path: Option<PathBuf>,
    pub saved_revision: u64,
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
    pub loop_enabled: bool,
    /// opt-in: also chase the last complete SysEx message on play/loop wrap
    /// (a chased GM/GS/XG reset can wipe the channel-state chase)
    pub chase_sysex: bool,
    /// the GUI drains `transport_req` and repaints on `gui_notify`; false in
    /// standalone `mcp-bridge --file` mode (feature-detected via editor_info)
    pub gui_attached: bool,
    /// drained by the GUI watcher
    pub transport_req: Vec<TransportReq>,
    /// open named transaction (begin_transaction) — staged edits live here
    /// until commit/rollback; never blocks GUI edits on the real document
    pub batch: Option<Batch>,
    /// bounded committed-transaction log (oldest evicted past TX_HISTORY_CAP)
    pub history: std::collections::VecDeque<TxRecord>,
    /// last agent-originated committed transaction — the GUI watches this to
    /// show "MCP: <label>" in the status bar
    pub last_mcp_tx: Option<TxRecord>,
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
    Committed { revision: u64, summary: ChangeSummary },
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
        Self {
            doc,
            undo: UndoStack::new(512),
            path: None,
            saved_revision: 0,
            gui_notify: Arc::new(AtomicU64::new(0)),
            dests: Vec::new(),
            default_dest: 0,
            track_dest: HashMap::new(),
            muted: HashSet::new(),
            soloed: HashSet::new(),
            metronome: false,
            loop_enabled: false,
            chase_sysex: false,
            gui_attached: false,
            transport_req: Vec::new(),
            batch: None,
            history: std::collections::VecDeque::new(),
            last_mcp_tx: None,
        }
    }

    /// Index into `dests` for `dest`, appending a fresh entry when absent.
    /// Missing MIDI ports keep their identity — `open_named` fails at play
    /// time, which surfaces a readable error instead of a wrong port.
    pub fn ensure_dest(&mut self, label: &str, dest: Destination) -> usize {
        if let Some(i) = self.dests.iter().position(|(_, d)| *d == dest) {
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
        let summary = change_summary(&ops);
        let tx = Transaction {
            label: label.into(),
            base: self.doc.revision(),
            ops,
        };
        let base = self.doc.revision();
        let rev = self.doc.apply(tx.clone())?;
        self.undo.push(tx);
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
                ops,
            };
            let rev = b.staging.apply(tx.clone())?;
            b.ops.extend(tx.ops);
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
                Ok(rev) => Ok(serde_json::json!({
                    "dry_run": true,
                    "valid": true,
                    "label": label,
                    "ops": n_ops,
                    "summary": change_summary_json(&changes),
                    "would_be_revision": rev,
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
                if is_note_on(before)
                    && (before.tick != after.tick || key(before) != key(after))
                {
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
        }
    }
    s.tracks_touched = tracks.into_iter().collect();
    s
}

fn merge_summary(a: &mut ChangeSummary, b: &ChangeSummary) {
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

fn change_summary_json(s: &ChangeSummary) -> serde_json::Value {
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
    })
}

fn tx_record_json(r: &TxRecord) -> serde_json::Value {
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

#[derive(Clone)]
pub struct MidiService {
    doc: SharedDoc,
}

impl MidiService {
    pub fn new(doc: SharedDoc) -> Self {
        Self { doc }
    }
}

fn tool(name: &'static str, description: &str, schema: serde_json::Value) -> Tool {
    let obj = schema.as_object().cloned().unwrap_or_default();
    Tool::new(name, description.to_string(), Arc::new(obj))
}

fn object_schema(props: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": props })
}

/// Cap on hex-encoded payloads accepted from a request — hex_to_bytes on an
/// unbounded string would let one request allocate arbitrarily much.
const MAX_HEX_BYTES: usize = 1 << 20; // 1 MiB decoded
/// Cap on the number of events one apply_patch op may insert.
const MAX_INSERT_EVENTS: usize = 10_000;
/// Cap on the number of rows a read tool may return in one call.
const MAX_QUERY_LIMIT: usize = 10_000;

/// Contract version of the whole MCP tool surface. Bump on ANY breaking
/// change: renaming/removing a tool, renaming required arguments, or
/// narrowing a response. Additive changes (new tool, new optional arg, new
/// response field) don't require a bump — but the checked-in schema
/// snapshot test still fails on every surface diff, so even additive
/// changes are deliberate.
pub const MCP_SURFACE_VERSION: u32 = 1;

/// One tool's contract metadata as advertised by `editor_info` and covered
/// by the schema snapshot test. `version` starts at 1 and is bumped when
/// the tool's input schema or response shape changes incompatibly;
/// `deprecated` is set when a tool is scheduled for removal (value = what
/// to use instead) so agents can migrate before it disappears.
pub struct ToolSpec {
    pub name: &'static str,
    pub version: u32,
    pub deprecated: Option<&'static str>,
    pub tool: Tool,
}

fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !s.len().is_multiple_of(2) || s.len() / 2 > MAX_HEX_BYTES {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

fn bytes_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn event_json(e: &Event) -> serde_json::Value {
    let (kind, detail) = match &e.kind {
        EventKind::Channel { status, data, len } => (
            "channel",
            serde_json::json!({
                "status": format!("0x{status:02x}"), "type": format!("0x{:02x}", status & 0xF0),
                "channel": status & 0x0F, "data": [data[0], data[1]], "len": len,
            }),
        ),
        EventKind::Meta { meta_type, data } => (
            "meta",
            serde_json::json!({"type": format!("0x{meta_type:02x}"), "data_hex": bytes_hex(data)}),
        ),
        EventKind::SysEx(d) => ("sysex", serde_json::json!({"data_hex": bytes_hex(d)})),
        EventKind::Escape(d) => ("escape", serde_json::json!({"data_hex": bytes_hex(d)})),
    };
    serde_json::json!({
        "id": e.id, "tick": e.tick, "seq": e.seq, "kind": kind,
        "detail": detail, "raw_hex": e.raw_body.as_ref().map(|b| bytes_hex(b)),
    })
}

fn note_json(n: &document::Note) -> serde_json::Value {
    serde_json::json!({
        "track": n.track, "channel": n.channel, "key": n.key, "vel": n.vel,
        "start": n.start_tick, "end": n.end_tick, "on_id": n.on_id, "off_id": n.off_id,
    })
}

// ── pagination ──────────────────────────────────────────────────────────
// A cursor is `{revision}.{sort-key fields joined by '.'}` — the key of the
// previous page's last row, minted under a document revision. Positions are
// only stable on a fixed revision, so a revision mismatch is a structured
// stale-cursor error (with a restart hint), never a silently wrong page.
fn parse_cursor(s: &str, key_len: usize) -> Result<Vec<u64>, serde_json::Value> {
    let bad = || {
        serde_json::json!({
            "error": "bad_cursor",
            "hint": "cursors are opaque — re-issue the query without 'cursor'",
        })
    };
    let parts: Vec<_> = s.split('.').collect();
    if parts.len() != key_len + 1 {
        return Err(bad());
    }
    parts
        .iter()
        .map(|p| p.parse::<u64>().map_err(|_| bad()))
        .collect()
}

/// `args["cursor"]` → resume key, or an error response (bad shape / stale).
fn cursor_arg(
    sh: &Shared,
    args: &serde_json::Value,
    key_len: usize,
) -> Result<Option<Vec<u64>>, CallToolResponse> {
    match &args["cursor"] {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) => match parse_cursor(s, key_len) {
            Err(e) => Err(err_json(e.to_string())),
            Ok(c) => {
                let cur = sh.view().revision();
                if c[0] != cur {
                    return Err(err_json(
                        serde_json::json!({
                            "error": "stale_cursor",
                            "cursor_revision": c[0],
                            "current_revision": cur,
                            "hint": "document changed — restart pagination without 'cursor'",
                        })
                        .to_string(),
                    ));
                }
                Ok(Some(c[1..].to_vec()))
            }
        },
        _ => Err(err_json("'cursor' must be a string")),
    }
}

/// `args["fields"]` — top-level key allowlist applied to each emitted row so
/// callers can drop heavy fields (raw_hex/data_hex) they don't need.
fn field_projection(args: &serde_json::Value) -> Option<Vec<String>> {
    args["fields"].as_array().map(|a| {
        a.iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect()
    })
}

fn project_fields(mut v: serde_json::Value, fields: &Option<Vec<String>>) -> serde_json::Value {
    if let Some(f) = fields {
        if let Some(m) = v.as_object_mut() {
            m.retain(|k, _| f.iter().any(|x| x == k));
        }
    }
    v
}

/// The `editor_info` response — one call gives an agent everything it needs
/// to feature-detect the running editor and its tool surface instead of
/// probing behavior through trial-and-error mutations.
fn editor_info_json(sh: &Shared) -> serde_json::Value {
    let division = match sh.view().division {
        smf_core::Division::Metrical(ppq) => {
            serde_json::json!({"kind": "metrical", "ppq": ppq})
        }
        smf_core::Division::Smpte {
            fps,
            ticks_per_frame,
        } => serde_json::json!({"kind": "smpte", "fps": fps, "ticks_per_frame": ticks_per_frame}),
    };
    serde_json::json!({
        "name": "midi-editor",
        "version": env!("CARGO_PKG_VERSION"),
        "commit": env!("MIDI_EDITOR_COMMIT"),
        "mcp_surface_version": MCP_SURFACE_VERSION,
        "transports": ["streamable-http", "stdio-bridge"],
        "document": {
            "format": sh.view().format,
            "division": division,
            "tracks": sh.view().tracks.len(),
            "revision": sh.view().revision(),
            "open_transaction": sh.batch.as_ref().map(|b| serde_json::json!({
                "label": b.label, "staged_ops": b.ops.len(),
            })),
        },
        "features": {
            "smf": {
                "formats": [0, 1, 2],
                "sysex": true,
                "escape_events": true,
                "byte_exact_roundtrip": true,
                "text_encodings": ["auto", "utf-8", "shift-jis", "latin-1"],
            },
            "destinations": ["midi_port", "vst3"],
            "editing": {
                "undo": true,
                "redo": true,
                "dry_run": true,
                "base_revision": true,
                "atomic_transactions": true,
                "batch_transactions": true,
                "transaction_history": true,
            },
            "queries": {
                "cursor_pagination": true,
                "field_projection": true,
            },
            // these only work while the desktop app hosts the document
            "transport": sh.gui_attached,
            "midi_recording": sh.gui_attached,
            "vst3_host": sh.gui_attached,
        },
        "tools": tool_specs()
            .iter()
            .map(|s| serde_json::json!({
                "name": s.name,
                "version": s.version,
                "deprecated": s.deprecated,
            }))
            .collect::<Vec<_>>(),
    })
}

fn summary_json(d: &Document, path: &Option<PathBuf>, saved_rev: u64) -> serde_json::Value {
    let last_tick = doc_last_tick(d);
    serde_json::json!({
        "format": d.format,
        "division": format!("{:?}", d.division),
        "tracks": d.tracks.iter().enumerate().map(|(i, t)| serde_json::json!({
            "index": i,
            "name": t.name.as_ref().map(|b| smf_core::decode_text(b, d.text_encoding_hint())),
            "events": t.events.len(),
        })).collect::<Vec<_>>(),
        "events": d.tracks.iter().map(|t| t.events.len()).sum::<usize>(),
        "notes": d.notes().len(),
        "last_tick": last_tick,
        "duration_us": d.tempo_map.tick_to_us(last_tick),
        "revision": d.revision(),
        "path": path,
        "dirty": d.revision() != saved_rev,
    })
}

#[derive(Debug)]
enum PatchError {
    Msg(String),
}

fn find_event(doc: &Document, id: EventId) -> Option<(usize, usize)> {
    for (ti, t) in doc.tracks.iter().enumerate() {
        if let Some(ei) = t.events.iter().position(|e| e.id == id) {
            return Some((ti, ei));
        }
    }
    None
}

/// Parse the JSON op list into `document::Op`s against `doc` (ids allocated
/// via `doc.alloc_event_id`, before-images looked up here so the client only
/// specifies intent).
fn build_ops(doc: &mut Document, ops: &[serde_json::Value]) -> Result<Vec<Op>, PatchError> {
    /// Required, in-range track index — destructive ops must never silently
    /// fall back to track 0.
    fn track_arg(doc: &Document, op: &serde_json::Value) -> Result<usize, PatchError> {
        let t = op["track"]
            .as_u64()
            .ok_or_else(|| PatchError::Msg("'track' is required".into()))? as usize;
        if t >= doc.tracks.len() {
            return Err(PatchError::Msg(format!(
                "no track {t} (document has {})",
                doc.tracks.len()
            )));
        }
        Ok(t)
    }

    let mut out = Vec::new();
    for op in ops {
        let kind = op.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "insert_note" => {
                let track = track_arg(doc, op)?;
                let key = op["key"].as_u64().unwrap_or(60).clamp(0, 127) as u8;
                let vel = op["vel"].as_u64().unwrap_or(100).clamp(1, 127) as u8;
                let start = op["start"].as_u64().unwrap_or(0);
                let dur = op["dur"].as_u64().unwrap_or(480);
                let ch = op["channel"].as_u64().unwrap_or(0).clamp(0, 15) as u8;
                let on_id = doc.alloc_event_id();
                let off_id = doc.alloc_event_id();
                out.push(Op::InsertEvents {
                    track,
                    events: vec![
                        Event {
                            id: on_id,
                            tick: start,
                            seq: u32::MAX / 2,
                            raw_body: None,
                            kind: EventKind::Channel {
                                status: 0x90 | ch,
                                data: [key, vel],
                                len: 2,
                            },
                        },
                        Event {
                            id: off_id,
                            tick: start.saturating_add(dur),
                            seq: u32::MAX / 2,
                            raw_body: None,
                            kind: EventKind::Channel {
                                status: 0x80 | ch,
                                data: [key, 0],
                                len: 2,
                            },
                        },
                    ],
                });
            }
            "insert_events" => {
                let track = track_arg(doc, op)?;
                let events_json = op["events"].as_array().cloned().unwrap_or_default();
                if events_json.len() > MAX_INSERT_EVENTS {
                    return Err(PatchError::Msg(format!(
                        "too many events in one op ({} > {MAX_INSERT_EVENTS})",
                        events_json.len()
                    )));
                }
                let mut events = Vec::new();
                for ev in events_json {
                    let tick = ev["tick"].as_u64().unwrap_or(0);
                    let seq = ev["seq"].as_u64().unwrap_or(u32::MAX as u64 / 2) as u32;
                    let kind_json = &ev["kind"];
                    let kind = if let Some(c) = kind_json.get("channel") {
                        let status = c["status"].as_u64().unwrap_or(0x90) as u8;
                        let data: Vec<u8> = c["data"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|v| v.as_u64().map(|x| x as u8)).collect())
                            .unwrap_or_default();
                        EventKind::Channel {
                            status,
                            data: [data.first().copied().unwrap_or(0), data.get(1).copied().unwrap_or(0)],
                            len: data.len().clamp(1, 2) as u8,
                        }
                    } else if let Some(m) = kind_json.get("meta") {
                        let mt = m["type"].as_u64().unwrap_or(0) as u8;
                        let data = if let Some(h) = m["data_hex"].as_str() {
                            hex_to_bytes(h).ok_or_else(|| {
                                PatchError::Msg("invalid or oversized data_hex".into())
                            })?
                        } else if let Some(u) = m["data_utf8"].as_str() {
                            u.as_bytes().to_vec()
                        } else {
                            vec![]
                        };
                        EventKind::Meta {
                            meta_type: mt,
                            data: Bytes::from(data),
                        }
                    } else if let Some(h) = kind_json["sysex_hex"].as_str() {
                        EventKind::SysEx(Bytes::from(
                            hex_to_bytes(h)
                                .ok_or_else(|| PatchError::Msg("invalid or oversized sysex_hex".into()))?,
                        ))
                    } else {
                        return Err(PatchError::Msg("bad event kind".into()));
                    };
                    events.push(Event {
                        id: doc.alloc_event_id(),
                        tick,
                        seq,
                        raw_body: None,
                        kind,
                    });
                }
                out.push(Op::InsertEvents { track, events });
            }
            "remove_events" => {
                let mut by_track: std::collections::HashMap<usize, Vec<(usize, Event)>> =
                    Default::default();
                for id in op["ids"].as_array().cloned().unwrap_or_default() {
                    let id = id.as_u64().unwrap_or(0);
                    if let Some((ti, ei)) = find_event(doc, id) {
                        by_track
                            .entry(ti)
                            .or_default()
                            .push((ei, doc.tracks[ti].events[ei].clone()));
                    } else {
                        return Err(PatchError::Msg(format!("unknown event id {id}")));
                    }
                }
                for (track, removed) in by_track {
                    out.push(Op::RemoveEvents { track, removed });
                }
            }
            "move_note" => {
                let on_id = op["on_id"].as_u64().unwrap_or(0);
                let dtick = op["dtick"].as_i64().unwrap_or(0);
                let dkey = op["dkey"].as_i64().unwrap_or(0) as i32;
                let dlen = op["dur_dtick"].as_i64().unwrap_or(0);
                if find_event(doc, on_id).is_none() {
                    return Err(PatchError::Msg(format!("unknown on_id {on_id}")));
                }
                let notes = doc.notes();
                let Some(note) = notes.iter().find(|n| n.on_id == on_id) else {
                    return Err(PatchError::Msg(format!("event {on_id} is not a NoteOn")));
                };
                let mv = |eid: EventId, base_tick: u64| -> Result<Op, PatchError> {
                    let (et, ei) = find_event(doc, eid).ok_or_else(|| {
                        PatchError::Msg(format!("event {eid} vanished while building ops"))
                    })?;
                    let before = doc.tracks[et].events[ei].clone();
                    let mut after = before.clone();
                    after.tick = (base_tick as i64
                        + dtick
                        + if eid != on_id { dlen } else { 0 })
                    .max(0) as u64;
                    if let EventKind::Channel { data, .. } = &mut after.kind {
                        data[0] = (note.key as i32 + dkey).clamp(0, 127) as u8;
                    }
                    after.raw_body = None;
                    Ok(Op::UpdateEvent {
                        track: et,
                        before,
                        after,
                    })
                };
                out.push(mv(on_id, note.start_tick)?);
                if let Some(off_id) = note.off_id {
                    out.push(mv(off_id, note.end_tick.unwrap_or(note.start_tick))?);
                }
            }
            "set_tempo" => {
                let tick = op["tick"].as_u64().unwrap_or(0);
                let bpm = op["bpm"].as_f64().unwrap_or(120.0);
                let mpq = (60_000_000.0 / bpm).round() as u32;
                let data = Bytes::from(mpq.to_be_bytes()[1..4].to_vec());
                // overwrite an existing SetTempo at this tick if present (track 0
                // convention), else insert into track 0
                let existing = doc.tracks.first().and_then(|t| {
                    t.events.iter().find(|e| {
                        e.tick == tick
                            && matches!(e.kind, EventKind::Meta { meta_type: 0x51, .. })
                    })
                });
                match existing {
                    Some(e) => {
                        let mut after = e.clone();
                        after.kind = EventKind::Meta {
                            meta_type: 0x51,
                            data: data.clone(),
                        };
                        after.raw_body = None;
                        out.push(Op::UpdateEvent {
                            track: 0,
                            before: e.clone(),
                            after,
                        });
                    }
                    None => out.push(Op::InsertEvents {
                        track: 0,
                        events: vec![Event {
                            id: doc.alloc_event_id(),
                            tick,
                            seq: 0,
                            raw_body: None,
                            kind: EventKind::Meta {
                                meta_type: 0x51,
                                data,
                            },
                        }],
                    }),
                }
            }
            other => return Err(PatchError::Msg(format!("unknown op '{other}'"))),
        }
    }
    Ok(out)
}

fn ok_json(v: serde_json::Value) -> CallToolResponse {
    CallToolResult::success(vec![ContentBlock::text(v.to_string())]).into()
}

fn err_json(msg: impl Into<String>) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(msg.into())]).into()
}

impl ServerHandler for MidiService {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("midi-editor", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Pure-SMF MIDI editor. All edits go through Document::apply transactions; \
                 one tool call = one undo step, or begin_transaction groups many calls into \
                 one named checkpoint commit. Ticks are absolute PPQ ticks; keys 0-127; \
                 channels 0-15. Use query_events to find event ids for edits.",
            )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_specs()
            .into_iter()
            .find(|s| s.name == name)
            .map(|s| s.tool)
    }

    fn list_tools(
        &self,
        _req: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(
            tool_specs().into_iter().map(|s| s.tool).collect(),
        )))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, McpError>> + '_ {
        let name = request.name.to_string();
        let args = request.arguments.unwrap_or_default();
        let args = serde_json::Value::Object(args);
        let shared = self.doc.clone();
        async move { Ok(dispatch(&name, &args, shared)) }
    }
}

/// The tool registry — single source for `list_tools`, dispatch, the
/// `editor_info` feature report, and the schema snapshot test. Every tool
/// starts at `version: 1`; bump on breaking schema changes and set
/// `deprecated` (with a migration hint) before removing one.
pub fn tool_specs() -> Vec<ToolSpec> {
    let spec = |name: &'static str, description: &str, schema: serde_json::Value| ToolSpec {
        name,
        version: 1,
        deprecated: None,
        tool: tool(name, description, schema),
    };
    vec![
        spec(
            "editor_info",
            "Editor capabilities contract: semver, commit/build id, MCP surface version, supported SMF features, destination kinds, live-feature flags (transport/undo/dry_run/base_revision), and the per-tool version/deprecation table. Call first for feature detection.",
            object_schema(serde_json::json!({})),
        ),
        spec(
            "document_summary",
            "JSON summary: format/division, per-track names+counts, note count, duration, revision, dirty flag",
            object_schema(serde_json::json!({})),
        ),
        spec(
            "begin_transaction",
            "Open a named checkpoint: every later edit tool stages on a private document copy (reads see staged state) until commit_transaction folds them into ONE undo step or rollback_transaction discards them. One open batch at a time; ~5min idle auto-rollback keeps abandoned sessions from pinning the document. Args: label?.",
            object_schema(serde_json::json!({
                "label": {"type": "string"},
            })),
        ),
        spec(
            "commit_transaction",
            "Fold the open transaction's staged ops into a single undo step labelled with the checkpoint name. dry_run:true validates the merged ops against the committed document without applying and keeps the transaction open. Errors with stale_base when the document changed since begin (concurrent edit) — the batch stays open for rollback/re-plan.",
            object_schema(serde_json::json!({
                "dry_run": {"type": "boolean"},
            })),
        ),
        spec(
            "rollback_transaction",
            "Discard the open transaction. The committed document is left exactly as it was at begin_transaction (byte-for-byte) — staged ops never touched it.",
            object_schema(serde_json::json!({})),
        ),
        spec(
            "transaction_status",
            "Open-transaction state: label, base/staged revisions, staged op count, age and time until auto-rollback.",
            object_schema(serde_json::json!({})),
        ),
        spec(
            "transaction_history",
            "Bounded log of committed transactions (cap 64, newest first): {revision, base_revision, label, origin (gui|mcp), kind (commit|undo|redo), summary}. Args: limit? (default 20).",
            object_schema(serde_json::json!({
                "limit": {"type": "integer"},
            })),
        ),
        spec(
            "changes_since_revision",
            "All transactions committed after `revision` plus their merged change summary — verify an edit's effect without re-querying the document. `truncated` when the bounded history no longer reaches that far back.",
            object_schema(serde_json::json!({
                "revision": {"type": "integer"},
            })),
        ),
        spec(
            "list_notes",
            "Paired note view (NoteOn+NoteOff). Args: track?, from_tick?, to_tick?, limit? (default 500, max 10000), cursor?, fields? (row key allowlist). Returns notes + next_cursor + revision; a stale cursor returns a structured restart hint.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"},
                "from_tick": {"type": "integer"},
                "to_tick": {"type": "integer"},
                "limit": {"type": "integer"},
                "cursor": {"type": "string"},
                "fields": {"type": "array", "items": {"type": "string"}},
            })),
        ),
        spec(
            "query_events",
            "Raw SMF events (id, tick, seq, kind, raw_hex) in chronological (tick, seq, track, id) order. Args: track?, from_tick?, to_tick?, limit? (default 500, max 10000), cursor?, fields? (omit e.g. raw_hex to slim rows), offset? (legacy). Returns events + next_cursor + revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"},
                "from_tick": {"type": "integer"},
                "to_tick": {"type": "integer"},
                "limit": {"type": "integer"},
                "offset": {"type": "integer"},
                "cursor": {"type": "string"},
                "fields": {"type": "array", "items": {"type": "string"}},
            })),
        ),
        spec(
            "diagnostics",
            "Import-quality findings over the raw event layer: dangling noteOn, zero-length notes, missing End-of-Track, tempo events outside the conductor track. Each has code/track/tick/event_id + detail.",
            object_schema(serde_json::json!({})),
        ),
        spec(
            "normalize",
            "Resolve import-quality findings as one undo step. Args: codes? (array of diagnostic codes; omitted = fix all). Returns resolved/failed counts.",
            object_schema(serde_json::json!({"codes": {"type": "array", "items": {"type": "string"}}})),
        ),
        spec(
            "apply_patch",
            "Atomic edit as one undo step. Optional base_revision: when given it must match document_summary.revision (optimistic concurrency). \
             dry_run:true returns the op breakdown without applying. ops: insert_note {track,key,vel,start,dur,channel} | \
             insert_events {track,events:[{tick,seq,kind:{channel|meta|sysex_hex}}]} | \
             remove_events {ids} | move_note {on_id,dtick,dkey,dur_dtick} | \
             set_tempo {tick,bpm}",
            object_schema(serde_json::json!({
                "base_revision": {"type": "integer"},
                "label": {"type": "string"},
                "dry_run": {"type": "boolean"},
                "ops": {"type": "array", "items": {"type": "object"}},
            })),
        ),
        spec(
            "undo",
            "Revert the last transaction (shared with GUI edits)",
            object_schema(serde_json::json!({})),
        ),
        spec(
            "redo",
            "Replay the last undone transaction",
            object_schema(serde_json::json!({})),
        ),
        spec(
            "save",
            "Serialize the document to SMF and write it. Args: path? (defaults to the document's open path)",
            object_schema(serde_json::json!({"path": {"type": "string"}})),
        ),
        spec(
            "get_tempo_map",
            "Tempo breakpoints: [{tick, us_per_quarter, bpm, cumulative_us}] + ppq. Read before editing tempo or converting ticks<->time.",
            object_schema(serde_json::json!({})),
        ),
        spec(
            "get_meta",
            "Meta events (names, markers, lyrics, text, tempo, time-sig) with text decoded (UTF-8/SJIS), track-major order. Args: track?, meta_type? (hex int), limit?, cursor?, fields? (omit data_hex to slim rows). Returns meta + next_cursor + revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"},
                "meta_type": {"type": "integer"},
                "limit": {"type": "integer"},
                "cursor": {"type": "string"},
                "fields": {"type": "array", "items": {"type": "string"}},
            })),
        ),
        spec(
            "get_cc",
            "Latest controller value per (track, channel, cc) — the current CC state. Args: track?, channel?, cc?, limit?, cursor?, fields?. Returns cc + next_cursor + revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"},
                "channel": {"type": "integer"},
                "cc": {"type": "integer"},
                "limit": {"type": "integer"},
                "cursor": {"type": "string"},
                "fields": {"type": "array", "items": {"type": "string"}},
            })),
        ),
        spec(
            "list_midi_ports",
            "Enumerate real MIDI outputs/inputs on this machine (WinMM): [{index, name}]. Use names in set_track_destination.",
            object_schema(serde_json::json!({})),
        ),
        spec(
            "list_destinations",
            "Output routing: catalog [{index, label, kind, port_name|plugin_path}], default_dest, per-track overrides, mute/solo.",
            object_schema(serde_json::json!({})),
        ),
        spec(
            "set_track_destination",
            "Route a track to an output. Args: track, destination: {\"midi_port\":\"<name>\"} | {\"vst3\":\"<bundle path>\"} | \"default\" (inherit). Unknown destinations are remembered and fail at play time.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"},
                "destination": {},
            })),
        ),
        spec(
            "transport",
            "Ask the GUI transport: {action: \"play\"|\"stop\"|\"seek\", tick?}. Only works while the app is running.",
            object_schema(serde_json::json!({
                "action": {"type": "string"},
                "tick": {"type": "integer"},
            })),
        ),
        spec(
            "quantize",
            "Snap note onsets to a grid (duration preserved). Args: track? (all when omitted), from?, to?, grid? (ticks, default ppq/4), strength? (0-100, default 100). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "grid": {"type": "integer"}, "strength": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "transpose",
            "Shift note pitch. Args: track?, from?, to?, semitones (+/-). Notes leaving 0..127 are skipped. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "semitones": {"type": "integer"}, "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "scale_velocity",
            "Multiply note velocities. Args: track?, from?, to?, factor (e.g. 1.2 = +20%). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "factor": {"type": "number"}, "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "set_channel",
            "Retarget all channel events in range to one channel. Args: track, from?, to?, channel (1-16). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "channel": {"type": "integer"}, "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "set_program",
            "Program change (with optional bank CC0/CC32) on a track. Args: track, tick, program (0-127), channel? (default track channel), bank_msb?, bank_lsb?. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "tick": {"type": "integer"},
                "program": {"type": "integer"}, "channel": {"type": "integer"},
                "bank_msb": {"type": "integer"}, "bank_lsb": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "set_cc",
            "Insert controller events. Args: track, channel? (default track channel), points: [{tick, cc, value}] — or scalar {tick, cc, value}. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "channel": {"type": "integer"},
                "tick": {"type": "integer"}, "cc": {"type": "integer"}, "value": {"type": "integer"},
                "points": {"type": "array", "items": {"type": "object"}},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "set_pitch_bend",
            "Insert a pitch-bend event. Args: track, tick, value (0..16383, 8192=center), channel?. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "tick": {"type": "integer"},
                "value": {"type": "integer"}, "channel": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "set_tempo",
            "Set/replace tempo at a tick (conductor track). Args: tick, bpm. Optional base_revision.",
            object_schema(serde_json::json!({
                "tick": {"type": "integer"}, "bpm": {"type": "number"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "set_time_signature",
            "Set/replace time signature at a tick. Args: tick, num (beats/bar), den (beat value 4=quarter,8=eighth). Optional base_revision.",
            object_schema(serde_json::json!({
                "tick": {"type": "integer"}, "num": {"type": "integer"}, "den": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "set_track_channel",
            "Set the track's default channel (FF20 meta). Args: track, channel (1-16). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "channel": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "set_track_name",
            "Set track name (UTF-8 meta 0x03). Args: track, name. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "name": {"type": "string"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "add_track",
            "Append a track (with optional name). Args: name?. Optional base_revision.",
            object_schema(serde_json::json!({
                "name": {"type": "string"}, "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "remove_track",
            "Remove a track entirely. Args: track. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "delete_range",
            "Delete channel events in [from,to) (notes delete whole). Args: track, from, to. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "duplicate_range",
            "Copy channel events in [from,to) to start at `to`. Args: track, from, to. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
    ]
}

fn dispatch(
    name: &str,
    args: &serde_json::Value,
    shared: SharedDoc,
) -> CallToolResponse {
    // recover from a poisoned lock: a panic in an earlier critical section
    // must not take down every later request
    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
    sh.expire_batch();
    match name {
        "editor_info" => ok_json(editor_info_json(&sh)),
        "document_summary" => {
            let mut v = summary_json(sh.view(), &sh.path, sh.saved_revision);
            if let Some(b) = &sh.batch {
                v["transaction"] = serde_json::json!({
                    "open": true, "label": b.label,
                    "base_revision": b.base, "staged_ops": b.ops.len(),
                });
            }
            ok_json(v)
        }
        "begin_transaction" => {
            let label = args["label"]
                .as_str()
                .unwrap_or("mcp transaction")
                .to_string();
            match sh.begin_batch(label) {
                Ok(base) => {
                    let b = sh.batch.as_ref().unwrap();
                    ok_json(serde_json::json!({
                        "open": true, "label": b.label, "base_revision": base,
                        "ttl_seconds": BATCH_TTL.as_secs(),
                    }))
                }
                Err(r) => r,
            }
        }
        "commit_transaction" => {
            let dry = args["dry_run"].as_bool().unwrap_or(false);
            match sh.commit_batch(dry) {
                Ok(v) => ok_json(v),
                Err(r) => r,
            }
        }
        "rollback_transaction" => match sh.batch.take() {
            Some(b) => ok_json(serde_json::json!({
                "rolled_back": true, "label": b.label,
                "discarded_ops": b.ops.len(),
            })),
            None => err_json("no open transaction"),
        },
        "transaction_status" => match &sh.batch {
            Some(b) => ok_json(serde_json::json!({
                "open": true,
                "label": b.label,
                "base_revision": b.base,
                "staged_ops": b.ops.len(),
                "staged_revision": b.staging.revision(),
                "age_s": b.last_activity.elapsed().as_secs(),
                "expires_in_s": BATCH_TTL.saturating_sub(b.last_activity.elapsed()).as_secs(),
            })),
            None => ok_json(serde_json::json!({"open": false})),
        },
        "diagnostics" => {
            let diags = sh.view().diagnose();
            ok_json(serde_json::json!({
                "count": diags.len(),
                "diagnostics": diags.iter().map(|d| serde_json::json!({
                    "code": d.code,
                    "track": d.track,
                    "tick": d.tick,
                    "event_id": d.event,
                    "detail": d.detail,
                })).collect::<Vec<_>>(),
            }))
        }
        "normalize" => {
            let code_strs: Vec<String> = args["codes"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let before = sh.view().diagnose().len();
            let codes: Vec<&str> = code_strs.iter().map(String::as_str).collect();
            let ops = sh.view_mut().fix_ops(&codes);
            if ops.is_empty() {
                return ok_json(serde_json::json!({"fixed": 0, "remaining": before}));
            }
            match sh.apply_or_stage("normalize", ops) {
                Ok(outcome) => {
                    let remaining = sh.view().diagnose().len();
                    apply_reply(
                        outcome,
                        serde_json::json!({"fixed": before - remaining, "remaining": remaining}),
                    )
                }
                Err(e) => err_json(e.to_string()),
            }
        }
        "list_notes" => {
            let track = args["track"].as_u64().map(|v| v as usize);
            let from = args["from_tick"].as_u64().unwrap_or(0);
            let to = args["to_tick"].as_u64().unwrap_or(u64::MAX);
            let limit = args["limit"].as_u64().unwrap_or(500).min(MAX_QUERY_LIMIT as u64) as usize;
            let fields = field_projection(&args);
            let after = match cursor_arg(&sh, &args, 4) {
                Ok(a) => a,
                Err(r) => return r,
            };
            // total sort key (start, key, track, on_id) — deterministic on a
            // fixed revision, so a cursor page neither duplicates nor skips
            let key =
                |n: &document::Note| (n.start_tick, n.key as u64, n.track as u64, n.on_id);
            let mut notes: Vec<_> = sh
                .view()
                .notes()
                .into_iter()
                .filter(|n| n.start_tick >= from && n.start_tick <= to)
                .filter(|n| track.is_none() || Some(n.track) == track)
                .collect();
            notes.sort_by_key(|n| key(n));
            let pos = after.map_or(0, |c| {
                notes.partition_point(|n| key(n) <= (c[0], c[1], c[2], c[3]))
            });
            let end = (pos + limit).min(notes.len());
            let rev = sh.view().revision();
            let next = (end > pos && end < notes.len()).then(|| {
                let k = key(&notes[end - 1]);
                format!("{}.{}.{}.{}.{}", rev, k.0, k.1, k.2, k.3)
            });
            ok_json(serde_json::json!({
                "count": end - pos,
                "notes": notes[pos..end].iter().map(|n| project_fields(note_json(n), &fields)).collect::<Vec<_>>(),
                "next_cursor": next,
                "revision": rev,
            }))
        }
        "query_events" => {
            let track = args["track"].as_u64().map(|v| v as usize);
            let from = args["from_tick"].as_u64().unwrap_or(0);
            let to = args["to_tick"].as_u64().unwrap_or(u64::MAX);
            let limit = args["limit"].as_u64().unwrap_or(500).min(MAX_QUERY_LIMIT as u64) as usize;
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let fields = field_projection(&args);
            let after = match cursor_arg(&sh, &args, 4) {
                Ok(a) => a,
                Err(r) => return r,
            };
            // collect light (tick, seq, track, id, idx) refs only; JSON
            // encoding happens for the window, not for every match
            let mut hits: Vec<(u64, u32, u64, u64, usize)> = Vec::new();
            for (ti, t) in sh.view().tracks.iter().enumerate() {
                if track.is_some() && track != Some(ti) {
                    continue;
                }
                for (ei, e) in t.events.iter().enumerate() {
                    if e.tick >= from && e.tick <= to {
                        hits.push((e.tick, e.seq, ti as u64, e.id, ei));
                    }
                }
            }
            // total chronological key (tick, seq, track, id) — no ties
            hits.sort_by_key(|h| (h.0, h.1, h.2, h.3));
            let total = hits.len();
            let pos = after.map_or(0, |c| {
                hits.partition_point(|h| (h.0, h.1, h.2, h.3) <= (c[0], c[1] as u32, c[2], c[3]))
            });
            let pos = (pos + offset).min(total); // legacy offset still honored
            let end = (pos + limit).min(total);
            let rev = sh.view().revision();
            let next = (end > pos && end < total).then(|| {
                let h = &hits[end - 1];
                format!("{}.{}.{}.{}.{}", rev, h.0, h.1, h.2, h.3)
            });
            let evs: Vec<_> = hits[pos..end]
                .iter()
                .map(|&(_, _, ti, _, ei)| {
                    let mut j = event_json(&sh.view().tracks[ti as usize].events[ei]);
                    j["track"] = ti.into();
                    project_fields(j, &fields)
                })
                .collect();
            ok_json(serde_json::json!({
                "total": total, "count": evs.len(), "events": evs,
                "next_cursor": next, "revision": rev,
            }))
        }
        "apply_patch" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let label = args["label"].as_str().unwrap_or("mcp patch");
            let ops_json = args["ops"].as_array().cloned().unwrap_or_default();
            let ops = match build_ops(sh.view_mut(), &ops_json) {
                Ok(o) => o,
                Err(PatchError::Msg(m)) => return err_json(m),
            };
            if ops.is_empty() {
                return err_json("no ops");
            }
            if args["dry_run"].as_bool().unwrap_or(false) {
                // describe what applying would do — nothing is committed
                let detail = ops
                    .iter()
                    .map(|op| match op {
                        Op::InsertEvents { track, events } => serde_json::json!({
                            "op": "insert", "track": track, "events": events.len()}),
                        Op::RemoveEvents { track, removed } => serde_json::json!({
                            "op": "remove", "track": track, "events": removed.len()}),
                        Op::UpdateEvent { track, after, .. } => serde_json::json!({
                            "op": "update", "track": track, "event_id": after.id}),
                        Op::InsertTrack { index, .. } => serde_json::json!({
                            "op": "insert_track", "index": index}),
                        Op::RemoveTrack { index, .. } => serde_json::json!({
                            "op": "remove_track", "index": index}),
                        Op::UpdateTrack { index, after, .. } => serde_json::json!({
                            "op": "update_track", "index": index,
                            "name": after.name.as_ref().map(|b| String::from_utf8_lossy(b).into_owned())}),
                    })
                    .collect::<Vec<_>>();
                return ok_json(serde_json::json!({
                    "dry_run": true, "applied": false, "ops": detail,
                }));
            }
            match sh.apply_or_stage(label, ops) {
                Ok(outcome) => apply_reply(outcome, serde_json::json!({})),
                Err(e) => err_json(e.to_string()),
            }
        }
        "undo" => {
            if sh.batch.is_some() {
                return err_json(
                    "an edit transaction is open — commit_transaction or rollback_transaction first",
                );
            }
            let base = sh.doc.revision();
            // summarize the tx about to be reverted before it pops off the stack
            let pending_ops = sh.undo.peek_done().map(|t| t.ops.clone());
            let res = {
                let Shared { doc, undo, .. } = &mut *sh;
                undo.undo(doc)
            };
            match res {
                Some(l) => {
                    let rev = sh.doc.revision();
                    let summary = change_summary(&pending_ops.unwrap_or_default());
                    sh.record_history(TxRecord {
                        base,
                        revision: rev,
                        label: l.clone(),
                        origin: TxOrigin::Mcp,
                        kind: TxKind::Undo,
                        summary: summary.clone(),
                    });
                    sh.gui_notify.fetch_add(1, Ordering::Relaxed);
                    ok_json(serde_json::json!({
                        "undone": l, "revision": rev, "summary": change_summary_json(&summary),
                    }))
                }
                None => err_json("nothing to undo"),
            }
        }
        "redo" => {
            if sh.batch.is_some() {
                return err_json(
                    "an edit transaction is open — commit_transaction or rollback_transaction first",
                );
            }
            let base = sh.doc.revision();
            let pending_ops = sh.undo.peek_undone().map(|t| t.ops.clone());
            let res = {
                let Shared { doc, undo, .. } = &mut *sh;
                undo.redo(doc)
            };
            match res {
                Some(l) => {
                    let rev = sh.doc.revision();
                    let summary = change_summary(&pending_ops.unwrap_or_default());
                    sh.record_history(TxRecord {
                        base,
                        revision: rev,
                        label: l.clone(),
                        origin: TxOrigin::Mcp,
                        kind: TxKind::Redo,
                        summary: summary.clone(),
                    });
                    sh.gui_notify.fetch_add(1, Ordering::Relaxed);
                    ok_json(serde_json::json!({
                        "redone": l, "revision": rev, "summary": change_summary_json(&summary),
                    }))
                }
                None => err_json("nothing to redo"),
            }
        }
        "transaction_history" => {
            let limit = args["limit"]
                .as_u64()
                .unwrap_or(20)
                .min(TX_HISTORY_CAP as u64) as usize;
            let txs: Vec<_> = sh
                .history
                .iter()
                .rev()
                .take(limit)
                .map(tx_record_json)
                .collect();
            ok_json(serde_json::json!({
                "count": txs.len(),
                "history_cap": TX_HISTORY_CAP,
                "transactions": txs,
            }))
        }
        "changes_since_revision" => {
            let Some(from) = args["revision"].as_u64() else {
                return err_json("'revision' is required");
            };
            let cur = sh.doc.revision();
            if from > cur {
                return err_json(
                    serde_json::json!({
                        "error": "future_revision",
                        "current_revision": cur,
                    })
                    .to_string(),
                );
            }
            let txs: Vec<&TxRecord> =
                sh.history.iter().filter(|r| r.revision > from).collect();
            // coverage is only trustworthy while the oldest retained record
            // reaches back to `from`; older entries were evicted at the cap
            let truncated = from < cur
                && match sh.history.front() {
                    Some(f) => f.base > from,
                    None => true,
                };
            let mut agg = ChangeSummary::default();
            for r in &txs {
                merge_summary(&mut agg, &r.summary);
            }
            ok_json(serde_json::json!({
                "from_revision": from,
                "current_revision": cur,
                "truncated": truncated,
                "hint": if truncated {
                    serde_json::Value::String(format!(
                        "history is bounded at {TX_HISTORY_CAP} entries — re-query the document for full state"
                    ))
                } else {
                    serde_json::Value::Null
                },
                "count": txs.len(),
                "aggregate": change_summary_json(&agg),
                "transactions": txs.iter().map(|r| tx_record_json(r)).collect::<Vec<_>>(),
            }))
        }
        "save" => {
            let path = args["path"]
                .as_str()
                .map(PathBuf::from)
                .or_else(|| sh.path.clone());
            let Some(p) = path else {
                return err_json("no path — pass one or open a file in the editor");
            };
            let (bytes, rev) = (
                sh.doc.serialize(smf_core::WriteOptions {
                    running_status: false,
                }),
                sh.doc.revision(),
            );
            drop(sh); // never hold the editor lock across disk I/O
            match write_atomic(&p, &bytes) {
                Ok(()) => {
                    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
                    sh.saved_revision = rev;
                    ok_json(serde_json::json!({"saved": p.to_string_lossy(), "revision": rev}))
                }
                Err(e) => err_json(e.to_string()),
            }
        }
        "get_tempo_map" => {
            let tm = &sh.view().tempo_map;
            ok_json(serde_json::json!({
                "ppq": tm.ppq(),
                "points": tm.points().iter().map(|(tick, mpq, cum)| serde_json::json!({
                    "tick": tick, "us_per_quarter": mpq,
                    "bpm": (60_000_000.0 / *mpq as f64 * 100.0).round() / 100.0,
                    "cumulative_us": cum,
                })).collect::<Vec<_>>(),
            }))
        }
        "get_meta" => {
            let track = args["track"].as_u64().map(|v| v as usize);
            let mt = args["meta_type"].as_u64().map(|v| v as u8);
            let limit = args["limit"].as_u64().unwrap_or(500).min(MAX_QUERY_LIMIT as u64) as usize;
            let fields = field_projection(&args);
            let after = match cursor_arg(&sh, &args, 4) {
                Ok(a) => a,
                Err(r) => return r,
            };
            let hint = sh.view().text_encoding_hint();
            // light refs first; JSON encoding only for the page window
            let mut hits: Vec<(u64, u64, u32, u64, usize)> = Vec::new();
            for (ti, t) in sh.view().tracks.iter().enumerate() {
                if track.is_some() && track != Some(ti) {
                    continue;
                }
                for (ei, e) in t.events.iter().enumerate() {
                    if let EventKind::Meta { meta_type, .. } = &e.kind {
                        if mt.is_some() && mt != Some(*meta_type) {
                            continue;
                        }
                        hits.push((ti as u64, e.tick, e.seq, e.id, ei));
                    }
                }
            }
            // track-major stable key (track, tick, seq, id)
            hits.sort_by_key(|h| (h.0, h.1, h.2, h.3));
            let pos = after.map_or(0, |c| {
                hits.partition_point(|h| (h.0, h.1, h.2, h.3) <= (c[0], c[1], c[2] as u32, c[3]))
            });
            let end = (pos + limit).min(hits.len());
            let rev = sh.view().revision();
            let next = (end > pos && end < hits.len()).then(|| {
                let h = &hits[end - 1];
                format!("{}.{}.{}.{}.{}", rev, h.0, h.1, h.2, h.3)
            });
            let out: Vec<_> = hits[pos..end]
                .iter()
                .map(|&(ti, _, _, _, ei)| {
                    let e = &sh.view().tracks[ti as usize].events[ei];
                    let EventKind::Meta { meta_type, data } = &e.kind else {
                        unreachable!()
                    };
                    // only 0x01-0x0F are text-family metas; the rest
                    // (tempo, time sig, ports, ...) are binary payloads
                    let text = if (0x01..=0x0f).contains(meta_type) {
                        serde_json::Value::String(smf_core::decode_text(data, hint))
                    } else {
                        serde_json::Value::Null
                    };
                    project_fields(
                        serde_json::json!({
                            "track": ti, "id": e.id, "tick": e.tick,
                            "type": format!("0x{meta_type:02x}"),
                            "text": text,
                            "data_hex": bytes_hex(data),
                        }),
                        &fields,
                    )
                })
                .collect();
            ok_json(serde_json::json!({
                "count": out.len(), "meta": out,
                "next_cursor": next, "revision": rev,
            }))
        }
        "get_cc" => {
            let track = args["track"].as_u64().map(|v| v as usize);
            let chan = args["channel"].as_u64().map(|v| v as u8);
            let ccn = args["cc"].as_u64().map(|v| v as u8);
            let limit = args["limit"].as_u64().unwrap_or(500).min(MAX_QUERY_LIMIT as u64) as usize;
            let fields = field_projection(&args);
            let after = match cursor_arg(&sh, &args, 3) {
                Ok(a) => a,
                Err(r) => return r,
            };
            // latest value wins; events are already tick-sorted
            let mut latest: HashMap<(usize, u8, u8), (u64, u8)> = HashMap::new();
            for (ti, t) in sh.view().tracks.iter().enumerate() {
                if track.is_some() && track != Some(ti) {
                    continue;
                }
                for e in &t.events {
                    if let EventKind::Channel { status, data, .. } = &e.kind {
                        if status & 0xF0 == 0xB0 {
                            let (ch, cc, val) = (status & 0x0F, data[0], data[1]);
                            if chan.is_some() && chan != Some(ch) || ccn.is_some() && ccn != Some(cc) {
                                continue;
                            }
                            latest.insert((ti, ch, cc), (e.tick, val));
                        }
                    }
                }
            }
            let mut rows: Vec<_> = latest.into_iter().collect();
            rows.sort_by_key(|(k, _)| *k);
            // cursor key is the row's own (track, channel, cc)
            let pos = after.map_or(0, |c| {
                rows.partition_point(|(k, _)| {
                    (k.0 as u64, k.1 as u64, k.2 as u64) <= (c[0], c[1], c[2])
                })
            });
            let end = (pos + limit).min(rows.len());
            let rev = sh.view().revision();
            let next = (end > pos && end < rows.len()).then(|| {
                let k = &rows[end - 1].0;
                format!("{}.{}.{}.{}", rev, k.0, k.1, k.2)
            });
            ok_json(serde_json::json!({
                "count": end - pos,
                "cc": rows[pos..end].iter().map(|((t, ch, cc), (tick, v))| project_fields(
                    serde_json::json!({
                        "track": t, "channel": ch + 1, "cc": cc, "value": v, "at_tick": tick,
                    }),
                    &fields,
                )).collect::<Vec<_>>(),
                "next_cursor": next, "revision": rev,
            }))
        }
        "list_midi_ports" => {
            drop(sh); // WinMM enumeration must not stall the editor
            let outs = midi_io::list_outputs()
                .unwrap_or_default()
                .iter()
                .map(|p| serde_json::json!({"index": p.index, "name": p.name}))
                .collect::<Vec<_>>();
            let ins = midi_io::list_inputs()
                .unwrap_or_default()
                .iter()
                .map(|p| serde_json::json!({"index": p.index, "name": p.name}))
                .collect::<Vec<_>>();
            ok_json(serde_json::json!({"outputs": outs, "inputs": ins}))
        }
        "list_destinations" => ok_json(dests_json(&sh)),
        "set_track_destination" => {
            let track = match args["track"].as_u64() {
                Some(t) => t as usize,
                None => return err_json("track required"),
            };
            if track >= sh.view().tracks.len() {
                return err_json(format!("no track {track}"));
            }
            let d = &args["destination"];
            if d.as_str() == Some("default") || d.is_null() {
                sh.track_dest.remove(&track);
            } else {
                let dest = if let Some(p) = d["midi_port"].as_str() {
                    Destination::MidiPort {
                        port_name: p.to_string(),
                    }
                } else if let Some(p) = d["vst3"].as_str() {
                    Destination::Plugin {
                        plugin_path: p.to_string(),
                    }
                } else {
                    return err_json(
                        "destination must be \"default\", {\"midi_port\": name} or {\"vst3\": path}",
                    );
                };
                let label = dest_label(&dest);
                let idx = sh.ensure_dest(&label, dest);
                sh.track_dest.insert(track, idx);
            }
            sh.gui_notify.fetch_add(1, Ordering::Relaxed);
            ok_json(dests_json(&sh))
        }
        "transport" => {
            let req = match args["action"].as_str() {
                Some("play") => Some(TransportReq::Play),
                Some("stop") => Some(TransportReq::Stop),
                Some("seek") => Some(TransportReq::Seek {
                    tick: args["tick"].as_u64().unwrap_or(0),
                }),
                _ => None,
            };
            match req {
                Some(r) => {
                    sh.transport_req.push(r);
                    sh.gui_notify.fetch_add(1, Ordering::Relaxed);
                    ok_json(serde_json::json!({"queued": true}))
                }
                None => err_json("action must be play|stop|seek"),
            }
        }
        "quantize" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let grid = args["grid"].as_u64().unwrap_or_else(|| sh.view().tempo_map.ppq() / 4);
            let strength = args["strength"].as_u64().unwrap_or(100) as u32;
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().quantize_ops(t, from, to, grid, strength));
            }
            apply_ops(&mut sh, "quantize", ops)
        }
        "transpose" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let st = args["semitones"].as_i64().unwrap_or(0) as i32;
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().transpose_ops(t, from, to, st));
            }
            apply_ops(&mut sh, "transpose", ops)
        }
        "scale_velocity" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let f = args["factor"].as_f64().unwrap_or(1.0);
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().scale_velocity_ops(t, from, to, f));
            }
            apply_ops(&mut sh, "scale velocity", ops)
        }
        "set_channel" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let ch = args["channel"].as_u64().unwrap_or(1).clamp(1, 16) as u8 - 1;
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().set_channel_ops(t, from, to, ch));
            }
            apply_ops(&mut sh, "set channel", ops)
        }
        "set_program" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let track = match req_track(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let tick = args["tick"].as_u64().unwrap_or(0);
            let ch = args["channel"]
                .as_u64()
                .map(|c| (c.clamp(1, 16) - 1) as u8)
                .unwrap_or_else(|| sh.view().tracks.get(track).map(|t| t.out_channel).unwrap_or(0));
            let ops = sh.view_mut().set_program_ops(
                track,
                tick,
                ch,
                args["program"].as_u64().unwrap_or(0) as u8,
                args["bank_msb"].as_u64().map(|v| v as u8),
                args["bank_lsb"].as_u64().map(|v| v as u8),
            );
            apply_ops(&mut sh, "set program", ops)
        }
        "set_cc" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let track = match req_track(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let ch = args["channel"]
                .as_u64()
                .map(|c| (c.clamp(1, 16) - 1) as u8)
                .unwrap_or_else(|| sh.view().tracks.get(track).map(|t| t.out_channel).unwrap_or(0));
            let mut ops = Vec::new();
            if let Some(points) = args["points"].as_array() {
                for p in points {
                    ops.extend(sh.view_mut().set_cc_ops(
                        track,
                        p["tick"].as_u64().unwrap_or(0),
                        ch,
                        p["cc"].as_u64().unwrap_or(7) as u8,
                        p["value"].as_u64().unwrap_or(0) as u8,
                    ));
                }
            } else {
                ops.extend(sh.view_mut().set_cc_ops(
                    track,
                    args["tick"].as_u64().unwrap_or(0),
                    ch,
                    args["cc"].as_u64().unwrap_or(7) as u8,
                    args["value"].as_u64().unwrap_or(0) as u8,
                ));
            }
            apply_ops(&mut sh, "set cc", ops)
        }
        "set_pitch_bend" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let track = match req_track(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let ch = args["channel"]
                .as_u64()
                .map(|c| (c.clamp(1, 16) - 1) as u8)
                .unwrap_or_else(|| sh.view().tracks.get(track).map(|t| t.out_channel).unwrap_or(0));
            let ops = sh.view_mut().set_pitch_bend_ops(
                track,
                args["tick"].as_u64().unwrap_or(0),
                ch,
                args["value"].as_u64().unwrap_or(8192) as u16,
            );
            apply_ops(&mut sh, "pitch bend", ops)
        }
        "set_tempo" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let ops = sh.view_mut().set_tempo_ops(
                args["tick"].as_u64().unwrap_or(0),
                args["bpm"].as_f64().unwrap_or(120.0),
            );
            apply_ops(&mut sh, "set tempo", ops)
        }
        "set_time_signature" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let ops = sh.view_mut().set_time_sig_ops(
                args["tick"].as_u64().unwrap_or(0),
                args["num"].as_u64().unwrap_or(4) as u8,
                args["den"].as_u64().unwrap_or(4) as u8,
            );
            apply_ops(&mut sh, "set time signature", ops)
        }
        "set_track_channel" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let track = match req_track(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let ops = sh.view_mut().set_track_channel_ops(
                track,
                (args["channel"].as_u64().unwrap_or(1).clamp(1, 16) - 1) as u8,
            );
            apply_ops(&mut sh, "set track channel", ops)
        }
        "set_track_name" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let track = match req_track(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let ops = sh
                .view_mut()
                .set_track_name_ops(track, args["name"].as_str().unwrap_or(""));
            apply_ops(&mut sh, "set track name", ops)
        }
        "add_track" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let ops = sh.view_mut().add_track_ops(args["name"].as_str());
            apply_ops(&mut sh, "add track", ops)
        }
        "remove_track" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let track = match req_track(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let ops = sh.view_mut().remove_track_ops(track);
            apply_ops(&mut sh, "remove track", ops)
        }
        "delete_range" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let track = match req_track(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let (from, to) = region(args);
            let ops = sh.view_mut().delete_range_ops(track, from, to);
            apply_ops(&mut sh, "delete range", ops)
        }
        "duplicate_range" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let track = match req_track(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let from = args["from"].as_u64().unwrap_or(0);
            // default `to` = end of song: a full u64::MAX span would push
            // every copy to a nonsense saturated tick
            let to = args["to"].as_u64().unwrap_or_else(|| doc_last_tick(sh.view()));
            let ops = sh.view_mut().duplicate_range_ops(track, from, to);
            apply_ops(&mut sh, "duplicate range", ops)
        }
        _ => err_json(format!("unknown tool '{name}'")),
    }
}

fn check_base(sh: &Shared, args: &serde_json::Value) -> Option<CallToolResponse> {
    match args["base_revision"].as_u64() {
        Some(b) if b != sh.view().revision() => Some(err_json(format!(
            "stale base_revision: current is {}; call document_summary",
            sh.view().revision()
        ))),
        _ => None,
    }
}

/// Required, in-range `track` argument for destructive tools — a missing or
/// invalid index must error, never silently fall back to track 0.
// CallToolResponse is rmcp's own (large) enum; error returns are exceptional
// and don't justify boxing
#[allow(clippy::result_large_err)]
fn req_track(sh: &Shared, args: &serde_json::Value) -> Result<usize, CallToolResponse> {
    match args["track"].as_u64() {
        None => Err(err_json("'track' is required")),
        Some(t) if (t as usize) < sh.view().tracks.len() => Ok(t as usize),
        Some(t) => Err(err_json(format!(
            "no track {t} (document has {})",
            sh.view().tracks.len()
        ))),
    }
}

fn doc_last_tick(d: &Document) -> u64 {
    d.tracks
        .iter()
        .flat_map(|t| t.events.iter().map(|e| e.tick))
        .max()
        .unwrap_or(0)
}

/// Write via temp file + rename so a crash mid-save can't truncate the
/// target (std::fs::rename replaces an existing destination on Windows).
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{stem}.sav{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn region(args: &serde_json::Value) -> (u64, u64) {
    (
        args["from"].as_u64().unwrap_or(0),
        args["to"].as_u64().unwrap_or(u64::MAX),
    )
}

/// track arg -> [track]; omitted -> all tracks; out-of-range -> error
#[allow(clippy::result_large_err)] // see req_track
fn sel_tracks(sh: &Shared, args: &serde_json::Value) -> Result<Vec<usize>, CallToolResponse> {
    match args["track"].as_u64() {
        None => Ok((0..sh.view().tracks.len()).collect()),
        Some(t) if (t as usize) < sh.view().tracks.len() => Ok(vec![t as usize]),
        Some(t) => Err(err_json(format!(
            "no track {t} (document has {})",
            sh.view().tracks.len()
        ))),
    }
}

fn apply_ops(sh: &mut Shared, label: &str, ops: Vec<Op>) -> CallToolResponse {
    if ops.is_empty() {
        return ok_json(serde_json::json!({"applied": false, "ops": 0}));
    }
    match sh.apply_or_stage(label, ops) {
        Ok(outcome) => apply_reply(outcome, serde_json::json!({})),
        Err(e) => err_json(e.to_string()),
    }
}

/// Uniform mutation reply: `{applied, revision, summary}` committed, or
/// `{staged, pending_ops, staged_revision, summary}` inside an open
/// transaction — `summary` is the op-derived change diff (issue #14).
fn apply_reply(outcome: StageOutcome, mut v: serde_json::Value) -> CallToolResponse {
    match outcome {
        StageOutcome::Committed { revision, summary } => {
            v["applied"] = true.into();
            v["revision"] = revision.into();
            v["summary"] = change_summary_json(&summary);
        }
        StageOutcome::Staged {
            pending_ops,
            staged_revision,
            summary,
        } => {
            v["staged"] = true.into();
            v["pending_ops"] = pending_ops.into();
            v["staged_revision"] = staged_revision.into();
            v["summary"] = change_summary_json(&summary);
        }
    }
    ok_json(v)
}

fn dest_label(d: &Destination) -> String {
    match d {
        Destination::MidiPort { port_name } => port_name.clone(),
        Destination::Plugin { plugin_path } => {
            format!("{} [VST3]", plugin_path)
        }
    }
}

fn dest_json(d: &Destination) -> serde_json::Value {
    match d {
        Destination::MidiPort { port_name } => {
            serde_json::json!({"kind": "midi_port", "port_name": port_name})
        }
        Destination::Plugin { plugin_path } => {
            serde_json::json!({"kind": "vst3", "plugin_path": plugin_path})
        }
    }
}

fn dests_json(sh: &Shared) -> serde_json::Value {
    serde_json::json!({
        "destinations": sh.dests.iter().enumerate().map(|(i, (label, d))| {
            let mut j = dest_json(d);
            j["index"] = i.into();
            j["label"] = label.clone().into();
            j
        }).collect::<Vec<_>>(),
        "default_dest": sh.default_dest,
        "track_dest": sh.track_dest.iter().map(|(t, d)| (*t, *d)).collect::<HashMap<usize, usize>>(),
        "muted": sh.muted.iter().copied().collect::<Vec<_>>(),
        "soloed": sh.soloed.iter().copied().collect::<Vec<_>>(),
        "metronome": sh.metronome,
        "loop_enabled": sh.loop_enabled,
    })
}

/// Serve over stdio (standalone `--file` mode or tests).
pub async fn serve_stdio(doc: SharedDoc) -> anyhow::Result<()> {
    let service = MidiService::new(doc)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

/// Serve Streamable-HTTP on `addr` (e.g. "127.0.0.1:7878") at path `/mcp`.
/// When `token` is Some, requests must carry `Authorization: Bearer <token>`.
/// Host/Origin validation stays at rmcp's loopback defaults.
pub async fn serve_http(
    doc: SharedDoc,
    addr: &str,
    token: Option<String>,
) -> anyhow::Result<()> {
    use axum::middleware::Next;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let factory = {
        let doc = doc.clone();
        move || -> Result<MidiService, std::io::Error> { Ok(MidiService::new(doc.clone())) }
    };
    let service = StreamableHttpService::new(
        factory,
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let mut app = axum::Router::new().route_service("/mcp", service);
    if let Some(tok) = token {
        app = app.layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: Next| {
                let tok = tok.clone();
                async move {
                    let ok = req
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .map(|v| v == format!("Bearer {tok}"))
                        .unwrap_or(false);
                    if ok {
                        next.run(req).await
                    } else {
                        axum::response::Response::builder()
                            .status(401)
                            .body(axum::body::Body::from("unauthorized"))
                            .unwrap()
                    }
                }
            },
        ));
    }
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("mcp http listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn shared() -> SharedDoc {
        let note = |tick: u64, status: u8, d0: u8, d1: u8| smf_core::Event {
            tick,
            seq: 0,
            raw_body: None,
            kind: EventKind::Channel {
                status,
                data: [d0, d1],
                len: 2,
            },
        };
        let f = smf_core::File {
            format: 1,
            division: smf_core::Division::Metrical(480),
            tracks: vec![
                smf_core::Track { events: vec![] },
                smf_core::Track {
                    events: vec![note(0, 0x90, 60, 100), note(480, 0x80, 60, 0)],
                },
            ],
            warnings: vec![],
        };
        Arc::new(Mutex::new(Shared::new(Document::from_file(f))))
    }

    /// dispatch a tool and decode its (is_error, first text block as JSON)
    fn call(shared: &SharedDoc, name: &str, args: serde_json::Value) -> (bool, serde_json::Value) {
        match dispatch(name, &args, shared.clone()) {
            CallToolResponse::Complete(r) => {
                let is_err = r.is_error.unwrap_or(false);
                let text = match r.content.first() {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    other => panic!("expected text content, got {other:?}"),
                };
                (is_err, serde_json::from_str(&text).unwrap_or(json!(null)))
            }
            other => panic!("unexpected response kind: {other:?}"),
        }
    }

    fn note_count(shared: &SharedDoc) -> usize {
        shared.lock().unwrap().doc.notes().len()
    }

    #[test]
    fn editor_info_reports_contract() {
        let sh = shared();
        let (err, v) = call(&sh, "editor_info", json!({}));
        assert!(!err);
        assert_eq!(v["name"], "midi-editor");
        assert!(v["version"].as_str().unwrap().contains('.'));
        assert!(v["commit"].as_str().is_some());
        assert_eq!(v["mcp_surface_version"], MCP_SURFACE_VERSION);
        assert_eq!(v["document"]["revision"], 0);
        assert_eq!(v["features"]["editing"]["base_revision"], true);
        // standalone (test) mode: no GUI-hosted features
        assert_eq!(v["features"]["transport"], false);
        // every listed tool is dispatchable and carries contract metadata
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(tools.len(), tool_specs().len());
        for t in tools {
            assert!(t["version"].as_u64().unwrap() >= 1);
            assert!(t["name"].as_str().is_some());
        }
        assert!(tools.iter().any(|t| t["name"] == "apply_patch"));
    }

    #[test]
    fn every_spec_is_listed_and_dispatchable() {
        // the registry is the single source of truth for the tool surface
        let names: Vec<_> = tool_specs().iter().map(|s| s.name).collect();
        assert_eq!(names.len(), {
            let mut n = names.clone();
            n.sort();
            n.dedup();
            n.len()
        });
    }

    #[test]
    fn document_summary_reports_tracks() {
        let sh = shared();
        let (err, v) = call(&sh, "document_summary", json!({}));
        assert!(!err);
        assert_eq!(v["tracks"].as_array().unwrap().len(), 2);
        assert_eq!(v["events"], 2);
    }

    #[test]
    fn remove_track_requires_valid_track() {
        let sh = shared();
        let (err, _) = call(&sh, "remove_track", json!({}));
        assert!(err, "missing track must error, not delete track 0");
        let (err, _) = call(&sh, "remove_track", json!({"track": 99}));
        assert!(err);
        assert_eq!(sh.lock().unwrap().doc.tracks.len(), 2, "nothing deleted");
    }

    #[test]
    fn apply_patch_failure_is_atomic() {
        let sh = shared();
        let rev0 = sh.lock().unwrap().doc.revision();
        // second op targets a nonexistent track — nothing may be applied
        let (err, _) = call(
            &sh,
            "apply_patch",
            json!({"ops": [
                {"op": "insert_note", "track": 1, "key": 64, "start": 480, "dur": 240},
                {"op": "insert_note", "track": 99, "key": 65, "start": 0, "dur": 240},
            ]}),
        );
        assert!(err);
        let shg = sh.lock().unwrap();
        assert_eq!(shg.doc.revision(), rev0, "no revision bump on failure");
        assert_eq!(shg.doc.tracks[1].events.len(), 2, "op 1 not half-applied");
        drop(shg);
        // a valid patch at the same base revision still works
        let (err, v) = call(
            &sh,
            "apply_patch",
            json!({
                "base_revision": rev0,
                "ops": [{"op": "insert_note", "track": 1, "key": 64, "start": 480, "dur": 240}]
            }),
        );
        assert!(!err);
        assert_eq!(v["applied"], true);
        assert_eq!(note_count(&sh), 2);
        // undo (shared with GUI) reverts it
        let (err, _) = call(&sh, "undo", json!({}));
        assert!(!err);
        assert_eq!(note_count(&sh), 1);
    }

    #[test]
    fn insert_note_rejects_unknown_track() {
        let sh = shared();
        let (err, _) = call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "insert_note", "track": 5}]}),
        );
        assert!(err, "unknown track must error before any op applies");
    }

    #[test]
    fn duplicate_range_defaults_to_song_end() {
        let sh = shared();
        // single note 0..480 (off at 480); omitting `to` duplicates to the
        // end of song instead of a u64::MAX span
        let (err, _) = call(&sh, "duplicate_range", json!({"track": 1, "from": 0}));
        assert!(!err);
        let ticks: Vec<(u64, u8)> = sh.lock().unwrap().doc.tracks[1]
            .events
            .iter()
            .map(|e| {
                let EventKind::Channel { status, .. } = &e.kind else {
                    unreachable!()
                };
                (e.tick, *status)
            })
            .collect();
        // original on/off at 0/480 plus the copy on/off at 480/960 (the
        // copy-on shares tick+seq with the original off — inserted first)
        assert_eq!(
            ticks,
            vec![(0, 0x90), (480, 0x90), (480, 0x80), (960, 0x80)]
        );
    }

    #[test]
    fn transaction_commit_is_one_undo_step() {
        let sh = shared();
        let rev0 = sh.lock().unwrap().doc.revision();
        let (err, v) = call(&sh, "begin_transaction", json!({"label": "fix chorus"}));
        assert!(!err);
        assert_eq!(v["base_revision"], rev0);
        // stage two separate edits
        let (err, v) = call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "insert_note", "track": 1, "key": 62, "start": 0, "dur": 240}]}),
        );
        assert!(!err);
        assert_eq!(v["staged"], true);
        // reads inside the batch see staged state; the real doc is untouched
        let (_err, v) = call(&sh, "list_notes", json!({}));
        assert_eq!(v["count"], 2);
        assert_eq!(
            sh.lock().unwrap().doc.notes().len(),
            1,
            "real document unchanged while staged"
        );
        let (err, _) = call(&sh, "set_track_name", json!({"track": 1, "name": "Chorus"}));
        assert!(!err);
        // commit merges both calls into ONE undo step
        let (err, v) = call(&sh, "commit_transaction", json!({}));
        assert!(!err);
        assert_eq!(v["committed"], true);
        assert_eq!(v["label"], "fix chorus");
        assert_eq!(v["ops"], 2);
        assert_eq!(sh.lock().unwrap().doc.notes().len(), 2);
        let (err, v) = call(&sh, "undo", json!({}));
        assert!(!err);
        assert_eq!(v["undone"], "fix chorus");
        assert_eq!(note_count(&sh), 1, "single undo reverted both edits");
        assert!(sh.lock().unwrap().doc.tracks[1].name.is_none());
    }

    #[test]
    fn rollback_leaves_document_unchanged() {
        let sh = shared();
        let before = sh
            .lock()
            .unwrap()
            .doc
            .serialize(smf_core::WriteOptions {
                running_status: false,
            });
        call(&sh, "begin_transaction", json!({"label": "experiment"}));
        call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "insert_note", "track": 1, "key": 65, "start": 0, "dur": 120}]}),
        );
        call(&sh, "set_tempo", json!({"tick": 0, "bpm": 90.0}));
        let (err, v) = call(&sh, "rollback_transaction", json!({}));
        assert!(!err);
        assert_eq!(v["discarded_ops"], 2);
        let shg = sh.lock().unwrap();
        let after = shg.doc.serialize(smf_core::WriteOptions {
            running_status: false,
        });
        assert_eq!(before, after, "byte-for-byte unchanged after rollback");
        assert_eq!(shg.doc.revision(), 0);
    }

    #[test]
    fn commit_reports_stale_conflict_on_concurrent_edit() {
        let sh = shared();
        call(&sh, "begin_transaction", json!({"label": "agent work"}));
        call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "insert_note", "track": 1, "key": 60, "start": 960, "dur": 120}]}),
        );
        // a GUI edit lands on the real document mid-batch
        let ops = sh.lock().unwrap().doc.add_track_ops(Some("gui track"));
        sh.lock().unwrap().apply("gui edit", ops).unwrap();
        let (err, v) = call(&sh, "commit_transaction", json!({}));
        assert!(err);
        assert_eq!(v["error"], "stale_base");
        // the conflicted batch stays open — caller decides (rollback here)
        let (err, v) = call(&sh, "transaction_status", json!({}));
        assert!(!err);
        assert_eq!(v["open"], true);
        call(&sh, "rollback_transaction", json!({}));
    }

    #[test]
    fn dry_run_commit_validates_and_keeps_batch() {
        let sh = shared();
        call(&sh, "begin_transaction", json!({}));
        call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "insert_note", "track": 1, "key": 60, "start": 0, "dur": 120}]}),
        );
        let (err, v) = call(&sh, "commit_transaction", json!({"dry_run": true}));
        assert!(!err);
        assert_eq!(v["valid"], true);
        assert_eq!(v["would_be_revision"], 1);
        assert_eq!(sh.lock().unwrap().doc.revision(), 0, "dry run applied nothing");
        let (_err, v) = call(&sh, "transaction_status", json!({}));
        assert_eq!(v["open"], true, "batch still open after dry_run");
        let (err, v) = call(&sh, "commit_transaction", json!({}));
        assert!(!err && v["committed"] == true);
    }

    #[test]
    fn abandoned_batch_expires() {
        let sh = shared();
        call(&sh, "begin_transaction", json!({"label": "forgotten"}));
        // push the checkpoint past its TTL
        sh.lock().unwrap().batch.as_mut().unwrap().last_activity =
            Instant::now() - Duration::from_secs(400);
        let (err, v) = call(&sh, "transaction_status", json!({}));
        assert!(!err);
        assert_eq!(v["open"], false, "idle batch auto-rolled-back");
        assert_eq!(sh.lock().unwrap().doc.revision(), 0);
    }

    #[test]
    fn mutation_replies_carry_change_summary() {
        let sh = shared();
        let (err, v) = call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "insert_note", "track": 1, "key": 64, "start": 480, "dur": 240}]}),
        );
        assert!(!err);
        let s = &v["summary"];
        assert_eq!(s["inserted"], 2, "on + off events");
        assert_eq!(s["notes"]["inserted"], 1);
        assert_eq!(s["tracks_touched"], json!([1]));
        assert_eq!(s["tick_range"], json!([480, 720]));
        // a move is reported as moved, not as delete+insert
        let (_err, v) = call(&sh, "list_notes", json!({}));
        let on_id = v["notes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["key"] == 64)
            .unwrap()["on_id"]
            .as_u64()
            .unwrap();
        let (err, v) = call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "move_note", "on_id": on_id, "dtick": 240, "dkey": 2}]}),
        );
        assert!(!err);
        assert_eq!(v["summary"]["notes"]["moved"], 1);
        assert_eq!(v["summary"]["notes"]["inserted"], 0);
        assert_eq!(v["summary"]["notes"]["removed"], 0);
    }

    #[test]
    fn history_and_changes_since_revision() {
        let sh = shared();
        call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "insert_note", "track": 1, "key": 64, "start": 480, "dur": 240}]}),
        );
        call(&sh, "set_tempo", json!({"tick": 0, "bpm": 90.0}));
        let (err, v) = call(&sh, "transaction_history", json!({}));
        assert!(!err);
        assert_eq!(v["count"], 2);
        // newest first; origin attribution
        assert_eq!(v["transactions"][0]["label"], "set tempo");
        assert_eq!(v["transactions"][0]["origin"], "mcp");
        // last agent-originated tx is what the GUI status bar surfaces
        assert_eq!(
            sh.lock().unwrap().last_mcp_tx.as_ref().unwrap().label,
            "set tempo"
        );

        let (err, v) = call(&sh, "changes_since_revision", json!({"revision": 1}));
        assert!(!err);
        assert_eq!(v["count"], 1);
        assert_eq!(v["aggregate"]["meta_changes"], 1);
        assert_eq!(v["truncated"], false);

        // undo is recorded too — revision moves are visible both ways
        call(&sh, "undo", json!({}));
        let (_, v) = call(&sh, "changes_since_revision", json!({"revision": 2}));
        assert_eq!(v["transactions"][0]["kind"], "undo");

        let (err, _) = call(&sh, "changes_since_revision", json!({"revision": 999}));
        assert!(err, "future revision is an error, not an empty diff");
    }

    #[test]
    fn gui_apply_is_not_attributed_to_mcp() {
        let sh = shared();
        let ops = sh.lock().unwrap().doc.add_track_ops(Some("gui"));
        sh.lock().unwrap().apply("gui edit", ops).unwrap();
        let (_, v) = call(&sh, "transaction_history", json!({}));
        assert_eq!(v["transactions"][0]["origin"], "gui");
        assert!(sh.lock().unwrap().last_mcp_tx.is_none());
    }

    #[test]
    fn history_is_bounded() {
        let sh = shared();
        for i in 0..(TX_HISTORY_CAP + 8) {
            let (err, _) = call(
                &sh,
                "set_tempo",
                json!({"tick": i as u64 * 1000, "bpm": 100.0 + i as f64}),
            );
            assert!(!err);
        }
        let (_, v) = call(&sh, "transaction_history", json!({"limit": 1000}));
        assert_eq!(v["count"], TX_HISTORY_CAP, "history capped");
        // coverage no longer reaches revision 0 — the flag tells the agent
        // to fall back to a full document query instead of trusting a gap
        let (_, v) = call(&sh, "changes_since_revision", json!({"revision": 0}));
        assert_eq!(v["truncated"], true);
    }

    /// Walk a paginated tool to exhaustion, returning every row emitted.
    fn paged(
        sh: &SharedDoc,
        tool: &str,
        args: serde_json::Value,
        rows: &str,
    ) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        let mut cursor = serde_json::Value::Null;
        for _ in 0..100 {
            let mut a = args.clone();
            a["cursor"] = cursor;
            let (err, v) = call(sh, tool, a);
            assert!(!err, "{tool} errored: {v}");
            out.extend(v[rows].as_array().unwrap().clone());
            match v["next_cursor"].as_str() {
                Some(c) => cursor = c.into(),
                None => return out,
            }
        }
        panic!("{tool}: pagination did not terminate");
    }

    #[test]
    fn list_notes_paginates_without_gaps() {
        let sh = shared(); // fixture already has one note (key 60 @0-480)
        let ops: Vec<_> = (1..7)
            .map(|i| {
                json!({"op": "insert_note", "track": 1, "key": 60 + i,
                       "start": i * 480, "dur": 240})
            })
            .collect();
        call(&sh, "apply_patch", json!({"ops": ops}));
        let rows = paged(&sh, "list_notes", json!({"limit": 3}), "notes");
        assert_eq!(rows.len(), 7);
        let ids: std::collections::HashSet<_> =
            rows.iter().map(|n| n["on_id"].as_u64().unwrap()).collect();
        assert_eq!(ids.len(), 7, "no duplicates across pages");
        let starts: Vec<_> = rows.iter().map(|n| n["start"].as_u64().unwrap()).collect();
        let mut sorted = starts.clone();
        sorted.sort();
        assert_eq!(starts, sorted, "pages stay in (start, key, track, id) order");
    }

    #[test]
    fn query_events_pages_and_field_projection() {
        let sh = shared();
        call(
            &sh,
            "apply_patch",
            json!({"ops": (0..4).map(|i| json!({"op": "insert_note", "track": 1,
                "key": 64, "start": i * 960, "dur": 120})).collect::<Vec<_>>()}),
        );
        // 1 fixture note + 4 inserted = 10 channel events
        let rows = paged(
            &sh,
            "query_events",
            json!({"limit": 4, "fields": ["id", "tick"]}),
            "events",
        );
        assert_eq!(rows.len(), 10);
        let ids: std::collections::HashSet<_> =
            rows.iter().map(|e| e["id"].as_u64().unwrap()).collect();
        assert_eq!(ids.len(), 10);
        for e in &rows {
            let obj = e.as_object().unwrap();
            assert_eq!(obj.len(), 2, "fields projection dropped everything else");
            assert!(obj.contains_key("id") && obj.contains_key("tick"));
        }
    }

    #[test]
    fn stale_cursor_is_reported() {
        let sh = shared();
        call(
            &sh,
            "apply_patch",
            json!({"ops": (0..4).map(|i| json!({"op": "insert_note", "track": 1,
                "key": 64, "start": i * 960, "dur": 120})).collect::<Vec<_>>()}),
        );
        let (_, v) = call(&sh, "query_events", json!({"limit": 2}));
        let cursor = v["next_cursor"].as_str().unwrap().to_string();
        // any mutation bumps the revision the cursor was minted under
        call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "insert_note", "track": 1, "key": 70}]}),
        );
        let (err, v) = call(&sh, "query_events", json!({"cursor": cursor}));
        assert!(err);
        let s = v.to_string();
        assert!(s.contains("stale_cursor") && s.contains("current_revision") && s.contains("hint"));
        let (err, _) = call(&sh, "list_notes", json!({"cursor": "not-a-cursor"}));
        assert!(err, "malformed cursors are rejected, not ignored");
    }

    #[test]
    fn meta_and_cc_reads_paginate() {
        let sh = shared();
        let evs: Vec<_> = (0..5)
            .map(|i| {
                json!({"tick": i * 240, "kind": {"meta": {"type": 3, "data_utf8": format!("m{i}")}}})
            })
            .chain((0..4).map(|i| {
                json!({"tick": i * 120, "kind": {"channel": {"status": 176, "data": [20 + i, i]}}})
            }))
            .collect();
        call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "insert_events", "track": 0, "events": evs}]}),
        );
        let metas = paged(&sh, "get_meta", json!({"meta_type": 3, "limit": 2}), "meta");
        assert_eq!(metas.len(), 5);
        let ids: std::collections::HashSet<_> =
            metas.iter().map(|m| m["id"].as_u64().unwrap()).collect();
        assert_eq!(ids.len(), 5);
        let ccs = paged(&sh, "get_cc", json!({"limit": 2}), "cc");
        assert_eq!(ccs.len(), 4, "one row per (track,channel,cc)");
        // projection drops the per-row heavy field
        let metas = paged(
            &sh,
            "get_meta",
            json!({"meta_type": 3, "limit": 100, "fields": ["id", "tick"]}),
            "meta",
        );
        for m in &metas {
            assert!(m.get("data_hex").is_none() && m.get("text").is_none());
        }
    }

    #[test]
    fn undo_is_blocked_while_batch_open() {
        let sh = shared();
        call(&sh, "begin_transaction", json!({}));
        let (err, _) = call(&sh, "undo", json!({}));
        assert!(err);
    }

    #[test]
    fn query_events_pages_without_materializing_json() {
        let sh = shared();
        let (err, v) = call(&sh, "query_events", json!({"limit": 1, "offset": 1}));
        assert!(!err);
        assert_eq!(v["total"], 2);
        assert_eq!(v["events"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn hex_payloads_are_capped() {
        assert!(hex_to_bytes("ab").is_some());
        assert!(hex_to_bytes(&"ab".repeat(MAX_HEX_BYTES + 1)).is_none());
        assert!(hex_to_bytes("abc").is_none(), "odd length rejected");
        // oversized hex in a request is an error, not a silent empty payload
        let sh = shared();
        let big = "ab".repeat(MAX_HEX_BYTES + 1);
        let (err, _) = call(
            &sh,
            "apply_patch",
            json!({"ops": [{"op": "insert_events", "track": 0, "events": [
                {"tick": 0, "kind": {"meta": {"type": 1, "data_hex": big}}}
            ]}]}),
        );
        assert!(err);
    }

    #[test]
    fn write_atomic_replaces_existing_file() {
        let dir = std::env::temp_dir().join("midi-editor-mcp-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("savetest.mid");
        std::fs::write(&p, b"old").unwrap();
        write_atomic(&p, b"new contents").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"new contents");
        // no temp litter left beside the target
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".savetest"))
            .collect();
        assert!(leftovers.is_empty());
        let _ = std::fs::remove_file(&p);
    }
}

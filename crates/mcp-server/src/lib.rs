//! Embedded MCP server.
//!
//! Topology:
//!   - the GUI app hosts Streamable-HTTP on 127.0.0.1, sharing one `SharedDoc`
//!     with the editor. HTTP requests are Bearer-authenticated by default:
//!     `MIDI_MCP_TOKEN` wins, otherwise a random token is provisioned into
//!     `%LOCALAPPDATA%\midi-editor\mcp-token` on first launch and re-read per
//!     request (rotation/revocation need no restart). Unauthenticated mode
//!     requires the explicit `MIDI_MCP_ALLOW_INSECURE=1` opt-out.
//!   - `mcp-bridge` connects to that endpoint as an rmcp client and re-serves
//!     it over stdio so stdio-only clients (Claude Desktop etc.) can reach it
//!     (it discovers the provisioned token file automatically). With `--file`
//!     it can also serve a standalone document without the app.
//!
//! Every mutation goes through `Document::apply(Transaction)` on the shared
//! doc — the exact same path GUI edits take — so undo is unified.
//!
//! ## Threat model: loopback HTTP transport
//!
//! The embedded endpoint binds to loopback only, so remote machines cannot
//! reach it. The residual risks are *local*: a hostile web page in the user's
//! browser (CSRF / DNS rebinding), and other processes running as the same
//! user.
//!
//! - **DNS rebinding / CSRF**: a browser page can issue cross-origin requests
//!   to `127.0.0.1`. Defence: requests carrying an `Origin` header must name a
//!   loopback origin (`http(s)://localhost|127.0.0.1|[::1]:<any port>`), and a
//!   present `Host` header must be a loopback host — enforced by
//!   [`loopback_guard`] *and* rmcp's own allowlists (defence in depth).
//!   Non-browser MCP clients send no `Origin` and are unaffected.
//! - **Unauthenticated local access**: when `MIDI_MCP_TOKEN` is unset, any
//!   local process can call mutating tools. This is accepted for convenience
//!   in the desktop app; setting `MIDI_MCP_TOKEN` enables Bearer auth. The
//!   effective mode is reported in the `diagnostics` tool output and on
//!   stderr at startup.
//! - **Web content never gains filesystem access** beyond what the MCP tools
//!   themselves expose; the guard only rejects browser-origin *requests*, it
//!   is not a substitute for authentication.

use bytes::Bytes;
use commands::UndoStack;
use document::{ApplyError, Document, Event, EventId, Op, Transaction};
use midi_io::Destination;
use rmcp::model::*;
use rmcp::service::{RequestContext, ServiceExt};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use smf_core::EventKind;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod service;
pub use persist::write_atomic;

/// A transport action the MCP side requests and the GUI poller drains —
/// playback itself lives in the app process (owns sinks/audio), MCP just asks.
#[derive(Debug, Clone, PartialEq)]
pub enum TransportReq {
    Play,
    Stop,
    Seek { tick: u64 },
}

/// MCP authentication posture of the transport serving `Shared`. Written by
/// `serve_http` at startup; `Stdio` is the default for stdio/frontends. The
/// status bar renders it so an unauthenticated endpoint is never invisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthMode {
    /// process-local stdio — inherits the spawning client's trust
    Stdio,
    /// `Authorization: Bearer` required
    Bearer,
    /// serving HTTP without any credential check (explicit opt-out)
    Open,
}

impl McpAuthMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Bearer => "bearer",
            Self::Open => "open",
        }
    }
}

/// The effective MCP security posture, reported by the `diagnostics` tool and
/// renderable by the GUI. Lives in `Shared` (not on the service) so the stdio
/// frontend, the in-app HTTP server, and the UI all see the same facts.
/// Never carries credential material — only the mode names and the
/// credential's provenance.
#[derive(Debug, Clone)]
pub struct SecurityReport {
    /// e.g. "streamable-http 127.0.0.1:7878" or "stdio"
    pub transport: String,
    /// typed auth mode — the status bar keys on this
    pub auth_mode: McpAuthMode,
    /// e.g. "auto-provisioned token file (…\mcp-token)" — tooltips/logs
    pub auth_detail: String,
    /// which Host header values are accepted
    pub host_policy: String,
    /// which Origin header values are accepted
    pub origin_policy: String,
}

impl SecurityReport {
    /// stdio transports inherit the trust of the spawning process — no HTTP
    /// attack surface exists, so Host/Origin checks do not apply.
    pub fn stdio() -> Self {
        Self {
            transport: "stdio".into(),
            auth_mode: McpAuthMode::Stdio,
            auth_detail: "process-local (no HTTP)".into(),
            host_policy: "n/a".into(),
            origin_policy: "n/a".into(),
        }
    }

    pub fn http(bind: &str, auth_mode: McpAuthMode, auth_detail: String) -> Self {
        Self {
            transport: format!("streamable-http {bind}"),
            auth_mode,
            auth_detail,
            host_policy: "loopback only (localhost/127.0.0.1/[::1])".into(),
            origin_policy: "absent or loopback only".into(),
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "transport": self.transport,
            "auth": self.auth_mode.as_str(),
            "auth_detail": self.auth_detail,
            "host_policy": self.host_policy,
            "origin_policy": self.origin_policy,
        })
    }
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
    pub loop_enabled: bool,
    /// opt-in: also chase the last complete SysEx message on play/loop wrap
    /// (a chased GM/GS/XG reset can wipe the channel-state chase)
    pub chase_sysex: bool,
    /// the GUI drains `transport_req` and repaints on `gui_notify`; false in
    /// standalone `mcp-bridge --file` mode (feature-detected via editor_info)
    pub gui_attached: bool,
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
    /// show "MCP: <label>" in the status bar
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
            loop_enabled: false,
            chase_sysex: false,
            gui_attached: false,
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
/// Cap on the ops array of one apply_patch call — clients paginate large
/// edits instead of one request forcing an unbounded build pass.
const MAX_PATCH_OPS: usize = 1_000;
/// Cap on diagnostics rows returned in one call; the full count is still
/// reported so callers know there is more.
const MAX_DIAG_RESULTS: usize = 500;
/// One HTTP request body may not exceed this — an over-large JSON-RPC post
/// must be rejected before it allocates.
const MAX_HTTP_BODY_BYTES: usize = 4 << 20; // 4 MiB
/// In-flight MCP requests are bounded; excess gets an immediate 429 rather
/// than queueing unboundedly behind the document lock.
const MAX_CONCURRENT_REQUESTS: usize = 16;
/// Time budget for producing a response. The SSE stream's Response object is
/// produced up front, so this bounds time-to-response, not stream lifetime.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

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

/// Which transport the document is being served over — the file-write
/// policy differs because stdio inherits the spawning client's trust while
/// HTTP may be reached by any local process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsScope {
    /// Embedded HTTP server: explicit `path` args must land under the
    /// document's directory or `MIDI_MCP_ALLOWED_ROOTS`.
    Http,
    /// Standalone stdio (`mcp-bridge --file`): additionally allows the
    /// working directory, plus `MIDI_MCP_STDIO_ALLOWED_ROOTS`.
    Stdio,
}

/// `;`-separated directory list from an env var, canonicalized.
/// Unresolvable entries are dropped rather than trusted.
fn roots_from_env(var: &str) -> Vec<PathBuf> {
    std::env::var(var)
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::fs::canonicalize(s).ok())
        .collect()
}

/// Canonicalize a write target: an existing file resolves fully (symlinks,
/// junctions, `..` — everything); a new file resolves through its parent so
/// a link inside the parent can't smuggle the write elsewhere.
fn canonical_for_write(path: &std::path::Path) -> Result<PathBuf, String> {
    if let Ok(c) = std::fs::canonicalize(path) {
        return Ok(c);
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{} has no directory component", path.display()))?;
    let canon_dir = std::fs::canonicalize(parent)
        .map_err(|e| format!("cannot resolve {}: {e}", parent.display()))?;
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    Ok(canon_dir.join(name))
}

/// Resolve `path` against the save policy: canonicalize first, *then* check
/// containment — the order matters, authorization on a non-canonical path is
/// what traversal attacks exploit. Returns the canonical path to write.
fn authorize_write(
    doc_path: &Option<PathBuf>,
    scope: FsScope,
    extra_roots: &[PathBuf],
    path: &std::path::Path,
) -> Result<PathBuf, String> {
    let canon = canonical_for_write(path)?;
    let mut roots: Vec<PathBuf> = Vec::new();
    // the current document's own directory is always writable
    if let Some(dp) = doc_path {
        if let Some(dir) = dp.parent() {
            if let Ok(d) = std::fs::canonicalize(dir) {
                roots.push(d);
            }
        }
    }
    if scope == FsScope::Stdio {
        if let Ok(cwd) = std::env::current_dir().and_then(|d| std::fs::canonicalize(d)) {
            roots.push(cwd);
        }
    }
    roots.extend(extra_roots.iter().cloned());
    if roots.iter().any(|r| canon.starts_with(r)) {
        Ok(canon)
    } else {
        let env_var = match scope {
            FsScope::Http => "MIDI_MCP_ALLOWED_ROOTS",
            FsScope::Stdio => "MIDI_MCP_STDIO_ALLOWED_ROOTS",
        };
        Err(format!(
            "{} is outside the MCP save scope; allowed roots: {}; \
             add a directory with {env_var}",
            canon.display(),
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join("; "),
        ))
    }
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

/// `document_summary` over document `d` — `sh.view()` inside a batch (staged
/// state, read-your-writes) or `sh.doc` otherwise; `sh` supplies path,
/// saved-revision, and the security report.
fn summary_json(d: &Document, sh: &Shared) -> serde_json::Value {
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
        // format 2: tracks are independent sequences — a single merged
        // duration would lie, so report each sequence's own span
        "sequential": d.is_sequential(),
        "last_tick": last_tick,
        "duration_us": (!d.is_sequential()).then(|| d.tempo_map.tick_to_us(last_tick)),
        "durations_us": d.is_sequential().then(|| {
            (0..d.tracks.len()).map(|i| {
                let m = d.tempo_map_for(i);
                m.tick_to_us(d.track_end_tick(i))
            }).collect::<Vec<_>>()
        }),
        "revision": d.revision(),
        "path": sh.path,
        "dirty": d.revision() != sh.saved_revision,
        // auth posture only — never the credential itself
        "mcp_auth": {
            "mode": sh.mcp_security.auth_mode.as_str(),
            "detail": sh.mcp_security.auth_detail
        },
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
                            .map(|a| {
                                a.iter()
                                    .filter_map(|v| v.as_u64().map(|x| x as u8))
                                    .collect()
                            })
                            .unwrap_or_default();
                        EventKind::Channel {
                            status,
                            data: [
                                data.first().copied().unwrap_or(0),
                                data.get(1).copied().unwrap_or(0),
                            ],
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
                        EventKind::SysEx(Bytes::from(hex_to_bytes(h).ok_or_else(|| {
                            PatchError::Msg("invalid or oversized sysex_hex".into())
                        })?))
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
                    after.tick = (base_tick as i64 + dtick + if eid != on_id { dlen } else { 0 })
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
                            && matches!(
                                e.kind,
                                EventKind::Meta {
                                    meta_type: 0x51,
                                    ..
                                }
                            )
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
            .with_server_info(Implementation::new(
                "midi-editor",
                env!("CARGO_PKG_VERSION"),
            ))
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
            "Serialize the document to SMF and write it. Args: path? (defaults to the document's open path; \
             an explicit path must be under the document's directory — or MIDI_MCP_ALLOWED_ROOTS / \
             MIDI_MCP_STDIO_ALLOWED_ROOTS)",
            object_schema(serde_json::json!({"path": {"type": "string"}})),
        ),
        spec(
            "get_tempo_map",
            "Tempo breakpoints: [{tick, us_per_quarter, bpm, cumulative_us}] + ppq (null for SMPTE — fps/ticks_per_frame are reported instead). Format-2 files report per-sequence maps (each track is an independent timeline). Read before editing tempo or converting ticks<->time.",
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
            "Snap note onsets to a grid (duration preserved). Args: track? (all when omitted), from?, to?, grid? (ticks, default ppq/4 metrical / one frame SMPTE), strength? (0-100, default 100). Optional base_revision.",
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
            "Set/replace tempo at a tick. Args: tick, bpm, track? (default 0 = conductor; for format-2 files pass the sequence's track). Optional base_revision.",
            object_schema(serde_json::json!({
                "tick": {"type": "integer"}, "bpm": {"type": "number"},
                "track": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "set_time_signature",
            "Set/replace time signature at a tick. Args: tick, num (beats/bar), den (beat value 4=quarter,8=eighth), track? (default 0; pass the sequence's track for format-2). Optional base_revision.",
            object_schema(serde_json::json!({
                "tick": {"type": "integer"}, "num": {"type": "integer"}, "den": {"type": "integer"},
                "track": {"type": "integer"},
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

fn dispatch(name: &str, args: &serde_json::Value, shared: SharedDoc) -> CallToolResponse {
    // recover from a poisoned lock: a panic in an earlier critical section
    // must not take down every later request
    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
    sh.expire_batch();
    match name {
        "editor_info" => ok_json(editor_info_json(&sh)),
        "document_summary" => {
            let mut v = summary_json(sh.view(), &sh);
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
                "count": diags.len().min(MAX_DIAG_RESULTS),
                "total": diags.len(),
                "truncated": diags.len() > MAX_DIAG_RESULTS,
                "security": sh.mcp_security.to_json(),
                "diagnostics": diags.iter().take(MAX_DIAG_RESULTS).map(|d| serde_json::json!({
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
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
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
            let limit = args["limit"]
                .as_u64()
                .unwrap_or(500)
                .min(MAX_QUERY_LIMIT as u64) as usize;
            let fields = field_projection(&args);
            let after = match cursor_arg(&sh, &args, 4) {
                Ok(a) => a,
                Err(r) => return r,
            };
            // total sort key (start, key, track, on_id) — deterministic on a
            // fixed revision, so a cursor page neither duplicates nor skips
            let key = |n: &document::Note| (n.start_tick, n.key as u64, n.track as u64, n.on_id);
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
            let limit = args["limit"]
                .as_u64()
                .unwrap_or(500)
                .min(MAX_QUERY_LIMIT as u64) as usize;
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
            if ops_json.len() > MAX_PATCH_OPS {
                return err_json(format!(
                    "ops array too large ({} > {MAX_PATCH_OPS}); \
                     split into multiple apply_patch calls",
                    ops_json.len()
                ));
            }
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
            let txs: Vec<&TxRecord> = sh.history.iter().filter(|r| r.revision > from).collect();
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
            let explicit = args["path"].as_str().map(PathBuf::from);
            let expect_revision = args["base_revision"].as_u64();
            // an explicit path is a file-write primitive — scope it; the
            // document's own path is always allowed (backwards compatible)
            let path: Option<PathBuf> = match explicit {
                Some(p) => {
                    let extra = match sh.fs_scope {
                        FsScope::Http => roots_from_env("MIDI_MCP_ALLOWED_ROOTS"),
                        FsScope::Stdio => roots_from_env("MIDI_MCP_STDIO_ALLOWED_ROOTS"),
                    };
                    match authorize_write(&sh.path, sh.fs_scope, &extra, &p) {
                        Ok(c) => Some(c),
                        Err(e) => return err_json(e),
                    }
                }
                None => sh.path.clone(),
            };
            drop(sh); // never hold the editor lock across disk I/O
            match service::save_document(
                &shared,
                service::SaveRequest {
                    path: path.as_deref(),
                    expect_revision,
                },
            ) {
                Ok(out) => ok_json(serde_json::json!({
                    "saved": out.path.to_string_lossy(),
                    "revision": out.revision,
                    "committed": out.committed,
                    "leftover_temps": out.leftovers.iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect::<Vec<_>>(),
                })),
                Err(e) => err_json(e.to_string()),
            }
        }
        "get_tempo_map" => {
            let tm = &sh.view().tempo_map;
            let (fps, tpf) = match sh.view().division {
                smf_core::Division::Smpte {
                    fps,
                    ticks_per_frame,
                } => (Some(fps), Some(ticks_per_frame)),
                smf_core::Division::Metrical(_) => (None, None),
            };
            let point = |(tick, mpq, cum): &(u64, u32, u64)| serde_json::json!({
                "tick": tick, "us_per_quarter": mpq,
                "bpm": (60_000_000.0 / *mpq as f64 * 100.0).round() / 100.0,
                "cumulative_us": cum,
            });
            ok_json(if sh.view().is_sequential() {
                // format 2: every sequence is timed by its own tempo map
                serde_json::json!({
                    "scope": "per-sequence",
                    "ppq": tm.ppq(),
                    "fps": fps,
                    "ticks_per_frame": tpf,
                    "sequences": (0..sh.view().tracks.len()).map(|i| serde_json::json!({
                        "track": i,
                        "points": sh.view().tempo_map_for(i).points().iter().map(point).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                })
            } else {
                serde_json::json!({
                    // None for SMPTE: there is no quarter note — use
                    // fps*ticks_per_frame for tick<->time math instead
                    "ppq": tm.ppq(),
                    "fps": fps,
                    "ticks_per_frame": tpf,
                    "ticks_per_second": fps.map(|f| f as u64 * tpf.unwrap_or(1) as u64),
                    "points": tm.points().iter().map(point).collect::<Vec<_>>(),
                })
            })
        }
        "get_meta" => {
            let track = args["track"].as_u64().map(|v| v as usize);
            let mt = args["meta_type"].as_u64().map(|v| v as u8);
            let limit = args["limit"]
                .as_u64()
                .unwrap_or(500)
                .min(MAX_QUERY_LIMIT as u64) as usize;
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
            let limit = args["limit"]
                .as_u64()
                .unwrap_or(500)
                .min(MAX_QUERY_LIMIT as u64) as usize;
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
                            if chan.is_some() && chan != Some(ch)
                                || ccn.is_some() && ccn != Some(cc)
                            {
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
            // default grid: a 16th note for metrical, one frame for SMPTE
            // — never a pretend PPQ
            let grid = args["grid"]
                .as_u64()
                .unwrap_or_else(|| sh.view().time_display().min_grid_ticks());
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
                .unwrap_or_else(|| {
                    sh.view()
                        .tracks
                        .get(track)
                        .map(|t| t.out_channel)
                        .unwrap_or(0)
                });
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
                .unwrap_or_else(|| {
                    sh.view()
                        .tracks
                        .get(track)
                        .map(|t| t.out_channel)
                        .unwrap_or(0)
                });
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
                .unwrap_or_else(|| {
                    sh.view()
                        .tracks
                        .get(track)
                        .map(|t| t.out_channel)
                        .unwrap_or(0)
                });
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
                args["track"].as_u64().unwrap_or(0) as usize,
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
                args["track"].as_u64().unwrap_or(0) as usize,
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
            let to = args["to"]
                .as_u64()
                .unwrap_or_else(|| doc_last_tick(sh.view()));
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
    // stdio clients inherit the spawning process's trust — the scope loosens
    // to also cover the working directory and its own env roots
    doc.lock().unwrap_or_else(|e| e.into_inner()).fs_scope = FsScope::Stdio;
    let service = MidiService::new(doc)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

/// Loopback hostnames accepted in `Host`/`Origin` authority checks.
const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// `Origin` values the endpoint accepts: any port on a loopback host, either
/// HTTP scheme. rmcp matches `(scheme, host, port)` tuples where an absent
/// allowlist port is a wildcard, so these six entries cover local browser
/// tooling (e.g. MCP Inspector) while rejecting every remote origin.
const LOOPBACK_ORIGINS: &[&str] = &[
    "http://localhost",
    "https://localhost",
    "http://127.0.0.1",
    "https://127.0.0.1",
    "http://[::1]",
    "https://[::1]",
];

fn is_loopback_host(host: &str) -> bool {
    LOOPBACK_HOSTS.contains(&host.to_ascii_lowercase().as_str())
}

/// Host part of a `host[:port]` / `[v6][:port]` authority, lowercased.
/// Anything surprising — userinfo, whitespace, unbalanced brackets, a stray
/// colon, a non-numeric port — is malformed, not loopback.
fn authority_host(authority: &str) -> Option<String> {
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace)
    {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (v6, tail) = rest.split_once(']')?;
        match tail.strip_prefix(':') {
            None if tail.is_empty() => {}
            Some(port) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {}
            _ => return None,
        }
        return Some(v6.to_ascii_lowercase());
    }
    match authority.split(':').collect::<Vec<_>>().as_slice() {
        [host] if !host.is_empty() => Some(host.to_ascii_lowercase()),
        [host, port]
            if !host.is_empty() && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) =>
        {
            Some(host.to_ascii_lowercase())
        }
        _ => None,
    }
}

/// An `Origin` header value is acceptable iff it is `http(s)://<loopback>`
/// with any port. `Origin: null`, remote hosts, userinfo tricks, and
/// malformed values all fail — non-browser clients simply omit the header.
fn origin_is_loopback(origin: &str) -> bool {
    let origin = origin.trim();
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    let Some(rest) = rest else { return false };
    let authority = rest.split('/').next().unwrap_or("");
    authority_host(authority).is_some_and(|h| is_loopback_host(&h))
}

fn forbidden(reason: &'static str) -> axum::response::Response {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::FORBIDDEN)
        .body(axum::body::Body::from(reason))
        .expect("static response")
}

/// Explicit browser-origin / DNS-rebinding guard for the loopback endpoint —
/// the primary check described by the MCP transport guidance for local HTTP
/// servers. A present `Host` must name a loopback host; a present `Origin`
/// must name a loopback origin. Non-browser MCP clients send neither and are
/// unaffected; browser `fetch`/`XHR` from a rebinded or remote page always
/// carries a hostile `Origin` and is rejected before tool dispatch.
async fn loopback_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(host) = req.headers().get(axum::http::header::HOST) {
        let ok = host
            .to_str()
            .ok()
            .and_then(authority_host)
            .is_some_and(|h| is_loopback_host(&h));
        if !ok {
            tracing::warn!(host = ?host, "mcp http: rejected non-loopback Host");
            return forbidden("forbidden host");
        }
    }
    if let Some(origin) = req.headers().get(axum::http::header::ORIGIN) {
        if !origin.to_str().ok().is_some_and(origin_is_loopback) {
            tracing::warn!(origin = ?origin, "mcp http: rejected non-loopback Origin");
            return forbidden("forbidden origin");
        }
    }
    next.run(req).await
}

// ---------- HTTP authentication ----------

/// Where the bearer token comes from. `File` is re-read on every request so
/// rotating or revoking the credential (rewrite/delete the file) takes effect
/// without restarting the app.
pub enum TokenSource {
    /// `MIDI_MCP_TOKEN` / `--token`: fixed for the server's lifetime.
    Fixed(String),
    /// Auto-provisioned per-user token file.
    File(PathBuf),
}

impl TokenSource {
    fn token(&self) -> Option<String> {
        match self {
            Self::Fixed(t) => Some(t.clone()),
            Self::File(p) => read_token_file(p),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Fixed(_) => "MIDI_MCP_TOKEN".into(),
            Self::File(p) => format!("auto-provisioned token file ({})", p.display()),
        }
    }
}

/// Authentication posture for the HTTP endpoint.
pub enum HttpAuth {
    /// `Authorization: Bearer <token>` required.
    Token(TokenSource),
    /// Explicit opt-out (`MIDI_MCP_ALLOW_INSECURE`) — unauthenticated loopback.
    Insecure,
}

/// Per-user token file: `%LOCALAPPDATA%\midi-editor\mcp-token` normally —
/// a directory only the owning user can read, so other local accounts cannot
/// steal the credential.
pub fn token_file_path() -> PathBuf {
    for var in ["LOCALAPPDATA", "APPDATA"] {
        if let Ok(d) = std::env::var(var) {
            if !d.is_empty() {
                return PathBuf::from(d).join("midi-editor").join("mcp-token");
            }
        }
    }
    std::env::temp_dir().join("midi-editor-mcp-token")
}

/// Generated tokens are 64 lowercase hex; a file/env-provided token just has
/// to be a single line of printable ASCII.
fn token_is_valid(t: &str) -> bool {
    !t.is_empty() && t.len() <= 256 && t.bytes().all(|b| b.is_ascii_graphic())
}

fn read_token_file(path: &std::path::Path) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?.trim().to_string();
    token_is_valid(&t).then_some(t)
}

/// Provision the token file on first launch; reuse it afterwards. Never
/// overwrites a healthy file, and regenerates a corrupt/empty one.
fn ensure_token_file(path: &std::path::Path) -> std::io::Result<()> {
    if read_token_file(path).is_some() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(std::io::Error::other)?;
    write_atomic(path, bytes_hex(&raw).as_bytes()).map_err(std::io::Error::other)
}

/// The stored auto-provisioned token, for `mcp-bridge` to pass through when
/// no `--token`/`MIDI_MCP_TOKEN` was given — keeps stdio clients ergonomic.
pub fn read_stored_token() -> Option<String> {
    read_token_file(&token_file_path())
}

/// Resolve the effective HTTP auth posture, most explicit wins:
///   1. `MIDI_MCP_TOKEN` (non-empty)     → fixed bearer token
///   2. `MIDI_MCP_ALLOW_INSECURE` truthy → explicit unauthenticated opt-out
///   3. otherwise                        → auto-provisioned token file
/// Errors instead of silently serving unauthenticated when provisioning
/// fails — failing open would hand mutating tools to any local process.
pub fn resolve_http_auth() -> anyhow::Result<HttpAuth> {
    if let Ok(t) = std::env::var("MIDI_MCP_TOKEN") {
        if token_is_valid(&t) {
            return Ok(HttpAuth::Token(TokenSource::Fixed(t)));
        }
    }
    let insecure = std::env::var("MIDI_MCP_ALLOW_INSECURE")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if insecure {
        return Ok(HttpAuth::Insecure);
    }
    let path = token_file_path();
    ensure_token_file(&path).map_err(|e| {
        anyhow::anyhow!("cannot provision MCP auth token at {}: {e}", path.display())
    })?;
    Ok(HttpAuth::Token(TokenSource::File(path)))
}

/// Constant-time string equality — the token isn't length-secret, so early
/// exit on length is fine; the byte loop itself must not short-circuit.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Throttle authentication failures per client IP. A local port scanner or
/// hostile page gets a bounded number of guesses, then 429s — and every
/// failure is logged by IP only, never with the presented credential.
struct AuthLimiter {
    fails: Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>>,
}

const AUTH_FAIL_MAX: u32 = 10;
const AUTH_FAIL_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

impl AuthLimiter {
    fn new() -> Self {
        Self {
            fails: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// May this IP attempt another auth check right now?
    fn allow_attempt(&self, ip: std::net::IpAddr) -> bool {
        let mut m = self.fails.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((n, first)) = m.get(&ip) {
            if first.elapsed() < AUTH_FAIL_WINDOW && *n >= AUTH_FAIL_MAX {
                return false;
            }
        }
        // keep the map bounded — loopback space is tiny but don't grow forever
        if m.len() > 1024 {
            m.clear();
        }
        true
    }

    fn record(&self, ip: std::net::IpAddr, ok: bool) {
        let mut m = self.fails.lock().unwrap_or_else(|e| e.into_inner());
        match m.get_mut(&ip) {
            Some((n, first)) if first.elapsed() < AUTH_FAIL_WINDOW => {
                if ok {
                    *n = 0;
                } else {
                    *n += 1;
                }
            }
            _ => {
                m.insert(ip, (if ok { 0 } else { 1 }, std::time::Instant::now()));
            }
        }
    }
}

fn http_error(status: axum::http::StatusCode, msg: &'static str) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .body(axum::body::Body::from(msg))
        .expect("static response")
}

// ---------- HTTP request limits ----------

/// Structured rejection for transport-level limits — a JSON-RPC-shaped error
/// body so MCP clients surface something actionable instead of raw HTTP text.
fn limit_error(status: axum::http::StatusCode, msg: &'static str) -> axum::response::Response {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": null,
        "error": {"code": -32000, "message": msg},
    })
    .to_string();
    axum::response::Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .expect("static response")
}

/// Bound a request's concurrency slot and total time-to-response. Dropping
/// the future on client disconnect cancels the work — axum/tokio do that for
/// free; the permit releases either way.
async fn bounded_request(
    gate: Arc<tokio::sync::Semaphore>,
    timeout: std::time::Duration,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Ok(_permit) = gate.try_acquire_owned() else {
        return limit_error(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "MCP concurrency limit reached — retry later",
        );
    };
    match tokio::time::timeout(timeout, next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => limit_error(axum::http::StatusCode::GATEWAY_TIMEOUT, "request timed out"),
    }
}

/// Build the `/mcp` router with the full HTTP security posture: the explicit
/// [`loopback_guard`] (outermost layer), bounded concurrency/time, Bearer
/// auth middleware (with per-IP failure throttling), body-size cap and rmcp's
/// own Host/Origin allowlists configured explicitly rather than left at
/// library defaults. Split out of [`serve_http`] so tests can mount it on an
/// ephemeral port.
pub fn mcp_http_router(doc: SharedDoc, addr: &str, auth: HttpAuth) -> axum::Router {
    use axum::middleware::Next;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::net::SocketAddr;

    let (auth_mode, auth_detail) = match &auth {
        HttpAuth::Token(src) => (McpAuthMode::Bearer, src.describe()),
        HttpAuth::Insecure => (McpAuthMode::Open, "MIDI_MCP_ALLOW_INSECURE".into()),
    };
    doc.lock().unwrap_or_else(|e| e.into_inner()).mcp_security =
        SecurityReport::http(addr, auth_mode, auth_detail);

    let factory = {
        let doc = doc.clone();
        move || -> Result<MidiService, std::io::Error> { Ok(MidiService::new(doc.clone())) }
    };
    // Explicit allowlists: the library default disables Origin validation,
    // which is exactly the DNS-rebinding gap this guards. Loopback Hosts and
    // loopback Origins only; bodies are capped at MAX_HTTP_BODY_BYTES.
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(LOOPBACK_HOSTS.iter().copied())
        .with_allowed_origins(LOOPBACK_ORIGINS.iter().copied())
        .with_max_request_body_bytes(MAX_HTTP_BODY_BYTES);
    let service =
        StreamableHttpService::new(factory, Arc::new(LocalSessionManager::default()), config);

    let mut app = axum::Router::new().route_service("/mcp", service);
    if let HttpAuth::Token(src) = auth {
        let src = Arc::new(src);
        let limiter = Arc::new(AuthLimiter::new());
        app = app.layer(axum::middleware::from_fn(
            move |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
                  req: axum::extract::Request,
                  next: Next| {
                let (src, limiter) = (src.clone(), limiter.clone());
                async move {
                    let ip = peer.ip();
                    if !limiter.allow_attempt(ip) {
                        tracing::warn!(ip = %ip, "mcp http: auth attempts throttled");
                        return http_error(
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            "too many failed auth attempts — retry later",
                        );
                    }
                    let presented = req
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "));
                    let expected = src.token();
                    let ok = match (presented, expected) {
                        (Some(p), Some(e)) => constant_time_eq(p, &e),
                        _ => false,
                    };
                    limiter.record(ip, ok);
                    if ok {
                        next.run(req).await
                    } else {
                        // log the attempt, never the credential
                        tracing::warn!(ip = %ip, "mcp http: auth failed");
                        http_error(axum::http::StatusCode::UNAUTHORIZED, "unauthorized")
                    }
                }
            },
        ));
    }
    // cheap limit checks run before auth/body work, but after the loopback
    // guard so hostile Host/Origin is rejected even under a saturated gate
    let gate = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS));
    let app = app.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: Next| {
            let gate = gate.clone();
            async move { bounded_request(gate, REQUEST_TIMEOUT, req, next).await }
        },
    ));
    // guard is applied last so it is the outermost layer: hostile Host/Origin
    // requests are rejected before auth and before tool dispatch.
    app.layer(axum::middleware::from_fn(loopback_guard))
}

/// Serve Streamable-HTTP on `addr` (e.g. "127.0.0.1:7878") at path `/mcp`.
/// `auth` comes from [`resolve_http_auth`] — Bearer by default, explicitly
/// opted-out `Insecure` otherwise.
/// `shutdown`: resolve to stop accepting connections and drain in-flight
/// requests (axum graceful shutdown — open connections finish their work).
pub async fn serve_http(
    doc: SharedDoc,
    addr: &str,
    auth: HttpAuth,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let app = mcp_http_router(doc, addr, auth);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("mcp http listening on {addr}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown.await;
    })
    .await?;
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
        let (is_err, text) = call_text(shared, name, args);
        (is_err, serde_json::from_str(&text).unwrap_or(json!(null)))
    }

    /// dispatch a tool and decode its (is_error, first text block verbatim)
    fn call_text(shared: &SharedDoc, name: &str, args: serde_json::Value) -> (bool, String) {
        match dispatch(name, &args, shared.clone()) {
            CallToolResponse::Complete(r) => {
                let is_err = r.is_error.unwrap_or(false);
                let text = match r.content.first() {
                    Some(ContentBlock::Text(t)) => t.text.clone(),
                    other => panic!("expected text content, got {other:?}"),
                };
                (is_err, text)
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
        let before = sh.lock().unwrap().doc.serialize(smf_core::WriteOptions {
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
        assert_eq!(
            sh.lock().unwrap().doc.revision(),
            0,
            "dry run applied nothing"
        );
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
        assert_eq!(
            starts, sorted,
            "pages stay in (start, key, track, id) order"
        );
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

    // ---------- issue #10: auth ----------
    // ---------- issue #16: file-system scope ----------

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir()
            .join("midi-editor-mcp-tests")
            .join(format!("{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn token_file_provisions_once_then_reuses() {
        let p = tmpdir("tok").join("sub").join("mcp-token");
        ensure_token_file(&p).unwrap();
        let t1 = read_token_file(&p).unwrap();
        assert_eq!(t1.len(), 64, "32 bytes of hex");
        assert!(t1.bytes().all(|b| b.is_ascii_hexdigit()));
        // second launch reuses, doesn't rotate
        ensure_token_file(&p).unwrap();
        assert_eq!(read_token_file(&p).unwrap(), t1);
        // a corrupt file is regenerated rather than trusted
        std::fs::write(&p, "not-a-token\nextra").unwrap();
        ensure_token_file(&p).unwrap();
        let t2 = read_token_file(&p).unwrap();
        assert_eq!(t2.len(), 64);
        assert_ne!(t1, t2);
        let _ = std::fs::remove_dir_all(p.parent().unwrap().parent().unwrap());
    }

    #[test]
    fn token_validation_rejects_bad_values() {
        assert!(token_is_valid("abc123"));
        assert!(!token_is_valid(""));
        assert!(!token_is_valid("has space"));
        assert!(!token_is_valid("line\nbreak"));
        assert!(!token_is_valid(&"x".repeat(257)));
    }

    #[test]
    fn constant_time_compares_exactly() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }

    /// Start the real router on an ephemeral port; returns the bound address.
    async fn start_http(auth: HttpAuth) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        let app = mcp_http_router(shared(), &addr, auth)
            .into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("serve");
        });
        addr
    }

    fn mcp_headers(token: Option<&str>) -> Vec<(&'static str, String)> {
        let mut h: Vec<(&'static str, String)> = vec![
            ("Content-Type", "application/json".into()),
            ("Accept", "application/json, text/event-stream".into()),
        ];
        if let Some(t) = token {
            h.push(("Authorization", format!("Bearer {t}")));
        }
        h
    }
    async fn authed_post(addr: &str, token: Option<&str>) -> u16 {
        let h = mcp_headers(token);
        let pairs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
        http_post(addr, None, &pairs, INIT).await
    }

    #[tokio::test]
    async fn bearer_token_required_and_checked() {
        let addr = start_http(HttpAuth::Token(TokenSource::Fixed("s3cret".into()))).await;
        assert_eq!(authed_post(&addr, None).await, 401, "no creds rejected");
        assert_eq!(authed_post(&addr, Some("wrong")).await, 401);
        assert_eq!(authed_post(&addr, Some("s3cret")).await, 200);
    }

    #[tokio::test]
    async fn token_file_source_rotates_without_restart() {
        let dir = tmpdir("rotate");
        let p = dir.join("mcp-token");
        std::fs::write(&p, "tok-a").unwrap();
        let addr = start_http(HttpAuth::Token(TokenSource::File(p.clone()))).await;
        assert_eq!(authed_post(&addr, Some("tok-a")).await, 200);
        // rotate: rewrite the file — next request must require the new token
        std::fs::write(&p, "tok-b").unwrap();
        assert_eq!(
            authed_post(&addr, Some("tok-a")).await,
            401,
            "old token revoked"
        );
        assert_eq!(
            authed_post(&addr, Some("tok-b")).await,
            200,
            "new token live"
        );
        // revoke: delete the file — everything fails closed
        std::fs::remove_file(&p).unwrap();
        assert_eq!(authed_post(&addr, Some("tok-b")).await, 401);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn auth_failures_are_rate_limited() {
        let addr = start_http(HttpAuth::Token(TokenSource::Fixed("s3cret".into()))).await;
        for _ in 0..AUTH_FAIL_MAX {
            assert_eq!(authed_post(&addr, Some("bad")).await, 401);
        }
        // past the limit the client is throttled — even with the right token
        assert_eq!(authed_post(&addr, Some("bad")).await, 429);
        assert_eq!(authed_post(&addr, Some("s3cret")).await, 429);
    }

    #[test]
    fn resolve_prefers_env_then_opt_out_then_file() {
        // env vars are process-global; keep every mutation inside this one
        // test so nothing races with a sibling
        let dir = tmpdir("resolve");
        std::env::set_var("MIDI_MCP_TOKEN", "envtok");
        std::env::set_var("LOCALAPPDATA", &dir);
        std::env::remove_var("MIDI_MCP_ALLOW_INSECURE");
        match resolve_http_auth().unwrap() {
            HttpAuth::Token(TokenSource::Fixed(t)) => assert_eq!(t, "envtok"),
            _ => panic!("env token must win"),
        }
        std::env::remove_var("MIDI_MCP_TOKEN");
        std::env::set_var("MIDI_MCP_ALLOW_INSECURE", "1");
        assert!(matches!(resolve_http_auth().unwrap(), HttpAuth::Insecure));
        std::env::remove_var("MIDI_MCP_ALLOW_INSECURE");
        match resolve_http_auth().unwrap() {
            HttpAuth::Token(TokenSource::File(p)) => {
                assert_eq!(p, dir.join("midi-editor").join("mcp-token"));
                assert!(read_token_file(&p).is_some(), "provisioned on resolve");
            }
            _ => panic!("default must auto-provision a token file"),
        }
        std::env::remove_var("LOCALAPPDATA");
    }

    /// shared() pointed at a real temp dir as its document path
    fn shared_in(dir: &std::path::Path) -> SharedDoc {
        let sh = shared();
        sh.lock().unwrap().path = Some(dir.join("song.mid"));
        sh
    }

    #[test]
    fn save_inside_doc_dir_writes() {
        let dir = tmpdir("inside");
        let sh = shared_in(&dir);
        let out = dir.join("out.mid");
        let (err, v) = call(&sh, "save", json!({"path": out.to_string_lossy()}));
        assert!(!err, "{v}");
        assert!(out.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summary_reports_auth_mode_not_secret() {
        let sh = shared();
        let (err, v) = call(&sh, "document_summary", json!({}));
        assert!(!err);
        assert_eq!(v["mcp_auth"]["mode"], "stdio");
        let _router = mcp_http_router(
            sh.clone(),
            "127.0.0.1:0",
            HttpAuth::Token(TokenSource::Fixed("dontleak".into())),
        );
        let (err, v) = call(&sh, "document_summary", json!({}));
        assert!(!err);
        assert_eq!(v["mcp_auth"]["mode"], "bearer");
        assert_eq!(v["mcp_auth"]["detail"], "MIDI_MCP_TOKEN");
        assert!(!v.to_string().contains("dontleak"), "token never surfaces");
    }

    #[test]
    fn authority_parsing_accepts_only_wellformed() {
        assert_eq!(
            authority_host("127.0.0.1:7878").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(authority_host("LOCALHOST").as_deref(), Some("localhost"));
        assert_eq!(authority_host("[::1]:7878").as_deref(), Some("::1"));
        assert_eq!(authority_host("[::1]").as_deref(), Some("::1"));
        // malformed / hostile spellings
        for bad in [
            "",
            "127.0.0.1:",
            ":7878",
            "a:b:c",
            "127.0.0.1:8x",
            "[::1",
            "::1]",
            "user@127.0.0.1",
            "evil.com@127.0.0.1",
            "127.0.0.1 @evil.com",
        ] {
            assert!(authority_host(bad).is_none(), "{bad:?} must be malformed");
        }
        // parses fine but is not loopback
        assert!(!is_loopback_host(
            &authority_host("127.0.0.1.evil.com").unwrap()
        ));
        assert!(!is_loopback_host(&authority_host("localhost.").unwrap()));
        assert!(!is_loopback_host(&authority_host("127.1").unwrap()));
    }

    #[test]
    fn origin_check_accepts_only_loopback() {
        for good in [
            "http://localhost",
            "https://localhost:6274",
            "http://127.0.0.1:7878",
            "http://[::1]:3000",
            "https://[::1]",
        ] {
            assert!(origin_is_loopback(good), "{good:?} must pass");
        }
        for bad in [
            "null",
            "",
            "https://evil.com",
            "http://127.0.0.1.evil.com",
            "http://evil.com@127.0.0.1",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "http://127.0.0.1@evil.com",
            "localhost",       // missing scheme
            "ftp://localhost", // wrong scheme
        ] {
            assert!(!origin_is_loopback(bad), "{bad:?} must be rejected");
        }
    }

    /// Start the real router on an ephemeral port; returns the bound address.
    async fn start_http_insecure() -> String {
        start_http(HttpAuth::Insecure).await
    }

    /// Raw HTTP/1.1 POST (no client library needed); returns the status code.
    /// `host` overrides the Host header; `headers` may carry any others.
    async fn http_post(
        addr: &str,
        host: Option<&str>,
        headers: &[(&str, &str)],
        body: &str,
    ) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let host = host.unwrap_or(addr);
        let mut req = format!(
            "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if !host.is_empty() {
            req += &format!("Host: {host}\r\n");
        }
        for (k, v) in headers {
            req += &format!("{k}: {v}\r\n");
        }
        req += "\r\n";
        req += body;
        s.write_all(req.as_bytes()).await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), s.read_to_end(&mut buf))
            .await
            .expect("response timed out");
        let text = String::from_utf8_lossy(&buf);
        text.split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or_else(|| panic!("no status line in {text:?}"))
    }

    /// Raw HTTP/1.1 request; returns the status code.
    async fn http_req(
        addr: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        for (k, v) in headers {
            req += &format!("{k}: {v}\r\n");
        }
        req += "\r\n";
        s.write_all(req.as_bytes()).await.unwrap();
        s.write_all(body).await.unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(30), s.read_to_end(&mut buf))
            .await
            .expect("response timed out");
        String::from_utf8_lossy(&buf)
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
    }

    /// A well-formed initialize request — the request every MCP client starts
    /// with. Proves legitimate local clients still get through the guard.
    const INIT: &str = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","#,
        r#""params":{"protocolVersion":"2025-03-26","capabilities":{},"#,
        r#""clientInfo":{"name":"t","version":"0"}}}"#,
    );
    const MCP_HEADERS: &[(&str, &str)] = &[
        ("Content-Type", "application/json"),
        ("Accept", "application/json, text/event-stream"),
    ];

    #[tokio::test]
    async fn legitimate_client_initialize_passes() {
        let addr = start_http_insecure().await;
        let status = http_post(&addr, None, MCP_HEADERS, INIT).await;
        assert_eq!(status, 200);
    }

    #[tokio::test]
    async fn hostile_origins_are_rejected() {
        let addr = start_http_insecure().await;
        for origin in [
            "https://evil.com",
            "null",
            "http://127.0.0.1.attacker.tld",
            "http://user@127.0.0.1:7878",
        ] {
            let headers: Vec<_> = MCP_HEADERS
                .iter()
                .cloned()
                .chain([("Origin", origin)])
                .collect();
            let status = http_post(&addr, None, &headers, INIT).await;
            assert_eq!(status, 403, "Origin {origin:?} must be rejected");
        }
    }

    #[tokio::test]
    async fn hostile_hosts_are_rejected() {
        let addr = start_http_insecure().await;
        for host in [
            "evil.com",
            "127.0.0.1.evil.com",
            "localhost.evil.com",
            "user@127.0.0.1",
        ] {
            let status = http_post(&addr, Some(host), MCP_HEADERS, INIT).await;
            assert_eq!(status, 403, "Host {host:?} must be rejected");
        }
    }

    #[tokio::test]
    async fn loopback_ipv4_ipv6_and_loopback_origin_pass() {
        let addr = start_http_insecure().await;
        // Host variants the guard must accept (the socket is IPv4 but the
        // Host header is validated by value, not by interface)
        for host in [
            "localhost:7878",
            "127.0.0.1:7878",
            "[::1]:7878",
            "localhost",
        ] {
            let status = http_post(&addr, Some(host), MCP_HEADERS, INIT).await;
            assert_eq!(status, 200, "Host {host:?} must pass");
        }
        // local browser tooling origins pass too
        for origin in [
            "http://localhost:6274",
            "https://127.0.0.1:3000",
            "http://[::1]:9",
        ] {
            let headers: Vec<_> = MCP_HEADERS
                .iter()
                .cloned()
                .chain([("Origin", origin)])
                .collect();
            let status = http_post(&addr, None, &headers, INIT).await;
            assert_eq!(status, 200, "Origin {origin:?} must pass");
        }
    }

    #[tokio::test]
    async fn diagnostics_reports_http_security_mode() {
        let addr = start_http_insecure().await;
        let sh = shared();
        // mounting the router is what stamps the security report
        let _app = mcp_http_router(
            sh.clone(),
            &addr,
            HttpAuth::Token(TokenSource::Fixed("t0k3n".into())),
        );
        let (err, v) = call(&sh, "diagnostics", json!({}));
        assert!(!err);
        assert_eq!(v["security"]["auth"], "bearer");
        assert!(v["security"]["transport"]
            .as_str()
            .unwrap()
            .contains("streamable-http"));
        // and the report never leaks credential material
        assert!(!v.to_string().contains("t0k3n"));
    }

    #[test]
    fn diagnostics_reports_stdio_mode_by_default() {
        let sh = shared();
        let (err, v) = call(&sh, "diagnostics", json!({}));
        assert!(!err);
        assert_eq!(v["security"]["transport"], "stdio");
    }

    // ---------- issue #11: limits ----------

    #[tokio::test]
    async fn normal_request_passes_limits() {
        let addr = start_http_insecure().await;
        assert_eq!(
            http_req(&addr, "POST", "/mcp", MCP_HEADERS, INIT.as_bytes()).await,
            200
        );
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let addr = start_http_insecure().await;
        let body = vec![b'x'; MAX_HTTP_BODY_BYTES + 1];
        let status = http_req(&addr, "POST", "/mcp", MCP_HEADERS, &body).await;
        assert_eq!(
            status, 413,
            "over {MAX_HTTP_BODY_BYTES} bytes must not reach dispatch"
        );
    }

    /// A stub endpoint behind `bounded_request` lets the limits be exercised
    /// with tiny values instead of the production constants.
    async fn stub_limited(slots: usize, timeout: std::time::Duration) -> String {
        use axum::middleware::Next;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let gate = Arc::new(tokio::sync::Semaphore::new(slots));
        let app = axum::Router::new()
            .route(
                "/slow",
                axum::routing::get(|| async {
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    "ok"
                }),
            )
            .layer(axum::middleware::from_fn(
                move |req: axum::extract::Request, next: Next| {
                    let gate = gate.clone();
                    async move { bounded_request(gate, timeout, req, next).await }
                },
            ));
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        addr
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_requests_are_bounded() {
        let addr = stub_limited(2, std::time::Duration::from_secs(30)).await;
        let mut set = tokio::task::JoinSet::new();
        for _ in 0..8 {
            let addr = addr.clone();
            set.spawn(async move { http_req(&addr, "GET", "/slow", &[], &[]).await });
        }
        let mut codes = Vec::new();
        while let Some(c) = set.join_next().await {
            codes.push(c.unwrap());
        }
        assert!(
            codes.iter().filter(|&&c| c == 429).count() >= 5,
            "slots held by slow requests must reject the flood: {codes:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn slow_requests_time_out() {
        let addr = stub_limited(8, std::time::Duration::from_millis(50)).await;
        assert_eq!(http_req(&addr, "GET", "/slow", &[], &[]).await, 504);
    }

    #[test]
    fn apply_patch_ops_array_is_bounded() {
        let sh = shared();
        let ops = vec![serde_json::json!({"op": "insert_note"}); MAX_PATCH_OPS + 1];
        let (err, text) = call_text(&sh, "apply_patch", json!({"ops": ops}));
        assert!(err);
        assert!(text.contains("too large"), "{text}");
        // exactly at the cap the size check must not fire
        let ops = vec![serde_json::json!({"op": "bogus"}); MAX_PATCH_OPS];
        let (err, text) = call_text(&sh, "apply_patch", json!({"ops": ops}));
        assert!(err);
        assert!(!text.contains("too large"), "{text}");
    }

    #[test]
    fn concurrent_saves_do_not_collide_on_temp_name() {
        let dir = std::env::temp_dir()
            .join("midi-editor-mcp-tests")
            .join(format!("saves-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("song.mid");
        std::thread::scope(|s| {
            for tag in ["a", "b"] {
                let p = p.clone();
                s.spawn(move || {
                    for i in 0..50 {
                        write_atomic(&p, format!("{tag}{i}").as_bytes())
                            .expect("save must not fail under concurrency");
                    }
                });
            }
        });
        let final_bytes = std::fs::read(&p).unwrap();
        assert!(final_bytes == b"a49" || final_bytes == b"b49");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_with_no_arg_uses_doc_path() {
        let dir = tmpdir("noarg");
        let sh = shared_in(&dir);
        // no path arg → document path, always allowed even outside roots
        let (err, v) = call(&sh, "save", json!({}));
        assert!(!err, "{v}");
        assert!(dir.join("song.mid").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_traversal_cannot_escape_root() {
        let dir = tmpdir("trav");
        let allowed = dir.join("allowed");
        std::fs::create_dir_all(&allowed).unwrap();
        let sh = shared_in(&allowed);
        // `../..` out of the document dir must be canonicalized then rejected
        let evil = allowed.join("..").join("..").join("evil.mid");
        let (err, _) = call(&sh, "save", json!({"path": evil.to_string_lossy()}));
        assert!(err);
        assert!(
            !dir.parent().unwrap().join("evil.mid").exists(),
            "escaped write happened"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diagnostics_report_total_and_truncation() {
        let sh = shared();
        let (err, v) = call(&sh, "diagnostics", json!({}));
        assert!(!err);
        assert_eq!(v["total"], v["count"]);
        assert_eq!(v["truncated"], false);
    }

    #[test]
    fn save_outside_all_roots_is_actionable_error() {
        let allowed = tmpdir("scope-a");
        let elsewhere = tmpdir("scope-b");
        let sh = shared_in(&allowed);
        let target = elsewhere.join("x.mid");
        let (err, text) = call_text(&sh, "save", json!({"path": target.to_string_lossy()}));
        assert!(err);
        assert!(text.contains("outside the MCP save scope"), "{text}");
        assert!(text.contains("MIDI_MCP_ALLOWED_ROOTS"), "{text}");
        assert!(!target.exists());
        let _ = (
            std::fs::remove_dir_all(&allowed),
            std::fs::remove_dir_all(&elsewhere),
        );
    }

    #[test]
    fn save_via_reparse_point_cannot_escape() {
        // directory junction needs no privilege on Windows — other
        // platforms can't forge one here, so the test is Windows-only
        #[cfg(windows)]
        {
            let dir = tmpdir("junction");
            let allowed = dir.join("allowed");
            let outside = dir.join("outside");
            std::fs::create_dir_all(&allowed).unwrap();
            std::fs::create_dir_all(&outside).unwrap();
            std::process::Command::new("cmd")
                .args(["/c", "mklink", "/J"])
                .arg(allowed.join("link"))
                .arg(&outside)
                .status()
                .expect("mklink");
            assert!(allowed.join("link").exists());
            let sh = shared_in(&allowed);
            // looks inside the allowed root, resolves outside it
            let via_link = allowed.join("link").join("evil.mid");
            let (err, text) = call_text(&sh, "save", json!({"path": via_link.to_string_lossy()}));
            assert!(err, "junction must be resolved before the root check");
            assert!(text.contains("outside the MCP save scope"), "{text}");
            assert!(!outside.join("evil.mid").exists());
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[test]
    fn stdio_scope_also_allows_cwd() {
        let cwd = std::env::current_dir().unwrap();
        // parent must exist for canonicalization — use the cwd itself
        let target = cwd.join("stdio-write.mid");
        // HTTP scope refuses (not under doc dir or extra roots)
        let none: Option<PathBuf> = None;
        assert!(authorize_write(&none, FsScope::Http, &[], &target).is_err());
        // stdio trusts the spawning client: cwd is an implicit root
        assert!(authorize_write(&none, FsScope::Stdio, &[], &target).is_ok());
    }

    #[test]
    fn env_roots_extend_the_scope() {
        let dir = tmpdir("envroots");
        let target = dir.join("ok.mid");
        std::env::set_var("MIDI_MCP_ALLOWED_ROOTS", &dir);
        let roots = roots_from_env("MIDI_MCP_ALLOWED_ROOTS");
        std::env::remove_var("MIDI_MCP_ALLOWED_ROOTS");
        let none: Option<PathBuf> = None;
        assert_eq!(
            authorize_write(&none, FsScope::Http, &roots, &target).unwrap(),
            std::fs::canonicalize(&dir).unwrap().join("ok.mid")
        );
        let _ = std::fs::remove_dir_all(&dir);
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

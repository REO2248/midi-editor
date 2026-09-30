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
    /// drained by the GUI watcher
    pub transport_req: Vec<TransportReq>,
}

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
            transport_req: Vec::new(),
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

    /// Apply a transaction and push it onto the shared undo stack.
    /// Returns the new revision.
    pub fn apply(&mut self, label: &str, ops: Vec<Op>) -> Result<u64, ApplyError> {
        let tx = Transaction {
            label: label.into(),
            base: self.doc.revision(),
            ops,
        };
        let rev = self.doc.apply(tx.clone())?;
        self.undo.push(tx);
        self.gui_notify.fetch_add(1, Ordering::Relaxed);
        Ok(rev)
    }
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
                 one tool call = one undo step. Ticks are absolute PPQ ticks; keys 0-127; \
                 channels 0-15. Use query_events to find event ids for edits.",
            )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        tool_defs().into_iter().find(|(n, _)| *n == name).map(|(_, t)| t)
    }

    fn list_tools(
        &self,
        _req: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(
            tool_defs().into_iter().map(|(_, t)| t).collect(),
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

fn tool_defs() -> Vec<(&'static str, Tool)> {
    vec![
        (
            "document_summary",
            tool(
                "document_summary",
                "JSON summary: format/division, per-track names+counts, note count, duration, revision, dirty flag",
                object_schema(serde_json::json!({})),
            ),
        ),
        (
            "list_notes",
            tool(
                "list_notes",
                "Paired note view (NoteOn+NoteOff). Args: track?, from_tick?, to_tick?, limit?",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"},
                    "from_tick": {"type": "integer"},
                    "to_tick": {"type": "integer"},
                    "limit": {"type": "integer"},
                })),
            ),
        ),
        (
            "query_events",
            tool(
                "query_events",
                "Raw SMF events (id, tick, seq, kind, raw_hex). Args: track?, from_tick?, to_tick?, limit?, offset?",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"},
                    "from_tick": {"type": "integer"},
                    "to_tick": {"type": "integer"},
                    "limit": {"type": "integer"},
                    "offset": {"type": "integer"},
                })),
            ),
        ),
        (
            "diagnostics",
            tool(
                "diagnostics",
                "Import-quality findings over the raw event layer: dangling noteOn, zero-length notes, missing End-of-Track, tempo events outside the conductor track. Each has code/track/tick/event_id + detail.",
                object_schema(serde_json::json!({})),
            ),
        ),
        (
            "normalize",
            tool(
                "normalize",
                "Resolve import-quality findings as one undo step. Args: codes? (array of diagnostic codes; omitted = fix all). Returns resolved/failed counts.",
                object_schema(serde_json::json!({"codes": {"type": "array", "items": {"type": "string"}}})),
            ),
        ),
        (
            "apply_patch",
            tool(
                "apply_patch",
                "Atomic edit as one undo step. Optional base_revision: when given it must match document_summary.revision (optimistic concurrency) \
                 (optimistic concurrency). dry_run:true returns the op breakdown without applying. ops: insert_note {track,key,vel,start,dur,channel} | \
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
        ),
        (
            "undo",
            tool("undo", "Revert the last transaction (shared with GUI edits)", object_schema(serde_json::json!({}))),
        ),
        (
            "redo",
            tool("redo", "Replay the last undone transaction", object_schema(serde_json::json!({}))),
        ),
        (
            "save",
            tool(
                "save",
                "Serialize the document to SMF and write it. Args: path? (defaults to the document's open path)",
                object_schema(serde_json::json!({"path": {"type": "string"}})),
            ),
        ),
        (
            "get_tempo_map",
            tool(
                "get_tempo_map",
                "Tempo breakpoints: [{tick, us_per_quarter, bpm, cumulative_us}] + ppq. Read before editing tempo or converting ticks<->time.",
                object_schema(serde_json::json!({})),
            ),
        ),
        (
            "get_meta",
            tool(
                "get_meta",
                "Meta events (names, markers, lyrics, text, tempo, time-sig) with text decoded (UTF-8/SJIS). Args: track?, meta_type? (hex int)",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"},
                    "meta_type": {"type": "integer"},
                })),
            ),
        ),
        (
            "get_cc",
            tool(
                "get_cc",
                "Latest controller value per (track, channel, cc) — the current CC state. Args: track?, channel?, cc?",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"},
                    "channel": {"type": "integer"},
                    "cc": {"type": "integer"},
                })),
            ),
        ),
        (
            "list_midi_ports",
            tool(
                "list_midi_ports",
                "Enumerate real MIDI outputs/inputs on this machine (WinMM): [{index, name}]. Use names in set_track_destination.",
                object_schema(serde_json::json!({})),
            ),
        ),
        (
            "list_destinations",
            tool(
                "list_destinations",
                "Output routing: catalog [{index, label, kind, port_name|plugin_path}], default_dest, per-track overrides, mute/solo.",
                object_schema(serde_json::json!({})),
            ),
        ),
        (
            "set_track_destination",
            tool(
                "set_track_destination",
                "Route a track to an output. Args: track, destination: {\"midi_port\":\"<name>\"} | {\"vst3\":\"<bundle path>\"} | \"default\" (inherit). Unknown destinations are remembered and fail at play time.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"},
                    "destination": {},
                })),
            ),
        ),
        (
            "transport",
            tool(
                "transport",
                "Ask the GUI transport: {action: \"play\"|\"stop\"|\"seek\", tick?}. Only works while the app is running.",
                object_schema(serde_json::json!({
                    "action": {"type": "string"},
                    "tick": {"type": "integer"},
                })),
            ),
        ),
        (
            "quantize",
            tool(
                "quantize",
                "Snap note onsets to a grid (duration preserved). Args: track? (all when omitted), from?, to?, grid? (ticks, default ppq/4), strength? (0-100, default 100). Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                    "grid": {"type": "integer"}, "strength": {"type": "integer"},
                    "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "transpose",
            tool(
                "transpose",
                "Shift note pitch. Args: track?, from?, to?, semitones (+/-). Notes leaving 0..127 are skipped. Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                    "semitones": {"type": "integer"}, "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "scale_velocity",
            tool(
                "scale_velocity",
                "Multiply note velocities. Args: track?, from?, to?, factor (e.g. 1.2 = +20%). Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                    "factor": {"type": "number"}, "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "set_channel",
            tool(
                "set_channel",
                "Retarget all channel events in range to one channel. Args: track, from?, to?, channel (1-16). Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                    "channel": {"type": "integer"}, "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "set_program",
            tool(
                "set_program",
                "Program change (with optional bank CC0/CC32) on a track. Args: track, tick, program (0-127), channel? (default track channel), bank_msb?, bank_lsb?. Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "tick": {"type": "integer"},
                    "program": {"type": "integer"}, "channel": {"type": "integer"},
                    "bank_msb": {"type": "integer"}, "bank_lsb": {"type": "integer"},
                    "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "set_cc",
            tool(
                "set_cc",
                "Insert controller events. Args: track, channel? (default track channel), points: [{tick, cc, value}] — or scalar {tick, cc, value}. Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "channel": {"type": "integer"},
                    "tick": {"type": "integer"}, "cc": {"type": "integer"}, "value": {"type": "integer"},
                    "points": {"type": "array", "items": {"type": "object"}},
                    "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "set_pitch_bend",
            tool(
                "set_pitch_bend",
                "Insert a pitch-bend event. Args: track, tick, value (0..16383, 8192=center), channel?. Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "tick": {"type": "integer"},
                    "value": {"type": "integer"}, "channel": {"type": "integer"},
                    "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "set_tempo",
            tool(
                "set_tempo",
                "Set/replace tempo at a tick (conductor track). Args: tick, bpm. Optional base_revision.",
                object_schema(serde_json::json!({
                    "tick": {"type": "integer"}, "bpm": {"type": "number"},
                    "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "set_time_signature",
            tool(
                "set_time_signature",
                "Set/replace time signature at a tick. Args: tick, num (beats/bar), den (beat value 4=quarter,8=eighth). Optional base_revision.",
                object_schema(serde_json::json!({
                    "tick": {"type": "integer"}, "num": {"type": "integer"}, "den": {"type": "integer"},
                    "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "set_track_channel",
            tool(
                "set_track_channel",
                "Set the track's default channel (FF20 meta). Args: track, channel (1-16). Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "channel": {"type": "integer"},
                    "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "set_track_name",
            tool(
                "set_track_name",
                "Set track name (UTF-8 meta 0x03). Args: track, name. Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "name": {"type": "string"},
                    "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "add_track",
            tool(
                "add_track",
                "Append a track (with optional name). Args: name?. Optional base_revision.",
                object_schema(serde_json::json!({
                    "name": {"type": "string"}, "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "remove_track",
            tool(
                "remove_track",
                "Remove a track entirely. Args: track. Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "delete_range",
            tool(
                "delete_range",
                "Delete channel events in [from,to) (notes delete whole). Args: track, from, to. Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                    "base_revision": {"type": "integer"},
                })),
            ),
        ),
        (
            "duplicate_range",
            tool(
                "duplicate_range",
                "Copy channel events in [from,to) to start at `to`. Args: track, from, to. Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                    "base_revision": {"type": "integer"},
                })),
            ),
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
    match name {
        "document_summary" => ok_json(summary_json(&sh.doc, &sh.path, sh.saved_revision)),
        "diagnostics" => {
            let diags = sh.doc.diagnose();
            ok_json(serde_json::json!({
                "count": diags.len().min(MAX_DIAG_RESULTS),
                "total": diags.len(),
                "truncated": diags.len() > MAX_DIAG_RESULTS,
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
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let before = sh.doc.diagnose().len();
            let codes: Vec<&str> = code_strs.iter().map(String::as_str).collect();
            let ops = sh.doc.fix_ops(&codes);
            if ops.is_empty() {
                return ok_json(serde_json::json!({"fixed": 0, "remaining": before}));
            }
            match sh.apply("normalize", ops) {
                Ok(rev) => {
                    let remaining = sh.doc.diagnose().len();
                    ok_json(serde_json::json!({"fixed": before - remaining, "remaining": remaining, "revision": rev}))
                }
                Err(e) => err_json(e.to_string()),
            }
        }
        "list_notes" => {
            let track = args["track"].as_u64().map(|v| v as usize);
            let from = args["from_tick"].as_u64().unwrap_or(0);
            let to = args["to_tick"].as_u64().unwrap_or(u64::MAX);
            let limit = args["limit"].as_u64().unwrap_or(500).min(MAX_QUERY_LIMIT as u64) as usize;
            let notes: Vec<_> = sh
                .doc
                .notes()
                .into_iter()
                .filter(|n| n.start_tick >= from && n.start_tick <= to)
                .filter(|n| track.is_none() || Some(n.track) == track)
                .take(limit)
                .collect();
            ok_json(serde_json::json!({
                "count": notes.len(),
                "notes": notes.iter().map(note_json).collect::<Vec<_>>(),
            }))
        }
        "query_events" => {
            let track = args["track"].as_u64().map(|v| v as usize);
            let from = args["from_tick"].as_u64().unwrap_or(0);
            let to = args["to_tick"].as_u64().unwrap_or(u64::MAX);
            let limit = args["limit"].as_u64().unwrap_or(500).min(MAX_QUERY_LIMIT as u64) as usize;
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            // collect light (tick, seq, track, idx) refs only; JSON encoding
            // happens for the offset/limit window, not for every match
            let mut hits: Vec<(u64, u32, usize, usize)> = Vec::new();
            for (ti, t) in sh.doc.tracks.iter().enumerate() {
                if track.is_some() && track != Some(ti) {
                    continue;
                }
                for (ei, e) in t.events.iter().enumerate() {
                    if e.tick >= from && e.tick <= to {
                        hits.push((e.tick, e.seq, ti, ei));
                    }
                }
            }
            hits.sort_by_key(|h| (h.0, h.1));
            let total = hits.len();
            let evs: Vec<_> = hits
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|(_, _, ti, ei)| {
                    let mut j = event_json(&sh.doc.tracks[ti].events[ei]);
                    j["track"] = ti.into();
                    j
                })
                .collect();
            ok_json(serde_json::json!({"total": total, "events": evs}))
        }
        "apply_patch" => {
            let base = args["base_revision"].as_u64();
            if base.is_some() && base != Some(sh.doc.revision()) {
                return err_json(format!(
                    "stale base_revision: current is {}; call document_summary",
                    sh.doc.revision()
                ));
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
            let ops = match build_ops(&mut sh.doc, &ops_json) {
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
            match sh.apply(label, ops) {
                Ok(rev) => ok_json(serde_json::json!({"applied": true, "revision": rev})),
                Err(e) => err_json(e.to_string()),
            }
        }
        "undo" => {
            let res = {
                let Shared { doc, undo, .. } = &mut *sh;
                undo.undo(doc)
            };
            match res {
                Some(l) => {
                    sh.gui_notify.fetch_add(1, Ordering::Relaxed);
                    ok_json(serde_json::json!({"undone": l, "revision": sh.doc.revision()}))
                }
                None => err_json("nothing to undo"),
            }
        }
        "redo" => {
            let res = {
                let Shared { doc, undo, .. } = &mut *sh;
                undo.redo(doc)
            };
            match res {
                Some(l) => {
                    sh.gui_notify.fetch_add(1, Ordering::Relaxed);
                    ok_json(serde_json::json!({"redone": l, "revision": sh.doc.revision()}))
                }
                None => err_json("nothing to redo"),
            }
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
            let tm = &sh.doc.tempo_map;
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
            let hint = sh.doc.text_encoding_hint();
            let mut out = Vec::new();
            for (ti, t) in sh.doc.tracks.iter().enumerate() {
                if track.is_some() && track != Some(ti) {
                    continue;
                }
                for e in &t.events {
                    if let EventKind::Meta { meta_type, data } = &e.kind {
                        if mt.is_some() && mt != Some(*meta_type) {
                            continue;
                        }
                        // only 0x01-0x0F are text-family metas; the rest
                        // (tempo, time sig, ports, ...) are binary payloads
                        let text = if (0x01..=0x0f).contains(meta_type) {
                            serde_json::Value::String(
                                smf_core::decode_text(data, hint))
                        } else {
                            serde_json::Value::Null
                        };
                        out.push(serde_json::json!({
                            "track": ti, "id": e.id, "tick": e.tick,
                            "type": format!("0x{meta_type:02x}"),
                            "text": text,
                            "data_hex": bytes_hex(data),
                        }));
                    }
                }
            }
            ok_json(serde_json::json!({"count": out.len(), "meta": out}))
        }
        "get_cc" => {
            let track = args["track"].as_u64().map(|v| v as usize);
            let chan = args["channel"].as_u64().map(|v| v as u8);
            let ccn = args["cc"].as_u64().map(|v| v as u8);
            // latest value wins; events are already tick-sorted
            let mut latest: HashMap<(usize, u8, u8), (u64, u8)> = HashMap::new();
            for (ti, t) in sh.doc.tracks.iter().enumerate() {
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
            ok_json(serde_json::json!({
                "count": rows.len(),
                "cc": rows.iter().map(|((t, ch, cc), (tick, v))| serde_json::json!({
                    "track": t, "channel": ch + 1, "cc": cc, "value": v, "at_tick": tick,
                })).collect::<Vec<_>>(),
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
            if track >= sh.doc.tracks.len() {
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
            let grid = args["grid"].as_u64().unwrap_or_else(|| sh.doc.tempo_map.ppq() / 4);
            let strength = args["strength"].as_u64().unwrap_or(100) as u32;
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.doc.quantize_ops(t, from, to, grid, strength));
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
                ops.extend(sh.doc.transpose_ops(t, from, to, st));
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
                ops.extend(sh.doc.scale_velocity_ops(t, from, to, f));
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
                ops.extend(sh.doc.set_channel_ops(t, from, to, ch));
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
                .unwrap_or_else(|| sh.doc.tracks.get(track).map(|t| t.out_channel).unwrap_or(0));
            let ops = sh.doc.set_program_ops(
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
                .unwrap_or_else(|| sh.doc.tracks.get(track).map(|t| t.out_channel).unwrap_or(0));
            let mut ops = Vec::new();
            if let Some(points) = args["points"].as_array() {
                for p in points {
                    ops.extend(sh.doc.set_cc_ops(
                        track,
                        p["tick"].as_u64().unwrap_or(0),
                        ch,
                        p["cc"].as_u64().unwrap_or(7) as u8,
                        p["value"].as_u64().unwrap_or(0) as u8,
                    ));
                }
            } else {
                ops.extend(sh.doc.set_cc_ops(
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
                .unwrap_or_else(|| sh.doc.tracks.get(track).map(|t| t.out_channel).unwrap_or(0));
            let ops = sh.doc.set_pitch_bend_ops(
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
            let ops = sh.doc.set_tempo_ops(
                args["tick"].as_u64().unwrap_or(0),
                args["bpm"].as_f64().unwrap_or(120.0),
            );
            apply_ops(&mut sh, "set tempo", ops)
        }
        "set_time_signature" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let ops = sh.doc.set_time_sig_ops(
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
            let ops = sh.doc.set_track_channel_ops(
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
            let ops = sh.doc.set_track_name_ops(track, args["name"].as_str().unwrap_or(""));
            apply_ops(&mut sh, "set track name", ops)
        }
        "add_track" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let ops = sh.doc.add_track_ops(args["name"].as_str());
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
            let ops = sh.doc.remove_track_ops(track);
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
            let ops = sh.doc.delete_range_ops(track, from, to);
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
            let to = args["to"].as_u64().unwrap_or_else(|| doc_last_tick(&sh.doc));
            let ops = sh.doc.duplicate_range_ops(track, from, to);
            apply_ops(&mut sh, "duplicate range", ops)
        }
        _ => err_json(format!("unknown tool '{name}'")),
    }
}

fn check_base(sh: &Shared, args: &serde_json::Value) -> Option<CallToolResponse> {
    match args["base_revision"].as_u64() {
        Some(b) if b != sh.doc.revision() => Some(err_json(format!(
            "stale base_revision: current is {}; call document_summary",
            sh.doc.revision()
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
        Some(t) if (t as usize) < sh.doc.tracks.len() => Ok(t as usize),
        Some(t) => Err(err_json(format!(
            "no track {t} (document has {})",
            sh.doc.tracks.len()
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
    // unique per write: two concurrent saves sharing one pid-temp name would
    // otherwise have the second write clobber the first's temp file mid-rename
    static TEMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TEMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".{stem}.sav{}.{}", std::process::id(), seq));
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
        None => Ok((0..sh.doc.tracks.len()).collect()),
        Some(t) if (t as usize) < sh.doc.tracks.len() => Ok(vec![t as usize]),
        Some(t) => Err(err_json(format!(
            "no track {t} (document has {})",
            sh.doc.tracks.len()
        ))),
    }
}

fn apply_ops(sh: &mut Shared, label: &str, ops: Vec<Op>) -> CallToolResponse {
    if ops.is_empty() {
        return ok_json(serde_json::json!({"applied": false, "ops": 0}));
    }
    match sh.apply(label, ops) {
        Ok(rev) => ok_json(serde_json::json!({"applied": true, "revision": rev})),
        Err(e) => err_json(e.to_string()),
    }
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

/// Build the `/mcp` router: body-size cap (rmcp), bounded concurrency and a
/// time budget (middleware), then optional Bearer auth. Split out of
/// [`serve_http`] so tests can mount it on an ephemeral port.
pub fn mcp_http_router(doc: SharedDoc, token: Option<String>) -> axum::Router {
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
        StreamableHttpServerConfig::default()
            .with_max_request_body_bytes(MAX_HTTP_BODY_BYTES),
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
    // outermost layer: cheap limit checks run before auth/body work
    let gate = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS));
    app.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: Next| {
            let gate = gate.clone();
            async move { bounded_request(gate, REQUEST_TIMEOUT, req, next).await }
        },
    ))
}

/// Serve Streamable-HTTP on `addr` (e.g. "127.0.0.1:7878") at path `/mcp`.
/// When `token` is Some, requests must carry `Authorization: Bearer <token>`.
/// Host/Origin validation stays at rmcp's loopback defaults.
pub async fn serve_http(
    doc: SharedDoc,
    addr: &str,
    token: Option<String>,
) -> anyhow::Result<()> {
    let app = mcp_http_router(doc, token);
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

    // ---------- issue #11: limits ----------

    /// Start the real router on an ephemeral port; returns the bound address.
    async fn start_http() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            axum::serve(listener, mcp_http_router(shared(), None))
                .await
                .expect("serve");
        });
        addr
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
    async fn normal_request_passes_limits() {
        let addr = start_http().await;
        assert_eq!(http_req(&addr, "POST", "/mcp", MCP_HEADERS, INIT.as_bytes()).await, 200);
    }

    #[tokio::test]
    async fn oversized_body_is_rejected() {
        let addr = start_http().await;
        let body = vec![b'x'; MAX_HTTP_BODY_BYTES + 1];
        let status = http_req(&addr, "POST", "/mcp", MCP_HEADERS, &body).await;
        assert_eq!(status, 413, "over {MAX_HTTP_BODY_BYTES} bytes must not reach dispatch");
    }

    /// A stub endpoint behind `bounded_request` lets the limits be exercised
    /// with tiny values instead of the production constants.
    async fn stub_limited(slots: usize, timeout: std::time::Duration) -> String {
        use axum::middleware::Next;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap();
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
        assert!(codes.iter().filter(|&&c| c == 429).count() >= 5,
            "slots held by slow requests must reject the flood: {codes:?}");
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
    fn diagnostics_report_total_and_truncation() {
        let sh = shared();
        let (err, v) = call(&sh, "diagnostics", json!({}));
        assert!(!err);
        assert_eq!(v["total"], v["count"]);
        assert_eq!(v["truncated"], false);
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

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
use smf_core::EventKind;
use rmcp::model::*;
use rmcp::service::{RequestContext, ServiceExt};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// Shared editor state. The GUI owns one `Arc`; MCP handlers hold clones and
/// lock briefly per request. `gui_notify` is bumped on every MCP-side edit so
/// the UI can poll and repaint (the GUI has no push channel into views).
pub struct Shared {
    pub doc: Document,
    pub undo: UndoStack,
    pub path: Option<PathBuf>,
    pub saved_revision: u64,
    pub gui_notify: Arc<AtomicU64>,
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
        }
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
    Tool::new(name, description.to_string(), Arc::new(schema.as_object().unwrap().clone()))
}

fn object_schema(props: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "type": "object", "properties": props })
}

fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if s.len() % 2 != 0 {
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
    let last_tick = d
        .tracks
        .iter()
        .flat_map(|t| t.events.iter().map(|e| e.tick))
        .max()
        .unwrap_or(0);
    serde_json::json!({
        "format": d.format,
        "division": format!("{:?}", d.division),
        "tracks": d.tracks.iter().enumerate().map(|(i, t)| serde_json::json!({
            "index": i,
            "name": t.name.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()),
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
    let mut out = Vec::new();
    for op in ops {
        let kind = op.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "insert_note" => {
                let track = op["track"].as_u64().unwrap_or(0) as usize;
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
                            tick: start + dur,
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
                let track = op["track"].as_u64().unwrap_or(0) as usize;
                let mut events = Vec::new();
                for ev in op["events"].as_array().cloned().unwrap_or_default() {
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
                            len: data.len().min(2).max(1) as u8,
                        }
                    } else if let Some(m) = kind_json.get("meta") {
                        let mt = m["type"].as_u64().unwrap_or(0) as u8;
                        let data = if let Some(h) = m["data_hex"].as_str() {
                            hex_to_bytes(h).unwrap_or_default()
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
                        EventKind::SysEx(Bytes::from(hex_to_bytes(h).unwrap_or_default()))
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
                let Some((ti, on_i)) = find_event(doc, on_id) else {
                    return Err(PatchError::Msg(format!("unknown on_id {on_id}")));
                };
                let notes = doc.notes();
                let Some(note) = notes.iter().find(|n| n.on_id == on_id) else {
                    return Err(PatchError::Msg(format!("event {on_id} is not a NoteOn")));
                };
                let mv = |eid: EventId, base_tick: u64| {
                    let (_, ei) = find_event(doc, eid).unwrap();
                    let before = doc.tracks[ti].events[ei].clone();
                    let mut after = before.clone();
                    after.tick = (base_tick as i64
                        + dtick
                        + if eid != on_id { dlen } else { 0 })
                    .max(0) as u64;
                    if let EventKind::Channel { data, .. } = &mut after.kind {
                        data[0] = (note.key as i32 + dkey).clamp(0, 127) as u8;
                    }
                    after.raw_body = None;
                    Op::UpdateEvent {
                        track: ti,
                        before,
                        after,
                    }
                };
                let _ = on_i;
                out.push(mv(on_id, note.start_tick));
                if let Some(off_id) = note.off_id {
                    out.push(mv(off_id, note.end_tick.unwrap_or(note.start_tick)));
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
                 (optimistic concurrency). ops: insert_note {track,key,vel,start,dur,channel} | \
                 insert_events {track,events:[{tick,seq,kind:{channel|meta|sysex_hex}}]} | \
                 remove_events {ids} | move_note {on_id,dtick,dkey,dur_dtick} | \
                 set_tempo {tick,bpm}",
                object_schema(serde_json::json!({
                    "base_revision": {"type": "integer"},
                    "label": {"type": "string"},
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
    ]
}

fn dispatch(
    name: &str,
    args: &serde_json::Value,
    shared: SharedDoc,
) -> CallToolResponse {
    let mut sh = shared.lock().unwrap();
    match name {
        "document_summary" => ok_json(summary_json(&sh.doc, &sh.path, sh.saved_revision)),
        "diagnostics" => {
            let diags = sh.doc.diagnose();
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
            let limit = args["limit"].as_u64().unwrap_or(500) as usize;
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
            let limit = args["limit"].as_u64().unwrap_or(500) as usize;
            let offset = args["offset"].as_u64().unwrap_or(0) as usize;
            let mut evs = Vec::new();
            for (ti, t) in sh.doc.tracks.iter().enumerate() {
                if track.is_some() && track != Some(ti) {
                    continue;
                }
                for e in &t.events {
                    if e.tick >= from && e.tick <= to {
                        evs.push((ti, e));
                    }
                }
            }
            evs.sort_by_key(|(_, e)| (e.tick, e.seq));
            let total = evs.len();
            let evs: Vec<_> = evs
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|(ti, e)| {
                    let mut j = event_json(e);
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
            let ops = match build_ops(&mut sh.doc, &ops_json) {
                Ok(o) => o,
                Err(PatchError::Msg(m)) => return err_json(m),
            };
            if ops.is_empty() {
                return err_json("no ops");
            }
            match sh.apply(label, ops) {
                Ok(rev) => ok_json(serde_json::json!({"applied": true, "revision": rev})),
                Err(e) => err_json(e.to_string()),
            }
        }
        "undo" => match { let Shared { doc, undo, .. } = &mut *sh; undo.undo(doc) } {
            Some(l) => {
                sh.gui_notify.fetch_add(1, Ordering::Relaxed);
                ok_json(serde_json::json!({"undone": l, "revision": sh.doc.revision()}))
            }
            None => err_json("nothing to undo"),
        },
        "redo" => match { let Shared { doc, undo, .. } = &mut *sh; undo.redo(doc) } {
            Some(l) => {
                sh.gui_notify.fetch_add(1, Ordering::Relaxed);
                ok_json(serde_json::json!({"redone": l, "revision": sh.doc.revision()}))
            }
            None => err_json("nothing to redo"),
        },
        "save" => {
            let path = args["path"]
                .as_str()
                .map(PathBuf::from)
                .or_else(|| sh.path.clone());
            match path {
                Some(p) => {
                    let bytes = sh.doc.serialize(smf_core::WriteOptions {
                        running_status: false,
                    });
                    match std::fs::write(&p, bytes) {
                        Ok(_) => {
                            sh.saved_revision = sh.doc.revision();
                            ok_json(serde_json::json!({"saved": p.to_string_lossy(), "revision": sh.saved_revision}))
                        }
                        Err(e) => err_json(e.to_string()),
                    }
                }
                None => err_json("no path — pass one or open a file in the editor"),
            }
        }
        _ => err_json(format!("unknown tool '{name}'")),
    }
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

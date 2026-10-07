//! MCP tool surface: schemas, dispatch, and document-side apply helpers.
//!
//! Every mutation funnels through `Document::apply(Transaction)` on the
//! `SharedDoc` — the same path GUI edits take — so undo stays unified.
//! `dispatch` is the single entry for all tool calls; `tool_specs` is the
//! versioned schema snapshot the `mcp-schema` test pins. Argument/region
//! helpers and the staged-batch apply path live here too.

use super::*;

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
pub(crate) const MAX_HEX_BYTES: usize = 1 << 20; // 1 MiB decoded
/// Cap on the number of events one apply_patch op may insert.
const MAX_INSERT_EVENTS: usize = 10_000;
/// Cap on the number of rows a read tool may return in one call.
const MAX_QUERY_LIMIT: usize = 10_000;
/// Cap on the ops array of one apply_patch call — clients paginate large
/// edits instead of one request forcing an unbounded build pass.
pub(crate) const MAX_PATCH_OPS: usize = 1_000;
/// Cap on diagnostics rows returned in one call; the full count is still
/// reported so callers know there is more.
const MAX_DIAG_RESULTS: usize = 500;
/// One HTTP request body may not exceed this — an over-large JSON-RPC post
/// must be rejected before it allocates.
pub(crate) const MAX_HTTP_BODY_BYTES: usize = 4 << 20; // 4 MiB
/// In-flight MCP requests are bounded; excess gets an immediate 429 rather
/// than queueing unboundedly behind the document lock.
pub(crate) const MAX_CONCURRENT_REQUESTS: usize = 16;
/// Time budget for producing a response. The SSE stream's Response object is
/// produced up front, so this bounds time-to-response, not stream lifetime.
pub(crate) const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Contract version of the whole MCP tool surface. Bump on ANY breaking
/// change: renaming/removing a tool, renaming required arguments, or
/// narrowing a response. Additive changes (new tool, new optional arg, new
/// response field) don't require a bump — but the checked-in schema
/// snapshot test still fails on every surface diff, so even additive
/// changes are deliberate.
pub const MCP_SURFACE_VERSION: u32 = 2;

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
pub(crate) fn roots_from_env(var: &str) -> Vec<PathBuf> {
    std::env::var(var)
        .unwrap_or_default()
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .filter_map(|s| std::fs::canonicalize(s).ok())
        .collect()
}

/// Reject Windows-hostile file names before any filesystem touch (#205):
/// NTFS Alternate Data Streams (`song.mid:hidden`), DOS device names
/// (`CON`, `NUL.mid`, `COM1`…), and trailing dots/spaces the Win32 layer
/// silently strips. On `\`-separated targets the colon check applies on
/// every platform a Windows share could be mounted from.
fn validate_file_name(path: &std::path::Path) -> Result<(), String> {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return Ok(()); // no name component (e.g. a root) — containment still applies
    };
    if name.contains(':') {
        return Err(format!(
            "{name:?}: ':' in a file name denotes an NTFS alternate data stream"
        ));
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(format!(
            "{name:?}: trailing dots/spaces are stripped by Windows and cannot be stored"
        ));
    }
    let stem = name.split('.').next().unwrap_or("");
    const DEVICES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d)) {
        return Err(format!("{name:?}: reserved DOS device name"));
    }
    Ok(())
}

/// Canonicalize a write target: an existing file resolves fully (symlinks,
/// junctions, `..` — everything); a new file resolves through its parent so
/// a link inside the parent can't smuggle the write elsewhere.
fn canonical_for_write(path: &std::path::Path) -> Result<PathBuf, String> {
    validate_file_name(path)?;
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
pub(crate) fn authorize_write(
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
        if let Ok(cwd) = std::env::current_dir().and_then(std::fs::canonicalize) {
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

pub(crate) fn hex_to_bytes(s: &str) -> Option<Vec<u8>> {
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !s.len().is_multiple_of(2) || s.len() / 2 > MAX_HEX_BYTES {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

pub(crate) fn bytes_hex(b: &[u8]) -> String {
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
        // release velocity + which wire form closes the note (0x80 vs
        // 0x90-vel0 — identical on the wire, distinct in the file)
        "off_vel": n.off_vel,
        "off_form": if n.off_via_on { "on_vel0" } else { "note_off" },
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
// the Err arm is the wire-level JSON-RPC error payload — it is the
// value being returned, not overhead, so boxing it buys nothing
#[allow(clippy::result_large_err)]
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
        "dirty": sh.is_dirty(),
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
    // a patch's ops are all built against the pre-patch state, so only the
    // FIRST insert per (track, tick) may use the state-reading canonical
    // placement (#194) — later ones at the same spot fall back to the
    // legacy high-seq append, which still sorts after everything merged
    let mut merged_once: std::collections::HashSet<(usize, u64)> = Default::default();
    for op in ops {
        let kind = op.get("op").and_then(|v| v.as_str()).unwrap_or("");
        match kind {
            "insert_note" => {
                let track = track_arg(doc, op)?;
                let key = op["key"].as_u64().unwrap_or(60).clamp(0, 127) as u8;
                let vel = op["vel"].as_u64().unwrap_or(100).clamp(1, 127) as u8;
                let start = op["start"].as_u64().unwrap_or(0);
                let dur = op["dur"].as_u64().unwrap_or(480);
                // a zero-duration note would place NoteOff at the NoteOn's
                // exact (tick, seq) — corrupting pairing (#187)
                if dur == 0 {
                    return Err(PatchError::Msg(
                        "'dur' must be at least 1 tick (zero-duration notes cannot be encoded)"
                            .into(),
                    ));
                }
                let ch = op["channel"].as_u64().unwrap_or(0).clamp(0, 15) as u8;
                // release velocity needs the real 0x80 off form — 0x90v0
                // has no byte to carry it in
                let off_vel = op["off_vel"].as_u64().unwrap_or(0).clamp(0, 127) as u8;
                let off_via_on = op["off_form"].as_str() == Some("on_vel0") && off_vel == 0;
                let on_kind = EventKind::Channel {
                    status: 0x90 | ch,
                    data: [key, vel],
                    len: 2,
                };
                let off_kind = EventKind::Channel {
                    status: (if off_via_on { 0x90 } else { 0x80 }) | ch,
                    data: [key, off_vel],
                    len: 2,
                };
                let off_tick = start.saturating_add(dur);
                let fresh_on = merged_once.insert((track, start));
                let fresh_off = merged_once.insert((track, off_tick));
                if !fresh_on && !fresh_off {
                    // canonical same-tick placement (#194): the release
                    // precedes note-ons at its tick, the attack follows
                    // existing setup
                    let (pair_ops, _) =
                        doc.insert_note_pair_ops(track, start, on_kind, off_tick, off_kind);
                    out.extend(pair_ops);
                } else {
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
                                kind: on_kind,
                            },
                            Event {
                                id: off_id,
                                tick: off_tick,
                                seq: u32::MAX / 2,
                                raw_body: None,
                                kind: off_kind,
                            },
                        ],
                    });
                }
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
                        pos: usize::MAX,
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
                            pos: usize::MAX,
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

pub(crate) fn err_json(msg: impl Into<String>) -> CallToolResponse {
    CallToolResult::error(vec![ContentBlock::text(msg.into())]).into()
}

impl ServerHandler for MidiService {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("midi-editor", env!("BUILD_IDENTITY")))
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
    // v2 edit tools: accept the begin_transaction tx_id to opt into
    // staging. A call without it (or with a stale id) commits directly —
    // a dead session's open batch can no longer absorb it (#185).
    // begin_transaction v2: the response now carries the batch's tx_id
    // (inputs unchanged).
    let begin_spec = |name: &'static str, description: &str, schema: serde_json::Value| ToolSpec {
        name,
        version: 2,
        deprecated: None,
        tool: tool(name, description, schema),
    };
    let edit_spec = |name: &'static str, description: &str, mut schema: serde_json::Value| {
        ToolSpec {
            name,
            version: 2,
            deprecated: None,
            tool: {
                if let Some(props) = schema.get_mut("properties").and_then(|p| p.as_object_mut()) {
                    props.insert(
                    "tx_id".to_string(),
                    serde_json::json!({
                        "type": "integer",
                        "description": "tx_id from begin_transaction — pass to stage this edit inside the open transaction"
                    }),
                );
                }
                tool(name, description, schema)
            },
        }
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
        begin_spec(
            "begin_transaction",
            "Open a named checkpoint: pass the returned tx_id back on every edit you want staged on the private copy (reads see staged state either way) until commit_transaction folds them into ONE undo step or rollback_transaction discards them. Edits without the matching tx_id commit directly. One open batch at a time; ~5min idle auto-rollback. Args: label?.",
            object_schema(serde_json::json!({
                "label": {"type": "string"},
            })),
        ),
        edit_spec(
            "commit_transaction",
            "Fold the open transaction's staged ops into a single undo step labelled with the checkpoint name. dry_run:true validates the merged ops against the committed document without applying and keeps the transaction open. Errors with stale_base when the document changed since begin (concurrent edit) — the batch stays open for rollback/re-plan.",
            object_schema(serde_json::json!({
                "dry_run": {"type": "boolean"},
            })),
        ),
        edit_spec(
            "rollback_transaction",
            "Discard the open transaction. The committed document is left exactly as it was at begin_transaction (byte-for-byte) — staged ops never touched it.",
            object_schema(serde_json::json!({})),
        ),
        edit_spec(
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
            "Import-quality findings over the raw event layer: dangling noteOn, zero-length notes, missing End-of-Track, tempo events outside the conductor track, overlapping (retriggered) noteOns. Each has code/track/tick/event_id + detail. Note pairing is deterministic LIFO per (channel,key); overlapping-noteon marks where that choice was ambiguous.",
            object_schema(serde_json::json!({})),
        ),
        edit_spec(
            "normalize",
            "Resolve import-quality findings as one undo step. Args: codes? (array of diagnostic codes; omitted = fix all). Returns resolved/failed counts. overlapping-noteon is reported but never auto-resolved — ambiguous performance data is preserved.",
            object_schema(serde_json::json!({"codes": {"type": "array", "items": {"type": "string"}}})),
        ),
        edit_spec(
            "apply_patch",
            "Atomic edit as one undo step. Optional base_revision: when given it must match document_summary.revision (optimistic concurrency). \
             dry_run:true returns the op breakdown without applying. ops: insert_note {track,key,vel,start,dur,channel,off_vel?,off_form?} | \
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
        edit_spec(
            "set_meta",
            "Create/update a text meta (0x01-0x0F: text, copyright, track/instrument name, lyric, marker, cue). Args: track? (0), tick, meta_type (1-15), text, id? (event id to overwrite), enc? (utf8|sjis|latin1, default utf8). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"},
                "tick": {"type": "integer"},
                "meta_type": {"type": "integer"},
                "text": {"type": "string"},
                "id": {"type": "integer"},
                "enc": {"type": "string"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "remove_meta",
            "Delete one meta event by id. Args: track, id. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "id": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_key_signature",
            "Set the song key signature (FF59). Args: sf (-7..7, negative = flats), mi (0 major / 1 minor), tick? (0). Optional base_revision.",
            object_schema(serde_json::json!({
                "sf": {"type": "integer"}, "mi": {"type": "integer"},
                "tick": {"type": "integer"},
                "base_revision": {"type": "integer"},
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
        ToolSpec {
            // v2: added the `set_loop` action + start/end args (#130)
            version: 2,
            ..spec(
                "transport",
                "Ask the GUI transport: {action: \"play\"|\"stop\"|\"seek\"|\"set_loop\", tick?, start?, end?}. set_loop sets/clears the explicit loop locators in ticks (omit a bound to clear it). Only works while the app is running.",
            object_schema(serde_json::json!({
                "action": {"type": "string"},
                "tick": {"type": "integer"},
                "start": {"type": "integer"},
                "end": {"type": "integer"},
            })))
        },
        edit_spec(
            "quantize",
            "Snap note onsets to a grid (duration preserved). Args: track? (all when omitted), from?, to?, grid? (ticks, default ppq/4 metrical / one frame SMPTE), strength? (0-100, default 100), channel? (1-16, scope inside a track — Format 0 files). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "grid": {"type": "integer"}, "strength": {"type": "integer"},
                "channel": {"type": "integer", "description": "1-16: only notes on this channel; default all"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "transpose",
            "Shift note pitch. Args: track?, from?, to?, semitones (+/-), channel? (1-16, scope inside a track — Format 0 files). Notes leaving 0..127 are skipped. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "semitones": {"type": "integer"},
                "channel": {"type": "integer", "description": "1-16: only notes on this channel; default all"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "scale_velocity",
            "Multiply note velocities. Args: track?, from?, to?, factor (e.g. 1.2 = +20%), channel? (1-16, scope inside a track — Format 0 files). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "factor": {"type": "number"},
                "channel": {"type": "integer", "description": "1-16: only notes on this channel; default all"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_release_velocity",
            "Set note-OFF (release) velocities. Args: track?, from?, to?, vel (0-127). vel>0 upgrades NoteOn-vel0 offs to real 0x80 note-offs; vel=0 keeps the stored form. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "vel": {"type": "integer"}, "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_channel",
            "Retarget all channel events in range to one channel. Args: track, from?, to?, channel (1-16). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "channel": {"type": "integer"}, "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_program",
            "Program change (with optional bank CC0/CC32) on a track. Args: track, tick, program (0-127), channel? (default track channel), bank_msb?, bank_lsb?. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "tick": {"type": "integer"},
                "program": {"type": "integer"}, "channel": {"type": "integer"},
                "bank_msb": {"type": "integer"}, "bank_lsb": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_cc",
            "Insert controller events. Args: track, channel? (default track channel), points: [{tick, cc, value}] — or scalar {tick, cc, value}. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "channel": {"type": "integer"},
                "tick": {"type": "integer"}, "cc": {"type": "integer"}, "value": {"type": "integer"},
                "points": {"type": "array", "items": {"type": "object"}},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_pitch_bend",
            "Insert a pitch-bend event. Args: track, tick, value (0..16383, 8192=center), channel?. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "tick": {"type": "integer"},
                "value": {"type": "integer"}, "channel": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "get_aftertouch",
                "List aftertouch (channel pressure 0xD0 / poly key pressure 0xA0) events. Args: track?, channel?, kind? (channel|poly, default both), key? (poly only).",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "channel": {"type": "integer"},
                    "kind": {"type": "string"}, "key": {"type": "integer"},
                }))
        ),
        edit_spec(
            "set_channel_pressure",
                "Insert channel-pressure (aftertouch 0xD0) events. Args: track, channel? (default track channel), points: [{tick, value}] — or scalar {tick, value}. Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "channel": {"type": "integer"},
                    "tick": {"type": "integer"}, "value": {"type": "integer"},
                    "points": {"type": "array", "items": {"type": "object"}},
                    "base_revision": {"type": "integer"},
                }))
        ),
        edit_spec(
            "set_poly_pressure",
                "Insert polyphonic key-pressure (aftertouch 0xA0) events. Args: track, channel? (default track channel), key? (default 60), points: [{tick, key, value}] — or scalar {tick, key, value}. Optional base_revision.",
                object_schema(serde_json::json!({
                    "track": {"type": "integer"}, "channel": {"type": "integer"},
                    "tick": {"type": "integer"}, "key": {"type": "integer"},
                    "value": {"type": "integer"},
                    "points": {"type": "array", "items": {"type": "object"}},
                    "base_revision": {"type": "integer"},
                }))
        ),
        spec(
            "get_rpn",
            "Semantic view of RPN/NRPN writes: [{track, channel, kind, param_msb, param_lsb, param14, name, tick, data_msb, data_lsb, value, ids}] — raw CC events are unchanged. Args: track?, channel?, kind? (\"rpn\"|\"nrpn\"), param_msb?, param_lsb?.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "channel": {"type": "integer"},
                "kind": {"type": "string"},
                "param_msb": {"type": "integer"}, "param_lsb": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_rpn",
            "Write a full RPN/NRPN sequence in valid order (selector MSB, selector LSB, data MSB, optional data LSB). Args: track, tick, kind? (\"rpn\"|\"nrpn\", default rpn), param_msb, param_lsb (0x7F/0x7F = null reset, no data written), data_msb, data_lsb?, channel?. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "tick": {"type": "integer"},
                "kind": {"type": "string"},
                "param_msb": {"type": "integer"}, "param_lsb": {"type": "integer"},
                "data_msb": {"type": "integer"}, "data_lsb": {"type": "integer"},
                "channel": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "update_rpn_value",
            "Rewrite the data-entry of an existing RPN/NRPN entry (selector order preserved). Args: id (any event id from get_rpn ids), data_msb, data_lsb? (omit = 7-bit, drops the LSB event). Optional base_revision.",
            object_schema(serde_json::json!({
                "id": {"type": "integer"},
                "data_msb": {"type": "integer"}, "data_lsb": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "update_rpn_param",
            "Retarget an existing RPN/NRPN entry to a new parameter number (selectors rewritten, data preserved). Args: id, param_msb, param_lsb. Optional base_revision.",
            object_schema(serde_json::json!({
                "id": {"type": "integer"},
                "param_msb": {"type": "integer"}, "param_lsb": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        spec(
            "get_instruments",
            "Instrument context: detected synth mode (gm1/gm2/gs/xg from reset SysEx) + every program change with effective bank MSB/LSB and friendly name (GM table; GS/XG drum kits on channel 10). Names are display-only — unknown banks return null names and stay numeric. No args.",
            object_schema(serde_json::json!({})),
        ),
        edit_spec(
            "remove_events",
                "Delete events by id (lane/event-list deletes). Args: ids: [event-id]. Optional base_revision.",
                object_schema(serde_json::json!({
                    "ids": {"type": "array", "items": {"type": "integer"}},
                    "base_revision": {"type": "integer"},
                }))
        ),
        edit_spec(
            "set_tempo",
            "Set/replace tempo at a tick. Args: tick, bpm, track? (default 0 = conductor; for format-2 files pass the sequence's track). Optional base_revision.",
            object_schema(serde_json::json!({
                "tick": {"type": "integer"}, "bpm": {"type": "number"},
                "track": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_time_signature",
            "Set/replace time signature at a tick. Args: tick, num (beats/bar), den (beat value 4=quarter,8=eighth), track? (default 0; pass the sequence's track for format-2). Optional base_revision.",
            object_schema(serde_json::json!({
                "tick": {"type": "integer"}, "num": {"type": "integer"}, "den": {"type": "integer"},
                "track": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_track_channel",
            "Set the track's channel-prefix meta (FF 20): a hint players/editors may honor, NOT a reroute — per-event channels rule playback. To retarget existing events use set_channel. Args: track, channel (1-16). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "channel": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_track_name",
            "Set track name (meta 0x03). Written in the file's charset by default (UTF-8 unless the file carries a Shift-JIS hint); pass enc to force one. Args: track, name, enc? (utf8|sjis|latin1). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "name": {"type": "string"},
                "enc": {"type": "string", "enum": ["utf8", "sjis", "latin1"],
                        "description": "Write encoding; default = file's own hint, else UTF-8"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "add_track",
            "Append a track (with optional name, encoded like set_track_name). Args: name?, enc? (utf8|sjis|latin1). Optional base_revision.",
            object_schema(serde_json::json!({
                "name": {"type": "string"},
                "enc": {"type": "string", "enum": ["utf8", "sjis", "latin1"],
                        "description": "Write encoding for the name; default = file's own hint, else UTF-8"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "remove_track",
            "Remove a track entirely. Args: track. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "delete_range",
            "Delete channel events in [from,to) (notes delete whole). Args: track, from, to, channel? (1-16, scope inside a track — Format 0 files). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "channel": {"type": "integer", "description": "1-16: only this channel's events; default all"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "duplicate_range",
            "Copy channel events in [from,to) to start at `to`. Args: track, from, to. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "split",
            "Split every note spanning `at` into two at that tick. Args: track?, from?, to? (scope = notes starting in range), at (tick, required). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "at": {"type": "integer"}, "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "join_notes",
            "Merge runs of same-pitch+channel notes that overlap or touch: the earliest NoteOn survives, the NoteOff moves to the run's end, interior events are removed. Args: track?, from?, to?. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "fix_overlaps",
            "Shorten notes overlapping the next same-pitch+channel note so they end at its start; event ids are preserved. Args: track?, from?, to?. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "legato",
            "Extend each note's end toward the next same-pitch+channel note's start. Args: track?, from?, to?, gap? (ticks; 0 = touch, >0 leaves a gap, <0 overlaps). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "gap": {"type": "integer"}, "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "set_length",
            "Set every note starting in range to exactly `ticks` long. Args: track?, from?, to?, ticks (required). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "ticks": {"type": "integer"}, "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "swing",
            "Swing: shift notes landing on odd `grid` cells later by `amount`% of a cell. Args: track?, from?, to?, grid (ticks, required), amount 0-100, dry_run? (preview summary without applying). Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "grid": {"type": "integer"}, "amount": {"type": "integer"},
                "dry_run": {"type": "boolean"}, "base_revision": {"type": "integer"},
            })),
        ),
        edit_spec(
            "humanize",
            "Deterministic seeded jitter on note starts and velocities. Args: track?, from?, to?, timing (max |tick|), vel (max |dv|), seed (integer; same seed+settings = identical output, recorded in the tx label), dry_run?. Optional base_revision.",
            object_schema(serde_json::json!({
                "track": {"type": "integer"}, "from": {"type": "integer"}, "to": {"type": "integer"},
                "timing": {"type": "integer"}, "vel": {"type": "integer"},
                "seed": {"type": "integer"},
                "dry_run": {"type": "boolean"}, "base_revision": {"type": "integer"},
            })),
        ),
    ]
    .into_iter()
    .map(|mut t| {
        // v3: optional `enc` write-encoding parameter (file-hint default,
        // #176) and optional `channel` scope on region ops (#195) —
        // additive, so only the affected tools bump
        if matches!(
            t.name,
            "set_track_name"
                | "add_track"
                | "quantize"
                | "transpose"
                | "scale_velocity"
                | "delete_range"
        ) {
            t.version = 3;
        }
        t
    })
    .collect()
}

/// Dispatch one MCP tool call by name, the same path `call_tool` takes.
/// `#[doc(hidden)]`: exposed for the workspace bench crate, not public API.
#[doc(hidden)]
pub fn dispatch(name: &str, args: &serde_json::Value, shared: SharedDoc) -> CallToolResponse {
    // recover from a poisoned lock: a panic in an earlier critical section
    // must not take down every later request
    let mut sh = shared.lock().unwrap_or_else(|e| e.into_inner());
    sh.expire_batch();
    // per-call staging intent (#185): an edit carrying the open batch's
    // tx_id stages; a standalone edit (or a stale session's id) commits
    // directly and can no longer be absorbed by a dead client's batch.
    // No reset afterwards — `call_tx_id` is only consulted inside a
    // dispatch (the save arm below drops the guard entirely).
    sh.call_tx_id = args["tx_id"].as_u64();
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
                Ok((base, tx_id)) => {
                    let b = sh.batch.as_ref().unwrap();
                    ok_json(serde_json::json!({
                        "open": true, "label": b.label, "base_revision": base,
                        "tx_id": tx_id,
                        "hint": "pass tx_id back on every edit you want staged in this transaction",
                        "ttl_seconds": BATCH_TTL.as_secs(),
                    }))
                }
                Err(r) => r,
            }
        }
        "commit_transaction" => {
            if let Some(r) = tx_guard(&sh, args) {
                return r;
            }
            let dry = args["dry_run"].as_bool().unwrap_or(false);
            match sh.commit_batch(dry) {
                Ok(v) => ok_json(v),
                Err(r) => r,
            }
        }
        "rollback_transaction" => {
            if let Some(r) = tx_guard(&sh, args) {
                return r;
            }
            match sh.batch.take() {
                Some(b) => ok_json(serde_json::json!({
                    "rolled_back": true, "label": b.label,
                    "discarded_ops": b.ops.len(),
                })),
                None => err_json("no open transaction"),
            }
        }
        "transaction_status" => {
            if let Some(r) = tx_guard(&sh, args) {
                return r;
            }
            match &sh.batch {
                Some(b) => ok_json(serde_json::json!({
                    "open": true,
                    "label": b.label,
                    "tx_id": b.tx_id,
                    "base_revision": b.base,
                    "staged_ops": b.ops.len(),
                    "staged_revision": b.staging.revision(),
                    "age_s": b.last_activity.elapsed().as_secs(),
                    "expires_in_s": BATCH_TTL.saturating_sub(b.last_activity.elapsed()).as_secs(),
                })),
                None => ok_json(serde_json::json!({"open": false})),
            }
        }
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
            let fields = field_projection(args);
            let after = match cursor_arg(&sh, args, 4) {
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
            let fields = field_projection(args);
            let after = match cursor_arg(&sh, args, 4) {
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
                        Op::SetFormat { before, after } => serde_json::json!({
                            "op": "set_format", "before": before, "after": after}),
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
            // unified undo (#204): may revert a document transaction or a
            // session change (GUI routing/mute/solo), whichever is newest
            match sh.undo_any() {
                Some((session, l)) => {
                    if session {
                        sh.gui_notify.fetch_add(1, Ordering::Relaxed);
                        ok_json(serde_json::json!({
                            "undone": l, "kind": "session", "revision": sh.doc.revision(),
                        }))
                    } else {
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
            match sh.redo_any() {
                Some((session, l)) => {
                    if session {
                        sh.gui_notify.fetch_add(1, Ordering::Relaxed);
                        ok_json(serde_json::json!({
                            "redone": l, "kind": "session", "revision": sh.doc.revision(),
                        }))
                    } else {
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
            let cur = sh.view().revision();
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
            let point = |(tick, mpq, cum): &(u64, u32, u64)| {
                serde_json::json!({
                    "tick": tick, "us_per_quarter": mpq,
                    "bpm": (60_000_000.0 / *mpq as f64 * 100.0).round() / 100.0,
                    "cumulative_us": cum,
                })
            };
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
                    // None for SMPTE: there is no quarter note — use the
                    // reported ticks_per_second (exact 30000/1001 rate for
                    // the -29 drop-frame division) for tick<->time math
                    "ppq": tm.ppq(),
                    "fps": fps,
                    "ticks_per_frame": tpf,
                    "ticks_per_second": fps.map(|_| {
                        document::TimeDisplay::of(sh.view().division).ticks_per_second()
                    }),
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
            let fields = field_projection(args);
            let after = match cursor_arg(&sh, args, 4) {
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
                        serde_json::json!({                            "track": ti, "id": e.id, "tick": e.tick,
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
            let fields = field_projection(args);
            let after = match cursor_arg(&sh, args, 3) {
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
        "get_aftertouch" => {
            let track = args["track"].as_u64().map(|v| v as usize);
            let chan = args["channel"].as_u64().map(|v| (v.clamp(1, 16) - 1) as u8);
            let kind = args["kind"].as_str();
            let key = args["key"].as_u64().map(|v| v as u8);
            let mut out = Vec::new();
            for (ti, t) in sh.view().tracks.iter().enumerate() {
                if track.is_some() && track != Some(ti) {
                    continue;
                }
                for e in &t.events {
                    if let EventKind::Channel { status, data, .. } = &e.kind {
                        let (st, ch) = (status & 0xF0, status & 0x0F);
                        if chan.is_some() && chan != Some(ch) {
                            continue;
                        }
                        let row = match st {
                            0xD0 if kind.is_none() || kind == Some("channel") => {
                                serde_json::json!({
                                    "track": ti, "id": e.id, "tick": e.tick,
                                    "kind": "channel", "channel": ch + 1,
                                    "value": data[0],
                                })
                            }
                            0xA0 if (kind.is_none() || kind == Some("poly"))
                                && key.is_none_or(|k| k == data[0]) =>
                            {
                                serde_json::json!({
                                    "track": ti, "id": e.id, "tick": e.tick,
                                    "kind": "poly", "channel": ch + 1,
                                    "key": data[0], "value": data[1],
                                })
                            }
                            _ => continue,
                        };
                        out.push(row);
                    }
                }
            }
            ok_json(serde_json::json!({"count": out.len(), "aftertouch": out}))
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
                .map(|p| serde_json::json!({"index": p.index, "name": p.name, "ord": p.ord}))
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
                        ord: d["ord"].as_u64().unwrap_or(0) as usize,
                    }
                } else if let Some(p) = d["vst3"].as_str() {
                    Destination::Plugin {
                        plugin_path: p.to_string(),
                        instance: None,
                        component_id: None,
                        vendor: None,
                        plugin_name: None,
                    }
                } else {
                    return err_json(
                        "destination must be \"default\", {\"midi_port\": name} or {\"vst3\": path}",
                    );
                };
                // adopt catalog metadata (component id, vendor, name) when the
                // path is a known scan result so stored routing is durable
                let catalog: Vec<Destination> = sh.dests.iter().map(|(_, d)| d.clone()).collect();
                let (dest, _) = midi_io::resolve_plugin_dest(&dest, &catalog);
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
                // explicit loop locators in ticks (#130); omit a bound to
                // clear it — `end` must exceed `start` to take effect
                Some("set_loop") => Some(TransportReq::SetLoop {
                    start: args["start"].as_u64(),
                    end: args["end"].as_u64(),
                }),
                _ => None,
            };
            match req {
                Some(r) => {
                    sh.transport_req.push(r);
                    sh.gui_notify.fetch_add(1, Ordering::Relaxed);
                    ok_json(serde_json::json!({"queued": true}))
                }
                None => err_json("action must be play|stop|seek|set_loop"),
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
            // optional channel scope — the only way to target one part of
            // a Format 0 file without splitting it (#195)
            let channel = args["channel"].as_u64().map(|c| (c.clamp(1, 16) - 1) as u8);
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(
                    sh.view_mut()
                        .quantize_ops(t, from, to, grid, strength, channel),
                );
            }
            apply_ops(&mut sh, "quantize", ops)
        }
        "transpose" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let st = args["semitones"].as_i64().unwrap_or(0) as i32;
            let channel = args["channel"].as_u64().map(|c| (c.clamp(1, 16) - 1) as u8);
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().transpose_ops(t, from, to, st, channel));
            }
            apply_ops(&mut sh, "transpose", ops)
        }
        "scale_velocity" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let f = args["factor"].as_f64().unwrap_or(1.0);
            let channel = args["channel"].as_u64().map(|c| (c.clamp(1, 16) - 1) as u8);
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().scale_velocity_ops(t, from, to, f, channel));
            }
            apply_ops(&mut sh, "scale velocity", ops)
        }
        "set_release_velocity" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let vel = args["vel"].as_u64().unwrap_or(0).clamp(0, 127) as u8;
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().set_release_velocity_ops(t, from, to, vel));
            }
            apply_ops(&mut sh, "set release velocity", ops)
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
        "set_channel_pressure" => {
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
                    ops.extend(sh.view_mut().set_channel_pressure_ops(
                        track,
                        p["tick"].as_u64().unwrap_or(0),
                        ch,
                        p["value"].as_u64().unwrap_or(0) as u8,
                    ));
                }
            } else {
                ops.extend(sh.view_mut().set_channel_pressure_ops(
                    track,
                    args["tick"].as_u64().unwrap_or(0),
                    ch,
                    args["value"].as_u64().unwrap_or(0) as u8,
                ));
            }
            apply_ops(&mut sh, "channel pressure", ops)
        }
        "set_poly_pressure" => {
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
            let emit = |doc: &mut Document, p: &serde_json::Value| {
                doc.set_poly_pressure_ops(
                    track,
                    p["tick"].as_u64().unwrap_or(0),
                    ch,
                    p["key"].as_u64().unwrap_or(60) as u8,
                    p["value"].as_u64().unwrap_or(0) as u8,
                )
            };
            if let Some(points) = args["points"].as_array() {
                for p in points {
                    ops.extend(emit(sh.view_mut(), p));
                }
            } else {
                ops.extend(emit(sh.view_mut(), args));
            }
            apply_ops(&mut sh, "poly pressure", ops)
        }
        "remove_events" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let ids: Vec<EventId> = args["ids"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_u64()).collect())
                .unwrap_or_default();
            if ids.is_empty() {
                return err_json("ids required");
            }
            let ops = sh.view_mut().remove_events_ops(&ids);
            if ops.is_empty() {
                return err_json("no matching events");
            }
            apply_ops(&mut sh, "remove events", ops)
        }
        "get_rpn" => {
            let track = args["track"].as_u64().map(|v| v as usize);
            let chan = args["channel"].as_u64().map(|v| (v.clamp(1, 16) - 1) as u8);
            let kind = args["kind"].as_str();
            let pm = args["param_msb"].as_u64().map(|v| v as u8);
            let pl = args["param_lsb"].as_u64().map(|v| v as u8);
            let entries: Vec<_> = sh
                .doc
                .rpn_entries()
                .into_iter()
                .filter(|e| {
                    track.is_none_or(|t| t == e.track)
                        && chan.is_none_or(|c| c == e.channel)
                        && kind.is_none_or(|k| (k == "nrpn") == e.nrpn)
                        && pm.is_none_or(|v| v == e.param_msb)
                        && pl.is_none_or(|v| v == e.param_lsb)
                })
                .collect();
            ok_json(serde_json::json!({
                "count": entries.len(),
                "entries": entries.iter().map(|e| serde_json::json!({
                    "track": e.track, "channel": e.channel + 1,
                    "kind": if e.nrpn { "nrpn" } else { "rpn" },
                    "param_msb": e.param_msb, "param_lsb": e.param_lsb,
                    "param14": e.param14(),
                    "name": e.param_name(),
                    "null": e.is_null(),
                    "tick": e.tick,
                    "data_msb": e.data_msb, "data_lsb": e.data_lsb,
                    "value": e.value(), "ids": e.ids(),
                })).collect::<Vec<_>>(),
            }))
        }
        "set_rpn" => {
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
            let (Some(pm), Some(pl), Some(dm)) = (
                args["param_msb"].as_u64(),
                args["param_lsb"].as_u64(),
                args["data_msb"].as_u64(),
            ) else {
                return err_json("param_msb, param_lsb, data_msb required");
            };
            let ops = sh.view_mut().set_rpn_ops(
                track,
                args["tick"].as_u64().unwrap_or(0),
                ch,
                args["kind"].as_str() == Some("nrpn"),
                pm as u8,
                pl as u8,
                dm as u8,
                args["data_lsb"].as_u64().map(|v| v as u8),
            );
            apply_ops(&mut sh, "set rpn", ops)
        }
        "update_rpn_value" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let id = match args["id"].as_u64() {
                Some(i) => i,
                None => return err_json("id required"),
            };
            let Some(dm) = args["data_msb"].as_u64() else {
                return err_json("data_msb required");
            };
            let Some(entry) = sh.view().rpn_entry_containing(id) else {
                return err_json(format!("no RPN/NRPN entry contains event {id}"));
            };
            let ops = sh.view_mut().update_rpn_value_ops(
                &entry,
                dm as u8,
                args["data_lsb"].as_u64().map(|v| v as u8),
            );
            apply_ops(&mut sh, "update rpn value", ops)
        }
        "update_rpn_param" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let id = match args["id"].as_u64() {
                Some(i) => i,
                None => return err_json("id required"),
            };
            let (Some(pm), Some(pl)) = (args["param_msb"].as_u64(), args["param_lsb"].as_u64())
            else {
                return err_json("param_msb, param_lsb required");
            };
            let Some(entry) = sh.view().rpn_entry_containing(id) else {
                return err_json(format!("no RPN/NRPN entry contains event {id}"));
            };
            let ops = sh
                .view_mut()
                .update_rpn_param_ops(&entry, pm as u8, pl as u8);
            apply_ops(&mut sh, "update rpn param", ops)
        }
        "get_instruments" => {
            let mode = sh.view().synth_mode();
            let pcs = sh.view().program_changes();
            ok_json(serde_json::json!({
                "mode": mode.map(|m| m.label()),
                "count": pcs.len(),
                "programs": pcs.iter().map(|p| serde_json::json!({
                    "track": p.track, "channel": p.channel + 1, "tick": p.tick,
                    "bank_msb": p.bank_msb, "bank_lsb": p.bank_lsb,
                    "program": p.program,
                    "name": sh.view().program_name(p),
                    "id": p.id,
                })).collect::<Vec<_>>(),
            }))
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
        "set_meta" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let track = match req_track(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let meta_type = args["meta_type"].as_u64().unwrap_or(0x06) as u8;
            if !(0x01..=0x0f).contains(&meta_type) {
                return err_json("meta_type must be a text type (0x01-0x0F)");
            }
            let enc = match args["enc"].as_str().unwrap_or("utf8") {
                "sjis" | "shiftjis" | "shift_jis" => smf_core::TextEncoding::ShiftJis,
                "latin1" | "latin-1" => smf_core::TextEncoding::Latin1,
                _ => smf_core::TextEncoding::Utf8,
            };
            let ops = sh.view_mut().set_meta_text_ops(
                track,
                args["tick"].as_u64().unwrap_or(0),
                meta_type,
                args["id"].as_u64().unwrap_or(0),
                args["text"].as_str().unwrap_or(""),
                Some(enc),
            );
            apply_ops(&mut sh, "set meta", ops)
        }
        "remove_meta" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let track = match req_track(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let ops = sh
                .doc
                .remove_meta_ops(track, args["id"].as_u64().unwrap_or(0));
            apply_ops(&mut sh, "remove meta", ops)
        }
        "set_key_signature" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let ops = sh.view_mut().set_key_sig_ops(
                args["tick"].as_u64().unwrap_or(0),
                args["sf"].as_i64().unwrap_or(0).clamp(-7, 7) as i8,
                args["mi"].as_u64().unwrap_or(0).min(1) as u8,
            );
            apply_ops(&mut sh, "set key signature", ops)
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
            // encoding: an explicit `enc` wins; absent, the file's own
            // charset hint (XF "JP" marker) keeps legacy files consistent —
            // UTF-8 is the last resort, never a silent rewrite (#176)
            let enc = match args["enc"].as_str() {
                Some("utf8") => Some(smf_core::TextEncoding::Utf8),
                Some("sjis") | Some("shiftjis") | Some("shift_jis") => {
                    Some(smf_core::TextEncoding::ShiftJis)
                }
                Some("latin1") | Some("latin-1") => Some(smf_core::TextEncoding::Latin1),
                _ => sh.doc.text_encoding_hint(),
            };
            let ops =
                sh.view_mut()
                    .set_track_name_ops(track, args["name"].as_str().unwrap_or(""), enc);
            apply_ops(&mut sh, "set track name", ops)
        }
        "add_track" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let enc = match args["enc"].as_str() {
                Some("utf8") => Some(smf_core::TextEncoding::Utf8),
                Some("sjis") | Some("shiftjis") | Some("shift_jis") => {
                    Some(smf_core::TextEncoding::ShiftJis)
                }
                Some("latin1") | Some("latin-1") => Some(smf_core::TextEncoding::Latin1),
                _ => sh.doc.text_encoding_hint(),
            };
            let ops = sh.view_mut().add_track_ops(args["name"].as_str(), enc);
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
            // optional channel scope (#195): one channel's events only —
            // notes still delete whole (their off rides along)
            let ops = match args["channel"].as_u64() {
                Some(c) => {
                    let nib = (c.clamp(1, 16) - 1) as u8;
                    sh.view_mut().delete_range_channel_ops(
                        track,
                        from,
                        to,
                        &std::collections::BTreeSet::from([nib]),
                    )
                }
                None => sh.view_mut().delete_range_ops(track, from, to),
            };
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
        "split" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let Some(at) = args["at"].as_u64() else {
                return err_json("split requires 'at' (tick)");
            };
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().split_ops(t, from, to, at));
            }
            apply_ops(&mut sh, "split", ops)
        }
        "join_notes" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().join_ops(t, from, to));
            }
            apply_ops(&mut sh, "join notes", ops)
        }
        "fix_overlaps" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().fix_overlaps_ops(t, from, to));
            }
            apply_ops(&mut sh, "fix overlaps", ops)
        }
        "legato" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let gap = args["gap"].as_i64().unwrap_or(0);
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().legato_ops(t, from, to, gap));
            }
            apply_ops(&mut sh, "legato", ops)
        }
        "set_length" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let Some(ticks) = args["ticks"].as_u64() else {
                return err_json("set_length requires 'ticks'");
            };
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().set_length_ops(t, from, to, ticks));
            }
            apply_ops(&mut sh, "set length", ops)
        }
        "swing" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let Some(grid) = args["grid"].as_u64() else {
                return err_json("swing requires 'grid' (ticks)");
            };
            let amount = args["amount"].as_u64().unwrap_or(50) as u32;
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().swing_ops(t, from, to, grid, amount));
            }
            dry_or_apply(
                &mut sh,
                &format!("swing {amount}% grid={grid}"),
                ops,
                args["dry_run"].as_bool().unwrap_or(false),
            )
        }
        "humanize" => {
            if let Some(r) = check_base(&sh, args) {
                return r;
            }
            let (from, to) = region(args);
            let timing = args["timing"].as_i64().unwrap_or(12);
            let vel = args["vel"].as_i64().unwrap_or(8) as i32;
            let seed = args["seed"].as_u64().unwrap_or(0);
            let tracks = match sel_tracks(&sh, args) {
                Ok(t) => t,
                Err(r) => return r,
            };
            let mut ops = Vec::new();
            for t in tracks {
                ops.extend(sh.view_mut().humanize_ops(t, from, to, timing, vel, seed));
            }
            // seed rides in the label so the history/undo entry is
            // self-describing and reproducible
            dry_or_apply(
                &mut sh,
                &format!("humanize seed={seed} timing={timing} vel={vel}"),
                ops,
                args["dry_run"].as_bool().unwrap_or(false),
            )
        }
        _ => err_json(format!("unknown tool '{name}'")),
    }
}

/// A transaction-lifecycle call carrying a `tx_id` that doesn't match the
/// open batch is another session poking at our checkpoint — refuse it
/// instead of committing/rolling back someone else's staging area (#185).
fn tx_guard(sh: &Shared, args: &serde_json::Value) -> Option<CallToolResponse> {
    match (args["tx_id"].as_u64(), sh.batch.as_ref()) {
        (Some(want), Some(b)) if want != b.tx_id => Some(err_json(
            serde_json::json!({
                "error": "wrong_transaction",
                "given_tx_id": want,
                "open_tx_id": b.tx_id,
                "hint": "this batch belongs to another session; use your own begin_transaction tx_id",
            })
            .to_string(),
        )),
        _ => None,
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

/// `dry_run` reports what would change without touching the document.
fn dry_or_apply(sh: &mut Shared, label: &str, ops: Vec<Op>, dry_run: bool) -> CallToolResponse {
    if dry_run {
        return ok_json(serde_json::json!({
            "dry_run": true,
            "label": label,
            "ops": ops.len(),
        }));
    }
    apply_ops(sh, label, ops)
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
        Destination::MidiPort { port_name, ord } => {
            // same-name sibling devices get a visible discriminator
            if *ord == 0 {
                port_name.clone()
            } else {
                format!("{port_name} #{}", ord + 1)
            }
        }
        Destination::Plugin { plugin_path, .. } => {
            format!("{} [VST3]", plugin_path)
        }
    }
}

fn dest_json(d: &Destination) -> serde_json::Value {
    match d {
        Destination::MidiPort { port_name, ord } => {
            serde_json::json!({"kind": "midi_port", "port_name": port_name, "ord": ord})
        }
        Destination::Plugin { plugin_path, .. } => {
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
            // offline destinations stay listed so assignments survive a
            // temporary unplug — `available` marks what is live now
            j["available"] = match d {
                Destination::MidiPort { port_name, ord } => {
                    sh.port_present.contains(&(port_name.clone(), *ord)).into()
                }
                Destination::Plugin { .. } => true.into(),
            };
            j
        }).collect::<Vec<_>>(),
        "default_dest": sh.default_dest,
        "track_dest": sh.track_dest.iter().map(|(t, d)| (*t, *d)).collect::<HashMap<usize, usize>>(),
        "muted": sh.muted.iter().copied().collect::<Vec<_>>(),
        "soloed": sh.soloed.iter().copied().collect::<Vec<_>>(),
        "metronome": sh.metronome,
        "loop_enabled": sh.loop_enabled,
        "loop_start": sh.loop_start,
        "loop_end": sh.loop_end,
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

#[cfg(test)]
mod write_validation_tests {
    use super::canonical_for_write;

    fn err(path: &str) -> String {
        canonical_for_write(std::path::Path::new(path)).unwrap_err()
    }

    #[test]
    fn alternate_data_streams_rejected() {
        for p in [
            r"C:\music\song.mid:hidden",
            r"C:\music\song.mid::$DATA",
            "/tmp/song.mid:stream",
        ] {
            let e = err(p);
            assert!(e.contains(':'), "{p} must be rejected as ADS: {e}");
        }
    }

    #[test]
    fn dos_device_names_rejected() {
        for p in [
            r"C:\music\CON",
            r"C:\music\con.mid",
            r"C:\music\NUL.mid",
            r"C:\music\com1",
            r"C:\music\LPT9.mid",
        ] {
            assert!(err(p).contains("device"), "{p} must be rejected");
        }
    }

    #[test]
    fn trailing_dots_and_spaces_rejected() {
        assert!(err(r"C:\music\song.mid.").contains("trailing"));
        assert!(err(r#"C:\music\song.mid "#).contains("trailing"));
    }

    #[test]
    fn ordinary_names_pass_validation() {
        // validation runs before any fs touch, so a nonexistent-but-legal
        // path fails only at the parent-canonicalize step, with a
        // different (resolution) error — not a validation error
        let e = canonical_for_write(std::path::Path::new(r"C:\no\such\dir\song.mid")).unwrap_err();
        assert!(!e.contains(':') || e.contains("cannot resolve"));
        assert!(!e.contains("device"));
        assert!(!e.contains("trailing"));
    }
}

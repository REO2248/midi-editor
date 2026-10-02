//! Editing gestures and the event-properties inspector: erase/cut/
//! copy/paste/duplicate, drag commit, nudges, region ops (quantize/
//! humanize/legato…), meta editing, and per-field prop edits. Every gesture
//! ends in `apply_tx` so GUI, palette and MCP edits share one undo path.

use super::*;

/// One captured clipboard event — tick re-anchored at the copy's earliest
/// tick so paste drops the material at the edit cursor. The raw SMF body
/// is kept: a copied event never loses bytes its kind doesn't model (#142).
#[derive(Clone)]
pub(crate) struct ClipEvent {
    pub(crate) dtick: u64,
    /// same-tick ordering position carried from the source file
    pub(crate) seq: u32,
    pub(crate) track: usize,
    pub(crate) raw_body: Option<bytes::Bytes>,
    pub(crate) kind: EventKind,
}

/// The OS-clipboard payload (#142): any event kinds, serialized to a
/// format-1 SMF fragment — one fragment track per source track so every
/// event's track identity survives the trip through another process
/// (#143). `src` is Some(t) only when every event came from one track — a
/// single-track copy pastes onto the active track, a multi-track copy
/// onto its own tracks.
#[derive(Clone)]
pub(crate) struct Clip {
    pub(crate) events: Vec<ClipEvent>,
    pub(crate) src: Option<usize>,
    /// division the dticks are expressed in — converted on paste when the
    /// target document divides differently
    pub(crate) division: smf_core::Division,
}

/// Clipboard metadata format version tag — bump when the payload shape
/// changes so stale payloads fail closed (#142). v2 packs one fragment
/// track per source track plus a `tracks`/`eots` table; v1's flattened
/// format-0 payload can't carry track identity and is rejected.
const CLIP_FMT: &str = "midi-editor/smf-clip/2";

fn is_eot_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Meta {
            meta_type: 0x2F,
            ..
        }
    )
}

/// hex encode (no base64 dep) — used for the SMF fragment in clipboard
/// metadata JSON.
fn hex_enc(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

fn hex_dec(s: &str) -> Option<Vec<u8>> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(2) {
        return None;
    }
    let v = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    (0..b.len())
        .step_by(2)
        .map(|i| Some(v(b[i])? * 16 + v(b[i + 1])?))
        .collect()
}

/// Serialize a captured selection to the OS-clipboard item: a readable
/// one-line summary as the text flavor (harmless in other apps) plus a
/// format-1 SMF fragment in the private metadata flavor — one fragment
/// track per distinct source track so the paste can restore each event's
/// original track identity (#142/#143).
pub(crate) fn clip_to_item(clip: &Clip) -> ClipboardItem {
    // fragment layout: one track per distinct source track, ascending.
    // `tracks[i]` maps fragment track i back to the original track index;
    // `eots[i]` counts the End-of-Track events that were actually
    // SELECTED on it — write() mints a terminator for any fragment track
    // lacking one, and that synthetic event must not leak into the paste
    let mut order: Vec<usize> = clip.events.iter().map(|e| e.track).collect();
    order.sort_unstable();
    order.dedup();
    let frag: Vec<smf_core::Track> = order
        .iter()
        .map(|&ot| smf_core::Track {
            events: clip
                .events
                .iter()
                .filter(|e| e.track == ot)
                .map(|e| smf_core::Event {
                    tick: e.dtick,
                    seq: e.seq,
                    raw_body: e.raw_body.clone(),
                    kind: e.kind.clone(),
                })
                .collect(),
        })
        .collect();
    let eots: Vec<usize> = order
        .iter()
        .map(|&ot| {
            clip.events
                .iter()
                .filter(|e| e.track == ot && is_eot_kind(&e.kind))
                .count()
        })
        .collect();
    let smf = smf_core::write(1, clip.division, &frag, smf_core::WriteOptions::default());
    let meta = serde_json::json!({
        "v": 2,
        "fmt": CLIP_FMT,
        "div": match clip.division {
            smf_core::Division::Metrical(ppq) => serde_json::json!(ppq),
            smf_core::Division::Smpte { fps, ticks_per_frame } =>
                serde_json::json!({"smpte": [fps, ticks_per_frame]}),
        },
        "tracks": order,
        "eots": eots,
        "smf": hex_enc(&smf),
    })
    .to_string();
    ClipboardItem::new_string_with_metadata(
        format!("midi-editor: {} events", clip.events.len()),
        meta,
    )
}

/// Parse the private clipboard flavor back into a `Clip`. Returns `None`
/// for foreign clipboard contents (no metadata or wrong format tag) so
/// paste falls through to the in-process clipboard. Each fragment track's
/// `tracks[i]` entry restores the events' original track index, and
/// `eots[i]` keeps only the End-of-Track events the selection actually
/// carried — the terminator write() synthesizes for a track without one
/// sorts last by (tick,seq) and is dropped here (#142/#143).
pub(crate) fn clip_from_item(item: &ClipboardItem) -> Option<Clip> {
    let meta = item.entries.iter().find_map(|e| match e {
        ClipboardEntry::String(cs) => cs.metadata.clone(),
        _ => None,
    })?;
    let v: serde_json::Value = serde_json::from_str(&meta).ok()?;
    if v.get("fmt").and_then(|f| f.as_str()) != Some(CLIP_FMT) {
        return None;
    }
    let tracks: Vec<usize> = v
        .get("tracks")?
        .as_array()?
        .iter()
        .map(|x| x.as_u64().map(|n| n as usize))
        .collect::<Option<_>>()?;
    let eots: Vec<usize> = v
        .get("eots")?
        .as_array()?
        .iter()
        .map(|x| x.as_u64().unwrap_or(0) as usize)
        .collect();
    let smf = hex_dec(v.get("smf")?.as_str()?)?;
    let f = smf_core::parse_lenient(&smf).ok()?;
    let division = match v.get("div") {
        Some(serde_json::Value::Number(n)) => {
            smf_core::Division::Metrical(n.as_u64().unwrap_or(480) as u16)
        }
        Some(serde_json::Value::Object(o)) => match o.get("smpte") {
            Some(serde_json::Value::Array(a)) if a.len() == 2 => smf_core::Division::Smpte {
                fps: a[0].as_u64().unwrap_or(25) as u8,
                ticks_per_frame: a[1].as_u64().unwrap_or(40) as u8,
            },
            _ => smf_core::Division::Metrical(480),
        },
        _ => smf_core::Division::Metrical(480),
    };
    let mut events = Vec::new();
    for (i, t) in f.tracks.into_iter().enumerate() {
        let orig = tracks.get(i).copied().unwrap_or(i);
        // the terminator write() appended sorts last by (tick,seq) —
        // keep only as many EOTs, earliest first, as the selection had
        let keep: BTreeSet<(u64, u32)> = t
            .events
            .iter()
            .filter(|e| is_eot_kind(&e.kind))
            .map(|e| (e.tick, e.seq))
            .take(eots.get(i).copied().unwrap_or(0))
            .collect();
        for e in t.events {
            if is_eot_kind(&e.kind) && !keep.contains(&(e.tick, e.seq)) {
                continue;
            }
            events.push(ClipEvent {
                dtick: e.tick,
                seq: e.seq,
                track: orig,
                raw_body: e.raw_body,
                kind: e.kind,
            });
        }
    }
    // dtick re-anchored at the fragment's earliest kept tick
    let lo = events.iter().map(|e| e.dtick).min().unwrap_or(0);
    for e in &mut events {
        e.dtick = e.dtick.saturating_sub(lo);
    }
    (!events.is_empty()).then(|| Clip {
        events,
        src: (tracks.len() == 1).then_some(tracks[0]),
        division,
    })
}

/// Meta edit dialog target: `id > 0` rewrites that event's bytes (same
/// meta_type), `id == 0` inserts a new meta at (track, tick).
#[derive(Clone, Copy)]
pub(crate) struct MetaEdit {
    pub(crate) track: usize,
    pub(crate) tick: u64,
    pub(crate) meta_type: u8,
    pub(crate) id: EventId,
}

/// An editable/read-only property shown in the event inspector. The same
/// key is reused across event kinds — the row label says what it means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PropField {
    Tick,
    Channel,
    D0,
    D1,
    /// 14-bit pitch-bend value, displayed as -8192..8191
    PbValue,
    /// meta event type byte — byte-level, warns
    MetaType,
    /// raw payload bytes as hex — byte-level, warns
    HexData,
    NoteStart,
    NoteEnd,
    NoteDur,
    NoteVel,
    NoteRelVel,
    NoteChannel,
    TrackChannel,
}

/// One row of the inspector: label + current value (+ edit/warn flags).
pub(crate) struct PropRow {
    pub(crate) field: Option<PropField>,
    pub(crate) label: SharedString,
    pub(crate) value: String,
    pub(crate) warn: bool,
}

/// What the inspector edits: event-list rows, roll-selected notes, or the
/// selected track itself.
pub(crate) enum PropTarget {
    Events(Vec<(usize, usize, EventId)>),
    Notes(Vec<EventId>),
    Track(usize),
}

/// i18n label for a field, specialized per event kind where useful.
pub(crate) fn prop_field_label(f: PropField, ev: Option<&DocEvent>) -> SharedString {
    let key = match f {
        PropField::Tick => "prop.tick",
        PropField::Channel | PropField::NoteChannel => "prop.channel",
        PropField::TrackChannel => "prop.chan_prefix",
        PropField::MetaType => "prop.meta_type",
        PropField::HexData => "prop.hex_data",
        PropField::NoteStart => "prop.start",
        PropField::NoteEnd => "prop.end",
        PropField::NoteDur => "prop.duration",
        PropField::NoteVel => "prop.velocity",
        PropField::NoteRelVel => "prop.rel_velocity",
        PropField::PbValue => "prop.pb_value",
        PropField::D0 | PropField::D1 => {
            let hi = match ev.map(|e| &e.kind) {
                Some(EventKind::Channel { status, .. }) => status & 0xF0,
                _ => 0,
            };
            match (hi, f) {
                (0x80, PropField::D0) => "prop.key",
                (0x80, PropField::D1) => "prop.rel_velocity",
                (0x90, PropField::D0) => "prop.key",
                (0x90, PropField::D1) => "prop.velocity",
                (0xA0, PropField::D0) => "prop.key",
                (0xA0, PropField::D1) => "prop.pressure",
                (0xB0, PropField::D0) => "prop.controller",
                (0xB0, PropField::D1) => "prop.value",
                (0xC0, PropField::D0) => "prop.program",
                (0xD0, PropField::D0) => "prop.pressure",
                _ => {
                    if f == PropField::D0 {
                        "prop.d0"
                    } else {
                        "prop.d1"
                    }
                }
            }
        }
    };
    t(key).into()
}

pub(crate) fn hex_of(data: &[u8]) -> String {
    data.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Tempo event (0x51) at an exact tick — (id, bpm) when one exists (#133).
pub(crate) fn tempo_event_at(d: &Document, track: usize, tick: u64) -> Option<(EventId, f64)> {
    d.tracks
        .get(track)?
        .events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::Meta {
                meta_type: 0x51,
                data,
            } if e.tick == tick && data.len() >= 3 => {
                let mpq = ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | data[2] as u32;
                Some((e.id, 60_000_000.0 / mpq.max(1) as f64))
            }
            _ => None,
        })
}

/// Signature event (0x58) at an exact tick — (id, numerator, denominator).
pub(crate) fn sig_event_at(d: &Document, track: usize, tick: u64) -> Option<(EventId, u8, u8)> {
    d.tracks
        .get(track)?
        .events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::Meta {
                meta_type: 0x58,
                data,
            } if e.tick == tick && data.len() >= 2 => Some((e.id, data[0], 1u8 << data[1].min(7))),
            _ => None,
        })
}

/// BPM in force at `tick` — the last tempo point at or before it (SMF
/// default 120 when the track carries none).
pub(crate) fn tempo_bpm_at(d: &Document, track: usize, tick: u64) -> f64 {
    d.tempo_map_for(track)
        .points()
        .iter()
        .rev()
        .find(|(t, _, _)| *t <= tick)
        .map(|(_, mpq, _)| 60_000_000.0 / *mpq as f64)
        .unwrap_or(120.0)
}

/// Per-kind inspector rows for one event. Editable rows carry `Some(field)`;
/// `warn` flags byte-level edits (malformed bytes can corrupt the event).
pub(crate) fn event_prop_rows(e: &DocEvent) -> Vec<PropRow> {
    let mut rows = vec![PropRow {
        field: Some(PropField::Tick),
        label: prop_field_label(PropField::Tick, Some(e)),
        value: e.tick.to_string(),
        warn: false,
    }];
    match &e.kind {
        EventKind::Channel { status, data, len } => {
            let hi = status & 0xF0;
            rows.push(PropRow {
                field: Some(PropField::Channel),
                label: prop_field_label(PropField::Channel, Some(e)),
                value: ((status & 0x0F) + 1).to_string(),
                warn: false,
            });
            if hi == 0xE0 {
                let v = (((data[1] as u16) << 7) | data[0] as u16) as i32 - 8192;
                rows.push(PropRow {
                    field: Some(PropField::PbValue),
                    label: prop_field_label(PropField::PbValue, Some(e)),
                    value: v.to_string(),
                    warn: false,
                });
            } else {
                rows.push(PropRow {
                    field: Some(PropField::D0),
                    label: prop_field_label(PropField::D0, Some(e)),
                    value: data[0].to_string(),
                    warn: false,
                });
                if *len >= 2 {
                    rows.push(PropRow {
                        field: Some(PropField::D1),
                        label: prop_field_label(PropField::D1, Some(e)),
                        value: data[1].to_string(),
                        warn: false,
                    });
                }
            }
        }
        EventKind::Meta { meta_type, data } => {
            rows.push(PropRow {
                field: Some(PropField::MetaType),
                label: prop_field_label(PropField::MetaType, Some(e)),
                value: format!("0x{meta_type:02X}"),
                warn: true,
            });
            rows.push(PropRow {
                field: Some(PropField::HexData),
                label: prop_field_label(PropField::HexData, Some(e)),
                value: hex_of(data),
                warn: true,
            });
        }
        EventKind::SysEx(data) | EventKind::Escape(data) => {
            rows.push(PropRow {
                field: Some(PropField::HexData),
                label: prop_field_label(PropField::HexData, Some(e)),
                value: hex_of(data),
                warn: true,
            });
        }
    }
    rows
}

pub(crate) fn note_prop_rows(n: &Note, d: &Document) -> Vec<PropRow> {
    let end = n.end_tick.map(|e| e.to_string()).unwrap_or_default();
    let dur = n
        .end_tick
        .map(|e| e.saturating_sub(n.start_tick).to_string())
        .unwrap_or_default();
    let rel = n
        .off_id
        .and_then(|id| find_event(d, id))
        .and_then(|(ti, ei)| match &d.tracks[ti].events[ei].kind {
            EventKind::Channel { data, .. } => Some(data[1].to_string()),
            _ => None,
        })
        .unwrap_or_default();
    let mk = |field: PropField, label: SharedString, value: String| PropRow {
        field: Some(field),
        label,
        value,
        warn: false,
    };
    vec![
        mk(
            PropField::NoteStart,
            t("prop.start").into(),
            n.start_tick.to_string(),
        ),
        mk(PropField::NoteEnd, t("prop.end").into(), end),
        mk(PropField::NoteDur, t("prop.duration").into(), dur),
        mk(
            PropField::NoteChannel,
            t("prop.channel").into(),
            (n.channel + 1).to_string(),
        ),
        mk(
            PropField::NoteVel,
            t("prop.velocity").into(),
            n.vel.to_string(),
        ),
        mk(PropField::NoteRelVel, t("prop.rel_velocity").into(), rel),
    ]
}

pub(crate) fn parse_num(text: &str, lo: i64, hi: i64, name: &str) -> Result<i64, String> {
    let v: i64 = text
        .trim()
        .parse()
        .map_err(|_| format!("invalid {name}: {text}"))?;
    if v < lo || v > hi {
        return Err(format!("{name} out of range {lo}..={hi}: {v}"));
    }
    Ok(v)
}

/// "80 3c 40" / "803c40" / "0x80,0x3c,0x40" / "" all parse; anything else
/// is rejected before a transaction exists.
pub(crate) fn parse_hex(text: &str) -> Result<Vec<u8>, String> {
    let cleaned: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if !cleaned.len().is_multiple_of(2) {
        return Err(format!("hex data needs whole bytes: {text}"));
    }
    (0..cleaned.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

pub(crate) fn find_event(d: &Document, id: EventId) -> Option<(usize, usize)> {
    for (ti, tr) in d.tracks.iter().enumerate() {
        for (ei, e) in tr.events.iter().enumerate() {
            if e.id == id {
                return Some((ti, ei));
            }
        }
    }
    None
}

/// Validate `text` for `field` against every target, producing the ops for
/// one transaction. Err = rejected before any transaction; Ok(vec![]) =
/// field unsupported by all targets (e.g. velocity on a meta event).
pub(crate) fn prop_edit_ops(
    d: &mut Document,
    target: &PropTarget,
    field: PropField,
    text: &str,
) -> Result<Vec<Op>, String> {
    match target {
        PropTarget::Track(ti) => match field {
            PropField::TrackChannel => {
                let ch = parse_num(text, 1, 16, "channel")? as u8 - 1;
                Ok(d.set_track_channel_ops(*ti, ch))
            }
            _ => Ok(vec![]),
        },
        PropTarget::Events(ids) => {
            let mut ops = Vec::new();
            for &(ti, ei, _) in ids {
                ops.extend(edit_event_field(d, ti, ei, field, text)?);
            }
            Ok(ops)
        }
        PropTarget::Notes(ids) => {
            let mut ops = Vec::new();
            for &on_id in ids {
                ops.extend(edit_note_field(d, on_id, field, text)?);
            }
            Ok(ops)
        }
    }
}

/// Edit one field of one event. Unsupported field-for-kind returns empty
/// ops (multi-select batch applies only where meaningful); invalid input
/// is an Err before any transaction exists.
pub(crate) fn edit_event_field(
    d: &mut Document,
    ti: usize,
    ei: usize,
    field: PropField,
    text: &str,
) -> Result<Vec<Op>, String> {
    let ev = match d.tracks.get(ti).and_then(|t| t.events.get(ei)) {
        Some(e) => e.clone(),
        None => return Err(format!("event not found: track {ti} index {ei}")),
    };
    // End-of-Track is the structural terminator: only its position is
    // user-editable (an intentional silent tail). The document keeps it
    // last; kind/payload edits would corrupt the structure.
    if matches!(
        ev.kind,
        EventKind::Meta {
            meta_type: 0x2F,
            ..
        }
    ) && field != PropField::Tick
    {
        return Err("End-of-Track is structural — only its tick may be edited".into());
    }
    let mk = |after: DocEvent| {
        vec![Op::UpdateEvent {
            track: ti,
            before: ev.clone(),
            after,
        }]
    };
    match field {
        PropField::Tick => {
            let mut a = ev.clone();
            a.tick = parse_num(text, 0, i64::MAX, "tick")? as u64;
            Ok(mk(a))
        }
        PropField::Channel => {
            let ch = parse_num(text, 1, 16, "channel")? as u8 - 1;
            if let EventKind::Channel { .. } = ev.kind {
                let mut a = ev.clone();
                if let EventKind::Channel { status, .. } = &mut a.kind {
                    *status = (*status & 0xF0) | ch;
                }
                Ok(mk(a))
            } else {
                Ok(vec![])
            }
        }
        PropField::D0 => {
            let v = parse_num(text, 0, 127, "data0")? as u8;
            if let EventKind::Channel { .. } = ev.kind {
                let mut a = ev.clone();
                if let EventKind::Channel { data, .. } = &mut a.kind {
                    data[0] = v;
                }
                Ok(mk(a))
            } else {
                Ok(vec![])
            }
        }
        PropField::D1 => {
            let v = parse_num(text, 0, 127, "data1")? as u8;
            match ev.kind {
                EventKind::Channel { len, .. } if len >= 2 => {
                    let mut a = ev.clone();
                    if let EventKind::Channel { data, .. } = &mut a.kind {
                        data[1] = v;
                    }
                    Ok(mk(a))
                }
                _ => Ok(vec![]),
            }
        }
        PropField::PbValue => {
            let v = parse_num(text, -8192, 8191, "pitch bend")?;
            if let EventKind::Channel { status, .. } = ev.kind {
                if status & 0xF0 != 0xE0 {
                    return Ok(vec![]);
                }
                let u = (v + 8192) as u16;
                let mut a = ev.clone();
                if let EventKind::Channel { data, .. } = &mut a.kind {
                    data[0] = (u & 0x7F) as u8;
                    data[1] = ((u >> 7) & 0x7F) as u8;
                }
                Ok(mk(a))
            } else {
                Ok(vec![])
            }
        }
        PropField::MetaType => {
            let s = text.trim();
            let mt = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(h) => {
                    u8::from_str_radix(h, 16).map_err(|_| format!("invalid meta type: {text}"))?
                }
                None => parse_num(s, 0, 255, "meta type")? as u8,
            };
            if let EventKind::Meta { .. } = ev.kind {
                let mut a = ev.clone();
                if let EventKind::Meta { meta_type, .. } = &mut a.kind {
                    *meta_type = mt;
                }
                Ok(mk(a))
            } else {
                Ok(vec![])
            }
        }
        PropField::HexData => {
            let bytes = parse_hex(text)?;
            let mut a = ev.clone();
            match &mut a.kind {
                EventKind::Meta { data, .. } | EventKind::SysEx(data) | EventKind::Escape(data) => {
                    *data = bytes.into();
                    Ok(mk(a))
                }
                _ => Ok(vec![]),
            }
        }
        // note-shaped fields apply through the note path, not single events
        _ => Ok(vec![]),
    }
}

/// Edit one field of one paired note (its NoteOn / NoteOff events).
/// Unsupported fields (e.g. duration on a dangling on) return empty ops.
pub(crate) fn edit_note_field(
    d: &mut Document,
    on_id: EventId,
    field: PropField,
    text: &str,
) -> Result<Vec<Op>, String> {
    let Some((oti, oei)) = find_event(d, on_id) else {
        return Ok(vec![]);
    };
    let note = d.notes().into_iter().find(|n| n.on_id == on_id);
    let Some(n) = note else { return Ok(vec![]) };
    let on = d.tracks[oti].events[oei].clone();
    let off = n
        .off_id
        .and_then(|id| find_event(d, id))
        .map(|(ti, ei)| d.tracks[ti].events[ei].clone());
    let mut ops = Vec::new();
    let mut upd = |pos: (usize, usize), after: DocEvent| {
        ops.push(Op::UpdateEvent {
            track: pos.0,
            before: d.tracks[pos.0].events[pos.1].clone(),
            after,
        });
    };
    match field {
        PropField::NoteStart => {
            let v = parse_num(text, 0, i64::MAX, "start")? as u64;
            if let Some(end) = n.end_tick {
                if v >= end {
                    return Err(format!("start must be before end ({end}): {v}"));
                }
            }
            let mut a = on.clone();
            a.tick = v;
            upd((oti, oei), a);
        }
        PropField::NoteEnd => {
            let v = parse_num(text, 0, i64::MAX, "end")? as u64;
            let Some(off) = off else {
                return Err("dangling note has no note-off to edit".into());
            };
            if v <= n.start_tick {
                return Err(format!("end must be after start ({}): {v}", n.start_tick));
            }
            let fti = find_event(d, n.off_id.unwrap()).unwrap();
            let mut a = off;
            a.tick = v;
            upd(fti, a);
        }
        PropField::NoteDur => {
            let v = parse_num(text, 1, i64::MAX, "duration")? as u64;
            let Some(off) = off else {
                return Err("dangling note has no note-off to edit".into());
            };
            let fti = find_event(d, n.off_id.unwrap()).unwrap();
            let mut a = off;
            a.tick = n.start_tick + v;
            upd(fti, a);
        }
        PropField::NoteVel => {
            let v = parse_num(text, 1, 127, "velocity")? as u8;
            let mut a = on.clone();
            if let EventKind::Channel { data, .. } = &mut a.kind {
                data[1] = v;
            }
            upd((oti, oei), a);
        }
        PropField::NoteRelVel => {
            let v = parse_num(text, 0, 127, "release velocity")? as u8;
            let Some(off) = off else {
                return Err("dangling note has no note-off to edit".into());
            };
            let fti = find_event(d, n.off_id.unwrap()).unwrap();
            let mut a = off;
            if let EventKind::Channel { data, .. } = &mut a.kind {
                data[1] = v;
            }
            upd(fti, a);
        }
        PropField::NoteChannel => {
            let ch = parse_num(text, 1, 16, "channel")? as u8 - 1;
            let mut a = on.clone();
            if let EventKind::Channel { status, .. } = &mut a.kind {
                *status = (*status & 0xF0) | ch;
            }
            upd((oti, oei), a);
            if let Some(off) = off {
                let fti = find_event(d, n.off_id.unwrap()).unwrap();
                let mut a = off;
                if let EventKind::Channel { status, .. } = &mut a.kind {
                    *status = (*status & 0xF0) | ch;
                }
                upd(fti, a);
            }
        }
        _ => return Ok(vec![]),
    }
    Ok(ops)
}

impl EditorView {
    /// delete the notes whose on_ids are in `erase_ids` as one undo step
    pub(crate) fn commit_erase(&mut self, cx: &mut Context<Self>) {
        let ids = std::mem::take(&mut self.erase_ids);
        if ids.is_empty() {
            return;
        }
        let sh = lock_shared(&self.shared);
        let mut ops = Vec::new();
        for &on_id in &ids {
            let off_id = self
                .notes
                .iter()
                .find(|n| n.on_id == on_id)
                .and_then(|n| n.off_id);
            if let Some(op) = Self::remove_note_op(&sh, on_id, off_id) {
                ops.push(op);
            }
        }
        drop(sh);
        if !ops.is_empty() {
            self.apply_tx("erase notes", ops);
        }
        self.selection.clear();
        cx.notify();
    }

    /// Click on an event-list row: plain = single-select + inspect, ctrl =
    /// toggle, shift = range from the last anchor. Diagnostic rows (no ref)
    /// just clear the inspector selection.
    pub(crate) fn ev_row_click(
        &mut self,
        row: usize,
        ctrl: bool,
        shift: bool,
        double: bool,
        cx: &mut Context<Self>,
    ) {
        self.ev_sel = row;
        let Some(&Some((_, _, id))) = self.event_refs.get(row) else {
            self.sel_events.clear();
            self.prop_field = None;
            self.meta_sel = None;
            cx.notify();
            return;
        };
        if shift {
            let anchor = self.ev_anchor.unwrap_or(row).min(self.event_refs.len() - 1);
            let (lo, hi) = (anchor.min(row), anchor.max(row));
            for (_, _, id) in self.event_refs[lo..=hi].iter().flatten() {
                self.sel_events.insert(*id);
            }
        } else if ctrl {
            if !self.sel_events.remove(&id) {
                self.sel_events.insert(id);
            }
            self.ev_anchor = Some(row);
        } else {
            self.sel_events = BTreeSet::from([id]);
            self.ev_anchor = Some(row);
        }
        self.prop_field = None;
        // clicking a meta row arms the `e`/Del meta-edit shortcut path
        self.meta_sel = self
            .event_refs
            .get(row)
            .copied()
            .flatten()
            .filter(|&(tr, ei, _)| {
                self.doc(|d| {
                    matches!(
                        d.tracks.get(tr).and_then(|t| t.events.get(ei)),
                        Some(e) if matches!(e.kind, EventKind::Meta { .. })
                    )
                })
            })
            .map(|(tr, _, id)| (tr, id));
        // double-clicking a meta row opens the edit dialog (deferred to
        // render — this path has no Window)
        if double {
            if let Some((tr, id)) = self.meta_sel {
                let m = self.doc(|d| {
                    d.tracks.get(tr).and_then(|t| {
                        t.events
                            .iter()
                            .find(|e| e.id == id)
                            .and_then(|e| match &e.kind {
                                EventKind::Meta { meta_type, .. } => Some((e.tick, *meta_type)),
                                _ => None,
                            })
                    })
                });
                if let Some((tick, mt)) = m {
                    self.meta_pending = Some((tr, tick, mt, id));
                }
            }
        }
        cx.notify();
    }

    /// Header + rows for the inspector panel, in its current mode
    /// (event / note / track depending on what is selected).
    pub(crate) fn prop_rows(&self, d: &Document) -> (SharedString, Vec<PropRow>) {
        let mut rows = Vec::new();
        if !self.sel_events.is_empty() {
            let sel: Vec<(usize, usize, EventId)> = self
                .event_refs
                .iter()
                .flatten()
                .copied()
                .filter(|(_, _, id)| self.sel_events.contains(id))
                .collect();
            if sel.len() == 1 {
                let (ti, ei, _) = sel[0];
                let e = &d.tracks[ti].events[ei];
                return (
                    tf("prop.event_title", &[("t", &(ti + 1).to_string())]).into(),
                    event_prop_rows(e),
                );
            }
            for f in [
                PropField::Tick,
                PropField::Channel,
                PropField::D0,
                PropField::D1,
            ] {
                rows.push(PropRow {
                    field: Some(f),
                    label: prop_field_label(f, None),
                    value: String::new(),
                    warn: false,
                });
            }
            return (
                tf("prop.multi_title", &[("n", &sel.len().to_string())]).into(),
                rows,
            );
        }
        if !self.selection.is_empty() {
            if self.selection.len() == 1 {
                let on_id = *self.selection.iter().next().unwrap();
                if let Some(n) = d.notes().into_iter().find(|n| n.on_id == on_id) {
                    // inspector echoes the chosen octave convention (#164)
                    let k = format!(
                        "{} ({})",
                        n.key,
                        a11y::note_name(n.key as i64 + self.mc_off() * 12)
                    );
                    return (
                        tf("prop.note_title", &[("k", &k)]).into(),
                        note_prop_rows(&n, d),
                    );
                }
            }
            rows.push(PropRow {
                field: Some(PropField::NoteChannel),
                label: prop_field_label(PropField::NoteChannel, None),
                value: String::new(),
                warn: false,
            });
            rows.push(PropRow {
                field: Some(PropField::NoteVel),
                label: prop_field_label(PropField::NoteVel, None),
                value: String::new(),
                warn: false,
            });
            return (
                tf(
                    "prop.multi_title",
                    &[("n", &self.selection.len().to_string())],
                )
                .into(),
                rows,
            );
        }
        // track mode
        let ti = self.sel_track;
        if let Some(tr) = d.tracks.get(ti) {
            let name = tr
                .name
                .as_deref()
                .map(|b| smf_core::decode_text(b, self.enc_override.or(d.text_encoding_hint())))
                .unwrap_or_default();
            rows.push(PropRow {
                field: None,
                label: t("prop.name").into(),
                value: name,
                warn: false,
            });
            rows.push(PropRow {
                field: Some(PropField::TrackChannel),
                label: t("prop.chan_prefix").into(),
                value: (tr.out_channel + 1).to_string(),
                warn: false,
            });
            rows.push(PropRow {
                field: None,
                label: t("prop.port").into(),
                value: tr.out_port.to_string(),
                warn: false,
            });
            rows.push(PropRow {
                field: None,
                label: t("prop.count").into(),
                value: tr.events.len().to_string(),
                warn: false,
            });
        }
        (
            tf("prop.track_title", &[("t", &(ti + 1).to_string())]).into(),
            rows,
        )
    }

    /// What the inspector's active edit applies to.
    pub(crate) fn prop_target(&self) -> PropTarget {
        if !self.sel_events.is_empty() {
            return PropTarget::Events(
                self.event_refs
                    .iter()
                    .flatten()
                    .copied()
                    .filter(|(_, _, id)| self.sel_events.contains(id))
                    .collect(),
            );
        }
        if !self.selection.is_empty() {
            return PropTarget::Notes(self.selection.iter().copied().collect());
        }
        PropTarget::Track(self.sel_track)
    }

    /// Commit the inspector's active field: parse + validate first, then a
    /// single transaction covering every selected target. Invalid input
    /// lands in the status line and no transaction is created.
    pub(crate) fn prop_apply(&mut self, cx: &mut Context<Self>) {
        let Some(field) = self.prop_field else { return };
        let text = self.prop_input.read(cx).value().to_string();
        let target = self.prop_target();
        let result = {
            let mut sh = lock_shared(&self.shared);
            prop_edit_ops(&mut sh.doc, &target, field, &text)
        };
        match result {
            Err(e) => {
                self.status = e.into();
            }
            Ok(ops) if ops.is_empty() => {
                self.status = t("prop.unsupported").into();
            }
            Ok(ops) => {
                self.apply_tx("edit property", ops);
                self.status = t("prop.applied").into();
            }
        }
        cx.notify();
    }

    /// Load the inspector field's current value into the input and focus it.
    pub(crate) fn prop_edit(
        &mut self,
        field: PropField,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.prop_field = Some(field);
        self.prop_input.update(cx, |i, cx| {
            i.set_value(value, window, cx);
        });
        let fh = self.prop_input.read(cx).focus_handle(cx);
        window.focus(&fh, cx);
        cx.notify();
    }

    /// Run a semantic region transform (`Document` *_ops generator) on the
    /// selection's range. With no selection this is a no-op — selection-
    /// scoped commands never silently retarget the whole track (#131); the
    /// explicit whole-track path is `apply_track_op`.
    /// The same generators power the MCP tools, so GUI and AI edits
    /// share semantics and undo.
    pub(crate) fn apply_region_op(
        &mut self,
        label: &str,
        f: impl Fn(&mut Document, usize, u64, u64) -> Vec<Op>,
    ) {
        if self.selection.is_empty() {
            self.status = t("status.nosel").into();
            return;
        }
        let mut tracks = BTreeSet::new();
        let (mut lo, mut hi) = (u64::MAX, 0u64);
        for n in self.notes.iter() {
            if self.selection.contains(&n.on_id) {
                tracks.insert(n.track);
                lo = lo.min(n.start_tick);
                hi = hi.max(n.end_tick.unwrap_or(n.start_tick));
            }
        }
        let (tracks, from, to) = (tracks.into_iter().collect::<Vec<_>>(), lo, hi + 1);
        let ops = {
            let mut sh = lock_shared(&self.shared);
            tracks
                .into_iter()
                .flat_map(|t| f(&mut sh.doc, t, from, to))
                .collect::<Vec<_>>()
        };
        if ops.is_empty() {
            self.status = format!("{label}: nothing to change").into();
        } else {
            self.apply_tx(label, ops);
            self.status = label.to_string().into();
        }
    }

    /// Explicit whole-track transform — the "Apply to Entire Track" menu.
    /// Equivalent to Select-All-then-transform, but names its scope so a
    /// user who means "the whole track" doesn't have to select anything.
    pub(crate) fn apply_track_op(
        &mut self,
        label: &str,
        f: impl Fn(&mut Document, usize, u64, u64) -> Vec<Op>,
    ) {
        let ops = {
            let mut sh = lock_shared(&self.shared);
            f(&mut sh.doc, self.sel_track, 0, u64::MAX)
        };
        if ops.is_empty() {
            self.status = format!("{label}: nothing to change").into();
        } else {
            self.apply_tx(label, ops);
            self.status = label.to_string().into();
        }
    }

    /// Track the tempo chip edits: the conductor for format 0/1, the
    /// viewed sequence for format 2.
    pub(crate) fn tempo_track(&self) -> usize {
        self.doc(|d| {
            if d.is_sequential() {
                self.sel_track.min(d.tracks.len().saturating_sub(1))
            } else {
                0
            }
        })
    }

    /// Split notes spanning the playhead: the selection when there is one,
    /// else every note on the selected track that straddles the line.
    pub(crate) fn split_at_playhead(&mut self, cx: &mut Context<Self>) {
        let at = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        let ops = {
            let mut sh = lock_shared(&self.shared);
            if self.selection.is_empty() {
                sh.doc.split_ops(self.sel_track, 0, u64::MAX, at)
            } else {
                sh.doc.split_ids_ops(&self.selection, at)
            }
        };
        if ops.is_empty() {
            self.status = t("status.split_none").into();
        } else {
            self.apply_tx("split", ops);
            self.status = "split".into();
        }
        cx.notify();
    }

    /// Nudge the tempo at the playhead — bumps the point in force there
    /// (writing a new tempo event at the playhead tick when none exists),
    /// never silently rewriting tick 0 (#133).
    pub(crate) fn bump_tempo(&mut self, delta: f64) {
        let tr = self.tempo_track();
        let tick = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        let cur = self.doc(|d| tempo_bpm_at(d, tr, tick));
        let ops = {
            let mut sh = lock_shared(&self.shared);
            sh.doc
                .set_tempo_ops(tr, tick, (cur + delta).clamp(10.0, 400.0))
        };
        self.apply_tx("set tempo", ops);
    }

    /// Cycle the signature in force at the playhead through common meters —
    /// writes at the playhead tick, preserving the stored cc/bb bytes on an
    /// existing event (#133).
    pub(crate) fn cycle_time_sig(&mut self) {
        const SIGS: [(u8, u8); 6] = [(4, 4), (3, 4), (2, 4), (5, 4), (6, 8), (7, 8)];
        let tr = self.tempo_track();
        let tick = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        let cur = self.doc(|d| {
            let m = d.meter_map_for(tr).meter_at(tick);
            (m.num, (1u32 << m.den_pow.min(15)) as u8)
        });
        let next = match SIGS.iter().position(|s| *s == cur) {
            Some(i) => SIGS[(i + 1) % SIGS.len()],
            None => SIGS[1],
        };
        let ops = {
            let mut sh = lock_shared(&self.shared);
            sh.doc.set_time_sig_ops(tr, tick, next.0, next.1)
        };
        self.apply_tx("set time signature", ops);
    }

    /// Tempo/signature edit dialog at the playhead tick — a matching event
    /// there is edited in place; otherwise a new one is inserted prefilled
    /// with the value in force (#133).
    pub(crate) fn open_tempo_sig_edit(
        &mut self,
        meta_type: u8,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let tr = self.tempo_track();
        let tick = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        let id = self.doc(|d| match meta_type {
            0x51 => tempo_event_at(d, tr, tick).map(|(id, _)| id),
            0x58 => sig_event_at(d, tr, tick).map(|(id, _, _)| id),
            _ => None,
        });
        self.open_meta_edit(tr, tick, meta_type, id.unwrap_or(0), window, cx);
    }

    /// Delete the tempo/signature event at the playhead tick (#133).
    pub(crate) fn delete_tempo_sig(&mut self, meta_type: u8, cx: &mut Context<Self>) {
        let tr = self.tempo_track();
        let tick = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        let id = self.doc(|d| match meta_type {
            0x51 => tempo_event_at(d, tr, tick).map(|(id, _)| id),
            0x58 => sig_event_at(d, tr, tick).map(|(id, _, _)| id),
            _ => None,
        });
        match id {
            Some(id) => {
                let ops = {
                    let mut sh = lock_shared(&self.shared);
                    sh.doc.remove_meta_ops(tr, id)
                };
                self.apply_tx("delete meta", ops);
            }
            None => self.status = t("status.meta_none").into(),
        }
        cx.notify();
    }

    #[allow(dead_code)]
    pub(crate) fn insert_note(&mut self, tick: u64, key: u8, cx: &mut Context<Self>) {
        let len = self.note_len.ticks(self);
        self.insert_note_len(tick, key, len, cx);
    }

    pub(crate) fn insert_note_len(&mut self, tick: u64, key: u8, len: u64, cx: &mut Context<Self>) {
        let (on_id, off_id, track, ch) = {
            let mut sh = lock_shared(&self.shared);
            let track = self.sel_track.min(sh.doc.tracks.len().saturating_sub(1));
            let prefix = sh.doc.tracks.get(track).map(|t| t.out_channel).unwrap_or(0);
            (
                sh.doc.alloc_event_id(),
                sh.doc.alloc_event_id(),
                track,
                self.edit_channel_of(track, prefix),
            )
        };
        let tick = self.snap_down(tick as i64).max(0) as u64;
        let on_vel = self.vel_src.resolve(self.last_vel);
        let on = DocEvent {
            id: on_id,
            tick,
            seq: u32::MAX / 2,
            raw_body: None,
            kind: EventKind::Channel {
                status: 0x90 | ch,
                data: [key, on_vel],
                len: 2,
            },
        };
        let off = DocEvent {
            id: off_id,
            tick: tick + len,
            seq: u32::MAX / 2,
            raw_body: None,
            kind: EventKind::Channel {
                status: 0x80 | ch,
                data: [key, 0],
                len: 2,
            },
        };
        self.last_note_len = len.max(1);
        self.last_vel = on_vel;
        self.apply_tx(
            "insert note",
            vec![Op::InsertEvents {
                track,
                events: vec![on, off],
            }],
        );
        self.selection = BTreeSet::from([on_id]);
        self.sel_events.clear();
        cx.notify();
    }

    /// Remove a note's on+off events; returns the op (or None if id unknown).
    pub(crate) fn remove_note_op(
        sh: &Shared,
        on_id: EventId,
        off_id: Option<EventId>,
    ) -> Option<Op> {
        let mut track = 0usize;
        let mut removed: Vec<(usize, document::Event)> = Vec::new();
        'outer: for (ti, t) in sh.doc.tracks.iter().enumerate() {
            for (ei, e) in t.events.iter().enumerate() {
                if e.id == on_id {
                    track = ti;
                    removed.push((ei, e.clone()));
                    break 'outer;
                }
            }
        }
        if removed.is_empty() {
            return None;
        }
        if let Some(off_id) = off_id {
            if let Some(pos) = sh.doc.tracks[track]
                .events
                .iter()
                .position(|e| e.id == off_id)
            {
                removed.push((pos, sh.doc.tracks[track].events[pos].clone()));
            }
        }
        Some(Op::RemoveEvents { track, removed })
    }

    /// Exact edit on the selected event-list row when it belongs to an
    /// RPN/NRPN entry: data events nudge the entered value (7-bit MSB step,
    /// 14-bit LSB step); selector events nudge the parameter number. The
    /// whole write stays in valid selector→data order because only the
    /// targeted bytes are rewritten.
    /// Semantic nudge for one event that belongs to an RPN/NRPN write:
    /// on a selector CC it moves the parameter number; on a data-entry CC
    /// it moves the written value. `None` when the event isn't in an entry.
    pub(crate) fn nudge_rpn_ops(
        sh: &mut Shared,
        id: EventId,
        delta: i32,
    ) -> Option<(Vec<Op>, String)> {
        let e = sh.doc.rpn_entry_containing(id)?;
        if e.sel_ids.contains(&id) {
            let param = (e.param14() as i32 + delta).clamp(0, 16383) as u16;
            Some((
                sh.doc
                    .update_rpn_param_ops(&e, (param >> 7) as u8, (param & 0x7F) as u8),
                format!("RPN {}.{}", param >> 7, param & 0x7F),
            ))
        } else if e.data_msb_id.is_some() || e.data_lsb_id.is_some() {
            let v = e.value()?;
            // 7-bit entries carry the value in data[1] itself; 14-bit
            // entries split it across CC6 (msb) + CC38 (lsb)
            let cap = if e.is_14bit() { 16383 } else { 127 };
            let nv = (v as i32 + delta).clamp(0, cap) as u16;
            let (msb, lsb) = if e.is_14bit() {
                ((nv >> 7) as u8, Some((nv & 0x7F) as u8))
            } else {
                (nv as u8, None)
            };
            Some((
                sh.doc.update_rpn_value_ops(&e, msb, lsb),
                format!("RPN = {nv}"),
            ))
        } else {
            Some((Vec::new(), String::new()))
        }
    }

    /// Delete acts on the focused context (#152): roll/track areas delete
    /// note selections, the lane deletes its marquee set, the event list
    /// deletes its selected rows.
    pub(crate) fn delete_selected(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.meta_sel.is_some() {
            self.delete_meta(cx);
            return;
        }
        self.delete_selected_area(self.area(window, cx), cx);
    }

    fn delete_selected_area(&mut self, area: FocusArea, cx: &mut Context<Self>) {
        let mut sh = lock_shared(&self.shared);
        let mut ops = Vec::new();
        let in_roll = matches!(
            area,
            FocusArea::Roll | FocusArea::Tracks | FocusArea::MenuBar
        );
        if in_roll {
            for &on_id in &self.selection {
                let off_id = self
                    .notes
                    .iter()
                    .find(|n| n.on_id == on_id)
                    .and_then(|n| n.off_id);
                if let Some(op) = Self::remove_note_op(&sh, on_id, off_id) {
                    ops.push(op);
                }
            }
        }
        // lane marquee selection + event-list row selection delete whole
        // events through the same transaction (skip ids a note op already
        // queues — a doubly-removed id would abort the whole transaction)
        let mut queued: BTreeSet<EventId> = BTreeSet::new();
        for op in &ops {
            if let Op::RemoveEvents { removed, .. } = op {
                queued.extend(removed.iter().map(|(_, e)| e.id));
            }
        }
        let mut extra: Vec<EventId> = Vec::new();
        if area == FocusArea::Events {
            extra.extend(self.sel_events.iter().copied());
        }
        if area == FocusArea::Lane {
            extra.extend(self.lane_sel.iter().copied());
        }
        extra.retain(|id| !queued.contains(id));
        // an event that is part of an RPN/NRPN write deletes the whole
        // parameter entry — removing a lone selector/data CC would leave
        // a corrupt half-write in the file
        let mut entries: Vec<document::RpnEntry> = Vec::new();
        let mut entry_ids: BTreeSet<EventId> = BTreeSet::new();
        for id in &extra {
            if entry_ids.contains(id) {
                continue;
            }
            if let Some(e) = sh.doc.rpn_entry_containing(*id) {
                entry_ids.extend(e.ids());
                entries.push(e);
            }
        }
        for e in &entries {
            ops.extend(sh.doc.remove_rpn_entry_ops(e));
        }
        queued.extend(entry_ids.iter().copied());
        let extra: Vec<EventId> = extra
            .into_iter()
            .filter(|id| !queued.contains(id))
            .collect();
        ops.extend(sh.doc.remove_events_ops(&extra));
        drop(sh);
        if !ops.is_empty() {
            self.apply_tx("delete", ops);
        }
        self.selection.clear();
        self.lane_sel.clear();
        self.sel_events.clear();
        cx.notify();
    }

    /// Adjust the event-list selection's primary value by `delta`
    /// (velocity/CC/pressure → data\[1\]; program/channel pressure → data\[0\];
    /// pitch bend → 14-bit). Exact per-event editing from the keyboard.
    pub(crate) fn nudge_sel_events(&mut self, delta: i32, cx: &mut Context<Self>) {
        if self.sel_events.is_empty() {
            return;
        }
        let wanted: BTreeSet<EventId> = self.sel_events.clone();
        let mut sh = lock_shared(&self.shared);
        let mut ops = Vec::new();
        // events inside an RPN/NRPN write take the semantic path: nudging
        // a selector moves the parameter number, nudging a data-entry CC
        // rewrites the whole entry coherently. Everything else gets the
        // plain data-byte nudge.
        let mut labels = Vec::new();
        let mut raw: BTreeSet<EventId> = wanted.clone();
        for id in &wanted {
            if let Some((eops, lbl)) = Self::nudge_rpn_ops(&mut sh, *id, delta) {
                ops.extend(eops);
                if !lbl.is_empty() {
                    labels.push(lbl);
                }
                raw.remove(id);
            }
        }
        for (ti, t) in sh.doc.tracks.iter().enumerate() {
            for e in &t.events {
                if !raw.contains(&e.id) {
                    continue;
                }
                let EventKind::Channel { status, .. } = &e.kind else {
                    continue;
                };
                let status = *status;
                let mut after = e.clone();
                let EventKind::Channel { data: d, .. } = &mut after.kind else {
                    unreachable!();
                };
                match status & 0xF0 {
                    0x90 | 0x80 | 0xA0 | 0xB0 => {
                        let lo = if status & 0xF0 == 0x90 { 1 } else { 0 };
                        d[1] = (d[1] as i32 + delta).clamp(lo, 127) as u8;
                    }
                    0xC0 | 0xD0 => d[0] = (d[0] as i32 + delta).clamp(0, 127) as u8,
                    0xE0 => {
                        let v = ((d[1] as i32) << 7 | d[0] as i32) + delta;
                        let v = v.clamp(0, 16383);
                        d[0] = (v & 0x7F) as u8;
                        d[1] = (v >> 7) as u8;
                    }
                    _ => continue,
                }
                ops.push(Op::UpdateEvent {
                    track: ti,
                    before: e.clone(),
                    after,
                });
            }
        }
        drop(sh);
        if !ops.is_empty() {
            self.apply_tx("edit event", ops);
            if let Some(l) = labels.first() {
                self.status = l.clone().into();
            }
        }
        cx.notify();
    }

    /// Open the meta edit dialog. `id > 0` edits that event (prefilled);
    /// `id == 0` creates a new meta at (track, tick).
    pub(crate) fn open_meta_edit(
        &mut self,
        track: usize,
        tick: u64,
        meta_type: u8,
        id: EventId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sh = lock_shared(&self.shared);
        let cur = if id != 0 {
            sh.doc
                .tracks
                .get(track)
                .and_then(|t| t.events.iter().find(|e| e.id == id))
                .and_then(|e| match &e.kind {
                    EventKind::Meta {
                        meta_type: mt,
                        data,
                    } if *mt == meta_type => {
                        if meta_type == 0x59 && data.len() >= 2 {
                            Some(format!(
                                "{} {}",
                                data[0] as i8,
                                if data[1] == 1 { "minor" } else { "major" }
                            ))
                        } else if meta_type == 0x51 && data.len() >= 3 {
                            let mpq =
                                ((data[0] as u32) << 16) | ((data[1] as u32) << 8) | data[2] as u32;
                            Some(format!("{:.2}", 60_000_000.0 / mpq.max(1) as f64))
                        } else if meta_type == 0x58 && data.len() >= 2 {
                            Some(format!("{}/{}", data[0], 1u32 << data[1].min(7)))
                        } else {
                            Some(smf_core::decode_text(data, None))
                        }
                    }
                    _ => None,
                })
                .unwrap_or_default()
        } else {
            // a fresh tempo/signature event pre-fills the value in force at
            // the insertion tick, not a blank or the tick-0 default (#133)
            match meta_type {
                0x51 => format!("{:.2}", tempo_bpm_at(&sh.doc, track, tick)),
                0x58 => {
                    let m = sh.doc.meter_map_for(track).meter_at(tick);
                    format!("{}/{}", m.num, 1u32 << m.den_pow.min(7))
                }
                _ => String::new(),
            }
        };
        drop(sh);
        self.meta_input.update(cx, |i, cx| {
            i.set_value(cur, window, cx);
        });
        self.meta_edit = Some(MetaEdit {
            track,
            tick,
            meta_type,
            id,
        });
        window.focus(&self.meta_input.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Apply the dialog: encode via `enc_override` (UTF-8 default) for text
    /// metas, parse "sf minor" / key names for key signature.
    pub(crate) fn commit_meta_edit(&mut self, cx: &mut Context<Self>) {
        let Some(me) = self.meta_edit.take() else {
            return;
        };
        let text = self.meta_input.read(cx).value().to_string();
        let ops = {
            let mut sh = lock_shared(&self.shared);
            if me.meta_type == 0x59 {
                match parse_key_sig(&text) {
                    Some((sf, mi)) => sh.doc.set_key_sig_ops(me.tick, sf, mi),
                    None => {
                        self.meta_edit = Some(me);
                        self.status = t("status.keysig_parse").into();
                        cx.notify();
                        return;
                    }
                }
            } else if me.meta_type == 0x51 {
                // numeric BPM — writes the tempo map at the dialog's tick
                match text.trim().parse::<f64>() {
                    Ok(bpm) if (10.0..=400.0).contains(&bpm) => {
                        sh.doc.set_tempo_ops(me.track, me.tick, bpm)
                    }
                    _ => {
                        self.meta_edit = Some(me);
                        self.status = t("status.tempo_parse").into();
                        cx.notify();
                        return;
                    }
                }
            } else if me.meta_type == 0x58 {
                // "n/d" — numerator and denominator value; cc/bb preserved
                let parse = text.trim().split_once('/').and_then(|(n, d)| {
                    Some((n.trim().parse::<u8>().ok()?, d.trim().parse::<u8>().ok()?))
                });
                match parse {
                    Some((n, d)) if n >= 1 && d >= 1 => {
                        sh.doc.set_time_sig_ops(me.track, me.tick, n, d)
                    }
                    _ => {
                        self.meta_edit = Some(me);
                        self.status = t("status.sig_parse").into();
                        cx.notify();
                        return;
                    }
                }
            } else {
                sh.doc.set_meta_text_ops(
                    me.track,
                    me.tick,
                    me.meta_type,
                    me.id,
                    &text,
                    self.enc_override,
                )
            }
        };
        if ops.is_empty() {
            self.status = t("status.meta_none").into();
        } else {
            self.apply_tx("meta", ops);
        }
        self.meta_refocus = true;
        cx.notify();
    }

    /// Delete the marker/meta selected via strip click or `[`/`]` nav.
    pub(crate) fn delete_meta(&mut self, cx: &mut Context<Self>) {
        let Some((track, id)) = self.meta_sel else {
            return;
        };
        let ops = {
            let mut sh = lock_shared(&self.shared);
            sh.doc.remove_meta_ops(track, id)
        };
        if ops.is_empty() {
            self.status = t("status.meta_none").into();
        } else {
            self.apply_tx("delete meta", ops);
            self.meta_sel = None;
        }
        cx.notify();
    }

    /// Meta type label for the dialog title + menu.
    pub(crate) fn meta_type_label(meta_type: u8) -> &'static str {
        match meta_type {
            0x01 => "meta.text",
            0x02 => "meta.copyright",
            0x04 => "meta.instrument",
            0x05 => "meta.lyric",
            0x06 => "meta.marker",
            0x07 => "meta.cue",
            0x51 => "meta.tempo",
            0x58 => "meta.timesig",
            0x59 => "meta.keysig",
            _ => "meta.text",
        }
    }

    /// Copy the selection into the note clipboard (`cut` also deletes it).
    /// Gather every selected event of the focused area as document
    /// events — notes contribute on+off pairs; the event-list and lane
    /// sets contribute their events verbatim (#142/#152). Falls back
    /// across sets so a mixed selection copies whole.
    fn selected_doc_events(&self, area: FocusArea) -> Vec<(usize, document::Event)> {
        let sh = lock_shared(&self.shared);
        let mut ids: BTreeSet<EventId> = match area {
            FocusArea::Events => self.sel_events.clone(),
            FocusArea::Lane => self.lane_sel.clone(),
            _ => self.selection.clone(),
        };
        // notes copy as on+off pairs — resolving on_id to both events
        let mut extra = Vec::new();
        if ids.is_empty() || area == FocusArea::Roll || area == FocusArea::Tracks {
            for n in self.notes.iter().filter(|n| ids.contains(&n.on_id)) {
                if let Some(off) = n.off_id {
                    extra.push(off);
                }
            }
        }
        ids.extend(extra);
        let mut out: Vec<(usize, document::Event)> = sh
            .doc
            .tracks
            .iter()
            .enumerate()
            .flat_map(|(ti, t)| t.events.iter().map(move |e| (ti, e)))
            .filter(|(_, e)| ids.contains(&e.id))
            .map(|(ti, e)| (ti, e.clone()))
            .collect();
        out.sort_by_key(|(_, e)| (e.tick, e.seq));
        out
    }

    /// Copy the focused area's selection to the OS clipboard (#142):
    /// every selected event kind, byte-faithful, readable by another
    /// midi-editor process. `cut` deletes afterwards in one transaction.
    pub(crate) fn copy_selected(&mut self, cut: bool, window: &mut Window, cx: &mut Context<Self>) {
        let area = self.area(window, cx);
        let evs = self.selected_doc_events(area);
        if evs.is_empty() {
            self.status = t("status.nosel").into();
            cx.notify();
            return;
        }
        let lo = evs.iter().map(|(_, e)| e.tick).min().unwrap_or(0);
        let div = lock_shared(&self.shared).doc.division;
        let src = {
            let mut ts = evs.iter().map(|(t, _)| *t);
            let first = ts.next();
            ts.all(|t| Some(t) == first).then_some(first).flatten()
        };
        let clip = Clip {
            events: evs
                .into_iter()
                .map(|(t, e)| ClipEvent {
                    dtick: e.tick.saturating_sub(lo),
                    seq: e.seq,
                    track: t,
                    raw_body: e.raw_body,
                    kind: e.kind,
                })
                .collect(),
            src,
            division: div,
        };
        let n = clip.events.len();
        cx.write_to_clipboard(clip_to_item(&clip));
        self.clipboard = Some(clip);
        if cut {
            self.delete_selected_area(area, cx);
        }
        self.status = tf("status.copied", &[("n", &n.to_string())]).into();
        cx.notify();
    }

    /// Insert a `Clip` at `anchor` — shared by paste/duplicate. Any event
    /// kinds; fresh ids; raw bodies preserved so SysEx/meta/text bytes
    /// round-trip (#142). A single-track copy lands on the active track;
    /// a multi-track copy keeps its own tracks; format 2 always lands in
    /// the viewed sequence (#143).
    pub(crate) fn insert_clip(
        &mut self,
        clip: &Clip,
        anchor: u64,
        label: &str,
        cx: &mut Context<Self>,
    ) {
        if clip.events.is_empty() {
            return;
        }
        // convert dticks when the source divided time differently
        let scale = match (clip.division, self.doc(|d| d.division)) {
            (smf_core::Division::Metrical(a), smf_core::Division::Metrical(b))
                if a != b && a > 0 =>
            {
                Some(b as f64 / a as f64)
            }
            _ => None,
        };
        let mut ops = Vec::new();
        let mut sel_ids = Vec::new();
        let mut sel_ev = Vec::new();
        {
            let mut sh = lock_shared(&self.shared);
            let ntr = sh.doc.tracks.len();
            // format 2: paste lands in the viewed sequence regardless of
            // which sequence the clipboard events were copied from
            let seq_target = sh
                .doc
                .is_sequential()
                .then(|| self.sel_track.min(ntr.saturating_sub(1)));
            let mut per_track: BTreeMap<usize, Vec<DocEvent>> = BTreeMap::new();
            for c in &clip.events {
                let track = seq_target.unwrap_or_else(|| {
                    if clip.src.is_some() {
                        self.sel_track.min(ntr.saturating_sub(1))
                    } else {
                        c.track.min(ntr.saturating_sub(1))
                    }
                });
                let d = c.dtick;
                let d = scale.map(|s| (d as f64 * s).round() as u64).unwrap_or(d);
                let tick = anchor + d;
                let id = sh.doc.alloc_event_id();
                match &c.kind {
                    EventKind::Channel { status, .. } if status & 0xF0 == 0x90 => {
                        sel_ids.push(id);
                    }
                    _ => sel_ev.push(id),
                }
                per_track.entry(track).or_default().push(DocEvent {
                    id,
                    tick,
                    seq: c.seq,
                    raw_body: c.raw_body.clone(),
                    kind: c.kind.clone(),
                });
            }
            for (track, events) in per_track {
                ops.push(Op::InsertEvents { track, events });
            }
        }
        self.apply_tx(label, ops);
        self.selection = sel_ids.into_iter().collect();
        self.sel_events = sel_ev.into_iter().collect();
        cx.notify();
    }

    /// Paste at the visible edit cursor — never re-snapped, never
    /// re-quantized (#143). The OS clipboard wins so another midi-editor
    /// process's copy pastes here; the in-process copy is the fallback
    /// (and covers hosts whose clipboard access fails) (#142).
    pub(crate) fn paste(&mut self, cx: &mut Context<Self>) {
        let ext = cx
            .read_from_clipboard()
            .and_then(|item| clip_from_item(&item));
        let clip = ext.or_else(|| self.clipboard.clone());
        let Some(clip) = clip else {
            self.status = t("status.noclip").into();
            cx.notify();
            return;
        };
        let anchor = self.cursor_tick;
        self.insert_clip(&clip, anchor, "paste", cx);
    }

    /// Duplicate the selection, tiled immediately after it (Ctrl+D).
    pub(crate) fn duplicate_selected(&mut self, cx: &mut Context<Self>) {
        // duplicates reuse the full clipboard capture so CC/meta pairs
        // tile the same way notes do
        let evs = self.selected_doc_events(FocusArea::Roll);
        if evs.is_empty() {
            self.status = t("status.nosel").into();
            cx.notify();
            return;
        }
        let lo = evs.iter().map(|(_, e)| e.tick).min().unwrap_or(0);
        let hi = evs.iter().map(|(_, e)| e.tick).max().unwrap_or(0) + 1;
        let div = lock_shared(&self.shared).doc.division;
        let clip = Clip {
            events: evs
                .into_iter()
                .map(|(t, e)| ClipEvent {
                    dtick: e.tick.saturating_sub(lo),
                    seq: e.seq,
                    track: t,
                    raw_body: e.raw_body,
                    kind: e.kind,
                })
                .collect(),
            // duplicate tiles in place — same tracks as the source
            src: None,
            division: div,
        };
        self.insert_clip(&clip, hi, "duplicate", cx);
    }

    /// Move every selected note by (dtick, dkey) — arrow-key nudge.
    pub(crate) fn nudge(&mut self, dtick: i64, dkey: i32, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        let mut ops = Vec::new();
        {
            let sh = lock_shared(&self.shared);
            for n in self
                .notes
                .iter()
                .filter(|n| self.selection.contains(&n.on_id))
            {
                let nk = (n.key as i32 + dkey).clamp(0, 127) as u8;
                if dtick == 0 && nk == n.key {
                    continue;
                }
                // the track may have been removed by an MCP edit/undo since
                // the note view was built — skip instead of indexing into it
                let Some(track) = sh.doc.tracks.get(n.track) else {
                    continue;
                };
                for e in track.events.iter() {
                    if e.id != n.on_id && n.off_id != Some(e.id) {
                        continue;
                    }
                    let mut after = e.clone();
                    after.tick = after.tick.saturating_add_signed(dtick);
                    // the pitch moves on BOTH ends — an off left at the old
                    // key leaves the on dangling and re-pairs wrong
                    if let EventKind::Channel { data, .. } = &mut after.kind {
                        data[0] = nk;
                    }
                    ops.push(Op::UpdateEvent {
                        track: n.track,
                        before: e.clone(),
                        after,
                    });
                }
            }
        }
        if !ops.is_empty() {
            self.apply_tx("nudge", ops);
        }
        cx.notify();
    }

    /// Up/down in the lane: ±velocity on the selected notes.
    pub(crate) fn nudge_vel(&mut self, dv: i32, cx: &mut Context<Self>) {
        if self.selection.is_empty() {
            return;
        }
        let mut ops = Vec::new();
        {
            let sh = lock_shared(&self.shared);
            for n in self
                .notes
                .iter()
                .filter(|n| self.selection.contains(&n.on_id))
            {
                let nv = (n.vel as i32 + dv).clamp(1, 127) as u8;
                if nv == n.vel {
                    continue;
                }
                let Some(track) = sh.doc.tracks.get(n.track) else {
                    continue;
                };
                for e in track.events.iter() {
                    if e.id != n.on_id {
                        continue;
                    }
                    let mut after = e.clone();
                    if let EventKind::Channel { data, .. } = &mut after.kind {
                        data[1] = nv;
                    }
                    ops.push(Op::UpdateEvent {
                        track: n.track,
                        before: e.clone(),
                        after,
                    });
                }
            }
        }
        if !ops.is_empty() {
            self.apply_tx("set velocity", ops);
        }
        cx.notify();
    }

    pub(crate) fn commit_drag(&mut self, cx: &mut Context<Self>) {
        // every release ends any sounding preview (draw scrub, pitch drag,
        // key strip) — the worker's own deadline is the backstop
        let was_scrub = self.scrub_key.is_some();
        self.audition_off();
        if was_scrub {
            cx.notify();
        }
        let Some(d) = self.drag.take() else { return };
        match d.mode {
            DragMode::Erase => {
                self.commit_erase(cx);
                return;
            }
            DragMode::Marquee => {
                // draw tool: the drag box (or a click's point) becomes a note
                if self.tool == Tool::Draw {
                    if (0..=127).contains(&d.a_key) {
                        let (a, b) = (d.a_tick.min(d.b_tick), d.a_tick.max(d.b_tick));
                        let len = (b - a).max(self.snap_ticks().max(1));
                        self.insert_note_len(a.max(0) as u64, d.a_key as u8, len as u64, cx);
                    }
                    return;
                }
                // rect select: notes intersecting the rubber-band box — the
                // box corners are keys from hit(); compare in row space so a
                // folded view selects exactly the visible rows it covers.
                // Same sequence gate as note_at/edge_at: a marquee must
                // never select another sequence's ghosts for deletion.
                let (t0, t1) = (d.a_tick.min(d.b_tick), d.a_tick.max(d.b_tick));
                let row_sel = |key: i32| -> i32 {
                    if (0..=127).contains(&key) {
                        self.row_of[key as usize]
                    } else {
                        -1
                    }
                };
                let (r0, r1) = (
                    row_sel(d.a_key).min(row_sel(d.b_key)),
                    row_sel(d.a_key).max(row_sel(d.b_key)),
                );
                let seq = self.is_seq();
                self.selection = self
                    .notes
                    .iter()
                    .filter(|n| {
                        (!seq || n.track == self.sel_track) && {
                            let st = n.start_tick as i64;
                            let en = n.end_tick.unwrap_or(n.start_tick) as i64;
                            let row = self.row_of[n.key as usize];
                            st <= t1 && en >= t0 && row >= r0 && row <= r1
                        }
                    })
                    .map(|n| n.on_id)
                    .collect();
                cx.notify();
                return;
            }
            DragMode::Resize => {
                if d.dtick == 0 {
                    return;
                }
                let Some(orig_end) = d.orig_end else { return };
                let new_end = (self
                    .snap_round(orig_end as i64 + d.dtick)
                    .max(d.orig_start as i64 + 1)) as u64;
                let sh = lock_shared(&self.shared);
                let mut ops = Vec::new();
                if let Some(off_id) = d.off_id {
                    // the track may be gone (MCP remove/undo during the drag)
                    if let Some(track) = sh.doc.tracks.get(d.track) {
                        for e in &track.events {
                            if e.id == off_id {
                                let mut after = e.clone();
                                after.tick = new_end;
                                ops.push(Op::UpdateEvent {
                                    track: d.track,
                                    before: e.clone(),
                                    after,
                                });
                            }
                        }
                    }
                }
                drop(sh);
                if !ops.is_empty() {
                    self.apply_tx("resize note", ops);
                }
                cx.notify();
                return;
            }
            DragMode::Velocity => {
                let vel = d.dkey.clamp(1, 127) as u8;
                let sh = lock_shared(&self.shared);
                let mut ops = Vec::new();
                if let Some(track) = sh.doc.tracks.get(d.track) {
                    for e in &track.events {
                        if e.id == d.on_id {
                            let mut after = e.clone();
                            if let EventKind::Channel { data, .. } = &mut after.kind {
                                data[1] = vel;
                            }
                            ops.push(Op::UpdateEvent {
                                track: d.track,
                                before: e.clone(),
                                after,
                            });
                        }
                    }
                }
                drop(sh);
                if !ops.is_empty() {
                    self.apply_tx("set velocity", ops);
                }
                cx.notify();
                return;
            }
            DragMode::Move | DragMode::Duplicate => {}
            DragMode::LaneMarquee => {
                // rubber-band inside a lane: select every lane event in the
                // (tick, value) box; Velocity mode selects notes instead
                let (t0, t1) = (d.a_tick.min(d.b_tick).max(0), d.a_tick.max(d.b_tick).max(0));
                let (v0, v1) = (d.a_key.min(d.b_key), d.a_key.max(d.b_key));
                let cfg = self.lanes.get(d.lane).copied().unwrap_or_default();
                if cfg.mode == LaneMode::Velocity {
                    self.selection = self
                        .notes
                        .iter()
                        .filter(|n| {
                            n.track == self.sel_track
                                && n.start_tick as i64 >= t0
                                && n.start_tick as i64 <= t1
                                && n.vel as i32 >= v0
                                && n.vel as i32 <= v1
                        })
                        .map(|n| n.on_id)
                        .collect();
                } else {
                    self.lane_sel = self
                        .lane_events_cached(cfg.mode, cfg.poly_key)
                        .iter()
                        .filter(|(_, tick, val, _key)| {
                            *tick as i64 >= t0 && *tick as i64 <= t1 && *val >= v0 && *val <= v1
                        })
                        .map(|(id, _, _, _)| *id)
                        .collect();
                }
                cx.notify();
                return;
            }
            DragMode::LaneResize => {
                // the height already tracked the cursor in update_drag —
                // committing only persists the new layout
                self.persist();
                cx.notify();
                return;
            }
            DragMode::LaneEvent => {
                // CC/PB/AT lane: update an existing event's value, or insert a
                // new one when the drag started on empty lane space
                // snap before locking: snap_down -> doc() re-acquires `sh`
                let ins_tick = self.snap_down(d.a_tick).max(0) as u64;
                let mut sh = lock_shared(&self.shared);
                let mut ops = Vec::new();
                // the track may be gone (MCP remove/undo during the drag)
                let Some(track_events) = sh.doc.tracks.get(d.track) else {
                    drop(sh);
                    cx.notify();
                    return;
                };
                let cfg = self.lanes.get(d.lane).copied().unwrap_or_default();
                let lane_mode = cfg.mode;
                if d.on_id == 0 {
                    let ch = self.edit_channel_of(d.track, track_events.out_channel);
                    let (status, data, len) = match lane_mode {
                        LaneMode::CC(cc) => (0xB0 | ch, [cc, d.dkey.clamp(0, 127) as u8], 2u8),
                        LaneMode::PitchBend => {
                            let v = d.dkey.clamp(0, 16383) as u16;
                            (0xE0 | ch, [(v & 0x7F) as u8, (v >> 7) as u8], 2)
                        }
                        LaneMode::ChanAT => (0xD0 | ch, [d.dkey.clamp(0, 127) as u8, 0], 1),
                        LaneMode::PolyAT => {
                            let key = cfg.poly_key.unwrap_or_else(|| {
                                // no filter: use the key of a note at that tick,
                                // else middle C
                                self.notes
                                    .iter()
                                    .filter(|n| {
                                        n.track == d.track
                                            && n.start_tick <= d.a_tick.max(0) as u64
                                            && n.end_tick.unwrap_or(u64::MAX)
                                                > d.a_tick.max(0) as u64
                                    })
                                    .min_by_key(|n| n.start_tick)
                                    .map(|n| n.key)
                                    .unwrap_or(60)
                            });
                            (0xA0 | ch, [key, d.dkey.clamp(0, 127) as u8], 2)
                        }
                        LaneMode::Velocity => unreachable!(),
                    };
                    let id = sh.doc.alloc_event_id();
                    ops.push(Op::InsertEvents {
                        track: d.track,
                        events: vec![DocEvent {
                            id,
                            tick: ins_tick,
                            seq: 0,
                            raw_body: None,
                            kind: EventKind::Channel { status, data, len },
                        }],
                    });
                } else {
                    for e in &track_events.events {
                        if e.id == d.on_id {
                            let mut after = e.clone();
                            if let EventKind::Channel { data, .. } = &mut after.kind {
                                match lane_mode {
                                    LaneMode::CC(_) => data[1] = d.dkey.clamp(0, 127) as u8,
                                    LaneMode::PitchBend => {
                                        let v = d.dkey.clamp(0, 16383) as u16;
                                        data[0] = (v & 0x7F) as u8;
                                        data[1] = (v >> 7) as u8;
                                    }
                                    LaneMode::ChanAT => data[0] = d.dkey.clamp(0, 127) as u8,
                                    LaneMode::PolyAT => data[1] = d.dkey.clamp(0, 127) as u8,
                                    LaneMode::Velocity => unreachable!(),
                                }
                            }
                            ops.push(Op::UpdateEvent {
                                track: d.track,
                                before: e.clone(),
                                after,
                            });
                        }
                    }
                }
                drop(sh);
                if !ops.is_empty() {
                    self.apply_tx("edit lane", ops);
                }
                cx.notify();
                return;
            }
        }
        if d.dtick == 0 && d.dkey == 0 {
            return;
        }
        // magnet: the dragged note's resulting start snaps to the grid
        let d = Drag {
            dtick: self.snap_round(d.orig_start as i64 + d.dtick) - d.orig_start as i64,
            ..d
        };
        if d.dtick == 0 && d.dkey == 0 {
            return;
        }
        // move every selected note by the same delta; fall back to the
        // dragged note if the selection never took (e.g. programmatic)
        let ids: BTreeSet<EventId> = if self.selection.contains(&d.on_id) {
            self.selection.clone()
        } else {
            BTreeSet::from([d.on_id])
        };
        let notes = self.notes.clone();
        let duplicate = d.mode == DragMode::Duplicate;
        let mut sh = lock_shared(&self.shared);
        // two passes: shift-and-clone while iterating immutably, mint ids after
        let mut staged: Vec<(usize, document::Event, bool)> = Vec::new();
        for (ti, t) in sh.doc.tracks.iter().enumerate() {
            for e in &t.events {
                let Some(n) = notes.iter().find(|n| {
                    ids.contains(&n.on_id) && (e.id == n.on_id || n.off_id == Some(e.id))
                }) else {
                    continue;
                };
                let is_on = e.id == n.on_id;
                let orig = if is_on {
                    n.start_tick
                } else {
                    n.end_tick.unwrap_or(n.start_tick)
                };
                let mut after = e.clone();
                after.tick = ((orig as i64 + d.dtick).max(0)) as u64;
                if let EventKind::Channel { data, .. } = &mut after.kind {
                    data[0] = (n.key as i32 + d.dkey).clamp(0, 127) as u8;
                }
                // raw_body is dropped centrally when the kind changed; a
                // pure time move keeps the verbatim body bytes
                staged.push((ti, after, is_on));
            }
        }
        let mut ops = Vec::new();
        for (ti, mut after, _is_on) in staged {
            if duplicate {
                after.id = sh.doc.alloc_event_id();
                ops.push(Op::InsertEvents {
                    track: ti,
                    events: vec![after],
                });
            } else {
                let before = sh.doc.tracks[ti]
                    .events
                    .iter()
                    .find(|e| {
                        e.id == {
                            // original id is preserved on `after` for Move
                            after.id
                        }
                    })
                    .cloned();
                if let Some(before) = before {
                    ops.push(Op::UpdateEvent {
                        track: ti,
                        before,
                        after,
                    });
                }
            }
        }
        drop(sh);
        if !ops.is_empty() {
            self.apply_tx(
                if duplicate {
                    "duplicate notes"
                } else {
                    "move notes"
                },
                ops,
            );
        }
        cx.notify();
    }
}

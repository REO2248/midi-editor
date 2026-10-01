//! SMF read/write layer.
//!
//! Read: midly (zero-copy, `SmfBytemap` gives each event's raw byte span).
//! Write: custom serializer that re-emits untouched events byte-verbatim and
//! only encodes new/edited events. Note: midly's bytemap span is the event
//! body *excluding* the leading delta VLQ (the status byte IS included for
//! channel events; a body starting <0x80 is a running-status event).

use bytes::Bytes;
use midly::{MetaMessage, MidiMessage, SmfBytemap, TrackEventKind};
use std::panic::{catch_unwind, AssertUnwindSafe};
use thiserror::Error;

/// GM/GS/XG display-name tables + reset-SysEx detection (display only).
pub mod gm;
pub use gm::{gm_drum_name, gm_program_name, kit_name, reset_hint, ModeHint};

#[derive(Debug, Error)]
pub enum Error {
    #[error("SMF parse failed: {0}")]
    Parse(String),
    #[error("parser panicked on malformed input")]
    Panic,
    /// Input is well-formed but exceeds a configured safety limit —
    /// reported separately from `Parse` so callers can tell
    /// "unsupported due to safety limit" apart from "malformed".
    #[error("input exceeds the '{limit}' safety limit: {actual} > {allowed}")]
    LimitExceeded {
        limit: &'static str,
        actual: u64,
        allowed: u64,
    },
}

/// Resource limits applied while parsing potentially hostile SMF input.
/// Byte-length checks run before any allocation; count checks run during
/// event materialization, so a pathological file fails with
/// `Error::LimitExceeded` instead of driving memory or CPU to exhaustion.
///
/// Derived structures (document notes, controller caches) stay bounded by
/// construction: every note/controller requires at least one event, so the
/// event caps bound them transitively.
#[derive(Debug, Clone)]
pub struct Limits {
    /// raw input bytes; checked before the parser runs
    pub max_file_bytes: usize,
    /// number of MTrk chunks accepted
    pub max_tracks: usize,
    /// total decoded events across all tracks
    pub max_events: usize,
    /// decoded events within a single track
    pub max_track_events: usize,
    /// payload bytes of a single meta / SysEx / escape event
    pub max_event_payload: usize,
}

impl Default for Limits {
    /// Generous but bounded: well past any real-world MIDI file (the
    /// largest published scores are a few MB and ~10⁵ events) yet far
    /// below anything that could exhaust desktop memory.
    fn default() -> Self {
        Self {
            max_file_bytes: 256 * 1024 * 1024,
            max_tracks: 4096,
            max_events: 8_000_000,
            max_track_events: 4_000_000,
            max_event_payload: 64 * 1024 * 1024,
        }
    }
}

impl Limits {
    /// Headless expert override: `MIDI_EDITOR_UNLIMITED_PARSE=1` (or
    /// `true`) opts out of every check — a deliberate choice for users who
    /// knowingly load out-of-limit files. Anything else keeps the
    /// documented defaults.
    pub fn from_env() -> Self {
        match std::env::var("MIDI_EDITOR_UNLIMITED_PARSE") {
            Ok(v) if v == "1" || v.eq_ignore_ascii_case("true") => Self::unlimited(),
            _ => Self::default(),
        }
    }

    /// Deliberate expert override for tooling that knows its input is
    /// trusted (corpus checkers, the fuzzer) — bypasses every check.
    pub fn unlimited() -> Self {
        Self {
            max_file_bytes: usize::MAX,
            max_tracks: usize::MAX,
            max_events: usize::MAX,
            max_track_events: usize::MAX,
            max_event_payload: usize::MAX,
        }
    }
}

fn limit_check(limit: &'static str, actual: usize, allowed: usize) -> Result<(), Error> {
    if actual > allowed {
        Err(Error::LimitExceeded {
            limit,
            actual: actual as u64,
            allowed: allowed as u64,
        })
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    /// channel voice/mode event; status byte includes channel nibble
    Channel { status: u8, data: [u8; 2], len: u8 },
    /// meta event: type byte + raw payload (encoding-agnostic, Shift-JIS safe)
    Meta { meta_type: u8, data: Bytes },
    /// F0 sysex payload, without the leading F0
    SysEx(Bytes),
    /// F7 escape sequence payload, without the leading F7
    Escape(Bytes),
}

#[derive(Debug, Clone)]
pub struct Event {
    /// absolute tick within the track
    pub tick: u64,
    /// ordering inside the same tick (file order)
    pub seq: u32,
    /// raw event bytes minus the leading delta VLQ. `None` = synthesized/edited.
    /// May start with a data byte (<0x80) when the source used running status.
    pub raw_body: Option<Bytes>,
    pub kind: EventKind,
}

#[derive(Debug)]
pub struct Track {
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Division {
    Metrical(u16),
    /// semantic fps (24/25/29/30) + ticks per frame
    Smpte {
        fps: u8,
        ticks_per_frame: u8,
    },
}

impl Default for Division {
    /// 480 PPQ — the de facto modern SMF resolution.
    fn default() -> Self {
        Self::Metrical(480)
    }
}

#[derive(Debug)]
pub struct File {
    pub format: u16,
    pub division: Division,
    pub tracks: Vec<Track>,
    /// non-fatal problems seen during load (non-MTrk chunks midly drops, etc.)
    pub warnings: Vec<String>,
}

pub fn parse(raw: &[u8]) -> Result<File, Error> {
    parse_with_limits(raw, &Limits::default())
}

/// `parse` under explicit resource limits — the entry point for callers
/// that need a non-default (or deliberate `Limits::unlimited()`) budget.
pub fn parse_with_limits(raw: &[u8], limits: &Limits) -> Result<File, Error> {
    limit_check("file size", raw.len(), limits.max_file_bytes)?;
    match catch_unwind(AssertUnwindSafe(|| SmfBytemap::parse(raw))) {
        Ok(Ok(m)) => file_from_map(raw, m, limits),
        Ok(Err(e)) => {
            // keep the strict-parser reason even when lenient recovery also
            // fails — "no MTrk chunks" alone hides the real defect
            parse_lenient_with_limits(raw, limits)
                .map(|mut f| {
                    f.warnings.insert(
                        0,
                        format!("strict parse failed ({e}); lenient recovery used"),
                    );
                    f
                })
                .map_err(|_| Error::Parse(format!("{e} (lenient recovery also failed)")))
        }
        Err(_) => parse_lenient_with_limits(raw, limits).map(|mut f| {
            f.warnings
                .insert(0, "strict parser panicked; lenient recovery used".into());
            f
        }),
    }
}

/// The strict (midly-backed) parse alone: no `catch_unwind` guard and no
/// lenient fallback, so errors — and panics — reach the caller unfiltered.
/// `parse` is the guarded entry point for untrusted input; this one exists
/// for fuzzers and validators that must see the raw behavior.
pub fn parse_strict(raw: &[u8]) -> Result<File, Error> {
    let map = SmfBytemap::parse(raw).map_err(|e| Error::Parse(format!("{e}")))?;
    file_from_map(raw, map, &Limits::unlimited())
}

fn file_from_map(raw: &[u8], map: SmfBytemap<'_>, limits: &Limits) -> Result<File, Error> {
    let mut warnings = Vec::new();
    detect_extra_chunks(raw, &mut warnings);

    let format = match map.header.format {
        midly::Format::SingleTrack => 0,
        midly::Format::Parallel => 1,
        midly::Format::Sequential => 2,
    };
    let division = match map.header.timing {
        midly::Timing::Metrical(t) => Division::Metrical(t.as_int()),
        midly::Timing::Timecode(fps, tpf) => Division::Smpte {
            fps: fps.as_int(),
            ticks_per_frame: tpf,
        },
    };

    limit_check("track count", map.tracks.len(), limits.max_tracks)?;
    let mut total_events = 0usize;
    let mut tracks = Vec::new();
    for t in map.tracks.iter() {
        let mut tick: u64 = 0;
        let mut events = Vec::new();
        for (seq, (span, ev)) in t.iter().enumerate() {
            limit_check("events in a track", seq + 1, limits.max_track_events)?;
            total_events += 1;
            limit_check("total events", total_events, limits.max_events)?;
            tick += ev.delta.as_int() as u64;
            let kind = convert_kind(&ev.kind);
            let payload = match &kind {
                EventKind::Meta { data, .. } | EventKind::SysEx(data) | EventKind::Escape(data) => {
                    data.len()
                }
                EventKind::Channel { .. } => 0,
            };
            limit_check("event payload", payload, limits.max_event_payload)?;
            events.push(Event {
                tick,
                seq: seq as u32,
                raw_body: Some(Bytes::copy_from_slice(span)),
                kind,
            });
        }
        tracks.push(Track { events });
    }

    Ok(File {
        format,
        division,
        tracks,
        warnings,
    })
}

fn detect_extra_chunks(raw: &[u8], warnings: &mut Vec<String>) {
    if raw.len() >= 4 && &raw[0..4] == b"RIFF" {
        // midly unwraps the RMID container; the writer emits bare SMF, so
        // the container is lost on round trip — say so instead of silence
        warnings.push(
            "RIFF/RMID container: the SMF payload is unwrapped on load; saving writes a bare SMF file".into(),
        );
        return;
    }
    if raw.len() < 14 || raw[0..4] != *b"MThd" {
        return;
    }
    let hlen = u32::from_be_bytes(raw[4..8].try_into().unwrap()) as usize;
    let mut pos = 8 + hlen;
    while pos + 8 <= raw.len() {
        let id = &raw[pos..pos + 4];
        let len = u32::from_be_bytes(raw[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if id != b"MTrk" {
            warnings.push(format!(
                "non-MTrk chunk {:?} ({} bytes) at offset {} is not preserved by the parser",
                String::from_utf8_lossy(id),
                len,
                pos
            ));
        }
        pos += 8 + len;
    }
    if pos < raw.len() {
        warnings.push(format!(
            "{} trailing bytes after last chunk",
            raw.len() - pos
        ));
    }
}

/// Tolerant event walker used when the strict parser rejects a file. Accepts
/// the common real-world violations: running status surviving meta/sysex
/// events, payload lengths over-running the chunk, and truncated tails.
/// Output is normalized on write (full status bytes), so the pipeline stays a
/// fixpoint even though byte-exactness is lost. Public so fuzzers can target
/// the recovery path directly; `parse` remains the normal entry point.
pub fn parse_lenient(raw: &[u8]) -> Result<File, Error> {
    parse_lenient_with_limits(raw, &Limits::default())
}

/// Lenient parse under explicit resource limits (see
/// `parse_with_limits`).
pub fn parse_lenient_with_limits(raw: &[u8], limits: &Limits) -> Result<File, Error> {
    limit_check("file size", raw.len(), limits.max_file_bytes)?;
    if raw.len() < 14 || &raw[0..4] != b"MThd" {
        return Err(Error::Parse("missing MThd header".into()));
    }
    let hlen = u32::from_be_bytes(raw[4..8].try_into().unwrap()) as usize;
    if hlen < 6 || 8 + hlen > raw.len() {
        return Err(Error::Parse("bad MThd length".into()));
    }
    let format = u16::from_be_bytes(raw[8..10].try_into().unwrap());
    let division = if raw[12] & 0x80 != 0 {
        Division::Smpte {
            fps: (-(raw[12] as i8 as i16)) as u8,
            ticks_per_frame: raw[13],
        }
    } else {
        Division::Metrical(u16::from_be_bytes(raw[12..14].try_into().unwrap()))
    };

    let mut warnings = Vec::new();
    detect_extra_chunks(raw, &mut warnings);

    let mut tracks = Vec::new();
    let mut total_events = 0usize;
    let mut pos = 8 + hlen;
    while pos + 8 <= raw.len() {
        let id = &raw[pos..pos + 4];
        let len = u32::from_be_bytes(raw[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_end = pos.saturating_add(8).saturating_add(len).min(raw.len());
        if id == b"MTrk" {
            limit_check("track count", tracks.len() + 1, limits.max_tracks)?;
            tracks.push(track_lenient(
                &raw[pos + 8..body_end],
                tracks.len(),
                &mut warnings,
                limits,
                &mut total_events,
            )?);
        }
        let next = pos.saturating_add(8).saturating_add(len);
        if next <= pos {
            break; // corrupt length — stop scanning chunks
        }
        pos = next;
    }
    if tracks.is_empty() {
        return Err(Error::Parse("no MTrk chunks".into()));
    }
    Ok(File {
        format,
        division,
        tracks,
        warnings,
    })
}

fn read_vlq_lenient(data: &[u8], mut p: usize) -> (u64, usize) {
    let mut v: u64 = 0;
    for _ in 0..10 {
        match data.get(p) {
            Some(&b) => {
                p += 1;
                // wrapping: a hostile 10-byte VLQ must not overflow; the
                // value is clamped to the buffer by callers anyway
                v = v.wrapping_shl(7) | (b & 0x7f) as u64;
                if b & 0x80 == 0 {
                    break;
                }
            }
            None => break,
        }
    }
    (v, p)
}

fn track_lenient(
    data: &[u8],
    tno: usize,
    warnings: &mut Vec<String>,
    limits: &Limits,
    total_events: &mut usize,
) -> Result<Track, Error> {
    let mut events = Vec::new();
    let mut tick = 0u64;
    let mut running: Option<u8> = None;
    let mut p = 0usize;
    while p < data.len() {
        let (delta, np) = read_vlq_lenient(data, p);
        p = np;
        if p >= data.len() {
            break;
        }
        tick = tick.saturating_add(delta);
        let ev_start = p;
        let st = data[p];
        let kind;
        let mut clean = true; // false when the raw span must not be re-emitted
        if st == 0xFF {
            p += 1;
            let Some(&mt) = data.get(p) else { break };
            p += 1;
            let (l, np) = read_vlq_lenient(data, p);
            if np == p || data[np - 1] & 0x80 != 0 {
                // the length VLQ is missing or continues past the chunk end —
                // the raw isn't self-delimiting, so verbatim re-emission would
                // let the next reader continue the VLQ into the following
                // event's delta bytes
                clean = false;
            }
            p = np;
            // u64 math: a corrupt VLQ length can exceed usize and must not
            // overflow the pointer arithmetic
            let overruns = l > (data.len() - p) as u64;
            let end = if overruns { data.len() } else { p + l as usize };
            if overruns {
                warnings.push(format!(
                    "track {tno}: meta 0x{mt:02x} payload overruns chunk (clamped)"
                ));
                clean = false; // raw_body holds the bogus declared length
            }
            kind = EventKind::Meta {
                meta_type: mt,
                data: Bytes::copy_from_slice(&data[p..end]),
            };
            p = end;
            // note: running status intentionally NOT cleared — old sequencers
            // emit running-status data bytes right after meta events
        } else if st == 0xF0 || st == 0xF7 {
            let is_sysex = st == 0xF0;
            p += 1;
            let (l, np) = read_vlq_lenient(data, p);
            if np == p || data[np - 1] & 0x80 != 0 {
                clean = false; // missing/unterminated length VLQ — see meta branch
            }
            p = np;
            let overruns = l > (data.len() - p) as u64;
            let end = if overruns { data.len() } else { p + l as usize };
            if overruns {
                warnings.push(format!(
                    "track {tno}: sysex/escape payload overruns chunk (clamped)"
                ));
                clean = false;
            }
            let payload = Bytes::copy_from_slice(&data[p..end]);
            kind = if is_sysex {
                EventKind::SysEx(payload)
            } else {
                EventKind::Escape(payload)
            };
            p = end;
        } else if (0x80..0xF0).contains(&st) {
            running = Some(st);
            p += 1;
            let want = if matches!(st >> 4, 0xC | 0xD) { 1 } else { 2 };
            if p + want > data.len() {
                warnings.push(format!("track {tno}: truncated channel event at end"));
                break;
            }
            kind = EventKind::Channel {
                status: st,
                data: [data[p], data.get(p + 1).copied().unwrap_or(0)],
                len: want as u8,
            };
            p += want;
        } else if st > 0xF0 {
            // F1-F6 system common / F8-FE realtime: not channel events and
            // have their own (often zero) data widths, so consuming them as
            // two-byte channel data corrupts the stream — and would make
            // lenient's channel set disagree with the writer's (0x80..0xF0).
            // Keep each byte as an opaque event so it round-trips verbatim.
            // Realtime bytes legally interleave inside running-status events,
            // which is why `running` is left untouched.
            warnings.push(format!(
                "track {tno}: system byte 0x{st:02x} kept as opaque event"
            ));
            kind = EventKind::Escape(Bytes::new());
            p += 1;
        } else {
            let Some(st) = running else {
                warnings.push(format!(
                    "track {tno}: data byte 0x{st:02x} with no running status — event dropped"
                ));
                p += 1;
                continue;
            };
            let want = if matches!(st >> 4, 0xC | 0xD) { 1 } else { 2 };
            if p + want > data.len() {
                warnings.push(format!("track {tno}: truncated channel event at end"));
                break;
            }
            kind = EventKind::Channel {
                status: st,
                data: [data[p], data.get(p + 1).copied().unwrap_or(0)],
                len: want as u8,
            };
            p += want;
        }
        let payload = match &kind {
            EventKind::Meta { data, .. } | EventKind::SysEx(data) | EventKind::Escape(data) => {
                data.len()
            }
            EventKind::Channel { .. } => 0,
        };
        limit_check("event payload", payload, limits.max_event_payload)?;
        limit_check(
            "events in a track",
            events.len() + 1,
            limits.max_track_events,
        )?;
        *total_events += 1;
        limit_check("total events", *total_events, limits.max_events)?;
        events.push(Event {
            tick,
            seq: events.len() as u32,
            raw_body: clean.then(|| Bytes::copy_from_slice(&data[ev_start..p])),
            kind,
        });
    }
    Ok(Track { events })
}

fn convert_kind(kind: &TrackEventKind<'_>) -> EventKind {
    match kind {
        TrackEventKind::Midi { channel, message } => {
            let status = midi_status(message) | channel.as_int();
            let (d0, d1, len) = midi_data(message);
            EventKind::Channel {
                status,
                data: [d0, d1],
                len,
            }
        }
        TrackEventKind::SysEx(data) => EventKind::SysEx(Bytes::copy_from_slice(data)),
        TrackEventKind::Escape(data) => EventKind::Escape(Bytes::copy_from_slice(data)),
        TrackEventKind::Meta(m) => meta_to_kind(m),
    }
}

fn midi_status(m: &MidiMessage) -> u8 {
    match m {
        MidiMessage::NoteOff { .. } => 0x80,
        MidiMessage::NoteOn { .. } => 0x90,
        MidiMessage::Aftertouch { .. } => 0xA0,
        MidiMessage::Controller { .. } => 0xB0,
        MidiMessage::ProgramChange { .. } => 0xC0,
        MidiMessage::ChannelAftertouch { .. } => 0xD0,
        MidiMessage::PitchBend { .. } => 0xE0,
    }
}

fn midi_data(m: &MidiMessage) -> (u8, u8, u8) {
    match m {
        MidiMessage::NoteOff { key, vel } | MidiMessage::NoteOn { key, vel } => {
            (key.as_int(), vel.as_int(), 2)
        }
        MidiMessage::Aftertouch { key, vel } => (key.as_int(), vel.as_int(), 2),
        MidiMessage::Controller { controller, value } => (controller.as_int(), value.as_int(), 2),
        MidiMessage::ProgramChange { program } => (program.as_int(), 0, 1),
        MidiMessage::ChannelAftertouch { vel } => (vel.as_int(), 0, 1),
        MidiMessage::PitchBend { bend } => {
            let v = bend.as_int();
            ((v & 0x7F) as u8, (v >> 7) as u8, 2)
        }
    }
}

fn meta(meta_type: u8, data: &[u8]) -> EventKind {
    EventKind::Meta {
        meta_type,
        data: Bytes::copy_from_slice(data),
    }
}

fn meta_to_kind(m: &MetaMessage<'_>) -> EventKind {
    match m {
        MetaMessage::TrackNumber(n) => match n {
            Some(v) => meta(0x00, &v.to_be_bytes()),
            None => meta(0x00, &[]),
        },
        MetaMessage::Text(d) => meta(0x01, d),
        MetaMessage::Copyright(d) => meta(0x02, d),
        MetaMessage::TrackName(d) => meta(0x03, d),
        MetaMessage::InstrumentName(d) => meta(0x04, d),
        MetaMessage::Lyric(d) => meta(0x05, d),
        MetaMessage::Marker(d) => meta(0x06, d),
        MetaMessage::CuePoint(d) => meta(0x07, d),
        MetaMessage::ProgramName(d) => meta(0x08, d),
        MetaMessage::DeviceName(d) => meta(0x09, d),
        MetaMessage::MidiChannel(ch) => meta(0x20, &[ch.as_int()]),
        MetaMessage::MidiPort(p) => meta(0x21, &[p.as_int()]),
        MetaMessage::EndOfTrack => meta(0x2F, &[]),
        MetaMessage::Tempo(t) => meta(0x51, &t.as_int().to_be_bytes()[1..]),
        MetaMessage::SmpteOffset(o) => {
            let rr: u8 = match o.fps() {
                midly::Fps::Fps24 => 0,
                midly::Fps::Fps25 => 1,
                midly::Fps::Fps29 => 2,
                midly::Fps::Fps30 => 3,
            };
            meta(
                0x54,
                &[
                    (rr << 5) | (o.hour() & 0x1F),
                    o.minute(),
                    o.second(),
                    o.frame(),
                    o.subframe(),
                ],
            )
        }
        MetaMessage::TimeSignature(n, d, c, t) => meta(0x58, &[*n, *d, *c, *t]),
        MetaMessage::KeySignature(sf, minor) => meta(0x59, &[*sf as u8, *minor as u8]),
        MetaMessage::SequencerSpecific(d) => meta(0x7F, d),
        MetaMessage::Unknown(ty, d) => meta(*ty, d),
    }
}

// ---- writer ----

#[derive(Debug, Clone, Copy, Default)]
pub struct WriteOptions {
    /// collapse consecutive same-status channel events to running status
    pub running_status: bool,
}

pub fn write_vlq(mut v: u64, out: &mut Vec<u8>) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    for b in &buf[i..buf.len() - 1] {
        out.push(b | 0x80);
    }
    out.push(buf[buf.len() - 1]);
}

fn encode_body(kind: &EventKind, out: &mut Vec<u8>) {
    match kind {
        EventKind::Channel {
            status,
            data,
            len: _,
        } => {
            out.push(*status);
            out.push(data[0]);
            // program change (0xC0..) and channel pressure (0xD0..) carry
            // exactly one data byte, every other channel status two — the
            // stored len can't be trusted: emitting the wrong count
            // desynchronizes the stream for conforming readers
            if !(0xC0..0xE0).contains(status) {
                out.push(data[1]);
            }
        }
        EventKind::Meta { meta_type, data } => {
            out.push(0xFF);
            out.push(*meta_type);
            write_vlq(data.len() as u64, out);
            out.extend_from_slice(data);
        }
        EventKind::SysEx(data) => {
            out.push(0xF0);
            write_vlq(data.len() as u64, out);
            out.extend_from_slice(data);
        }
        EventKind::Escape(data) => {
            out.push(0xF7);
            write_vlq(data.len() as u64, out);
            out.extend_from_slice(data);
        }
    }
}

/// status byte to emit before a raw body that begins with a data byte
/// (the source relied on running status). Only channel events qualify.
fn implied_status(kind: &EventKind) -> Option<u8> {
    match kind {
        EventKind::Channel { status, .. } => Some(*status),
        _ => None,
    }
}

/// Serialize tracks to SMF bytes. Events keep their stored `raw_body` when
/// present; ordering is by (tick, seq). Byte-verbatim when nothing was edited.
pub fn write(format_req: u16, division: Division, tracks: &[Track], opts: WriteOptions) -> Vec<u8> {
    let division_raw = match division {
        // the field is 15 bits; masking keeps write() from emitting a
        // header that reparses as SMPTE timecode (and panics midly at 0x80)
        Division::Metrical(t) => t & 0x7FFF,
        Division::Smpte {
            fps,
            ticks_per_frame,
        } => {
            // fps is stored negated in the header; going through i16 keeps
            // the degenerate fps=128 (from the lenient parser) from
            // overflowing the i8 negation
            (((fps as i16).wrapping_neg()) as u8 as u16) << 8 | ticks_per_frame as u16
        }
    };
    // the declared format is written verbatim — format/track-count
    // coherence is an invariant of the *document* layer (Op::SetFormat),
    // not something the serializer repairs silently. The one degenerate
    // edge the writer still covers: format 0 declares exactly one track,
    // so an empty track list still writes a single (empty) MTrk — without
    // it the header would be a file our own reader rejects.
    let format = format_req;
    let empty_track;
    let tracks: &[Track] = if format == 0 && tracks.is_empty() {
        empty_track = Track { events: vec![] };
        std::slice::from_ref(&empty_track)
    } else {
        tracks
    };
    let ntrks = tracks.len() as u16;

    let mut out = Vec::new();
    out.extend_from_slice(b"MThd");
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&format.to_be_bytes());
    out.extend_from_slice(&ntrks.to_be_bytes());
    out.extend_from_slice(&division_raw.to_be_bytes());

    for track in tracks {
        let mut body = Vec::with_capacity(4096);
        let mut prev_tick = 0u64;
        let mut prev_status: Option<u8> = None;
        let mut sorted: Vec<&Event> = track.events.iter().collect();
        sorted.sort_by_key(|e| (e.tick, e.seq));

        // End-of-Track is the structural terminator: exactly one, always
        // last. Stored EOTs mid-stream are skipped and re-emitted at the
        // track's real end — their max tick is kept so an intentional
        // silent tail survives.
        let mut last_eot: Option<&Event> = None;
        let mut eot_tick = 0u64;
        for ev in sorted {
            if matches!(
                ev.kind,
                EventKind::Meta {
                    meta_type: 0x2F,
                    ..
                }
            ) {
                eot_tick = eot_tick.max(ev.tick);
                last_eot = Some(ev);
                prev_status = None; // a meta event ends running status
                continue;
            }
            let delta = ev.tick - prev_tick;
            prev_tick = ev.tick;
            write_vlq(delta, &mut body);

            let body_bytes = match &ev.raw_body {
                Some(raw) if !raw.is_empty() => raw.clone(),
                _ => {
                    let mut b = Vec::new();
                    encode_body(&ev.kind, &mut b);
                    Bytes::from(b)
                }
            };

            let first = body_bytes[0];
            if first < 0x80 {
                // body relies on running status from the source stream
                if let Some(st) = implied_status(&ev.kind) {
                    if prev_status != Some(st) {
                        body.push(st);
                    }
                    prev_status = Some(st);
                }
                body.extend_from_slice(&body_bytes);
            } else {
                let is_channel = (0x80..0xF0).contains(&first);
                if opts.running_status && is_channel && prev_status == Some(first) {
                    body.extend_from_slice(&body_bytes[1..]);
                } else {
                    body.extend_from_slice(&body_bytes);
                }
                prev_status = if is_channel { Some(first) } else { None };
            }
        }
        // emit the single terminator: at the stored max EOT tick or the
        // last content tick, whichever is later
        let end = eot_tick.max(prev_tick);
        write_vlq(end - prev_tick, &mut body);
        match last_eot {
            Some(ev) => {
                let body_bytes = match &ev.raw_body {
                    Some(raw) if !raw.is_empty() => raw.clone(),
                    _ => Bytes::from(vec![0xFF, 0x2F, 0x00]),
                };
                body.extend_from_slice(&body_bytes);
            }
            None => body.extend_from_slice(&[0xFF, 0x2F, 0x00]),
        }

        out.extend_from_slice(b"MTrk");
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
    }
    out
}

/// Guessed encoding of a meta text payload. SMF never specifies an encoding;
/// Shift-JIS is the de-facto standard for Japanese-authored files, Latin-1
/// for Western ones, and some modern tools write UTF-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    ShiftJis,
    /// anything that decodes as neither UTF-8 nor SJIS: raw-ish fallback
    Latin1,
}

impl TextEncoding {
    pub fn label(self) -> &'static str {
        match self {
            TextEncoding::Utf8 => "UTF-8",
            TextEncoding::ShiftJis => "Shift-JIS",
            TextEncoding::Latin1 => "Latin-1",
        }
    }
}

/// Decode a meta text payload for display. `hint` wins over detection
/// (e.g. a file-wide XF "JP" marker or a user override).
pub fn decode_text(data: &[u8], hint: Option<TextEncoding>) -> String {
    let enc = hint.unwrap_or_else(|| guess_encoding(data));
    match enc {
        TextEncoding::Utf8 => String::from_utf8_lossy(data).into_owned(),
        TextEncoding::ShiftJis => encoding_rs::SHIFT_JIS.decode(data).0.into_owned(),
        TextEncoding::Latin1 => encoding_rs::WINDOWS_1252.decode(data).0.into_owned(),
    }
}

/// Encode a meta text payload for writing. The chosen encoding is explicit
/// — mirroring `decode_text` — so the bytes produced are deterministic
/// given (text, encoding).
pub fn encode_text(s: &str, enc: TextEncoding) -> Vec<u8> {
    match enc {
        TextEncoding::Utf8 => s.as_bytes().to_vec(),
        TextEncoding::ShiftJis => encoding_rs::SHIFT_JIS.encode(s).0.into_owned(),
        TextEncoding::Latin1 => encoding_rs::WINDOWS_1252.encode(s).0.into_owned(),
    }
}

/// Heuristic: valid UTF-8 wins (pure ASCII included); else SJIS if the byte
/// pattern parses cleanly as SJIS (no replacement chars produced); else
/// Latin-1.
pub fn guess_encoding(data: &[u8]) -> TextEncoding {
    if std::str::from_utf8(data).is_ok() {
        return TextEncoding::Utf8;
    }
    let (_, _, had_errors) = encoding_rs::SHIFT_JIS.decode(data);
    if !had_errors {
        return TextEncoding::ShiftJis;
    }
    TextEncoding::Latin1
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fixture: format-1, SJIS track name, running status, SysEx
    fn fixture() -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(b"MThd\x00\x00\x00\x06\x00\x01\x00\x02\x01\xE0");
        // track 0: conductor
        let t0: &[u8] = &[
            0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20, // tempo 500000
            0x00, 0xFF, 0x58, 0x04, 0x04, 0x02, 0x18, 0x08, // 4/4
            0x00, 0xFF, 0x2F, 0x00, // EOT
        ];
        // track 1: SJIS name "テスト", notes w/ running status, sysex
        let mut t1 = Vec::new();
        t1.extend_from_slice(&[0x00, 0xFF, 0x03, 0x06]);
        t1.extend_from_slice(&[0x83, 0x65, 0x83, 0x58, 0x83, 0x67]); // "テスト" Shift-JIS
        t1.extend_from_slice(&[0x00, 0x90, 0x3C, 0x64]);
        t1.extend_from_slice(&[0x60, 0x3C, 0x00]); // running status: NoteOn vel0
        t1.extend_from_slice(&[0x00, 0xF0, 0x04, 0x7E, 0x7F, 0x09, 0x01]); // GM on
        t1.extend_from_slice(&[0x00, 0xFF, 0x2F, 0x00]);
        f.extend_from_slice(b"MTrk");
        f.extend_from_slice(&(t0.len() as u32).to_be_bytes());
        f.extend_from_slice(t0);
        f.extend_from_slice(b"MTrk");
        f.extend_from_slice(&(t1.len() as u32).to_be_bytes());
        f.extend_from_slice(&t1);
        f
    }

    #[test]
    fn roundtrip_byte_identical() {
        let src = fixture();
        let f = parse(&src).unwrap();
        let out = write(f.format, f.division, &f.tracks, WriteOptions::default());
        assert_eq!(src, out, "round-trip must be byte identical");
    }

    #[test]
    fn sjis_preserved() {
        let f = parse(&fixture()).unwrap();
        match &f.tracks[1].events[0].kind {
            EventKind::Meta {
                meta_type: 0x03,
                data,
            } => {
                assert_eq!(
                    &data[..],
                    &[0x83, 0x65, 0x83, 0x58, 0x83, 0x67],
                    "track name raw bytes"
                );
            }
            other => panic!("expected track name meta, got {other:?}"),
        }
    }

    #[test]
    fn sjis_decodes() {
        let (sjis, _, _) = encoding_rs::SHIFT_JIS.encode("テスト");
        let sjis = sjis.as_ref();
        assert_eq!(guess_encoding(sjis), TextEncoding::ShiftJis);
        assert_eq!(decode_text(sjis, None), "テスト");
        let ascii = b"Lead";
        assert_eq!(guess_encoding(ascii), TextEncoding::Utf8);
        assert_eq!(decode_text(ascii, None), "Lead");
        // Latin-1 high bytes that are neither UTF-8 nor SJIS
        let latin = [0xE9u8, 0x20]; // e-acute + space
        assert_eq!(guess_encoding(&latin), TextEncoding::Latin1);
    }

    #[test]
    fn malformed_does_not_panic() {
        let mut bad = fixture();
        bad.truncate(bad.len() - 3);
        let _ = parse(&bad); // must not panic
    }

    /// minimal bare SMF used by the wrapper/parse tests below
    fn tiny_smf() -> Vec<u8> {
        let mut f = Vec::new();
        f.extend_from_slice(b"MThd\x00\x00\x00\x06\x00\x00\x00\x01\x01\xE0");
        let t = [0x00, 0x90, 0x3C, 0x64, 0x00, 0xFF, 0x2F, 0x00];
        f.extend_from_slice(b"MTrk");
        f.extend_from_slice(&(t.len() as u32).to_be_bytes());
        f.extend_from_slice(&t);
        f
    }

    #[test]
    fn hostile_vlq_length_does_not_panic() {
        // meta length declared as a 10-byte VLQ — must not overflow in debug
        let mut f = Vec::new();
        f.extend_from_slice(b"MThd\x00\x00\x00\x06\x00\x00\x00\x01\x01\xE0");
        let t: Vec<u8> = [
            &[0x00u8, 0xFF, 0x01][..],
            &[0xFF; 10][..], // length VLQ: ten continuation bytes
            &[0x41, 0x42][..],
        ]
        .concat();
        f.extend_from_slice(b"MTrk");
        f.extend_from_slice(&(t.len() as u32).to_be_bytes());
        f.extend_from_slice(&t);
        assert!(parse(&f).is_ok(), "lenient recovery must handle it");
    }

    #[test]
    fn smpte_degenerate_fps_roundtrips() {
        // division byte 0x80 is not a legal SMPTE fps; the lenient parser
        // keeps fps=128 and the writer must re-emit it without overflowing
        let mut f = tiny_smf();
        f[12] = 0x80;
        f[13] = 0x64;
        let parsed = parse(&f).unwrap();
        assert_eq!(
            parsed.division,
            Division::Smpte {
                fps: 128,
                ticks_per_frame: 0x64
            }
        );
        let out = write(
            parsed.format,
            parsed.division,
            &parsed.tracks,
            WriteOptions::default(),
        );
        assert_eq!(&out[12..14], &[0x80, 0x64]);
    }

    #[test]
    fn metrical_division_masked_to_15_bits() {
        // a Metrical division with bit 15 set is out of contract; the writer
        // masks it rather than emit a header that reparses as SMPTE
        let out = write(
            0,
            Division::Metrical(0x8000),
            &[Track { events: vec![] }],
            WriteOptions::default(),
        );
        assert_eq!(&out[12..14], &[0x00, 0x00]);
        let back = parse(&out).unwrap();
        assert_eq!(back.division, Division::Metrical(0));
    }

    #[test]
    fn format0_with_zero_tracks_writes_one_empty_track() {
        // format 0 declares exactly one track — an empty track list still
        // emits a single (empty) MTrk so the header stays a valid format-0
        // file every parser accepts. The FORMAT byte is still verbatim.
        let out = write(0, Division::Metrical(480), &[], WriteOptions::default());
        assert_eq!(&out[8..10], &[0x00, 0x00], "format verbatim");
        assert_eq!(&out[10..12], &[0x00, 0x01], "one MTrk for format 0");
        let back = parse(&out).unwrap();
        assert_eq!(back.format, 0);
        assert_eq!(back.tracks.len(), 1);
    }

    #[test]
    fn channel_len_follows_status() {
        // a stored len inconsistent with the status nibble must not reach
        // the byte stream — readers derive the data count from the status
        // and misalign everything after it
        let mk = |status, len| Event {
            tick: 0,
            seq: 0,
            raw_body: None,
            kind: EventKind::Channel {
                status,
                data: [0x40, 0x41],
                len,
            },
        };
        let one_track = |events| vec![Track { events }];
        // 0xD0 pressure is 1 data byte even when len says 2
        let out = write(
            1,
            Division::Metrical(480),
            &one_track(vec![mk(0xD3, 2)]),
            WriteOptions::default(),
        );
        assert!(out.windows(2).any(|w| w == [0xD3, 0x40]));
        assert!(!out.windows(3).any(|w| w == [0xD3, 0x40, 0x41]));
        // 0x90 note-on is 2 data bytes even when len says 1
        let out = write(
            1,
            Division::Metrical(480),
            &one_track(vec![mk(0x90, 1)]),
            WriteOptions::default(),
        );
        assert!(out.windows(3).any(|w| w == [0x90, 0x40, 0x41]));
    }

    #[test]
    fn riff_rmid_container_is_warned() {
        let smf = tiny_smf();
        let mut f = Vec::new();
        f.extend_from_slice(b"RIFF");
        let riff_len = 4 + 8 + smf.len();
        f.extend_from_slice(&(riff_len as u32).to_be_bytes());
        f.extend_from_slice(b"RMID");
        f.extend_from_slice(b"data");
        f.extend_from_slice(&(smf.len() as u32).to_be_bytes());
        f.extend_from_slice(&smf);
        let parsed = parse(&f).unwrap();
        assert_eq!(parsed.tracks.len(), 1);
        assert!(
            parsed.warnings.iter().any(|w| w.contains("RIFF/RMID")),
            "container loss must be surfaced: {:?}",
            parsed.warnings
        );
    }

    #[test]
    fn strict_failure_reason_survives_lenient_failure() {
        // MThd that is too short for both parsers — the error must carry the
        // strict reason, not just "no MTrk chunks"
        let err = parse(b"MThd\x00\x00\x00\x06\x00\x00\x00\x05").unwrap_err();
        assert!(err.to_string().contains("lenient recovery also failed"));
    }
}

//! Canonical document model.
//!
//! Two layers: the raw SMF event list is the single source of truth
//! (`Vec<Event>` sorted by `(tick, seq)`, per-event channel, `raw_body`
//! byte spans preserved); `NoteIndex`/`TempoMap` are derived views rebuilt
//! after each applied `Transaction`.
//!
//! ALL edits — GUI and MCP alike — go through `Document::apply`.

use bytes::Bytes;
use smf_core::{Division, EventKind};
use std::collections::HashMap;
use thiserror::Error;

pub type EventId = u64;
pub type Revision = u64;

#[derive(Debug, Clone)]
pub struct Event {
    pub id: EventId,
    pub tick: u64,
    pub seq: u32,
    pub raw_body: Option<Bytes>,
    pub kind: EventKind,
}

#[derive(Debug, Clone)]
pub struct Track {
    /// Shift-JIS/UTF-8-agnostic: decoded display name is a UI concern
    pub name: Option<Bytes>,
    /// output port (SMF `FF 21` meta convention), 0 = default
    pub out_port: u8,
    /// default channel for display; per-event channel still rules
    pub out_channel: u8,
    pub events: Vec<Event>,
}

/// One import diagnostic finding.
#[derive(Debug, Clone)]
pub struct Diagnostic {
    /// stable machine-readable id ("dangling-noteon", ...)
    pub code: &'static str,
    pub track: usize,
    pub tick: u64,
    pub event: Option<EventId>,
    pub detail: String,
}

#[derive(Debug)]
pub struct Document {
    pub format: u16,
    pub division: Division,
    pub tracks: Vec<Track>,
    revision: Revision,
    next_event_id: EventId,
    by_id: HashMap<EventId, (usize, usize)>, // id -> (track, event index)
    pub tempo_map: TempoMap,
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("stale base revision: expected {expected}, got {got}")]
    StaleRevision { expected: Revision, got: Revision },
    #[error("unknown event id {0}")]
    UnknownEvent(EventId),
    #[error("unknown track {0}")]
    UnknownTrack(usize),
}

/// One undo step. `before`/`after` are self-contained diffs so undo needs no
/// inverse computation.
#[derive(Debug, Clone)]
pub struct Transaction {
    pub label: String,
    pub base: Revision,
    pub ops: Vec<Op>,
}

#[derive(Debug, Clone)]
pub enum Op {
    InsertEvents {
        track: usize,
        events: Vec<Event>,
    },
    RemoveEvents {
        track: usize,
        /// (index, event) before-images for undo
        removed: Vec<(usize, Event)>,
    },
    UpdateEvent {
        track: usize,
        before: Event,
        after: Event,
    },
    /// Append a track at `index` (usually == tracks.len()); `track` is the
    /// inserted track's before-image so undo can remove it by position.
    InsertTrack { index: usize, track: Track },
    /// Remove the whole track; `track` is its before-image.
    RemoveTrack { index: usize, track: Track },
    /// Replace the track's display name meta (0x03) before-image kept.
    UpdateTrack { index: usize, before: Track, after: Track },
}

impl Document {
    pub fn from_file(f: smf_core::File) -> Self {
        let mut next_id: EventId = 1;
        let mut by_id = HashMap::new();
        let tracks: Vec<Track> = f
            .tracks
            .into_iter()
            .map(|t| {
                let mut name = None;
                let mut out_port = 0u8;
                let mut out_channel = 0u8;
                let events: Vec<Event> = t
                    .events
                    .into_iter()
                    .map(|e| {
                        let id = next_id;
                        next_id += 1;
                        // pick up conventional metas for display/restore
                        if let EventKind::Meta { meta_type, data } = &e.kind {
                            match *meta_type {
                                0x03 if name.is_none() => name = Some(data.clone()),
                                0x21 if !data.is_empty() => out_port = data[0],
                                0x20 if !data.is_empty() => out_channel = data[0],
                                _ => {}
                            }
                        }
                        by_id.insert(id, (0, 0)); // fixed below
                        Event {
                            id,
                            tick: e.tick,
                            seq: e.seq,
                            raw_body: e.raw_body,
                            kind: e.kind,
                        }
                    })
                    .collect();
                Track {
                    name,
                    out_port,
                    out_channel,
                    events,
                }
            })
            .collect();
        let mut doc = Document {
            format: f.format,
            division: f.division,
            tracks,
            revision: 0,
            next_event_id: next_id,
            by_id,
            tempo_map: TempoMap::default(),
        };
        doc.rebuild_index();
        doc.tempo_map = TempoMap::build(&doc.tracks, doc.division);
        doc
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    /// Allocate a fresh `EventId` — callers building `Op::InsertEvents` must
    /// mint ids here so they stay unique across undo/redo cycles.
    pub fn alloc_event_id(&mut self) -> EventId {
        let id = self.next_event_id;
        self.next_event_id += 1;
        id
    }

    fn rebuild_index(&mut self) {
        self.by_id.clear();
        for (ti, t) in self.tracks.iter().enumerate() {
            for (ei, e) in t.events.iter().enumerate() {
                self.by_id.insert(e.id, (ti, ei));
            }
        }
    }

    /// The single edit entry point shared by GUI and MCP. Atomic: either
    /// every op applies and the revision advances, or an error leaves the
    /// document (and its id index) exactly as it was.
    pub fn apply(&mut self, tx: Transaction) -> Result<Revision, ApplyError> {
        if tx.base != self.revision {
            return Err(ApplyError::StaleRevision {
                expected: self.revision,
                got: tx.base,
            });
        }
        let mut tracks = self.tracks.clone();
        for op in &tx.ops {
            apply_op(&mut tracks, op)?;
        }
        self.tracks = tracks;
        self.revision += 1;
        self.rebuild_index();
        self.tempo_map = TempoMap::build(&self.tracks, self.division);
        Ok(self.revision)
    }

    /// restore the `before` images of a transaction (undo)
    pub fn revert(&mut self, tx: &Transaction) {
        for op in tx.ops.iter().rev() {
            match op {
                Op::InsertEvents { track, events } => {
                    if let Some(t) = self.tracks.get_mut(*track) {
                        for e in events {
                            if let Some(pos) = t.events.iter().position(|x| x.id == e.id) {
                                t.events.remove(pos);
                            }
                        }
                    }
                }
                Op::RemoveEvents { track, removed } => {
                    if let Some(t) = self.tracks.get_mut(*track) {
                        for (_, e) in removed {
                            let pos = t
                                .events
                                .binary_search_by_key(&(e.tick, e.seq), |x| (x.tick, x.seq))
                                .unwrap_or_else(|p| p);
                            t.events.insert(pos, e.clone());
                        }
                    }
                }
                Op::UpdateEvent { track, before, .. } => {
                    if let Some(t) = self.tracks.get_mut(*track) {
                        if let Some(pos) = t.events.iter().position(|x| x.id == before.id) {
                            t.events[pos] = before.clone();
                            t.events.sort_by_key(|e| (e.tick, e.seq));
                        }
                    }
                }
                Op::InsertTrack { index, .. } => {
                    if *index < self.tracks.len() {
                        self.tracks.remove(*index);
                    }
                }
                Op::RemoveTrack { index, track } => {
                    self.tracks
                        .insert((*index).min(self.tracks.len()), track.clone());
                }
                Op::UpdateTrack { index, before, .. } => {
                    if let Some(t) = self.tracks.get_mut(*index) {
                        *t = before.clone();
                    }
                }
            }
            let ti = match op {
                Op::InsertEvents { track, .. }
                | Op::RemoveEvents { track, .. }
                | Op::UpdateEvent { track, .. } => Some(*track),
                Op::InsertTrack { index, .. }
                | Op::RemoveTrack { index, .. }
                | Op::UpdateTrack { index, .. } => Some(*index),
            };
            if let Some(ti) = ti {
                refresh_track_meta(&mut self.tracks, ti);
            }
        }
        self.revision += 1;
        self.rebuild_index();
        self.tempo_map = TempoMap::build(&self.tracks, self.division);
    }

    /// File-wide text-encoding hint from an XF `FF 09` charset marker
    /// ("JP" => Shift-JIS). Returns None when the file carries no marker.
    pub fn text_encoding_hint(&self) -> Option<smf_core::TextEncoding> {
        for t in &self.tracks {
            for e in &t.events {
                if let EventKind::Meta {
                    meta_type: 0x09,
                    data,
                } = &e.kind
                {
                    if String::from_utf8_lossy(data)
                        .to_ascii_uppercase()
                        .contains("JP")
                    {
                        return Some(smf_core::TextEncoding::ShiftJis);
                    }
                }
            }
        }
        None
    }

    /// Import-quality diagnostics over the raw event layer. Each finding is
    /// stable-identified (code + event id) so UI and MCP can both surface it.
    /// Nothing here mutates — normalization stays an explicit undoable edit.
    pub fn diagnose(&self) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            let mut eot = false;
            for e in &t.events {
                if matches!(e.kind, EventKind::Meta { meta_type: 0x2F, .. }) {
                    eot = true;
                }
                // tempo maps outside track 0 (format-1 files): legal but
                // most players ignore them — worth flagging
                if self.format == 1
                    && ti != 0
                    && matches!(e.kind, EventKind::Meta { meta_type: 0x51, .. })
                {
                    out.push(Diagnostic {
                        code: "tempo-outside-conductor",
                        track: ti,
                        tick: e.tick,
                        event: Some(e.id),
                        detail: "tempo change outside track 0 is ignored by many players".into(),
                    });
                }
            }
            if !eot && !t.events.is_empty() {
                out.push(Diagnostic {
                    code: "missing-eot",
                    track: ti,
                    tick: t.events.last().map(|e| e.tick).unwrap_or(0),
                    event: None,
                    detail: "track has no End-of-Track meta event".into(),
                });
            }
        }
        for n in self.notes() {
            if n.end_tick.is_none() {
                out.push(Diagnostic {
                    code: "dangling-noteon",
                    track: n.track,
                    tick: n.start_tick,
                    event: Some(n.on_id),
                    detail: format!("noteOn ch{} key{} never released", n.channel + 1, n.key),
                });
            } else if n.end_tick == Some(n.start_tick) {
                out.push(Diagnostic {
                    code: "zero-length-note",
                    track: n.track,
                    tick: n.start_tick,
                    event: Some(n.on_id),
                    detail: format!("zero-length note ch{} key{}", n.channel + 1, n.key),
                });
            }
        }
        out.sort_by_key(|d| (d.track, d.tick));
        out
    }

    /// Build the ops that resolve each diagnostic — one undo step for the
    /// whole batch. `codes` filters by diagnostic code; empty = fix all.
    pub fn fix_ops(&mut self, codes: &[&str]) -> Vec<Op> {
        let diags = self.diagnose();
        let notes = self.notes();
        let mut ops = Vec::new();
        for d in diags {
            if !codes.is_empty() && !codes.contains(&d.code) {
                continue;
            }
            match d.code {
                "missing-eot" => {
                    let id = self.alloc_event_id();
                    ops.push(Op::InsertEvents {
                        track: d.track,
                        events: vec![Event {
                            id,
                            tick: d.tick,
                            // after every existing event at the final tick
                            seq: self.next_seq(d.track, d.tick),
                            raw_body: None,
                            kind: EventKind::Meta {
                                meta_type: 0x2F,
                                data: Bytes::new(),
                            },
                        }],
                    });
                }
                "dangling-noteon" | "zero-length-note" => {
                    // remove the on event plus its paired off (zero-len keeps one)
                    let off_id = notes
                        .iter()
                        .find(|n| Some(n.on_id) == d.event)
                        .and_then(|n| n.off_id);
                    for id in [d.event, off_id].into_iter().flatten() {
                        if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                            let e = self.tracks[ti].events[ei].clone();
                            ops.push(Op::RemoveEvents {
                                track: ti,
                                removed: vec![(ei, e)],
                            });
                        }
                    }
                }
                "tempo-outside-conductor" => {
                    // move the tempo event into track 0 (same tick)
                    if let Some(id) = d.event {
                        if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                            let e = self.tracks[ti].events[ei].clone();
                            ops.push(Op::RemoveEvents {
                                track: ti,
                                removed: vec![(ei, e.clone())],
                            });
                            let mut moved = e;
                            moved.id = self.alloc_event_id();
                            ops.push(Op::InsertEvents {
                                track: 0,
                                events: vec![moved],
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        ops
    }

    /// `(absolute µs, source track index, raw channel message)` sorted by time.
    /// The track tag lets playback fan events out to per-track destinations.
    pub fn timeline_tagged(&self) -> Vec<(u64, usize, Vec<u8>)> {
        let mut out = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            for e in &t.events {
                if let EventKind::Channel { status, data, len } = &e.kind {
                    let mut b = Vec::with_capacity(3);
                    b.push(*status);
                    b.push(data[0]);
                    if *len == 2 {
                        b.push(data[1]);
                    }
                    out.push((self.tempo_map.tick_to_us(e.tick), ti, b));
                }
            }
        }
        out.sort_by_key(|(us, _, _)| *us);
        out
    }

    pub fn timeline(&self) -> Vec<(u64, Vec<u8>)> {
        self.timeline_tagged()
            .into_iter()
            .map(|(us, _, b)| (us, b))
            .collect()
    }

    /// Complete SysEx messages in wire form (`F0 … F7`) as
    /// `(µs, track, bytes)`, timestamped at the FIRST fragment. SMF allows a
    /// message to be split: an `F0` packet followed by further `F7` escape
    /// packets, with only the last one ending in `F7`; other events may be
    /// interleaved between the fragments. A message still open when a new
    /// `F0` starts (or at end of track) is closed by appending `F7`.
    /// Standalone `F7` escapes are arbitrary non-MIDI bytes and never start
    /// or join a message — they are never emitted.
    pub fn timeline_sysex(&self) -> Vec<(u64, usize, Vec<u8>)> {
        self.sysex_wire()
            .into_iter()
            .map(|(us, ti, b, _)| (us, ti, b))
            .collect()
    }

    /// The last SysEx message per track that the file itself terminated
    /// before `start_us`, retimed to `start_us`. This is the opt-in half of
    /// the chase: a chased GM/GS/XG reset would wipe the channel state
    /// `chase_events` just restored, so the caller gates it behind a
    /// preference (as Logic/Cubase do). Messages we closed by appending `F7`
    /// (abandoned by a new `F0` or end of track) are skipped — re-sending a
    /// truncated dump would only confuse the device.
    pub fn chase_sysex(&self, start_us: u64) -> Vec<(u64, usize, Vec<u8>)> {
        let mut last: HashMap<usize, Vec<u8>> = HashMap::new();
        for (us, ti, b, terminated) in self.sysex_wire() {
            if terminated && us < start_us {
                last.insert(ti, b);
            }
        }
        let mut tis: Vec<usize> = last.keys().copied().collect();
        tis.sort_unstable();
        tis.into_iter()
            .map(|ti| (start_us, ti, last.remove(&ti).unwrap()))
            .collect()
    }

    /// `(µs, track, wire bytes, terminated-in-file)` — the shared joining
    /// walk behind `timeline_sysex` and `chase_sysex`.
    fn sysex_wire(&self) -> Vec<(u64, usize, Vec<u8>, bool)> {
        let mut out = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            let mut open: Option<(u64, Vec<u8>)> = None;
            for e in &t.events {
                match &e.kind {
                    EventKind::SysEx(p) => {
                        if let Some((us, mut b)) = open.take() {
                            b.push(0xF7);
                            out.push((us, ti, b, false));
                        }
                        let us = self.tempo_map.tick_to_us(e.tick);
                        let mut b = Vec::with_capacity(p.len() + 2);
                        b.push(0xF0);
                        b.extend_from_slice(p);
                        if p.last() == Some(&0xF7) {
                            out.push((us, ti, b, true));
                        } else {
                            open = Some((us, b));
                        }
                    }
                    EventKind::Escape(p) => {
                        if open.is_some() {
                            let completes = p.last() == Some(&0xF7);
                            open.as_mut().unwrap().1.extend_from_slice(p);
                            if completes {
                                let (us, b) = open.take().unwrap();
                                out.push((us, ti, b, true));
                            }
                        }
                    }
                    _ => {}
                }
            }
            if let Some((us, mut b)) = open {
                b.push(0xF7);
                out.push((us, ti, b, false));
            }
        }
        out.sort_by_key(|(us, _, _, _)| *us);
        out
    }

    /// Chase events for starting playback at `start_us`: everything a synth
    /// needs to be in the state it would have reached by playing the timeline
    /// from the beginning. All output carries `us = start_us`; the caller
    /// inserts them at the schedule's seek point so they fire just before the
    /// first real event at/after the position (loop wraps re-send them the
    /// same way). Per (track, channel):
    ///
    /// - bank MSB/LSB, then program change (bank must precede PC)
    /// - last value of each CC 0-119 except the channel-mode range; CC64
    ///   (sustain) lands before the note restrikes below
    /// - the last RPN/NRPN selector followed by data entry (CC 6/38) — data
    ///   before the selector would hit the synth's stale cursor
    /// - pitch bend, channel aftertouch, poly aftertouch
    /// - note-ons of notes still sounding at `start_us` (their real note-offs
    ///   are already in the future part of the timeline — never re-sent)
    /// - notes released earlier but caught by a held pedal: re-struck as
    ///   note-on + note-off pairs AFTER CC64 so the pedal catches them
    ///
    /// Channel-mode messages in the prefix model what the receiver did:
    /// CC120 drops all sounding notes, CC123/124-127 act as note-offs (a
    /// held pedal still catches), CC121 clears controller state
    /// (bank/program survive per RP-015). SysEx is not part of this channel
    /// chase — see `chase_sysex` for the opt-in message chase.
    pub fn chase_events(&self, start_us: u64) -> Vec<(u64, usize, Vec<u8>)> {
        let mut out = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            let mut chans: HashMap<u8, ChaseState> = HashMap::new();
            for e in &t.events {
                if self.tempo_map.tick_to_us(e.tick) >= start_us {
                    break; // events sorted by (tick, seq); tick_to_us is monotonic
                }
                let EventKind::Channel { status, data, .. } = &e.kind else {
                    continue;
                };
                let ch = status & 0x0F;
                let st = chans.entry(ch).or_default();
                match (status & 0xF0, data[0], data[1]) {
                    (0x90, key, v) if v > 0 => st.pending[key as usize].push(v),
                    (0x80, key, _) | (0x90, key, _) => {
                        // LIFO pairing, same as the notes() view
                        if let Some(vel) = st.pending[key as usize].pop() {
                            if st.pedal_down {
                                st.sustained.push((key, vel));
                            }
                        }
                    }
                    (0xB0, ctl, v) => match ctl {
                        0 => st.bank_msb = Some(v),
                        32 => st.bank_lsb = Some(v),
                        6 => st.data_msb = Some(v),
                        38 => st.data_lsb = Some(v),
                        98 | 99 => {
                            st.sel_seen = true;
                            st.sel_nrpn = true;
                            st.sel_vals[(ctl - 98) as usize] = v;
                        }
                        100 | 101 => {
                            st.sel_seen = true;
                            st.sel_nrpn = false;
                            st.sel_vals[(ctl - 98) as usize] = v;
                        }
                        64 => {
                            st.pedal_down = v >= 64;
                            st.cc[64] = Some(v);
                            if !st.pedal_down {
                                st.sustained.clear();
                            }
                        }
                        120 => {
                            st.pending.iter_mut().for_each(|s| s.clear());
                            st.sustained.clear();
                        }
                        121 => {
                            // RP-015: controllers to default; bank/program survive
                            st.cc = std::array::from_fn(|_| None);
                            st.data_msb = None;
                            st.data_lsb = None;
                            st.sel_seen = false;
                            st.sel_nrpn = false;
                            st.sel_vals = [0; 4];
                            st.bend = None;
                            st.pressure = None;
                            st.poly = std::array::from_fn(|_| None);
                            st.pedal_down = false;
                            st.sustained.clear();
                        }
                        123 | 124..=127 => {
                            // all-notes-off semantics; hold pedal still catches
                            if st.pedal_down {
                                for (key, stack) in st.pending.iter_mut().enumerate() {
                                    for vel in stack.drain(..) {
                                        st.sustained.push((key as u8, vel));
                                    }
                                }
                            } else {
                                st.pending.iter_mut().for_each(|s| s.clear());
                            }
                        }
                        _ if ctl < 120 => st.cc[ctl as usize] = Some(v),
                        _ => {}
                    },
                    (0xC0, prog, _) => st.prog = Some(prog),
                    (0xD0, press, _) => st.pressure = Some(press),
                    (0xE0, lsb, msb) => st.bend = Some([lsb, msb]),
                    (0xA0, key, v) => st.poly[key as usize] = Some(v),
                    _ => {}
                }
            }
            // emit per channel, ascending, in the documented order
            let mut ch_list: Vec<u8> = chans.keys().copied().collect();
            ch_list.sort_unstable();
            macro_rules! push {
                ($bytes:expr) => {
                    out.push((start_us, ti, $bytes.to_vec()))
                };
            }
            for ch in ch_list {
                let st = &chans[&ch];
                if let Some(v) = st.bank_msb {
                    push!([0xB0 | ch, 0, v]);
                }
                if let Some(v) = st.bank_lsb {
                    push!([0xB0 | ch, 32, v]);
                }
                if let Some(p) = st.prog {
                    push!([0xC0 | ch, p]);
                }
                // bank (0/32) and data entry (6/38) live in dedicated fields,
                // so this only ever holds plain CC 1-119 (incl. 64 sustain)
                for (ctl, v) in st.cc.iter().enumerate() {
                    if let Some(v) = v {
                        push!([0xB0 | ch, ctl as u8, *v]);
                    }
                }
                if st.sel_seen {
                    let (msb, lsb) = if st.sel_nrpn { (99u8, 98u8) } else { (101, 100) };
                    push!([0xB0 | ch, msb, st.sel_vals[(msb - 98) as usize]]);
                    push!([0xB0 | ch, lsb, st.sel_vals[(lsb - 98) as usize]]);
                    if let Some(v) = st.data_msb {
                        push!([0xB0 | ch, 6, v]);
                    }
                    if let Some(v) = st.data_lsb {
                        push!([0xB0 | ch, 38, v]);
                    }
                }
                if let Some([lsb, msb]) = st.bend {
                    push!([0xE0 | ch, lsb, msb]);
                }
                if let Some(v) = st.pressure {
                    push!([0xD0 | ch, v]);
                }
                for (key, v) in st.poly.iter().enumerate() {
                    if let Some(v) = v {
                        push!([0xA0 | ch, key as u8, *v]);
                    }
                }
                for (key, stack) in st.pending.iter().enumerate() {
                    for vel in stack {
                        push!([0x90 | ch, key as u8, *vel]);
                    }
                }
                for &(key, vel) in &st.sustained {
                    push!([0x90 | ch, key, vel]);
                    push!([0x80 | ch, key, 0]);
                }
            }
        }
        out
    }

    pub fn serialize(&self, opts: smf_core::WriteOptions) -> Vec<u8> {
        let tracks: Vec<smf_core::Track> = self
            .tracks
            .iter()
            .map(|t| smf_core::Track {
                events: t
                    .events
                    .iter()
                    .map(|e| smf_core::Event {
                        tick: e.tick,
                        seq: e.seq,
                        raw_body: e.raw_body.clone(),
                        kind: e.kind.clone(),
                    })
                    .collect(),
            })
            .collect();
        smf_core::write(self.format, self.division, &tracks, opts)
    }
}

/// Apply one op to a working track list. Fails only on `UnknownTrack` /
/// before any partial state is visible — `Document::apply` commits only when
/// every op of the transaction succeeded.
fn apply_op(tracks: &mut Vec<Track>, op: &Op) -> Result<(), ApplyError> {
    match op {
        Op::InsertEvents { track, events } => {
            let t = tracks.get_mut(*track).ok_or(ApplyError::UnknownTrack(*track))?;
            for e in events {
                let pos = t
                    .events
                    .binary_search_by_key(&(e.tick, e.seq), |x| (x.tick, x.seq))
                    .unwrap_or_else(|p| p);
                t.events.insert(pos, e.clone());
            }
        }
        Op::RemoveEvents { track, removed } => {
            let t = tracks.get_mut(*track).ok_or(ApplyError::UnknownTrack(*track))?;
            for (_, e) in removed {
                if let Some(pos) = t.events.iter().position(|x| x.id == e.id) {
                    t.events.remove(pos);
                }
            }
        }
        Op::UpdateEvent { track, after, .. } => {
            let t = tracks.get_mut(*track).ok_or(ApplyError::UnknownTrack(*track))?;
            if let Some(pos) = t.events.iter().position(|x| x.id == after.id) {
                let mut after = after.clone();
                // a modified kind must be re-encoded on save; a stale raw_body
                // would silently revert the edit at serialize time
                if after.kind != t.events[pos].kind {
                    after.raw_body = None;
                }
                t.events[pos] = after;
                t.events.sort_by_key(|e| (e.tick, e.seq));
            }
        }
        Op::InsertTrack { index, track } => {
            tracks.insert((*index).min(tracks.len()), track.clone());
        }
        Op::RemoveTrack { index, .. } => {
            if *index < tracks.len() {
                tracks.remove(*index);
            }
        }
        Op::UpdateTrack { index, after, .. } => {
            if let Some(t) = tracks.get_mut(*index) {
                *t = after.clone();
            }
        }
    }
    // keep the track's cached conventional metas honest
    let ti = match op {
        Op::InsertEvents { track, .. }
        | Op::RemoveEvents { track, .. }
        | Op::UpdateEvent { track, .. } => Some(*track),
        Op::InsertTrack { index, .. }
        | Op::RemoveTrack { index, .. }
        | Op::UpdateTrack { index, .. } => Some(*index),
    };
    if let Some(ti) = ti {
        refresh_track_meta(tracks, ti);
    }
    Ok(())
}

/// Rescan the first name/out-port/out-channel metas after an edit so the
/// `Track` cache fields stay correct without callers doing it.
fn refresh_track_meta(tracks: &mut [Track], ti: usize) {
    if let Some(t) = tracks.get_mut(ti) {
        t.name = None;
        t.out_port = 0;
        t.out_channel = 0;
        for e in &t.events {
            if let EventKind::Meta { meta_type, data } = &e.kind {
                match *meta_type {
                    0x03 if t.name.is_none() => t.name = Some(data.clone()),
                    0x21 if !data.is_empty() => t.out_port = data[0],
                    0x20 if !data.is_empty() => t.out_channel = data[0],
                    _ => {}
                }
            }
        }
    }
}

/// A note rectangle derived by pairing NoteOn with its matching NoteOff
/// (or NoteOn-vel0). Dangling NoteOns stay visible (`end_tick: None`).
#[derive(Debug, Clone)]
pub struct Note {
    pub track: usize,
    pub channel: u8,
    pub key: u8,
    pub vel: u8,
    pub start_tick: u64,
    /// `None` = dangling NoteOn (import diagnostic surfaces these)
    pub end_tick: Option<u64>,
    pub on_id: EventId,
    pub off_id: Option<EventId>,
}

/// Per-(track, channel) state reconstructed by `Document::chase_events` while
/// scanning the prefix before the play position. `None` = never set (or reset
/// by CC121) → nothing is emitted for that slot.
#[derive(Debug)]
struct ChaseState {
    /// plain CC 1-119 values (bank 0/32 and data entry 6/38 live separately)
    cc: [Option<u8>; 128],
    bank_msb: Option<u8>,
    bank_lsb: Option<u8>,
    prog: Option<u8>,
    data_msb: Option<u8>,
    data_lsb: Option<u8>,
    /// any RPN/NRPN selector seen; NRPN vs RPN is whichever group wrote last
    sel_seen: bool,
    sel_nrpn: bool,
    /// CC98, CC99, CC100, CC101 values by `cc - 98`
    sel_vals: [u8; 4],
    bend: Option<[u8; 2]>,
    pressure: Option<u8>,
    poly: [Option<u8>; 128],
    pedal_down: bool,
    /// note-ons still held (key → stack of velocities, LIFO pairing)
    pending: [Vec<u8>; 128],
    /// notes already note-off'd but still ringing under the pedal
    sustained: Vec<(u8, u8)>,
}

impl Default for ChaseState {
    fn default() -> Self {
        Self {
            cc: std::array::from_fn(|_| None),
            poly: std::array::from_fn(|_| None),
            pending: std::array::from_fn(|_| Vec::new()),
            sel_vals: [0; 4],
            bank_msb: None,
            bank_lsb: None,
            prog: None,
            data_msb: None,
            data_lsb: None,
            sel_seen: false,
            sel_nrpn: false,
            bend: None,
            pressure: None,
            pedal_down: false,
            sustained: Vec::new(),
        }
    }
}

impl Document {
    /// Derived view over the raw event truth — O(events). Call after edits.
    pub fn notes(&self) -> Vec<Note> {
        let mut out = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            // (channel, key) -> pending NoteOn stack
            let mut pending: [[Vec<usize>; 128]; 16] =
                std::array::from_fn(|_| std::array::from_fn(|_| Vec::new()));
            let mut on_events: Vec<(u64, u8, EventId)> = Vec::new(); // tick, vel, id per noteOn
            for e in &t.events {
                let EventKind::Channel { status, data, .. } = &e.kind else {
                    continue;
                };
                let ch = (status & 0x0F) as usize;
                let msg = status & 0xF0;
                let key = data[0] as usize;
                match (msg, data[1]) {
                    (0x90, v) if v > 0 => {
                        pending[ch][key].push(on_events.len());
                        on_events.push((e.tick, v, e.id));
                    }
                    (0x80, _) | (0x90, _) => {
                        if let Some(idx) = pending[ch][key].pop() {
                            let (start, vel, on_id) = on_events[idx];
                            out.push(Note {
                                track: ti,
                                channel: ch as u8,
                                key: key as u8,
                                vel,
                                start_tick: start,
                                end_tick: Some(e.tick),
                                on_id,
                                off_id: Some(e.id),
                            });
                        }
                    }
                    _ => {}
                }
            }
            // dangling noteOns — kept visible for the import diagnostics report
            for (ch, keys) in pending.iter().enumerate() {
                for (key, stack) in keys.iter().enumerate() {
                    for &idx in stack {
                        let (start, vel, on_id) = on_events[idx];
                        out.push(Note {
                            track: ti,
                            channel: ch as u8,
                            key: key as u8,
                            vel,
                            start_tick: start,
                            end_tick: None,
                            on_id,
                            off_id: None,
                        });
                    }
                }
            }
        }
        out.sort_by_key(|n| (n.start_tick, n.key));
        out
    }
}

/// Semantic region transforms. Each method builds the low-level `Op`s for
/// one undoable Transaction — shared by the GUI and the MCP tool surface so
/// both see identical semantics. All take `&mut self` only to mint event ids.
impl Document {
    fn chan_event(status_nibble: u8, channel: u8, d0: u8, d1: u8) -> EventKind {
        EventKind::Channel {
            status: (status_nibble & 0xF0) | (channel & 0x0F),
            data: [d0, d1],
            len: 2,
        }
    }

    /// seq after every existing event at `tick` in `track`
    fn next_seq(&self, track: usize, tick: u64) -> u32 {
        self.tracks
            .get(track)
            .map(|t| {
                t.events
                    .iter()
                    .filter(|e| e.tick == tick)
                    .map(|e| e.seq)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// Quantize note starts inside [from,to) to `grid` ticks.
    /// `strength` 0..=100 interpolates between the original and the grid point;
    /// the note's duration is preserved (on+off shift together).
    pub fn quantize_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        grid: u64,
        strength: u32,
    ) -> Vec<Op> {
        let grid = grid.max(1) as i64;
        let str_f = strength.min(100) as f64 / 100.0;
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let start = n.start_tick as i64;
            let snapped = ((start + grid / 2) / grid) * grid;
            let new_start = (start as f64 + (snapped - start) as f64 * str_f).round() as i64;
            let delta = new_start - start;
            if delta == 0 {
                continue;
            }
            let ids: Vec<EventId> = [Some(n.on_id), n.off_id].into_iter().flatten().collect();
            for id in ids {
                if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                    let mut after = self.tracks[ti].events[ei].clone();
                    after.tick = after.tick.saturating_add_signed(delta);
                    ops.push(Op::UpdateEvent {
                        track: ti,
                        before: self.tracks[ti].events[ei].clone(),
                        after,
                    });
                }
            }
        }
        ops
    }

    /// Transpose all notes starting inside [from,to) by `semitones`
    /// (clamped to 0..=127; notes that would leave the range are skipped).
    pub fn transpose_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        semitones: i32,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let Some(new_key) = (n.key as i32 + semitones)
                .try_into()
                .ok()
                .filter(|k: &u8| *k <= 127)
            else {
                continue;
            };
            for id in [Some(n.on_id), n.off_id].into_iter().flatten() {
                if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                    let before = self.tracks[ti].events[ei].clone();
                    let mut after = before.clone();
                    if let EventKind::Channel { data, .. } = &mut after.kind {
                        data[0] = new_key;
                    }
                    ops.push(Op::UpdateEvent { track: ti, before, after });
                }
            }
        }
        ops
    }

    /// Multiply noteOn velocities inside [from,to) by `factor` (clamped 1..127).
    pub fn scale_velocity_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        factor: f64,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let nv = ((n.vel as f64 * factor).round() as i64).clamp(1, 127) as u8;
            if nv == n.vel {
                continue;
            }
            if let Some((ti, ei)) = self.by_id.get(&n.on_id).copied() {
                let before = self.tracks[ti].events[ei].clone();
                let mut after = before.clone();
                if let EventKind::Channel { data, .. } = &mut after.kind {
                    data[1] = nv;
                }
                ops.push(Op::UpdateEvent { track: ti, before, after });
            }
        }
        ops
    }

    /// Deterministic pseudo-random jitter for note starts/velocities in
    /// [from,to). `timing` = max |tick shift|, `vel` = max |velocity delta|.
    /// Seeded by event id — same document produces the same take (undo and
    /// MCP diffs stay reproducible).
    pub fn humanize_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        timing: i64,
        vel: i32,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let mut r = n.on_id
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(0xA076_1D64_78BD_642F);
            let mut next = || {
                r ^= r << 13;
                r ^= r >> 7;
                r ^= r << 17;
                r
            };
            let dt = if timing > 0 {
                (next() % (timing as u64 * 2 + 1)) as i64 - timing
            } else {
                0
            };
            let dv = if vel > 0 {
                (next() % (vel as u64 * 2 + 1)) as i32 - vel
            } else {
                0
            };
            if dt == 0 && dv == 0 {
                continue;
            }
            for id in [Some(n.on_id), n.off_id].into_iter().flatten() {
                if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                    let before = self.tracks[ti].events[ei].clone();
                    let mut after = before.clone();
                    after.tick = after.tick.saturating_add_signed(dt);
                    if id == n.on_id && dv != 0 {
                        if let EventKind::Channel { data, .. } = &mut after.kind {
                            data[1] = (data[1] as i32 + dv).clamp(1, 127) as u8;
                        }
                    }
                    ops.push(Op::UpdateEvent {
                        track: ti,
                        before,
                        after,
                    });
                }
            }
        }
        ops
    }

    /// Extend each note's end to the start of the next note ON THE SAME KEY
    /// (per-pitch legato — chord voicings stay intact).
    pub fn legato_ops(&mut self, track: usize, from: u64, to: u64) -> Vec<Op> {
        let mut notes: Vec<Note> = self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
            .collect();
        notes.sort_by_key(|n| (n.key, n.start_tick));
        let mut ops = Vec::new();
        for w in notes.windows(2) {
            let (cur, nxt) = (&w[0], &w[1]);
            if cur.key != nxt.key {
                continue;
            }
            let Some(off_id) = cur.off_id else { continue };
            let cur_end = cur.end_tick.unwrap_or(cur.start_tick);
            if nxt.start_tick <= cur_end {
                continue;
            }
            if let Some((ti, ei)) = self.by_id.get(&off_id).copied() {
                let before = self.tracks[ti].events[ei].clone();
                let mut after = before.clone();
                after.tick = nxt.start_tick;
                ops.push(Op::UpdateEvent {
                    track: ti,
                    before,
                    after,
                });
            }
        }
        ops
    }

    /// Set every note in [from,to) to exactly `ticks` long.
    pub fn set_length_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        ticks: u64,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let Some(off_id) = n.off_id else { continue };
            if let Some((ti, ei)) = self.by_id.get(&off_id).copied() {
                let new_end = n.start_tick.saturating_add(ticks.max(1));
                let before = self.tracks[ti].events[ei].clone();
                if before.tick == new_end {
                    continue;
                }
                let mut after = before.clone();
                after.tick = new_end;
                ops.push(Op::UpdateEvent {
                    track: ti,
                    before,
                    after,
                });
            }
        }
        ops
    }

    /// Set every noteOn velocity in [from,to) to `vel`.
    pub fn set_velocity_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        vel: u8,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            if n.vel == vel {
                continue;
            }
            if let Some((ti, ei)) = self.by_id.get(&n.on_id).copied() {
                let before = self.tracks[ti].events[ei].clone();
                let mut after = before.clone();
                if let EventKind::Channel { data, .. } = &mut after.kind {
                    data[1] = vel.clamp(1, 127);
                }
                ops.push(Op::UpdateEvent {
                    track: ti,
                    before,
                    after,
                });
            }
        }
        ops
    }

    /// Retarget every channel event in [from,to) to `channel` (0-indexed).
    pub fn set_channel_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        channel: u8,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        let t = match self.tracks.get(track) {
            Some(t) => t,
            None => return ops,
        };
        for e in &t.events {
            if e.tick < from || e.tick >= to {
                continue;
            }
            if let EventKind::Channel { status, .. } = &e.kind {
                if status & 0x0F == channel & 0x0F {
                    continue;
                }
                let mut after = e.clone();
                if let EventKind::Channel { status, .. } = &mut after.kind {
                    *status = (*status & 0xF0) | (channel & 0x0F);
                }
                ops.push(Op::UpdateEvent {
                    track,
                    before: e.clone(),
                    after,
                });
            }
        }
        ops
    }

    /// Insert a program change (with optional bank select CC0/CC32 first) —
    /// classic "pick a patch" as raw events.
    pub fn set_program_ops(
        &mut self,
        track: usize,
        tick: u64,
        channel: u8,
        program: u8,
        bank_msb: Option<u8>,
        bank_lsb: Option<u8>,
    ) -> Vec<Op> {
        let mut seq = self.next_seq(track, tick);
        let mut mk = |kind: EventKind| Event {
            id: self.alloc_event_id(),
            tick,
            seq: { let s = seq; seq = seq.saturating_add(1); s },
            raw_body: None,
            kind,
        };
        let mut events = Vec::new();
        if let Some(m) = bank_msb {
            events.push(mk(Self::chan_event(0xB0, channel, 0, m)));
        }
        if let Some(l) = bank_lsb {
            events.push(mk(Self::chan_event(0xB0, channel, 32, l)));
        }
        events.push(mk(EventKind::Channel {
            status: 0xC0 | (channel & 0x0F),
            data: [program & 0x7F, 0],
            len: 1,
        }));
        vec![Op::InsertEvents { track, events }]
    }

    /// Insert (or replace at same tick) one CC point.
    pub fn set_cc_ops(
        &mut self,
        track: usize,
        tick: u64,
        channel: u8,
        cc: u8,
        value: u8,
    ) -> Vec<Op> {
        let seq = self.next_seq(track, tick);
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick,
                seq,
                raw_body: None,
                kind: Self::chan_event(0xB0, channel, cc & 0x7F, value & 0x7F),
            }],
        }]
    }

    /// Pitch bend point (0..16383, center 8192).
    pub fn set_pitch_bend_ops(
        &mut self,
        track: usize,
        tick: u64,
        channel: u8,
        value: u16,
    ) -> Vec<Op> {
        let v = value.min(16383);
        let seq = self.next_seq(track, tick);
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick,
                seq,
                raw_body: None,
                kind: Self::chan_event(0xE0, channel, (v & 0x7F) as u8, (v >> 7) as u8),
            }],
        }]
    }

    /// Set/replace the tempo at `tick` on the conductor track (track 0).
    pub fn set_tempo_ops(&mut self, tick: u64, bpm: f64) -> Vec<Op> {
        let mpq = (60_000_000.0 / bpm.max(1.0)).round().clamp(1.0, 0xFF_FFFF as f64) as u32;
        let data = Bytes::copy_from_slice(&mpq.to_be_bytes()[1..]);
        // replace an existing tempo event at the same tick
        if let Some(e) = self
            .tracks
            .first()
            .and_then(|t| {
                t.events.iter().find(|e| {
                    e.tick == tick && matches!(e.kind, EventKind::Meta { meta_type: 0x51, .. })
                })
            })
            .cloned()
        {
            let mut after = e.clone();
            after.kind = EventKind::Meta {
                meta_type: 0x51,
                data,
            };
            return vec![Op::UpdateEvent {
                track: 0,
                before: e,
                after,
            }];
        }
        vec![Op::InsertEvents {
            track: 0,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick,
                seq: self.next_seq(0, tick),
                raw_body: None,
                kind: EventKind::Meta {
                    meta_type: 0x51,
                    data,
                },
            }],
        }]
    }

    /// Set/replace the time signature at `tick` on track 0 (denominator given
    /// as the actual value — 4, 8, … — encoded to the SMF power-of-two form).
    pub fn set_time_sig_ops(&mut self, tick: u64, num: u8, den: u8) -> Vec<Op> {
        let dd = (den.max(1) as f64).log2().round() as u8;
        let data = Bytes::copy_from_slice(&[num, dd, 24, 8]);
        if let Some(e) = self
            .tracks
            .first()
            .and_then(|t| {
                t.events.iter().find(|e| {
                    e.tick == tick && matches!(e.kind, EventKind::Meta { meta_type: 0x58, .. })
                })
            })
            .cloned()
        {
            let mut after = e.clone();
            after.kind = EventKind::Meta {
                meta_type: 0x58,
                data,
            };
            return vec![Op::UpdateEvent {
                track: 0,
                before: e,
                after,
            }];
        }
        vec![Op::InsertEvents {
            track: 0,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick,
                seq: self.next_seq(0, tick),
                raw_body: None,
                kind: EventKind::Meta {
                    meta_type: 0x58,
                    data,
                },
            }],
        }]
    }

    /// Set the track's output channel meta (`FF 20`): update the existing
    /// marker or insert one at tick 0.
    pub fn set_track_channel_ops(&mut self, track: usize, channel: u8) -> Vec<Op> {
        if let Some(e) = self
            .tracks
            .get(track)
            .and_then(|t| {
                t.events.iter().find(|e| {
                    matches!(e.kind, EventKind::Meta { meta_type: 0x20, .. })
                })
            })
            .cloned()
        {
            let mut after = e.clone();
            after.kind = EventKind::Meta {
                meta_type: 0x20,
                data: Bytes::copy_from_slice(&[channel & 0x0F]),
            };
            return vec![Op::UpdateEvent {
                track,
                before: e,
                after,
            }];
        }
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick: 0,
                seq: 0,
                raw_body: None,
                kind: EventKind::Meta {
                    meta_type: 0x20,
                    data: Bytes::copy_from_slice(&[channel & 0x0F]),
                },
            }],
        }]
    }

    /// Clone every channel event in [from,to) shifted to start at `to`.
    /// Notes keep their NoteOff even when it sits outside the range — a
    /// duplicated note must not hang. (Meta events stay behind — duplicating
    /// tempo/EOT would corrupt the structure.)
    pub fn duplicate_range_ops(&mut self, track: usize, from: u64, to: u64) -> Vec<Op> {
        let span = to.saturating_sub(from);
        if span == 0 {
            return vec![];
        }
        let t = match self.tracks.get(track) {
            Some(t) => t,
            None => return vec![],
        };
        let mut events: Vec<Event> = t
            .events
            .iter()
            .filter(|e| e.tick >= from && e.tick < to)
            .filter(|e| matches!(e.kind, EventKind::Channel { .. }))
            .cloned()
            .collect();
        // grab the NoteOff of any note whose start is in range — its tick may
        // lie past `to`, in which case the in-range filter missed it
        let have: std::collections::BTreeSet<EventId> =
            events.iter().map(|e| e.id).collect();
        let extra_offs: Vec<Event> = self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
            .filter_map(|n| n.off_id)
            .filter(|id| !have.contains(id))
            .filter_map(|id| {
                self.by_id
                    .get(&id)
                    .map(|&(ti, ei)| self.tracks[ti].events[ei].clone())
            })
            .collect();
        events.extend(extra_offs);
        for e in &mut events {
            e.tick = e.tick.saturating_add(span);
            e.id = self.alloc_event_id();
        }
        if events.is_empty() {
            return vec![];
        }
        vec![Op::InsertEvents { track, events }]
    }

    /// Delete every channel event in [from,to) plus the matching NoteOff of
    /// any note that starts inside the range (notes delete whole).
    pub fn delete_range_ops(&mut self, track: usize, from: u64, to: u64) -> Vec<Op> {
        let t = match self.tracks.get(track) {
            Some(t) => t,
            None => return vec![],
        };
        let mut ids: std::collections::BTreeSet<EventId> = t
            .events
            .iter()
            .filter(|e| e.tick >= from && e.tick < to)
            .filter(|e| matches!(e.kind, EventKind::Channel { .. }))
            .map(|e| e.id)
            .collect();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            ids.insert(n.on_id);
            if let Some(o) = n.off_id {
                ids.insert(o);
            }
        }
        ids.into_iter()
            .filter_map(|id| {
                self.by_id
                    .get(&id)
                    .copied()
                    .map(|(ti, ei)| Op::RemoveEvents {
                        track: ti,
                        removed: vec![(ei, self.tracks[ti].events[ei].clone())],
                    })
            })
            .collect()
    }

    /// Append a fresh track (EOT at tick 0) and optionally a name meta.
    pub fn add_track_ops(&mut self, name: Option<&str>) -> Vec<Op> {
        let mut events = vec![Event {
            id: self.alloc_event_id(),
            tick: 0,
            seq: 0,
            raw_body: None,
            kind: EventKind::Meta {
                meta_type: 0x2F,
                data: Bytes::new(),
            },
        }];
        if let Some(n) = name {
            events.insert(
                0,
                Event {
                    id: self.alloc_event_id(),
                    tick: 0,
                    seq: 0,
                    raw_body: None,
                    kind: EventKind::Meta {
                        meta_type: 0x03,
                        data: Bytes::copy_from_slice(n.as_bytes()),
                    },
                },
            );
        }
        vec![Op::InsertTrack {
            index: self.tracks.len(),
            track: Track {
                name: name.map(|n| Bytes::copy_from_slice(n.as_bytes())),
                out_port: 0,
                out_channel: 0,
                events,
            },
        }]
    }

    /// Remove track `index` entirely (undo restores it wholesale).
    pub fn remove_track_ops(&mut self, index: usize) -> Vec<Op> {
        match self.tracks.get(index) {
            Some(t) => vec![Op::RemoveTrack {
                index,
                track: t.clone(),
            }],
            None => vec![],
        }
    }

    /// Set/replace the track name meta (0x03) at tick 0.
    pub fn set_track_name_ops(&mut self, track: usize, name: &str) -> Vec<Op> {
        if let Some(e) = self
            .tracks
            .get(track)
            .and_then(|t| {
                t.events
                    .iter()
                    .find(|e| matches!(e.kind, EventKind::Meta { meta_type: 0x03, .. }))
            })
            .cloned()
        {
            let mut after = e.clone();
            after.kind = EventKind::Meta {
                meta_type: 0x03,
                data: Bytes::copy_from_slice(name.as_bytes()),
            };
            return vec![Op::UpdateEvent {
                track,
                before: e,
                after,
            }];
        }
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick: 0,
                seq: 0,
                raw_body: None,
                kind: EventKind::Meta {
                    meta_type: 0x03,
                    data: Bytes::copy_from_slice(name.as_bytes()),
                },
            }],
        }]
    }
}
#[derive(Debug, Default)]
pub struct TempoMap {
    /// (tick, us_per_quarter, cumulative_us)
    points: Vec<(u64, u32, u64)>,
    division: Division,
}

impl TempoMap {
    pub fn build(tracks: &[Track], division: Division) -> Self {
        let mut tempos: Vec<(u64, u32)> = tracks
            .iter()
            .flat_map(|t| &t.events)
            .filter_map(|e| match &e.kind {
                EventKind::Meta {
                    meta_type: 0x51,
                    data,
                } if data.len() == 3 => {
                    Some((e.tick, u32::from_be_bytes([0, data[0], data[1], data[2]])))
                }
                _ => None,
            })
            .collect();
        tempos.sort_by_key(|e| e.0);
        tempos.dedup_by_key(|e| e.0);

        let mut points = Vec::with_capacity(tempos.len());
        let mut cum: u64 = 0;
        let mut prev_tick = 0u64;
        let mut prev_mpq = 500_000u32; // default 120bpm
        for (tick, mpq) in tempos {
            if let Division::Metrical(ppq) = division {
                if ppq > 0 {
                    cum += (tick - prev_tick) * prev_mpq as u64 / ppq as u64;
                }
            }
            points.push((tick, mpq, cum));
            prev_tick = tick;
            prev_mpq = mpq;
        }
        TempoMap { points, division }
    }

    /// Breakpoints as (tick, µs-per-quarter, cumulative-µs); for MCP reads.
    pub fn points(&self) -> &[(u64, u32, u64)] {
        &self.points
    }

    pub fn ppq(&self) -> u64 {
        match self.division {
            Division::Metrical(p) => p.max(1) as u64,
            // SMPTE has no quarter note; 480 keeps UI grid math sane
            Division::Smpte { .. } => 480,
        }
    }

    /// Ticks per second for SMPTE timing (tempo events don't apply there).
    fn smpte_tps(&self) -> u64 {
        match self.division {
            Division::Smpte { fps, ticks_per_frame } => {
                fps.max(1) as u64 * ticks_per_frame.max(1) as u64
            }
            Division::Metrical(_) => 0,
        }
    }

    pub fn tick_to_us(&self, tick: u64) -> u64 {
        if let Division::Smpte { .. } = self.division {
            return ((tick as u128 * 1_000_000) / self.smpte_tps() as u128) as u64;
        }
        let ppq = match self.division {
            Division::Metrical(p) => p.max(1) as u64,
            Division::Smpte { .. } => unreachable!(),
        };
        let (t0, mpq, cum) = match self.points.binary_search_by_key(&tick, |p| p.0) {
            Ok(i) => self.points[i],
            Err(0) => (0, 500_000, 0),
            Err(i) => self.points[i - 1],
        };
        cum + (tick - t0) * mpq as u64 / ppq
    }

    /// Inverse of `tick_to_us` — for playhead positioning.
    pub fn us_to_tick(&self, us: u64) -> u64 {
        if let Division::Smpte { .. } = self.division {
            return ((us as u128 * self.smpte_tps() as u128) / 1_000_000) as u64;
        }
        let ppq = match self.division {
            Division::Metrical(p) => p.max(1) as u64,
            Division::Smpte { .. } => unreachable!(),
        };
        // last breakpoint whose cumulative time is <= us
        let i = match self.points.binary_search_by(|p| p.2.cmp(&us)) {
            Ok(i) => i,
            Err(0) => return us * ppq / 500_000,
            Err(i) => i - 1,
        };
        let (t0, mpq, cum) = self.points[i];
        t0 + (us - cum) * ppq / mpq.max(1) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc_with_note() -> Document {
        let ev = smf_core::Event {
            tick: 480,
            seq: 0,
            raw_body: None,
            kind: EventKind::Channel {
                status: 0x90,
                data: [60, 100],
                len: 2,
            },
        };
        let f = smf_core::File {
            format: 1,
            division: Division::Metrical(480),
            tracks: vec![smf_core::Track { events: vec![ev] }],
            warnings: vec![],
        };
        Document::from_file(f)
    }

    #[test]
    fn apply_checks_revision() {
        let mut d = doc_with_note();
        let bad = Transaction {
            label: "x".into(),
            base: 99,
            ops: vec![],
        };
        assert!(matches!(
            d.apply(bad),
            Err(ApplyError::StaleRevision { .. })
        ));
    }

    #[test]
    fn notes_pairing_and_dangling() {
        // one paired note (on 480/off 960) + one dangling NoteOn at 1440
        let mk = |tick, status, d0, d1| smf_core::Event {
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
            division: Division::Metrical(480),
            tracks: vec![smf_core::Track {
                events: vec![
                    mk(480, 0x90, 60, 100),
                    mk(960, 0x80, 60, 0),
                    mk(1440, 0x91, 64, 90),
                ],
            }],
            warnings: vec![],
        };
        let d = Document::from_file(f);
        let notes = d.notes();
        assert_eq!(notes.len(), 2);
        let paired = notes.iter().find(|n| n.key == 60).unwrap();
        assert_eq!((paired.start_tick, paired.end_tick), (480, Some(960)));
        let dangling = notes.iter().find(|n| n.key == 64).unwrap();
        assert_eq!((dangling.channel, dangling.end_tick), (1, None));
    }

    #[test]
    fn apply_and_revert() {
        let mut d = doc_with_note();
        let new_ev = Event {
            id: 999,
            tick: 0,
            seq: 0,
            raw_body: None,
            kind: EventKind::Channel {
                status: 0x90,
                data: [64, 90],
                len: 2,
            },
        };
        let tx = Transaction {
            label: "ins".into(),
            base: 0,
            ops: vec![Op::InsertEvents {
                track: 0,
                events: vec![new_ev],
            }],
        };
        d.apply(tx.clone()).unwrap();
        assert_eq!(d.tracks[0].events.len(), 2);
        d.revert(&tx);
        assert_eq!(d.tracks[0].events.len(), 1);
    }

    #[test]
    fn tempo_map_basic() {
        // 120bpm default, 480ppq: tick 480 -> 500000us
        let d = doc_with_note();
        assert_eq!(d.tempo_map.tick_to_us(480), 500_000);
    }

    // ---- chase_events ----

    fn chase_doc(events: Vec<smf_core::Event>) -> Document {
        let f = smf_core::File {
            format: 1,
            division: Division::Metrical(480),
            tracks: vec![smf_core::Track { events }],
            warnings: vec![],
        };
        Document::from_file(f)
    }

    fn ev(tick: u64, status: u8, d0: u8, d1: u8) -> smf_core::Event {
        smf_core::Event {
            tick,
            seq: 0,
            raw_body: None,
            kind: EventKind::Channel {
                status,
                data: [d0, d1],
                len: 2,
            },
        }
    }

    fn bytes_of(chase: &[(u64, usize, Vec<u8>)]) -> Vec<Vec<u8>> {
        chase.iter().map(|(_, _, b)| b.clone()).collect()
    }

    #[test]
    fn chase_at_zero_is_empty() {
        let d = chase_doc(vec![ev(0, 0x90, 60, 100)]);
        assert!(d.chase_events(0).is_empty());
    }

    #[test]
    fn chase_full_state_held_and_sustained_notes() {
        let d = chase_doc(vec![
            ev(0, 0xB0, 0, 1),     // bank MSB
            ev(1, 0xB0, 32, 2),    // bank LSB
            ev(2, 0xC0, 5, 0),     // program 5
            ev(10, 0xB0, 7, 100),  // volume
            ev(20, 0xB0, 7, 90),   // later volume wins
            ev(30, 0xE0, 3, 64),   // pitch bend
            ev(15, 0xA0, 60, 40),  // poly AT
            ev(16, 0xD0, 55, 0),   // channel AT
            ev(40, 0xB0, 64, 127), // pedal down
            ev(100, 0x90, 60, 100),
            ev(200, 0x90, 64, 80),
            ev(300, 0x80, 60, 0), // 60 released under pedal -> sustained
            ev(500, 0x90, 72, 70), // still held at the chase point
        ]);
        let chase = d.chase_events(d.tempo_map.tick_to_us(720));
        assert!(chase.iter().all(|&(us, tr, _)| us == d.tempo_map.tick_to_us(720) && tr == 0));
        assert_eq!(
            bytes_of(&chase),
            vec![
                vec![0xB0, 0, 1],    // bank MSB before PC
                vec![0xB0, 32, 2],   // bank LSB
                vec![0xC0, 5],       // program
                vec![0xB0, 7, 90],   // CC last value
                vec![0xB0, 64, 127], // pedal down before the pairs below
                vec![0xE0, 3, 64],   // bend
                vec![0xD0, 55],      // channel AT
                vec![0xA0, 60, 40],  // poly AT
                vec![0x90, 64, 80],  // held notes (ascending key)
                vec![0x90, 72, 70],
                vec![0x90, 60, 100], // sustained note re-struck on+off
                vec![0x80, 60, 0],
            ]
        );
    }

    #[test]
    fn chase_pedal_up_clears_sustained() {
        let d = chase_doc(vec![
            ev(40, 0xB0, 64, 127),
            ev(100, 0x90, 60, 100),
            ev(200, 0x80, 60, 0),
            ev(600, 0xB0, 64, 0),
        ]);
        let chase = d.chase_events(d.tempo_map.tick_to_us(720));
        assert_eq!(bytes_of(&chase), vec![vec![0xB0, 64, 0]]);
    }

    #[test]
    fn chase_cc121_clears_controllers_but_not_bank_program() {
        let d = chase_doc(vec![
            ev(0, 0xB0, 0, 1),
            ev(1, 0xB0, 32, 2),
            ev(2, 0xC0, 5, 0),
            ev(10, 0xB0, 7, 90),
            ev(30, 0xE0, 3, 64),
            ev(600, 0xB0, 121, 0), // reset all controllers
        ]);
        let chase = d.chase_events(d.tempo_map.tick_to_us(720));
        assert_eq!(
            bytes_of(&chase),
            vec![vec![0xB0, 0, 1], vec![0xB0, 32, 2], vec![0xC0, 5]]
        );
    }

    #[test]
    fn chase_mode_messages_drop_notes_and_are_not_chased() {
        // all-notes-off releases held notes (no pedal): nothing to restrike
        let d = chase_doc(vec![
            ev(100, 0x90, 60, 100),
            ev(600, 0xB0, 123, 0),
        ]);
        assert!(d.chase_events(d.tempo_map.tick_to_us(720)).is_empty());
        // all-sound-off kills even pedal-caught notes
        let d = chase_doc(vec![
            ev(10, 0xB0, 64, 127),
            ev(100, 0x90, 60, 100),
            ev(200, 0x80, 60, 0),
            ev(600, 0xB0, 120, 0),
        ]);
        assert_eq!(
            bytes_of(&d.chase_events(d.tempo_map.tick_to_us(720))),
            vec![vec![0xB0, 64, 127]]
        );
    }

    #[test]
    fn chase_rpn_nrpn_selector_then_data() {
        let d = chase_doc(vec![
            ev(10, 0xB0, 101, 0), // RPN 0,0 (pitch bend sensitivity)
            ev(11, 0xB0, 100, 0),
            ev(12, 0xB0, 6, 2),   // data MSB
            ev(20, 0xB0, 99, 1),  // switch to NRPN 1,3
            ev(21, 0xB0, 98, 3),
            ev(22, 0xB0, 38, 5),  // data LSB
        ]);
        assert_eq!(
            bytes_of(&d.chase_events(d.tempo_map.tick_to_us(720))),
            vec![
                vec![0xB0, 99, 1], // NRPN msb first, then lsb...
                vec![0xB0, 98, 3],
                vec![0xB0, 6, 2], // ...then data entry
                vec![0xB0, 38, 5],
            ]
        );
        // all-zero RPN (the common case) must still be chased
        let d = chase_doc(vec![ev(10, 0xB0, 101, 0), ev(11, 0xB0, 100, 0)]);
        assert_eq!(
            bytes_of(&d.chase_events(d.tempo_map.tick_to_us(720))),
            vec![vec![0xB0, 101, 0], vec![0xB0, 100, 0]]
        );
    }

    #[test]
    fn chase_excludes_events_at_the_play_position() {
        let d = chase_doc(vec![
            ev(10, 0xB0, 7, 100),
            ev(30, 0xE0, 3, 64), // exactly at the start: plays as a real event
        ]);
        let start = d.tempo_map.tick_to_us(30);
        let chase = d.chase_events(start);
        assert_eq!(bytes_of(&chase), vec![vec![0xB0, 7, 100]]);
    }

    #[test]
    fn chase_is_per_track() {
        let f = smf_core::File {
            format: 1,
            division: Division::Metrical(480),
            tracks: vec![
                smf_core::Track { events: vec![ev(10, 0xB0, 7, 10)] },
                smf_core::Track { events: vec![ev(10, 0xB0, 7, 20)] },
            ],
            warnings: vec![],
        };
        let d = Document::from_file(f);
        let chase = d.chase_events(d.tempo_map.tick_to_us(720));
        assert_eq!(
            chase,
            vec![
                (d.tempo_map.tick_to_us(720), 0, vec![0xB0, 7, 10]),
                (d.tempo_map.tick_to_us(720), 1, vec![0xB0, 7, 20]),
            ]
        );
    }

    #[test]
    fn chase_dangling_noteon_is_held() {
        let d = chase_doc(vec![ev(100, 0x91, 64, 90)]);
        assert_eq!(
            bytes_of(&d.chase_events(d.tempo_map.tick_to_us(720))),
            vec![vec![0x91, 64, 90]]
        );
    }

    // ---- SysEx timeline / chase ----

    fn sx(tick: u64, payload: &[u8]) -> smf_core::Event {
        smf_core::Event {
            tick,
            seq: 0,
            raw_body: None,
            kind: EventKind::SysEx(Bytes::copy_from_slice(payload)),
        }
    }

    fn esc(tick: u64, payload: &[u8]) -> smf_core::Event {
        smf_core::Event {
            tick,
            seq: 0,
            raw_body: None,
            kind: EventKind::Escape(Bytes::copy_from_slice(payload)),
        }
    }

    #[test]
    fn sysex_complete_and_split_messages_join() {
        // one complete message + one split across an F0 and two F7 escapes,
        // with a channel event interleaved between the fragments
        let d = chase_doc(vec![
            sx(0, &[0x7E, 0x7F, 0x09, 0x01, 0xF7]), // GM system on
            sx(100, &[0x41, 0x10, 0x42]),           // split head, no F7
            ev(110, 0x90, 60, 100),                 // interleaved event
            esc(120, &[0x12, 0x40]),                // continuation
            esc(130, &[0x00, 0xF7]),                // final fragment
        ]);
        let t0 = d.tempo_map.tick_to_us(0);
        let t100 = d.tempo_map.tick_to_us(100);
        assert_eq!(
            d.timeline_sysex(),
            vec![
                (t0, 0, vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7]),
                (
                    t100,
                    0,
                    vec![0xF0, 0x41, 0x10, 0x42, 0x12, 0x40, 0x00, 0xF7]
                ),
            ]
        );
    }

    #[test]
    fn sysex_standalone_escapes_and_open_messages() {
        // standalone escapes carry arbitrary bytes — never sent
        let d = chase_doc(vec![
            esc(10, &[0x01, 0x02]),
            sx(20, &[0x7E, 0x7F]), // never terminated
            sx(30, &[0x7E, 0x7F, 0x09, 0x01, 0xF7]), // new F0 closes it
        ]);
        let t20 = d.tempo_map.tick_to_us(20);
        let t30 = d.tempo_map.tick_to_us(30);
        assert_eq!(
            d.timeline_sysex(),
            vec![
                (t20, 0, vec![0xF0, 0x7E, 0x7F, 0xF7]), // closed with F7
                (t30, 0, vec![0xF0, 0x7E, 0x7F, 0x09, 0x01, 0xF7]),
            ]
        );
        // open at end of track is closed too
        let d = chase_doc(vec![sx(20, &[0x41, 0x10])]);
        assert_eq!(
            d.timeline_sysex(),
            vec![(d.tempo_map.tick_to_us(20), 0, vec![0xF0, 0x41, 0x10, 0xF7])]
        );
    }

    #[test]
    fn chase_sysex_picks_last_complete_per_track() {
        let d = chase_doc(vec![
            sx(0, &[0x7E, 0x7F, 0x09, 0x01, 0xF7]), // GM on
            sx(100, &[0x41, 0x10, 0x12, 0x00, 0xF7]), // later message wins
            sx(200, &[0x41, 0x10, 0x40]), // incomplete at the boundary: skipped
        ]);
        let start = d.tempo_map.tick_to_us(720);
        assert_eq!(
            d.chase_sysex(start),
            vec![(start, 0, vec![0xF0, 0x41, 0x10, 0x12, 0x00, 0xF7])]
        );
        // before any message: nothing
        assert!(d.chase_sysex(d.tempo_map.tick_to_us(0)).is_empty());
    }
}

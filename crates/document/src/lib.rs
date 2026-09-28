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

    /// The single edit entry point shared by GUI and MCP.
    pub fn apply(&mut self, tx: Transaction) -> Result<Revision, ApplyError> {
        if tx.base != self.revision {
            return Err(ApplyError::StaleRevision {
                expected: self.revision,
                got: tx.base,
            });
        }
        for op in &tx.ops {
            match op {
                Op::InsertEvents { track, events } => {
                    let t = self.tracks.get_mut(*track).ok_or(ApplyError::UnknownTrack(*track))?;
                    for e in events {
                        let pos = t
                            .events
                            .binary_search_by_key(&(e.tick, e.seq), |x| (x.tick, x.seq))
                            .unwrap_or_else(|p| p);
                        t.events.insert(pos, e.clone());
                    }
                }
                Op::RemoveEvents { track, .. } => {
                    let t = self.tracks.get_mut(*track).ok_or(ApplyError::UnknownTrack(*track))?;
                    for (_, e) in &op_removed(op) {
                        if let Some(pos) = t.events.iter().position(|x| x.id == e.id) {
                            t.events.remove(pos);
                        }
                    }
                }
                Op::UpdateEvent { track, after, .. } => {
                    let t = self.tracks.get_mut(*track).ok_or(ApplyError::UnknownTrack(*track))?;
                    if let Some(pos) = t.events.iter().position(|x| x.id == after.id) {
                        t.events[pos] = after.clone();
                        t.events.sort_by_key(|e| (e.tick, e.seq));
                    }
                }
            }
        }
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

fn op_removed(op: &Op) -> Vec<(usize, Event)> {
    match op {
        Op::RemoveEvents { removed, .. } => removed.clone(),
        _ => vec![],
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

/// SetTempo breakpoints + cumulative microseconds, binary-searched.
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
                    cum += (tick - prev_tick) as u64 * prev_mpq as u64 / ppq as u64;
                }
            }
            points.push((tick, mpq, cum));
            prev_tick = tick;
            prev_mpq = mpq;
        }
        TempoMap { points, division }
    }

    pub fn tick_to_us(&self, tick: u64) -> u64 {
        let ppq = match self.division {
            Division::Metrical(p) => p.max(1) as u64,
            Division::Smpte { .. } => return tick, // SMPTE handled separately
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
        let ppq = match self.division {
            Division::Metrical(p) => p.max(1) as u64,
            Division::Smpte { .. } => return us,
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
}

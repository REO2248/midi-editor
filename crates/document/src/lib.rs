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

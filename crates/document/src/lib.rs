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
use std::collections::{BTreeMap, HashMap};
use thiserror::Error;

mod timing;
pub use timing::{MeterEvent, MeterMap, PositionFormat, TimeDisplay};

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

#[derive(Debug, Clone)]
pub struct Document {
    pub format: u16,
    pub division: Division,
    pub tracks: Vec<Track>,
    pub(crate) revision: Revision,
    pub(crate) next_event_id: EventId,
    pub(crate) by_id: HashMap<EventId, (usize, usize)>, // id -> (track, event index)
    pub tempo_map: TempoMap,
    /// FF 58 breakpoints for the shared (conductor) timeline; format-2
    /// per-sequence maps derive on demand via `meter_map_for`.
    pub meter_map: MeterMap,
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("stale base revision: expected {expected}, got {got}")]
    StaleRevision { expected: Revision, got: Revision },
    #[error("unknown event id {0}")]
    UnknownEvent(EventId),
    #[error("unknown track {0}")]
    UnknownTrack(usize),
    #[error("a document must keep at least one track")]
    EmptyDocument,
    #[error("format {format} cannot declare {tracks} tracks")]
    FormatTrackMismatch { format: u16, tracks: usize },
}

/// One undo step. `before`/`after` are self-contained diffs so undo needs no
/// inverse computation.
#[derive(Debug, Clone)]
pub struct Transaction {
    pub label: String,
    pub base: Revision,
    pub ops: Vec<Op>,
}

/// Result of `Document::apply`: the new revision plus the *effective*
/// transaction — the caller's ops followed by any structural-normalization
/// ops the document synthesized (a lone End-of-Track is kept last after
/// every touched track). Undo of the effective transaction restores the
/// exact pre-edit state; redo replays it identically since normalization
/// is deterministic.
#[derive(Debug, Clone)]
pub struct Applied {
    pub revision: Revision,
    pub tx: Transaction,
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
        /// Index `before` occupied when the op applied — resolved by
        /// `Document::apply` (`usize::MAX` until then). Undo re-inserts
        /// `before` at exactly this slot so the original position among
        /// same-(tick,seq) events is restored byte-for-byte.
        pos: usize,
        before: Event,
        after: Event,
    },
    /// Append a track at `index` (usually == tracks.len()); `track` is the
    /// inserted track's before-image so undo can remove it by position.
    InsertTrack {
        index: usize,
        track: Track,
    },
    /// Remove the whole track; `track` is its before-image.
    RemoveTrack {
        index: usize,
        track: Track,
    },
    /// Replace the track's display name meta (0x03) before-image kept.
    UpdateTrack {
        index: usize,
        before: Track,
        after: Track,
    },
    /// Explicit SMF format conversion (0→1 when a track is added to a
    /// format-0 file, or an explicit convert command). Never implicit —
    /// the serializer writes exactly the declared format.
    SetFormat {
        before: u16,
        after: u16,
    },
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
    /// Release velocity carried by the off event's second data byte.
    /// Meaningful for the 0x80 form; always 0 for the 0x90-vel0 form and
    /// for dangling note-ons.
    pub off_vel: u8,
    /// Which wire form closes the note: `true` = NoteOn velocity 0 (0x90),
    /// `false` = real NoteOff (0x80). The forms are semantically identical
    /// to receivers but distinct in the file — preserved on edits.
    pub off_via_on: bool,
}

/// One RPN/NRPN parameter-write derived by scanning a track's event list.
/// Purely a view over raw events — selectors (CC 100/101 or 98/99) and data
/// entry (CC 6/38) keep their original order and bytes; `ids` point back to
/// the underlying events so edits can target them precisely.
#[derive(Debug, Clone)]
pub struct RpnEntry {
    pub track: usize,
    pub channel: u8,
    /// true = NRPN (CC 98/99), false = RPN (CC 100/101)
    pub nrpn: bool,
    /// selector bytes as stored (null selector is 0x7F/0x7F)
    pub param_msb: u8,
    pub param_lsb: u8,
    /// tick of the first selector event
    pub tick: u64,
    /// selector event ids in event order (usually 2)
    pub sel_ids: Vec<EventId>,
    pub data_msb: Option<u8>,
    pub data_lsb: Option<u8>,
    pub data_msb_id: Option<EventId>,
    pub data_lsb_id: Option<EventId>,
    /// further data-entry events after the first 6/38 pair
    pub extra_data_ids: Vec<EventId>,
}

impl RpnEntry {
    /// Null selector (0x7F/0x7F) — the spec's "end parameter" reset.
    pub fn is_null(&self) -> bool {
        self.param_msb == 0x7F && self.param_lsb == 0x7F
    }

    /// 14-bit parameter number (msb<<7 | lsb).
    pub fn param14(&self) -> u16 {
        ((self.param_msb as u16) << 7) | self.param_lsb as u16
    }

    /// Entered value: 14-bit when an LSB was written, else 7-bit MSB only.
    pub fn value(&self) -> Option<u16> {
        match (self.data_msb, self.data_lsb) {
            (Some(m), Some(l)) => Some(((m as u16) << 7) | l as u16),
            (Some(m), None) => Some(m as u16),
            _ => None,
        }
    }

    /// True when the value used 14-bit entry (CC6 + CC38).
    pub fn is_14bit(&self) -> bool {
        self.data_lsb.is_some()
    }

    /// Well-known RPN mnemonics; NRPN and unassigned RPN numbers are unnamed.
    pub fn param_name(&self) -> Option<&'static str> {
        if self.nrpn || self.is_null() {
            return None;
        }
        match (self.param_msb, self.param_lsb) {
            (0, 0) => Some("Pitch Bend Range"),
            (0, 1) => Some("Fine Tuning"),
            (0, 2) => Some("Coarse Tuning"),
            (0, 3) => Some("Tuning Program"),
            (0, 4) => Some("Tuning Bank"),
            (0, 5) => Some("Mod Depth Range"),
            (0x7F, _) => None,
            _ => None,
        }
    }

    /// Every event id belonging to this entry (selectors first, then data).
    pub fn ids(&self) -> Vec<EventId> {
        let mut v = self.sel_ids.clone();
        v.extend(self.data_msb_id);
        v.extend(self.data_lsb_id);
        v.extend_from_slice(&self.extra_data_ids);
        v
    }
}

/// A program-change event with the bank select state in effect at its tick.
/// Derived view — never written back to the event list.
#[derive(Debug, Clone)]
pub struct ProgramChange {
    pub track: usize,
    pub channel: u8,
    pub tick: u64,
    pub id: EventId,
    pub bank_msb: u8,
    pub bank_lsb: u8,
    pub program: u8,
}

#[derive(Debug, Default, Clone)]
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
                                       // fractional-µs remainder carried between segments (#224): dense
                                       // tempo ramps (50–100 events/bar, Δtick 5–10) lose <1µs per segment
                                       // to integer division, which accumulates to multi-millisecond
                                       // drift over a cue — the remainder keeps every segment exact
        let mut rem: u128 = 0;
        for (tick, mpq) in tempos {
            if let Division::Metrical(ppq) = division {
                if ppq > 0 {
                    // ticks can reach u64-scale via hostile VLQ deltas;
                    // saturate at "far future" instead of overflowing
                    let prod = (tick - prev_tick) as u128 * prev_mpq as u128 + rem;
                    let whole = prod / ppq as u128;
                    cum = cum.saturating_add(u64::try_from(whole).unwrap_or(u64::MAX));
                    rem = prod % ppq as u128;
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

    /// Ticks per quarter note for metrical divisions; `None` for SMPTE —
    /// there is no quarter note to derive one from, and returning a
    /// pretend value puts invented bar/beat positions in front of users.
    /// Callers needing a display grid should use
    /// [`Document::time_display`].
    pub fn ppq(&self) -> Option<u64> {
        match self.division {
            Division::Metrical(p) => Some(p.max(1) as u64),
            Division::Smpte { .. } => None,
        }
    }

    /// Exact SMPTE tick rate as `(numerator, denominator)` ticks/second
    /// (tempo events don't apply there). The `-29` division is 29.97
    /// drop-frame — 30000/1001 fps, not literal 29 (#211).
    fn smpte_rate(&self) -> (u64, u64) {
        match self.division {
            Division::Smpte {
                fps,
                ticks_per_frame,
            } => timing::smpte_rate(fps, ticks_per_frame),
            Division::Metrical(_) => (0, 1),
        }
    }

    pub fn tick_to_us(&self, tick: u64) -> u64 {
        if let Division::Smpte { .. } = self.division {
            let (num, den) = self.smpte_rate();
            return (((tick as u128) * 1_000_000 * den as u128) / num as u128).min(u64::MAX as u128)
                as u64;
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
        cum.saturating_add((tick - t0).saturating_mul(mpq as u64) / ppq)
    }

    /// Inverse of `tick_to_us` — for playhead positioning.
    pub fn us_to_tick(&self, us: u64) -> u64 {
        if let Division::Smpte { .. } = self.division {
            let (num, den) = self.smpte_rate();
            return (((us as u128) * num as u128) / (1_000_000 * den as u128)).min(u64::MAX as u128)
                as u64;
        }
        let ppq = match self.division {
            Division::Metrical(p) => p.max(1) as u64,
            Division::Smpte { .. } => unreachable!(),
        };
        // last breakpoint whose cumulative time is <= us
        let i = match self.points.binary_search_by(|p| p.2.cmp(&us)) {
            Ok(i) => i,
            Err(0) => return us.saturating_mul(ppq) / 500_000,
            Err(i) => i - 1,
        };
        let (t0, mpq, cum) = self.points[i];
        t0.saturating_add((us - cum).saturating_mul(ppq) / mpq.max(1) as u64)
    }
}

mod apply;
pub use apply::DocSnapshot;
mod ops;

#[cfg(test)]
mod tests;

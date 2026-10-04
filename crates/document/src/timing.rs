//! How a document's timing division is presented to the user.
//!
//! Metrical (PPQ) files position events on a musical grid — bars, beats,
//! fractions of a quarter note. SMPTE files have no quarter note at all:
//! their ticks are wall-clock sub-frames (`fps * ticks_per_frame` ticks
//! per second). Pretending a PPQ (the UI once fell back to 480) invents
//! bar/beat positions that mean nothing for these files, so the timing
//! mode is explicit here: timecode positions and a seconds/frames grid
//! for SMPTE, a bar/beat grid for metrical.

use smf_core::{Division, EventKind};

/// Exact SMPTE tick rate as a rational `(numerator, denominator)` —
/// ticks per second equals `num / den`. Every standard rate is integral
/// except the SMF `-29` division, which encodes 29.97 drop-frame =
/// 30000/1001 fps: computing it as literal `29 * ticks_per_frame`
/// undercounts the rate by 3.24% and drags playback proportionally
/// slow (#211).
pub fn smpte_rate(fps: u8, ticks_per_frame: u8) -> (u64, u64) {
    let tpf = ticks_per_frame.max(1) as u64;
    match fps {
        29 => (30_000 * tpf, 1001),
        f => (f.max(1) as u64 * tpf, 1),
    }
}

/// Presentation model for the document's [`Division`]. Everything the
/// chrome (ruler, grid, event rows, playhead readout, snap/quantize
/// menus, metronome) needs to draw time correctly derives from this —
/// never from a synthesized PPQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeDisplay {
    /// ticks per quarter note — musical bar/beat grid
    Metrical { ppq: u64 },
    /// nominal fps (24/25/29.97/30 — the SMF byte 29 encodes 29.97
    /// drop-frame) and ticks per frame
    Smpte { fps: u8, ticks_per_frame: u8 },
}

impl TimeDisplay {
    pub fn of(division: Division) -> Self {
        match division {
            Division::Metrical(p) => Self::Metrical {
                ppq: (p as u64).max(1),
            },
            Division::Smpte {
                fps,
                ticks_per_frame,
            } => Self::Smpte {
                fps,
                ticks_per_frame,
            },
        }
    }

    pub fn is_smpte(self) -> bool {
        matches!(self, Self::Smpte { .. })
    }

    /// Ticks per real wall-clock second, floored from the exact rational
    /// rate (`smpte_rate`) — `30000/1001 * ticks_per_frame` for the
    /// -29 drop-frame division, so 29.97/100 tpf reads 2997, not 2900.
    /// `0` for metrical.
    pub fn ticks_per_second(self) -> u64 {
        match self {
            Self::Smpte {
                fps,
                ticks_per_frame,
            } => {
                let (num, den) = smpte_rate(fps, ticks_per_frame);
                num / den.max(1)
            }
            Self::Metrical { .. } => 0,
        }
    }

    /// Frames the timecode display counts per *nominal* second. Straight
    /// fps for 24/25/30; for the -29 (29.97 drop) division numbering runs
    /// at nominal 30, so one displayed second is 30 frames — a ruler tick
    /// per real second would land on ragged timecodes like 00:00:00.29.
    fn nominal_fps(self) -> u64 {
        match self {
            Self::Smpte { fps: 29, .. } => 30,
            Self::Smpte { fps, .. } => fps.max(1) as u64,
            Self::Metrical { .. } => 0,
        }
    }

    /// Fine grid interval in ticks: one quarter note / one frame.
    pub fn cell_ticks(self) -> u64 {
        match self {
            Self::Metrical { ppq } => ppq,
            Self::Smpte {
                ticks_per_frame, ..
            } => ticks_per_frame.max(1) as u64,
        }
    }

    /// Coarse grid interval: one 4/4 bar / one displayed (timecode)
    /// second — `format_tick` lands on a round `*.00` at this interval
    /// for every fps, so ruler ticks always align with position labels.
    pub fn bar_ticks(self) -> u64 {
        match self {
            Self::Metrical { ppq } => ppq * 4,
            Self::Smpte { .. } => self.nominal_fps() * self.cell_ticks(),
        }
    }

    /// Ticks the snap divisor list subdivides: a whole note / one
    /// displayed second. "1/16" snap = a 16th note for metrical, a 16th
    /// of a timecode second for SMPTE — the grid is redefined in time
    /// units, not in fake beats.
    pub fn snap_base_ticks(self) -> u64 {
        self.bar_ticks()
    }

    /// Smallest sensible note/quantize quantum: a 16th note / one frame.
    pub fn min_grid_ticks(self) -> u64 {
        match self {
            Self::Metrical { ppq } => ppq / 4,
            Self::Smpte {
                ticks_per_frame, ..
            } => ticks_per_frame.max(1) as u64,
        }
    }

    /// Metronome click interval: one beat / one displayed second.
    pub fn click_ticks(self) -> u64 {
        match self {
            Self::Metrical { ppq } => ppq,
            Self::Smpte { .. } => self.bar_ticks(),
        }
    }

    /// Arrow-key nudge step when snap is off: a 32nd note / one frame.
    pub fn nudge_ticks(self) -> u64 {
        match self {
            Self::Metrical { ppq } => ppq / 8,
            Self::Smpte {
                ticks_per_frame, ..
            } => ticks_per_frame.max(1) as u64,
        }
    }

    /// (minor, major) grid intervals in ticks for a pixels-per-tick zoom.
    /// Minor lines collapse into major-only when they'd draw < 4px apart.
    pub fn grid_ticks(self, zoom: f32) -> (u64, u64) {
        let bar = self.bar_ticks();
        let cell = self.cell_ticks();
        let minor = if cell as f32 * zoom >= 4.0 { cell } else { bar };
        (minor, bar)
    }

    /// Short chrome badge identifying the timing mode: "480ppq" /
    /// "25fps" / "29.97df" — the explicit UI timing mode label.
    pub fn badge(self) -> String {
        match self {
            Self::Metrical { ppq } => format!("{ppq}ppq"),
            Self::Smpte { fps: 29, .. } => "29.97df".to_string(),
            Self::Smpte { fps, .. } => format!("{fps}fps"),
        }
    }

    /// Status-bar / event-row position string: `bar.beat.tick` for
    /// metrical, SMPTE timecode `hh:mm:ss.ff` for timecode files.
    /// Sub-frame remainder (a tick that isn't frame-aligned) shows as
    /// `+tt` so the position stays lossless.
    pub fn format_tick(self, tick: u64) -> String {
        match self {
            Self::Metrical { ppq } => {
                let bar = tick / (ppq * 4) + 1;
                let beat = (tick % (ppq * 4)) / ppq + 1;
                format!("{bar}.{beat}.{:>3}", tick % ppq)
            }
            Self::Smpte { .. } => {
                let tpf = self.cell_ticks();
                let frames = tick / tpf;
                let sub = tick % tpf;
                let s = format_smpte(self, frames);
                if sub == 0 {
                    s
                } else {
                    format!("{s}+{sub:02}")
                }
            }
        }
    }
}

/// One `FF 58` time-signature event decoded for bar/beat math. All four
/// payload bytes are carried — `nn dd cc bb` per the SMF spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeterEvent {
    /// `nn` — beats per bar (numerator, literal value)
    pub num: u8,
    /// `dd` — denominator as a power of two (3 = eighths)
    pub den_pow: u8,
    /// `cc` — MIDI clocks (24ths of a quarter) per metronome click
    pub click_clocks: u8,
    /// `bb` — 32nd notes per quarter (notational, informational)
    pub n32: u8,
}

impl MeterEvent {
    /// The meter a file with no FF58 is assumed to run on: 4/4 with a
    /// quarter-note click — the spec default.
    pub const COMMON: Self = Self {
        num: 4,
        den_pow: 2,
        click_clocks: 24,
        n32: 8,
    };

    /// Denominator as the literal note value (4, 8, 16, …).
    pub fn denominator(&self) -> u32 {
        1u32 << self.den_pow.min(31)
    }

    /// Sensible click interval for a signature written without a `cc`
    /// convention: dotted quarter (36 clocks) in compound meter, quarter
    /// otherwise — what Cubase/REAPER emit for 6/8, 9/8, 12/8.
    pub fn default_click_clocks(num: u8, den_pow: u8) -> u8 {
        if den_pow >= 3 && num > 3 && num.is_multiple_of(3) {
            36
        } else {
            24
        }
    }
}

/// The document's time-signature map: sorted `FF 58` breakpoints plus the
/// division, rebuilt after every transaction exactly like `TempoMap`.
/// All bar/beat/grid/metronome math in the app derives from this — never
/// from a hard-coded 4/4.
#[derive(Debug, Default, Clone)]
pub struct MeterMap {
    /// (tick, event) — breakpoints sorted ascending; at most one per tick
    /// (the last FF58 written at a tick wins).
    points: Vec<(u64, MeterEvent)>,
    division: Division,
}

impl MeterMap {
    /// Collect `FF 58` metas across `tracks` (conductor timeline) into
    /// breakpoints. Events missing a full 4-byte payload are skipped —
    /// they can exist in malformed files and carry no usable meter.
    pub fn build(tracks: &[crate::Track], division: Division) -> Self {
        let mut by_tick: std::collections::BTreeMap<u64, MeterEvent> =
            std::collections::BTreeMap::new();
        for e in tracks.iter().flat_map(|t| &t.events) {
            if let EventKind::Meta {
                meta_type: 0x58,
                data,
            } = &e.kind
            {
                if data.len() >= 4 {
                    by_tick.insert(
                        e.tick,
                        MeterEvent {
                            num: data[0].max(1),
                            den_pow: data[1].min(31),
                            click_clocks: data[2],
                            n32: data[3],
                        },
                    );
                }
            }
        }
        MeterMap {
            points: by_tick.into_iter().collect(),
            division,
        }
    }

    /// Breakpoints as (tick, event) — for MCP reads and tests.
    pub fn points(&self) -> &[(u64, MeterEvent)] {
        &self.points
    }

    /// Ticks per quarter note under any division. SMPTE files have no
    /// quarter — a "quarter" is defined as a quarter-second of timecode
    /// so bar math stays defined rather than collapsing to a fake PPQ.
    pub fn quarter_ticks(&self) -> u64 {
        match self.division {
            Division::Metrical(p) => (p as u64).max(1),
            Division::Smpte {
                fps,
                ticks_per_frame,
            } => {
                let (num, den) = smpte_rate(fps, ticks_per_frame);
                (num / (4 * den.max(1))).max(1)
            }
        }
    }

    /// Meter in force at `tick` — the latest breakpoint at or before it,
    /// else the spec's implicit common time.
    pub fn meter_at(&self, tick: u64) -> MeterEvent {
        match self.points.binary_search_by_key(&tick, |p| p.0) {
            Ok(i) => self.points[i].1,
            Err(0) => MeterEvent::COMMON,
            Err(i) => self.points[i - 1].1,
        }
    }

    /// Ticks per beat under `meter` (a whole note = 4 quarters).
    pub fn beat_ticks_of(&self, meter: MeterEvent) -> u64 {
        ((self.quarter_ticks() * 4) >> meter.den_pow.min(6)).max(1)
    }

    /// Ticks per bar under the meter in force at `tick`.
    pub fn bar_ticks_at(&self, tick: u64) -> u64 {
        let m = self.meter_at(tick);
        self.beat_ticks_of(m) * m.num as u64
    }

    /// Metronome click interval in ticks at `tick` — from the stored `cc`
    /// (MIDI clocks per click, 24 = quarter note).
    pub fn click_ticks_at(&self, tick: u64) -> u64 {
        let m = self.meter_at(tick);
        (self.quarter_ticks() * m.click_clocks as u64 / 24).max(1)
    }

    /// Walk the map and return (bar, beat, tick-in-beat) for `tick`,
    /// 1-based on the first two. A meter change that lands mid-bar starts
    /// a new bar — the truncated remainder counts as a full bar.
    pub fn tick_to_bbt(&self, tick: u64) -> (u64, u64, u64) {
        let mut pos = 0u64;
        let mut bar = 0u64;
        let mut m = MeterEvent::COMMON;
        for &(pt, pm) in &self.points {
            if pt == 0 {
                m = pm;
                continue;
            }
            if pt > tick {
                break;
            }
            let bar_t = self.beat_ticks_of(m) * m.num as u64;
            bar += (pt - pos).div_ceil(bar_t);
            pos = pt;
            m = pm;
        }
        let bt = self.beat_ticks_of(m);
        let bar_t = bt * m.num as u64;
        bar += (tick - pos) / bar_t;
        let rem = (tick - pos) % bar_t;
        (bar + 1, rem / bt + 1, rem % bt)
    }

    /// Tick on which the bar containing `tick` starts.
    pub fn bar_start_tick(&self, tick: u64) -> u64 {
        let mut pos = 0u64;
        let mut m = MeterEvent::COMMON;
        let mut start = 0u64;
        for &(pt, pm) in &self.points {
            if pt == 0 {
                m = pm;
                continue;
            }
            if pt > tick {
                break;
            }
            // everything before this breakpoint is sealed; the breakpoint
            // itself opens a bar under the new meter
            start = pt;
            pos = pt;
            m = pm;
        }
        let bar_t = self.beat_ticks_of(m) * m.num as u64;
        start + ((tick - pos) / bar_t) * bar_t
    }

    /// The bar boundary that ends the bar beginning at `start` (which
    /// must itself be a bar start). A meter breakpoint inside the bar's
    /// natural span ends it early — the new meter opens a fresh bar.
    fn bar_end_at(&self, start: u64) -> u64 {
        let m = self.meter_at(start);
        let natural = start + self.beat_ticks_of(m) * m.num as u64;
        match self
            .points
            .iter()
            .find(|(pt, _)| *pt > start && *pt < natural)
        {
            Some((pt, _)) => *pt,
            None => natural,
        }
    }

    /// Start of the bar following the one containing `tick`.
    pub fn next_bar_start(&self, tick: u64) -> u64 {
        self.bar_end_at(self.bar_start_tick(tick))
    }

    /// Start of the bar containing `tick` — or, when `tick` already sits
    /// on a bar line, of the bar before it ("go to previous bar").
    pub fn prev_bar_start(&self, tick: u64) -> u64 {
        let start = self.bar_start_tick(tick);
        if start < tick {
            return start;
        }
        if start == 0 {
            return 0;
        }
        // last bar boundary strictly before `start`
        let mut prev = 0u64;
        let mut cur = 0u64;
        loop {
            let n = self.bar_end_at(cur);
            if n <= cur {
                return prev;
            }
            if n >= start {
                // `cur` opens the bar that ends at (or contains) `start`
                // — itself the boundary just before it
                return cur;
            }
            prev = cur;
            cur = n;
        }
    }

    /// Tick where a `bars`-bar count-in ending at `start_tick` begins —
    /// walks bar lines BACKWARD from the position's own bar so the
    /// pre-region is metered by the signatures actually preceding the
    /// record point: counting into a 3/4 section after a 4/4 opening
    /// counts 3/4 bars, and a mid-bar start adds its pickup remainder on
    /// top of the full bars (#137).
    pub fn countin_start(&self, start_tick: u64, bars: u64) -> u64 {
        let mut t0 = self.bar_start_tick(start_tick);
        for _ in 0..bars {
            t0 = self.prev_bar_start(t0);
        }
        t0
    }

    /// Bar-line ticks in `[from, to)` — for ruler/grid drawing where bars
    /// are not a fixed stride once the meter changes.
    pub fn bar_starts_between(&self, from: u64, to: u64) -> Vec<u64> {
        let mut out = Vec::new();
        if to <= from {
            return out;
        }
        let mut cur = self.bar_start_tick(from);
        if cur < from {
            cur = self.next_bar_start(from);
        }
        while cur < to {
            out.push(cur);
            let n = self.next_bar_start(cur);
            if n <= cur {
                break;
            }
            cur = n;
        }
        out
    }

    /// Beat-line ticks in `[from, to)` as `(tick, is_downbeat)` — the
    /// minor grid for roll/ruler drawing. A bar truncated by a mid-bar
    /// meter change emits only the beats inside its short span; the new
    /// signature's own bar start is the next downbeat.
    pub fn beat_lines_between(&self, from: u64, to: u64) -> Vec<(u64, bool)> {
        let mut out = Vec::new();
        for bs in self.bar_starts_between(from, to) {
            let m = self.meter_at(bs);
            let beat = self.beat_ticks_of(m);
            let end = self.next_bar_start(bs);
            for i in 0..m.num as u64 {
                let t = bs + i * beat;
                if t >= end || t >= to {
                    break;
                }
                if t >= from {
                    out.push((t, i == 0));
                }
            }
        }
        out
    }

    /// `bar.beat.tick` position text under this map — the metrical
    /// equivalent of `TimeDisplay::format_tick`, following the file's
    /// real `FF 58` signatures rather than a fixed 4/4.
    pub fn format_bbt(&self, tick: u64) -> String {
        let (bar, beat, t) = self.tick_to_bbt(tick);
        format!("{bar}.{beat}.{t:>3}")
    }
}

/// Owned `tick → position text` formatter for UI code that snapshots
/// state (a11y subtrees, canvas paint closures): metrical positions
/// follow the document's real `FF 58` map; SMPTE positions show timecode.
#[derive(Debug, Clone)]
pub enum PositionFormat {
    /// `bar.beat.tick` under a track's meter map
    Bbt(MeterMap),
    /// `hh:mm:ss.ff` timecode
    Smpte(TimeDisplay),
}

impl PositionFormat {
    pub fn fmt(&self, tick: u64) -> String {
        match self {
            Self::Bbt(m) => m.format_bbt(tick),
            Self::Smpte(td) => td.format_tick(tick),
        }
    }
}

/// `hh:mm:ss.ff` timecode for a frame count. fps 24/25/30 use straight
/// frame numbering; the SMF `-29` division is 29.97 fps, displayed with
/// drop-frame numbering (nominal 30: frame numbers 0 and 1 skipped at
/// the start of every minute that isn't a multiple of 10).
fn format_smpte(td: TimeDisplay, frames: u64) -> String {
    let TimeDisplay::Smpte { fps, .. } = td else {
        unreachable!("format_smpte on metrical")
    };
    let nominal = if fps == 29 { 30 } else { fps.max(1) as u64 };
    let f_total = if fps == 29 {
        drop_frame_index(frames)
    } else {
        frames
    };
    let secs = f_total / nominal;
    let ff = f_total % nominal;
    format!(
        "{:02}:{:02}:{:02}.{:02}",
        secs / 3600,
        (secs / 60) % 60,
        secs % 60,
        ff
    )
}

/// Real frame count -> nominal-30 frame index under drop-frame
/// numbering. Every 10-minute block drops 18 frame numbers (2 at the
/// top of each minute 1-9; minute 0 and every 10th minute keep all).
fn drop_frame_index(frames: u64) -> u64 {
    const PER_10MIN: u64 = 17_982; // 30*60*10 - 18 real frames
    const MIN0: u64 = 1_800; // first minute of a block: no drop
    const PER_MIN: u64 = 1_798; // minutes 1-9: 1800 - 2 real frames
    let tens = frames / PER_10MIN;
    let block = frames % PER_10MIN;
    let within = if block < MIN0 {
        block
    } else {
        // skip 2 numbers at each of the 9 non-tenth minute boundaries
        MIN0 + (block - MIN0) + 2 * (1 + (block - MIN0) / PER_MIN)
    };
    tens * 18_000 + within
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrical_formats_bar_beat_tick() {
        let td = TimeDisplay::of(Division::Metrical(480));
        assert_eq!(td.format_tick(0), "1.1.  0");
        // bar 13, beat 4, tick 120
        assert_eq!(td.format_tick(12 * 1920 + 3 * 480 + 120), "13.4.120");
    }

    /// FF58 meta at `tick` for map construction in tests.
    fn sig(track_events: &mut Vec<crate::Event>, id: u64, tick: u64, n: u8, d_pow: u8) {
        track_events.push(crate::Event {
            id,
            tick,
            seq: tick as u32,
            raw_body: None,
            kind: EventKind::Meta {
                meta_type: 0x58,
                data: vec![n, d_pow, 24, 8].into(),
            },
        });
    }

    #[test]
    fn format_bbt_follows_the_real_map_not_4_4() {
        // 4/4 for one bar, then 6/8 — bar 2 starts at 1920 with 6 beats
        // of 240 ticks (eighth-note beats), not 4 beats of 480.
        let mut evs = Vec::new();
        sig(&mut evs, 1, 1920, 6, 3);
        let mm = MeterMap::build(
            &[crate::Track {
                name: None,
                out_port: 0,
                out_channel: 0,
                events: evs,
            }],
            Division::Metrical(480),
        );
        assert_eq!(mm.format_bbt(0), "1.1.  0");
        assert_eq!(mm.format_bbt(1920), "2.1.  0");
        // sixth eighth-note beat of bar 2 — inside the bar, not "bar 2.5"
        assert_eq!(mm.format_bbt(1920 + 5 * 240), "2.6.  0");
        // 1440 ticks after the change = 6/8 bar 2 done exactly (6*240) →
        // wait, 6*240=1440: bar 3 starts at 1920+1440=3360
        assert_eq!(mm.format_bbt(3360), "3.1.  0");
    }

    #[test]
    fn beat_lines_mark_downbeats_and_honor_meter_changes() {
        let mut evs = Vec::new();
        sig(&mut evs, 1, 0, 3, 2); // 3/4 from the top: 3 beats of 480
        let mm = MeterMap::build(
            &[crate::Track {
                name: None,
                out_port: 0,
                out_channel: 0,
                events: evs,
            }],
            Division::Metrical(480),
        );
        let lines = mm.beat_lines_between(0, 3 * 1440);
        // two full 3/4 bars + the third's beats before `to`
        assert_eq!(
            lines,
            vec![
                (0, true),
                (480, false),
                (960, false),
                (1440, true),
                (1920, false),
                (2400, false),
                (2880, true),
                (3360, false),
                (3840, false),
            ]
        );
        // a 4/4 map would have put bar lines at 1920/3840 — these are not
        assert!(!lines.iter().any(|&(t, d)| t == 1920 && d));
    }

    /// #137 — a count-in is metered by the region PRECEDING the record
    /// point, walked backward bar by bar; never extrapolated from tick 0.
    #[test]
    fn countin_start_walks_bars_backward_from_the_record_point() {
        let mut evs = Vec::new();
        // 4/4 (1920-tick bars) for two bars, then 3/4 (1440-tick bars)
        sig(&mut evs, 1, 3840, 3, 2);
        let mm = MeterMap::build(
            &[crate::Track {
                name: None,
                out_port: 0,
                out_channel: 0,
                events: evs,
            }],
            Division::Metrical(480),
        );
        // record into the second 3/4 bar (starts at 3840+1440=5280):
        // one bar of count-in is the FIRST 3/4 bar [3840,5280), not a
        // 1920-tick 4/4 bar — the old tick-0 math got this wrong
        assert_eq!(mm.countin_start(5280, 1), 3840);
        // two bars back covers both 3/4 bars — meter across the boundary
        // is honored per bar, not averaged
        assert_eq!(mm.countin_start(5280 + 1440, 2), 3840);
        // recording on the 3/4 change itself counts in the last 4/4 bar
        assert_eq!(mm.countin_start(3840, 1), 1920);
        // a mid-bar record point adds its pickup remainder: the full bar
        // BEFORE the containing bar is the region start
        assert_eq!(mm.countin_start(3840 + 720, 1), 1920);
        // count-in can't run before tick 0 — it saturates at the top
        assert_eq!(mm.countin_start(960, 4), 0);
    }

    #[test]
    fn smpte_formats_timecode() {
        let td = TimeDisplay::of(Division::Smpte {
            fps: 30,
            ticks_per_frame: 100,
        });
        assert_eq!(td.format_tick(0), "00:00:00.00");
        // 3000 ticks = 1 second exactly
        assert_eq!(td.format_tick(3000), "00:00:01.00");
        // sub-frame remainder stays visible, lossless
        assert_eq!(td.format_tick(456_789), "00:02:32.07+89");
    }

    #[test]
    fn smpte_all_framerates() {
        for (fps, want_1s) in [
            (24u8, "00:00:01.00"),
            (25, "00:00:01.00"),
            (30, "00:00:01.00"),
        ] {
            let td = TimeDisplay::of(Division::Smpte {
                fps,
                ticks_per_frame: 100,
            });
            assert_eq!(td.format_tick(fps as u64 * 100), want_1s, "fps {fps}");
        }
        // -29 division: drop-frame numbering at nominal 30
        let td = TimeDisplay::of(Division::Smpte {
            fps: 29,
            ticks_per_frame: 100,
        });
        assert_eq!(td.format_tick(0), "00:00:00.00");
        assert_eq!(td.format_tick(179_900), "00:00:59.29");
        // a real second is only 29 frames — still 00:00:00.29 nominal;
        // the displayed second boundary is 30 frames
        assert_eq!(td.format_tick(29 * 100), "00:00:00.29");
        assert_eq!(td.format_tick(30 * 100), "00:00:01.00");
        // minute boundary skips frame numbers 00 and 01
        assert_eq!(td.format_tick(180_000), "00:01:00.02");
        // ten-minute boundary drops nothing
        assert_eq!(td.format_tick(17_982 * 100), "00:10:00.00");
    }

    #[test]
    fn grid_intervals_are_time_based_for_smpte() {
        let met = TimeDisplay::of(Division::Metrical(480));
        assert_eq!(met.grid_ticks(0.08), (480, 1920));
        // far zoom-out collapses the quarter grid into bar lines only
        assert_eq!(met.grid_ticks(0.005), (1920, 1920));
        let sm = TimeDisplay::of(Division::Smpte {
            fps: 30,
            ticks_per_frame: 100,
        });
        assert_eq!((sm.cell_ticks(), sm.bar_ticks()), (100, 3000));
        assert_eq!(sm.grid_ticks(0.08), (100, 3000));
        assert_eq!(sm.grid_ticks(0.005), (3000, 3000));
        // snap/small-step quanta are fractions of a second or a frame —
        // none of the fake-480 beat math
        assert_eq!(sm.snap_base_ticks(), 3000);
        assert_eq!(sm.min_grid_ticks(), 100);
        assert_eq!(sm.click_ticks(), 3000);
        assert_eq!(sm.nudge_ticks(), 100);
        // the -29 division's displayed second is the nominal 30 frames;
        // its real rate is 30000/1001 fps → 2997.003 ticks/second
        let df = TimeDisplay::of(Division::Smpte {
            fps: 29,
            ticks_per_frame: 100,
        });
        assert_eq!(df.ticks_per_second(), 2997); // floor of the exact rate
        assert_eq!(df.bar_ticks(), 3000); // timecode second
        assert_eq!(df.format_tick(df.bar_ticks()), "00:00:01.00");
    }

    #[test]
    fn badges_name_the_mode() {
        assert_eq!(TimeDisplay::of(Division::Metrical(480)).badge(), "480ppq");
        assert_eq!(
            TimeDisplay::of(Division::Smpte {
                fps: 25,
                ticks_per_frame: 40
            })
            .badge(),
            "25fps"
        );
        assert_eq!(
            TimeDisplay::of(Division::Smpte {
                fps: 29,
                ticks_per_frame: 100
            })
            .badge(),
            "29.97df"
        );
    }
}

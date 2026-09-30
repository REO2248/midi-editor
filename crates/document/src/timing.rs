//! How a document's timing division is presented to the user.
//!
//! Metrical (PPQ) files position events on a musical grid — bars, beats,
//! fractions of a quarter note. SMPTE files have no quarter note at all:
//! their ticks are wall-clock sub-frames (`fps * ticks_per_frame` ticks
//! per second). Pretending a PPQ (the UI once fell back to 480) invents
//! bar/beat positions that mean nothing for these files, so the timing
//! mode is explicit here: timecode positions and a seconds/frames grid
//! for SMPTE, a bar/beat grid for metrical.

use smf_core::Division;

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

    /// Ticks per real wall-clock second — matches `TempoMap::smpte_tps`
    /// (`fps * ticks_per_frame`). `0` for metrical.
    pub fn ticks_per_second(self) -> u64 {
        match self {
            Self::Smpte {
                fps,
                ticks_per_frame,
            } => fps.max(1) as u64 * ticks_per_frame.max(1) as u64,
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
        for (fps, want_1s) in [(24u8, "00:00:01.00"), (25, "00:00:01.00"), (30, "00:00:01.00")] {
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
        // the -29 division's displayed second is the nominal 30 frames
        let df = TimeDisplay::of(Division::Smpte {
            fps: 29,
            ticks_per_frame: 100,
        });
        assert_eq!(df.ticks_per_second(), 2900); // wall-clock rate
        assert_eq!(df.bar_ticks(), 3000); // timecode second
        assert_eq!(df.format_tick(df.bar_ticks()), "00:00:01.00");
    }

    #[test]
    fn badges_name_the_mode() {
        assert_eq!(
            TimeDisplay::of(Division::Metrical(480)).badge(),
            "480ppq"
        );
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

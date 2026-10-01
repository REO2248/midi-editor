//! MIDI recording: take buffers, record modes, punch-in/out range, and
//! commit/discard of the armed take. Recorded bytes are collected by the
//! midi-io input worker into `Rec`; finishing a take converts them to a
//! single Transaction through `apply_tx`.

use super::*;

impl EditorView {
    /// Arm/disarm live capture from the first MIDI input port onto the
    /// selected track. Arm also starts playback so timing is audible; a
    /// second press commits the take as one undoable transaction.
    pub(crate) fn toggle_record(&mut self) {
        if self.rec.is_some() {
            self.finish_record();
            return;
        }
        let buf = std::sync::Arc::new(Mutex::new(Vec::new()));
        let buf2 = buf.clone();
        // optional count-in: one real bar under the viewed track's FF58
        // map for metrical, one displayed second for SMPTE
        let cin_us = if self.count_in {
            self.doc(|d| {
                let ticks = match d.time_display() {
                    TimeDisplay::Metrical { .. } => d.meter_map_for(self.sel_track).bar_ticks_at(0),
                    TimeDisplay::Smpte { .. } => self.td().bar_ticks(),
                };
                d.tempo_map_for(self.sel_track).tick_to_us(ticks)
            })
        } else {
            0
        };
        let cb = move |us, b: &[u8]| {
            buf2.lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((us, b.to_vec()));
        };
        let diag = midi_io::InputDiag::new();
        let opts = midi_io::InputOpts {
            latency_us: self.in_latency_ms * 1000,
            diag: Some(diag.clone()),
        };
        let opened = if self.midi_in.is_empty() {
            midi_io::Input::open_opts(0, opts, cb)
        } else {
            midi_io::Input::open_named_opts(&self.midi_in, opts, cb)
        };
        match opened {
            Ok(input) => {
                self.rec = Some(Rec {
                    input,
                    input_lost: false,
                    buf,
                    base_us: self.play_us,
                    cin_us,
                    diag,
                    loop_span_us: None,
                });
                if self.playback.is_none() {
                    self.start_playback();
                }
                // armed inside a loop: the transport wraps at the last
                // scheduled event — snapshot the span so past-wrap input can
                // be mapped back into it (per-pass take)
                if lock_shared(&self.shared).loop_enabled {
                    let end = self
                        .doc(|d| d.timeline_tagged())
                        .iter()
                        .map(|e| e.0)
                        .max()
                        .unwrap_or(0);
                    if end > self.loop_start_us {
                        if let Some(r) = self.rec.as_mut() {
                            r.loop_span_us = Some((self.loop_start_us, end));
                        }
                    }
                }
                self.status = tf(
                    "status.rec_armed",
                    &[("n", &(self.sel_track + 1).to_string())],
                )
                .into();
            }
            Err(e) => self.status = format!("rec: {e}").into(),
        }
    }

    /// Normalized punch window in ticks, when both bounds are set and in order.
    pub(crate) fn punch_range(&self) -> Option<(u64, u64)> {
        match (self.punch_in, self.punch_out) {
            (Some(a), Some(b)) if a < b => Some((a, b)),
            _ => None,
        }
    }

    /// Commit the captured take into the selected track (raw channel events;
    /// the notes() view pairs on/off for display). One transaction keeps the
    /// take a single undoable unit with its raw, pre-quantized timing —
    /// quantization is offered afterwards as a separate "quantize take" tx.
    pub(crate) fn finish_record(&mut self) {
        let Some(rec) = self.rec.take() else {
            return;
        };
        let msgs = std::mem::take(&mut *rec.buf.lock().unwrap_or_else(|e| e.into_inner()));
        let punch = self.punch_range();
        let mode = self.rec_mode;
        let mut sh = lock_shared(&self.shared);
        if sh.doc.tracks.is_empty() {
            let ops = sh.doc.add_track_ops(None);
            if let Err(e) = sh.apply("add track", ops) {
                drop(sh);
                self.status = tf("status.apply_failed", &[("e", &e.to_string())]).into();
                return;
            }
        }
        let track = self.sel_track.min(sh.doc.tracks.len().saturating_sub(1));
        // (pass, channel, event) — pass is the loop lap the event landed on
        let mut captured: Vec<(u64, u8, document::Event)> = Vec::new();
        for (us, b) in msgs {
            // channel voice messages only; realtime/sysex are not captured
            if b.is_empty() || b[0] < 0x80 || b[0] >= 0xF0 {
                continue;
            }
            let len = match b[0] & 0xF0 {
                0xC0 | 0xD0 => 1,
                _ => 2,
            };
            if b.len() < 1 + len as usize {
                continue;
            }
            if us < rec.cin_us {
                continue;
            }
            let doc_us = rec.base_us + (us - rec.cin_us);
            // armed inside a loop: input that arrives on later passes wraps
            // back into the span instead of spilling past the loop end
            let (doc_us, pass) = match rec.loop_span_us {
                Some((s, e)) if e > s && doc_us >= e => {
                    (s + (doc_us - s) % (e - s), (doc_us - s) / (e - s))
                }
                Some((s, e)) if e > s => (doc_us, (doc_us - s) / (e - s)),
                _ => (doc_us, 0),
            };
            // the take lands in the selected track — for format 2 that
            // sequence's own tempo map converts live-us back to ticks
            let tick = sh.doc.tempo_map_for(track).us_to_tick(doc_us);
            if let Some((a, b)) = punch {
                if tick < a || tick >= b {
                    continue;
                }
            }
            captured.push((
                pass,
                b[0] & 0x0F,
                document::Event {
                    id: sh.doc.alloc_event_id(),
                    tick,
                    seq: u32::MAX / 2,
                    raw_body: None,
                    kind: EventKind::Channel {
                        status: b[0],
                        data: [b[1], b.get(2).copied().unwrap_or(0)],
                        len,
                    },
                },
            ));
        }
        // replace-in-loop: each channel keeps only its latest pass, so a
        // multi-lap take commits one coherent layer per channel
        let events: Vec<document::Event> = if mode == RecMode::Replace && rec.loop_span_us.is_some()
        {
            let mut last_pass: HashMap<u8, u64> = HashMap::new();
            for (p, ch, _) in &captured {
                last_pass
                    .entry(*ch)
                    .and_modify(|e| *e = (*e).max(*p))
                    .or_insert(*p);
            }
            captured
                .into_iter()
                .filter(|(p, ch, _)| *p == last_pass[ch])
                .map(|(_, _, e)| e)
                .collect()
        } else {
            captured.into_iter().map(|(_, _, e)| e).collect()
        };
        let n = events.len();
        // timing diagnostic: how much callback delivery delay the backend
        // timestamps absorbed — would have been recorded as timing error
        {
            use std::sync::atomic::Ordering::Relaxed;
            let (st, un, gap) = (
                rec.diag.stamped.load(Relaxed),
                rec.diag.unstamped.load(Relaxed),
                rec.diag.gap_max_us.load(Relaxed),
            );
            tracing::debug!(
                "rec input timing: {st} device-stamped, {un} arrival-fallback, worst callback delay {}ms",
                gap / 1000
            );
        }
        if n == 0 {
            // an empty take must never erase: cancelled or out-of-punch
            // recording leaves the document untouched
            drop(sh);
            self.status = t("status.rec_no_events").into();
            return;
        }
        let (from, to) = (
            events.iter().map(|e| e.tick).min().unwrap_or(0),
            events.iter().map(|e| e.tick).max().unwrap_or(0) + 1,
        );
        let mut ops = Vec::new();
        if mode == RecMode::Replace {
            let (df, dt) = punch.unwrap_or((from, to));
            let channels: std::collections::BTreeSet<u8> = events
                .iter()
                .filter_map(|e| match e.kind {
                    EventKind::Channel { status, .. } => Some(status & 0x0F),
                    _ => None,
                })
                .collect();
            ops.extend(sh.doc.delete_range_channel_ops(track, df, dt, &channels));
        }
        ops.push(Op::InsertEvents { track, events });
        drop(sh);
        self.last_take = Some((track, from, to));
        self.apply_tx("record", ops);
        self.status = tf("status.rec_done", &[("n", &n.to_string())]).into();
    }

    /// Drop the armed take without committing — the document is untouched.
    /// (Document replacement paths already warn; this is the explicit cancel.)
    pub(crate) fn discard_record(&mut self) {
        if self.rec.take().is_some() {
            self.status = t("status.rec_discarded").into();
        }
    }

    /// Post-record quantize as its own reversible transaction — the take
    /// committed raw timing stays intact underneath in the undo stack.
    pub(crate) fn quantize_last_take(&mut self) {
        let Some((track, from, to)) = self.last_take else {
            self.status = t("status.no_take").into();
            return;
        };
        let grid = self.snap_ticks().max(self.ppq() as i64 / 4) as u64;
        let ops = {
            let mut sh = lock_shared(&self.shared);
            if track >= sh.doc.tracks.len() {
                drop(sh);
                self.status = t("status.no_take").into();
                return;
            }
            sh.doc.quantize_ops(track, from, to, grid, 100)
        };
        if ops.is_empty() {
            self.status = t("status.no_take").into();
            return;
        }
        self.apply_tx("quantize take", ops);
        self.status = t("status.fixed").into();
    }
}

/// Captured (µs, raw channel bytes) pairs from the input callback.
pub(crate) type RecBuf = std::sync::Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

/// What `finish_record` does with the events already on the armed track.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecMode {
    /// Insert the take over existing content (default).
    Overdub,
    /// In the same transaction, delete the in-range channel events the take
    /// re-records — only the range and channels the take actually carries,
    /// then insert it.
    Replace,
}

impl RecMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Overdub => "overdub",
            Self::Replace => "replace",
        }
    }
    pub(crate) fn from_label(s: &str) -> Option<Self> {
        match s {
            "overdub" => Some(Self::Overdub),
            "replace" => Some(Self::Replace),
            _ => None,
        }
    }
}

/// Armed recording: timestamps channel messages against the playhead's µs base.
pub(crate) struct Rec {
    pub(crate) input: midi_io::Input,
    /// the input port vanished mid-take — the watcher reconnects the exact
    /// (name, ord) endpoint when it returns
    pub(crate) input_lost: bool,
    pub(crate) buf: RecBuf,
    /// document time (µs) corresponding to Input's t=0
    pub(crate) base_us: u64,
    /// count-in duration — input before this is discarded
    pub(crate) cin_us: u64,
    /// jitter counters — how much callback-delivery delay the backend
    /// timestamps absorbed this take (surfaced as a debug diagnostic)
    pub(crate) diag: std::sync::Arc<midi_io::InputDiag>,
    /// (loop_start, loop_end) snapshot when the take was armed while a loop
    /// ran — events past the wrap map back into the span (per-pass replace)
    pub(crate) loop_span_us: Option<(u64, u64)>,
}

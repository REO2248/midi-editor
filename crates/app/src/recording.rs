//! MIDI recording: take buffers, record modes, punch-in/out range, and
//! commit/discard of the armed take. Recorded bytes are collected by the
//! midi-io input worker into `Rec`; finishing a take converts them to a
//! single Transaction through `apply_tx`.

use super::*;

impl EditorView {
    /// Count-in geometry for a record pass starting at `at_us` (#137):
    /// `(region start tick, record start tick, lead-in µs)`. The bars are
    /// walked BACKWARD from the record position's own bar — the lead-in
    /// is metered by the signatures and tempi actually covering the
    /// pre-region, so recording into a 3/4 section after a 4/4 opening
    /// counts 3/4 bars and a tempo ramp inside the count-in is heard in
    /// the clicks. SMPTE has no bar grid: a "bar" is the displayed second.
    fn countin_region(&self, at_us: u64) -> (u64, u64, u64) {
        if self.count_in_bars == 0 {
            return (0, 0, 0);
        }
        self.doc(|d| {
            let tm = d.tempo_map_for(self.sel_track);
            let start_tick = tm.us_to_tick(at_us);
            let t0 = match d.time_display() {
                TimeDisplay::Metrical { .. } => d
                    .meter_map_for(self.sel_track)
                    .countin_start(start_tick, self.count_in_bars as u64),
                TimeDisplay::Smpte { .. } => {
                    start_tick.saturating_sub(self.td().bar_ticks() * self.count_in_bars as u64)
                }
            };
            (t0, start_tick, at_us.saturating_sub(tm.tick_to_us(t0)))
        })
    }

    /// Open the configured MIDI input and build the armed `Rec` state.
    /// Arming never starts the transport (#159) — it only listens, and
    /// echoes input through `update_monitor` when the monitor mode allows.
    fn open_armed_input(&mut self) -> bool {
        let Some(track) = self.armed_track else {
            return false;
        };
        let buf: RecBuf = std::sync::Arc::new(Mutex::new(Vec::new()));
        let recording = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mon: std::sync::Arc<Mutex<Option<Box<dyn midi_io::EventSink>>>> =
            std::sync::Arc::new(Mutex::new(None));
        let dropped_rt = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let dropped_sx = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let in_ch = self.rec_in_ch;
        // SysEx policies ride atomics so toggling them while armed takes
        // effect live — the input callback never re-opens (#160)
        let sx_gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(self.rec_sysex));
        let mon_sx_gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            self.rec_mon_sysex,
        ));
        let (buf2, rec_flag, mon2, rt2, sx2, sx_gate2, mon_sx2) = (
            buf.clone(),
            recording.clone(),
            mon.clone(),
            dropped_rt.clone(),
            dropped_sx.clone(),
            sx_gate.clone(),
            mon_sx_gate.clone(),
        );
        let cb = move |us, b: &[u8]| {
            use std::sync::atomic::Ordering::Relaxed;
            if b.is_empty() {
                return;
            }
            let g = rec_gate(b[0], b.len(), in_ch);
            match g {
                // realtime (clock/start/stop/active-sensing) is transport
                // signalling, never document content or thru traffic (#160)
                RecGate::Realtime => {
                    rt2.fetch_add(1, Relaxed);
                    return;
                }
                RecGate::Filtered => return,
                // oversized SysEx is dropped at the gate, never buffered
                // into the take nor echoed thru (#160 memory bound)
                RecGate::Oversized => {
                    sx2.fetch_add(1, Relaxed);
                    return;
                }
                _ => {}
            }
            // SysEx capture has its own visible toggle — off means a bulk
            // dump can't land in the take even while recording (#160)
            if rec_flag.load(Relaxed) && rec_captures(g, sx_gate2.load(Relaxed)) {
                buf2.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push((us, b.to_vec()));
            }
            if let Some(sink) = mon2.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                // thru policy: SysEx echoes only when explicitly enabled —
                // the bulk-dump echo defaults off (#160)
                if mon_echoes(g, mon_sx2.load(Relaxed)) {
                    sink.send_at(b, 0);
                }
            }
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
                    recording,
                    base_us: self.play_us,
                    ref_us: 0,
                    cin_us: 0,
                    cin_region: None,
                    diag,
                    loop_span_us: None,
                    arm_track: track,
                    mon,
                    dropped_rt,
                    dropped_sx,
                    sx_gate,
                    mon_sx_gate,
                });
                self.update_monitor();
                true
            }
            Err(e) => {
                self.status = format!("rec: {e}").into();
                self.armed_track = None;
                false
            }
        }
    }

    /// Track Record Arm on/off for the selected track (#159). Arming only
    /// opens the input (+ monitor echo when enabled) — it never starts
    /// playback or recording. Disarming drops any take in progress.
    pub(crate) fn toggle_arm(&mut self) {
        if self.armed_track == Some(self.sel_track) {
            self.armed_track = None;
            if self.rec.take().is_some() {
                self.status = t("status.rec_disarmed").into();
            }
            return;
        }
        if self.rec.take().is_some() {
            // re-arming a different track discards the old track's take
            self.status = t("status.rec_take_lost").into();
        }
        self.armed_track = Some(self.sel_track);
        self.open_armed_input();
    }

    /// Transport Record (#159): engages capture on the armed track and
    /// starts playback if stopped; an unarmed selection arms first (DAW
    /// convention). Pressing while recording commits the take (punch-out —
    /// playback keeps running); transport Stop commits and halts.
    pub(crate) fn transport_record(&mut self, _cx: &mut Context<Self>) {
        if self
            .rec
            .as_ref()
            .is_some_and(|r| r.recording.load(std::sync::atomic::Ordering::Relaxed))
        {
            self.finish_record();
            return;
        }
        if self.armed_track.is_none() || self.rec.is_none() {
            self.armed_track = Some(self.sel_track);
            if !self.open_armed_input() {
                return;
            }
        }
        // a count-in only applies when this engage parks a fresh
        // transport start — a mid-pass punch can't pause a running
        // schedule, so no lead-in is banked for it (#137)
        let (cin_us, cin_region) = if self.playback.is_none() {
            let (t0, t1, cin) = self.countin_region(self.play_us);
            (cin, (cin > 0).then_some((t0, t1)))
        } else {
            (0, None)
        };
        if let Some(r) = self.rec.as_mut() {
            r.recording
                .store(true, std::sync::atomic::Ordering::Relaxed);
            r.buf.lock().unwrap_or_else(|e| e.into_inner()).clear();
            // the take's zero is "record engaged", not "input opened" —
            // monitoring may have run for minutes first (#159)
            r.ref_us = r.input.now_us();
            r.base_us = self.play_us;
            r.cin_us = cin_us;
            r.cin_region = cin_region;
        }
        if self.playback.is_none() {
            self.start_playback();
        }
        // armed inside a loop: the transport wraps at the right locator
        // back to the left one — snapshot the span so past-wrap input maps
        // back into it (per-pass take). Explicit locators (#130) define the
        // span; the legacy play-start→end wrap falls back to schedule end.
        if lock_shared(&self.shared).loop_enabled {
            let ctx = self.live_ctx();
            let (ls, le) = self.loop_range_us(&ctx);
            let end = le.or_else(|| self.doc(|d| d.timeline_tagged()).iter().map(|e| e.0).max());
            if let (Some(a), Some(b)) = (ls, end) {
                if b > a {
                    if let Some(r) = self.rec.as_mut() {
                        r.loop_span_us = Some((a, b));
                    }
                }
            }
        }
        self.update_monitor();
        self.status = tf(
            "status.rec_armed",
            &[("n", &(self.armed_track.unwrap_or(0) + 1).to_string())],
        )
        .into();
    }

    /// True while the transport is capturing input onto the armed track.
    pub(crate) fn is_recording(&self) -> bool {
        self.rec
            .as_ref()
            .is_some_and(|r| r.recording.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Re-evaluate the monitor echo sink (#159): In echoes always, Auto only
    /// while the transport is stopped, Off never. The armed track's routed
    /// destination is opened (MIDI port directly; a hosted plugin reuses its
    /// slot sink once ready).
    pub(crate) fn update_monitor(&mut self) {
        let playing = self.playback.as_ref().is_some_and(|p| p.is_running());
        let want = match self.monitor {
            MonMode::In => true,
            MonMode::Auto => !playing,
            MonMode::Off => false,
        };
        let Some(r) = self.rec.as_mut() else {
            return;
        };
        {
            let mut g = r.mon.lock().unwrap_or_else(|e| e.into_inner());
            if !want {
                *g = None;
                return;
            }
            if g.is_some() {
                return;
            }
        }
        let dest = lock_shared(&self.shared).dest_of(r.arm_track);
        let sink = self.open_monitor_sink(dest);
        if let Some(g) = self.rec.as_ref() {
            *g.mon.lock().unwrap_or_else(|e| e.into_inner()) = sink;
        }
    }

    fn open_monitor_sink(&mut self, dest: usize) -> Option<Box<dyn midi_io::EventSink>> {
        let d = lock_shared(&self.shared)
            .dests
            .get(dest)
            .map(|(_, d)| d.clone())?;
        match d {
            output::Destination::MidiPort { port_name, ord } => {
                midi_io::Output::open_ord(&port_name, ord)
                    .ok()
                    .map(|o| Box::new(midi_io::PortSink::new(o)) as Box<dyn midi_io::EventSink>)
            }
            output::Destination::Plugin { .. } => self
                .plugin_slots
                .get(&dest)
                .map(|s| Box::new(s.sink.clone()) as Box<dyn midi_io::EventSink>),
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
        let track = rec.arm_track.min(sh.doc.tracks.len().saturating_sub(1));
        // (pass, channel, event) — pass is the loop lap the event landed
        // on; channel is Some for channel voice, None for SysEx/escape
        let mut captured: Vec<(u64, Option<u8>, document::Event)> = Vec::new();
        for (us, b) in msgs {
            if b.is_empty() {
                continue;
            }
            // the take's zero is record-engage, not input-open (#159)
            let rel = us.saturating_sub(rec.ref_us);
            if rel < rec.cin_us {
                continue;
            }
            let doc_us = rec.base_us + (rel - rec.cin_us);
            // armed inside a loop: input that arrives on later passes wraps
            // back into the span instead of spilling past the loop end
            let (doc_us, pass) = match rec.loop_span_us {
                Some((s, e)) if e > s && doc_us >= e => {
                    (s + (doc_us - s) % (e - s), (doc_us - s) / (e - s))
                }
                Some((s, e)) if e > s => (doc_us, (doc_us - s) / (e - s)),
                _ => (doc_us, 0),
            };
            // the take lands on the armed track — for format 2 that
            // sequence's own tempo map converts live-us back to ticks
            let tick = sh.doc.tempo_map_for(track).us_to_tick(doc_us);
            if let Some((a, b)) = punch {
                if tick < a || tick >= b {
                    continue;
                }
            }
            let Some((ch, kind)) = wire_to_kind(&b) else {
                continue;
            };
            captured.push((
                pass,
                ch,
                document::Event {
                    id: sh.doc.alloc_event_id(),
                    tick,
                    seq: u32::MAX / 2,
                    raw_body: None,
                    kind,
                },
            ));
        }
        // dropped-input tallies ride the completion status (#160)
        let (rt, sx) = (
            rec.dropped_rt.load(std::sync::atomic::Ordering::Relaxed),
            rec.dropped_sx.load(std::sync::atomic::Ordering::Relaxed),
        );
        if rt + sx > 0 {
            tracing::debug!("rec input dropped: {rt} realtime, {sx} oversized sysex");
        }
        // replace-in-loop: each channel keeps only its latest pass, so a
        // multi-lap take commits one coherent layer per channel
        let events: Vec<document::Event> = if mode == RecMode::Replace && rec.loop_span_us.is_some()
        {
            let mut last_pass: HashMap<u8, u64> = HashMap::new();
            let mut max_pass = 0u64;
            for (p, ch, _) in &captured {
                max_pass = max_pass.max(*p);
                if let Some(ch) = ch {
                    last_pass
                        .entry(*ch)
                        .and_modify(|e| *e = (*e).max(*p))
                        .or_insert(*p);
                }
            }
            captured
                .into_iter()
                // channel events keep their latest pass per channel;
                // SysEx/escape (channel-less) keep the latest pass overall
                .filter(|(p, ch, _)| match ch {
                    Some(ch) => *p == last_pass[ch],
                    None => *p == max_pass,
                })
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
            self.ream_armed_input();
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
        self.ream_armed_input();
    }

    /// After a take commits the track stays armed (#159: arming is a state,
    /// not a one-shot) — reopen the input for the next take.
    fn ream_armed_input(&mut self) {
        if self.armed_track.is_some() && self.rec.is_none() {
            self.open_armed_input();
        }
    }

    /// Drop the armed take and disarm — the document is untouched.
    /// (Document replacement paths already warn; this is the explicit cancel.)
    pub(crate) fn discard_record(&mut self) {
        if self.rec.take().is_some() {
            self.armed_track = None;
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

/// Armed recording: listens on the configured input; `recording` gates
/// whether messages land in the take buffer or only pass through the
/// monitor (#159). Timestamps rebase at record-engage via `ref_us`.
pub(crate) struct Rec {
    pub(crate) input: midi_io::Input,
    /// the input port vanished mid-take — the watcher reconnects the exact
    /// (name, ord) endpoint when it returns
    pub(crate) input_lost: bool,
    pub(crate) buf: RecBuf,
    /// transport-record gate — capture only while set
    pub(crate) recording: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// document time (µs) corresponding to `ref_us` on the input clock
    pub(crate) base_us: u64,
    /// input-clock µs at the moment record engaged — monitoring time
    /// before it never shifts the take
    pub(crate) ref_us: u64,
    /// count-in duration — input before this is discarded
    pub(crate) cin_us: u64,
    /// count-in pre-region in ticks `(region start, record start)` — the
    /// parked pass emits its clicks inside this span shifted by `cin_us`
    /// so the boundary click lands exactly on capture start (#137)
    pub(crate) cin_region: Option<(u64, u64)>,
    /// jitter counters — how much callback-delivery delay the backend
    /// timestamps absorbed this take (surfaced as a debug diagnostic)
    pub(crate) diag: std::sync::Arc<midi_io::InputDiag>,
    /// (loop_start, loop_end) snapshot when the take was armed while a loop
    /// ran — events past the wrap map back into the span (per-pass replace)
    pub(crate) loop_span_us: Option<(u64, u64)>,
    /// the armed track — the take's target and the monitor echo route
    pub(crate) arm_track: usize,
    /// live monitor sink (Some = input echoes to the armed dest right now)
    pub(crate) mon: std::sync::Arc<Mutex<Option<Box<dyn midi_io::EventSink>>>>,
    /// realtime messages discarded at the gate (counted for diagnostics)
    pub(crate) dropped_rt: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// oversized SysEx discarded at the gate
    pub(crate) dropped_sx: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// live SysEx-capture toggle mirrored into the input callback (#160)
    pub(crate) sx_gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// live SysEx-echo toggle mirrored into the input callback (#160)
    pub(crate) mon_sx_gate: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Maximum captured SysEx payload per message (#160) — a runaway firmware
/// dump can't exhaust the take buffer. Applies to F0 AND F7 (escape /
/// continuation) chunks alike; larger input is dropped + counted.
pub(crate) const MAX_REC_SYSEX: usize = 1 << 20;

/// What the record input gate does with one raw message (#160).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum RecGate {
    /// realtime (0xF8+) — transport signalling, dropped and counted,
    /// never buffered or echoed
    Realtime,
    /// channel voice outside the armed track's input filter
    Filtered,
    /// F0/F7 over `MAX_REC_SYSEX` — dropped and counted, never echoed
    Oversized,
    /// F0/F7 within bounds — capture/echo follow their own toggles
    SysEx,
    /// channel voice — buffered while recording, echoed while monitoring
    Voice,
}

/// The record gate's classification of one input message: realtime,
/// channel-filtered, oversized SysEx, SysEx, or voice. `in_ch` filters
/// channel voice only — SysEx is channel-less and always passes (#159).
pub(crate) fn rec_gate(st: u8, len: usize, in_ch: Option<u8>) -> RecGate {
    if st >= 0xF8 {
        return RecGate::Realtime;
    }
    if st < 0xF0 && in_ch.is_some_and(|c| st & 0x0F != c) {
        return RecGate::Filtered;
    }
    if st == 0xF0 || st == 0xF7 {
        if len > MAX_REC_SYSEX {
            return RecGate::Oversized;
        }
        return RecGate::SysEx;
    }
    RecGate::Voice
}

/// Whether a gated message lands in the take while recording (#160):
/// voice always; SysEx only when the capture toggle is on.
pub(crate) fn rec_captures(g: RecGate, rec_sx: bool) -> bool {
    g == RecGate::Voice || (g == RecGate::SysEx && rec_sx)
}

/// Whether a gated message echoes to the monitor sink (#160): voice
/// always; SysEx only when the echo toggle is on (off by default — a
/// bulk dump must not blast the armed destination).
pub(crate) fn mon_echoes(g: RecGate, mon_sx: bool) -> bool {
    g == RecGate::Voice || (g == RecGate::SysEx && mon_sx)
}

/// One raw input message → `(channel, kind)` for the take: wire F0 keeps
/// its whole payload incl. the trailing F7 byte (SMF F0 + VLQ(len)), a
/// wire F7 becomes an SMF F7 escape — a SysEx delivered in split chunks
/// lands verbatim as F0 + F7 continuation events (#160). Channel voice
/// carries its channel so replace-in-loop can key on it.
pub(crate) fn wire_to_kind(b: &[u8]) -> Option<(Option<u8>, EventKind)> {
    if b.is_empty() {
        return None;
    }
    match b[0] {
        0xF0 if b.len() > 1 => Some((
            None,
            EventKind::SysEx(bytes::Bytes::copy_from_slice(&b[1..])),
        )),
        0xF7 if b.len() > 1 => Some((
            None,
            EventKind::Escape(bytes::Bytes::copy_from_slice(&b[1..])),
        )),
        0x80..=0xEF => {
            let len = match b[0] & 0xF0 {
                0xC0 | 0xD0 => 1,
                _ => 2,
            };
            if b.len() < 1 + len as usize {
                return None;
            }
            Some((
                Some(b[0] & 0x0F),
                EventKind::Channel {
                    status: b[0],
                    data: [b[1], b.get(2).copied().unwrap_or(0)],
                    len,
                },
            ))
        }
        _ => None,
    }
}

/// Input monitor mode (#159) — whether armed input echoes to its routed
/// destination.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MonMode {
    /// never echoes input
    Off,
    /// echoes only while the transport is stopped (default)
    Auto,
    /// always echoes, playing or recording
    In,
}

impl MonMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Auto => "auto",
            Self::In => "in",
        }
    }
    pub(crate) fn from_label(s: &str) -> Option<Self> {
        match s {
            "off" => Some(Self::Off),
            "auto" => Some(Self::Auto),
            "in" => Some(Self::In),
            _ => None,
        }
    }
}

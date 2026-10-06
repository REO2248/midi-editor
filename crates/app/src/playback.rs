//! Playback: play/stop/seek, playhead following, event routing to sinks,
//! and note audition. Live playback happens on output/midi-io worker
//! threads — this module only prepares routed event lists and toggles the
//! playback handle on `EditorView`.

use super::*;

pub(crate) fn track_audible(tr: usize, muted: &HashSet<usize>, soloed: &HashSet<usize>) -> bool {
    if !soloed.is_empty() {
        soloed.contains(&tr)
    } else {
        !muted.contains(&tr)
    }
}

/// Clip threshold (#203): linear peaks above this have left sample space
/// (1.0 = 0 dBFS); the extra hair guards against float noise at the rail.
pub(crate) const CLIP_LEVEL: f32 = 0.99;
/// Meter display floor in dB below full scale (#203). Peaks below it (and
/// zero) map to an empty bar — the meter element is only drawn when the
/// fraction is non-zero or the clip LED is latched, so golden screenshots
/// (rendered without playback) never show it.
const METER_FLOOR_DB: f32 = 48.0;
/// Headroom above 0 dBFS the bar's top edge stands for, so a clip stays
/// drawable instead of pegging at 100%.
const METER_HEADROOM_DB: f32 = 6.0;

/// Linear peak → 0..1 bar fraction on the meter's dB scale (#203):
/// -48 dBFS at the left edge, +6 dBFS at the right, so the 0 dBFS point
/// sits ~89% out and a clipped peak still grows the red segment. Silence
/// maps to 0.
pub(crate) fn meter_frac(peak: f32) -> f32 {
    if peak <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * peak.log10();
    ((db + METER_FLOOR_DB) / (METER_FLOOR_DB + METER_HEADROOM_DB)).clamp(0.0, 1.0)
}

/// Filter a per-track timeline to audible tracks and remap each event's
/// track onto its sink index; events whose destination has no open sink
/// (failed port, unavailable plugin) are dropped. Tracks with an explicit
/// `FF 20` channel assignment are re-channelized: voice-message status
/// bytes carry the track's channel, not the channel they were recorded
/// on (#221).
pub(crate) fn route_events(
    timeline: Vec<(u64, usize, Vec<u8>)>,
    audible: impl Fn(usize) -> bool,
    dest_of: impl Fn(usize) -> usize,
    chan_of: impl Fn(usize) -> Option<u8>,
    sink_of: &HashMap<usize, usize>,
) -> Vec<(u64, usize, Vec<u8>)> {
    timeline
        .into_iter()
        .filter(|(_, tr, _)| audible(*tr))
        .filter_map(|(us, tr, mut b)| {
            let sink = *sink_of.get(&dest_of(tr))?;
            if let Some(ch) = chan_of(tr) {
                if let Some(st) = b.first_mut() {
                    if (0x80..=0xEF).contains(st) {
                        *st = (*st & 0xF0) | (ch & 0x0F);
                    }
                }
            }
            Some((us, sink, b))
        })
        .collect()
}

/// Merge routed events into the playback schedule. `events` is the
/// concatenation of SysEx, channel, and metronome-click lists; the stable
/// µs sort keeps that order at equal times, so SysEx lands before channel
/// messages and both before clicks (setup traffic — GM/XG resets, patch
/// dumps — must arrive before notes struck at the same instant). Channel
/// chase splices after the last event before `start_us` so real events at
/// the exact play position override it; chased SysEx splices ahead of it so
/// a chased reset cannot wipe the program/CC state the channel chase
/// restored.
pub(crate) fn assemble_events(
    mut events: Vec<(u64, usize, Vec<u8>)>,
    chase: Vec<(u64, usize, Vec<u8>)>,
    chase_sysex: Vec<(u64, usize, Vec<u8>)>,
    start_us: u64,
) -> Vec<(u64, usize, Vec<u8>)> {
    events.sort_by_key(|e| e.0);
    let at = events.partition_point(|e| e.0 < start_us);
    events.splice(at..at, chase);
    events.splice(at..at, chase_sysex);
    events
}

/// Park a doc-domain schedule behind a count-in hold of `cin` µs
/// (#137): every event at or after `start_us` slides `cin` later so the
/// song begins emitting exactly when capture time begins — the hold
/// window is filled by `countin_clicks_of` output, appended afterward in
/// the already-parked domain. Both chase lists describe state AT the
/// pass start, so they retime to the deferred boundary rather than the
/// pre-start tail the worker skips. Returns the boundary µs to hand
/// `assemble_events` as its partition point (and to `position_us`-domain
/// callers as the start anchor).
pub(crate) fn park_schedule(
    events: &mut [(u64, usize, Vec<u8>)],
    chase: &mut [(u64, usize, Vec<u8>)],
    chase_sx: &mut [(u64, usize, Vec<u8>)],
    start_us: u64,
    cin: u64,
) -> u64 {
    let boundary = start_us + cin;
    if cin == 0 {
        return boundary;
    }
    for e in events.iter_mut() {
        if e.0 >= start_us {
            e.0 += cin;
        }
    }
    for e in chase.iter_mut().chain(chase_sx.iter_mut()) {
        e.0 = boundary;
    }
    boundary
}

/// Count-in clicks for the pre-region `(t0, t1)` ticks, emitted in the
/// parked domain — each click's doc µs plus `cin`, so the first lands at
/// the hold's start and the record point's own (accented) click lands
/// exactly on the deferred boundary (#137). The cadence steps in TICK
/// space — through the same meter map and `cc` clocks the running
/// metronome uses — clamped onto `t1` so a partial trailing beat can't
/// overshoot the boundary. Free so schedule tests drive it without a view.
#[allow(clippy::too_many_arguments)]
pub(crate) fn countin_clicks_of(
    mm: &document::MeterMap,
    tm: &document::TempoMap,
    smpte: bool,
    smpte_click_ticks: u64,
    region: (u64, u64),
    cin: u64,
    sink: usize,
) -> Vec<(u64, usize, Vec<u8>)> {
    let (t0, t1) = region;
    let mut out = Vec::new();
    let mut t = t0;
    loop {
        let accent = if smpte {
            true
        } else {
            t == t1 || mm.bar_start_tick(t) == t
        };
        let note = if accent { 76 } else { 77 };
        let us = tm.tick_to_us(t) + cin;
        out.push((us, sink, vec![0x99, note, 110]));
        out.push((us + 20_000, sink, vec![0x99, note, 0]));
        if t >= t1 {
            break;
        }
        t = t
            .saturating_add(if smpte {
                smpte_click_ticks
            } else {
                mm.click_ticks_at(t)
            })
            .min(t1);
    }
    out
}

/// `EditorView::transport_points` on a bare `Document` — free so tests can
/// drive it without a view. Tempo map + every `0x58` meter meta as `(µs,
/// TransportCmd)`, sorted by µs.
#[cfg(test)]
pub(crate) fn transport_points_of(d: &Document) -> Vec<(u64, output::TransportCmd)> {
    transport_points_for(d, None)
}

/// `tr` selects the sequence in format-2 documents (each sequence has its
/// own tempo map and meta events); `None` uses the document-level maps.
pub(crate) fn transport_points_for(
    d: &Document,
    tr: Option<usize>,
) -> Vec<(u64, output::TransportCmd)> {
    let owned;
    let (tm, tracks): (&document::TempoMap, &[document::Track]) = match tr {
        Some(t) => {
            owned = d.tempo_map_for(t);
            (&owned, std::slice::from_ref(&d.tracks[t]))
        }
        None => (&d.tempo_map, &d.tracks),
    };
    let mut pts: Vec<(u64, output::TransportCmd)> = tm
        .points()
        .iter()
        .map(|(_, mpq, us)| {
            (
                *us,
                output::TransportCmd::Tempo(60_000_000.0 / (*mpq).max(1) as f64),
            )
        })
        .collect();
    let mut sig_pts = Vec::new();
    for tr in tracks {
        for e in &tr.events {
            if let EventKind::Meta {
                meta_type: 0x58,
                data,
            } = &e.kind
            {
                if data.len() >= 2 {
                    sig_pts.push((
                        tm.tick_to_us(e.tick),
                        output::TransportCmd::TimeSig(i32::from(data[0]), 1i32 << (data[1] & 0x1f)),
                    ));
                }
            }
        }
    }
    // SMF defaults live outside the event stream: a map with no tempo/meta
    // at tick 0 still plays 120bpm in 4/4 — state the plugin must be told
    // explicitly since a chase before the first point yields nothing.
    if !matches!(pts.first(), Some((us, _)) if *us == 0) {
        pts.push((0, output::TransportCmd::Tempo(120.0)));
    }
    if !sig_pts.iter().any(|(us, _)| *us == 0) {
        sig_pts.push((0, output::TransportCmd::TimeSig(4, 4)));
    }
    pts.extend(sig_pts);
    pts.sort_by_key(|(us, _)| *us);
    pts
}

/// Snapshot of everything a schedule build reads from shared state —
/// taken fresh on each build so mute/solo/routing/toggles apply live.
pub(crate) struct LiveCtx {
    dests: Vec<(String, output::Destination)>,
    dest_of_track: HashMap<usize, usize>,
    muted: HashSet<usize>,
    soloed: HashSet<usize>,
    metronome: bool,
    /// explicit click destination index into `dests`; `None` follows the
    /// document default destination (#137)
    met_dest: Option<usize>,
    /// document default destination index — metronome fallback target
    default_dest: usize,
    /// count-in hold µs for this pass — the song is parked that long
    /// while the pre-region's clicks sound (#137); 0 = no count-in
    countin_us: u64,
    /// count-in pre-region `(region start, record start)` in ticks — the
    /// click cadence walks this span in the meter map's own domain and
    /// lands shifted by `countin_us` inside the hold (#137)
    countin_region: Option<(u64, u64)>,
    loop_enabled: bool,
    /// explicit loop locators in ticks (#130) — both  = unset
    loop_start: Option<u64>,
    loop_end: Option<u64>,
    chase_sysex: bool,
    sequential: bool,
    sxp: midi_io::SysexPolicy,
}

impl EditorView {
    /// Keep the playhead on screen while playing, per `follow` mode.
    /// Never fires while a drag is live or `follow_hold` is active.
    pub(crate) fn follow_playhead(&mut self, tick: u64) {
        let w = f32::from(self.roll_bounds.get().size.width);
        if w <= 0.0 {
            return;
        }
        let x = tick as f32 * self.zoom;
        match self.follow {
            Follow::Off => {}
            Follow::Page => {
                let m = 48.0;
                if x < self.scroll_x + m || x > self.scroll_x + w - m {
                    self.scroll_x = (x - w * 0.15).max(0.0);
                }
            }
            Follow::Smooth => {
                self.scroll_x = (x - w / 3.0).max(0.0);
            }
        }
        self.clamp_scroll();
    }

    /// Scroll so the playhead sits at viewport center.
    pub(crate) fn center_playhead(&mut self) {
        let w = f32::from(self.roll_bounds.get().size.width);
        if w <= 0.0 {
            return;
        }
        let tick = self.doc(|d| d.tempo_map.us_to_tick(self.play_us));
        self.scroll_x = (tick as f32 * self.zoom - w / 2.0).max(0.0);
        self.clamp_scroll();
    }

    /// Go to playhead — also the explicit "resume follow" gesture.
    pub(crate) fn go_playhead(&mut self, cx: &mut Context<Self>) {
        self.follow_hold = None;
        self.center_playhead();
        cx.notify();
    }

    /// Move the playhead `bars` measures (used by the ruler/minimap
    /// accessibility Increment/Decrement actions). Metrical docs walk the
    /// real FF58 bar lines of the viewed track's meter map; SMPTE docs
    /// step displayed seconds.
    pub(crate) fn seek_bars(&mut self, bars: i64, cx: &mut Context<Self>) {
        let cur = self.doc(|d| d.tempo_map_for(self.sel_track).us_to_tick(self.play_us));
        let tick = match self.td() {
            TimeDisplay::Metrical { .. } => self.doc(|d| {
                let mm = d.meter_map_for(self.sel_track);
                if bars > 0 {
                    (0..bars).fold(cur, |t, _| mm.next_bar_start(t))
                } else if bars < 0 {
                    (0..-bars).fold(cur, |t, _| mm.prev_bar_start(t))
                } else {
                    cur
                }
            }),
            TimeDisplay::Smpte { .. } => {
                let step = self.td().bar_ticks() as i64;
                (cur as i64 + bars * step).max(0) as u64
            }
        };
        let tick = tick.min(self.doc_end_ticks());
        self.play_us = self.doc(|d| d.tempo_map_for(self.sel_track).tick_to_us(tick));
        cx.notify();
    }

    /// Move the playhead to `tick`; `play` (or an already-playing transport)
    /// restarts the engine from there. The engine stop runs first —
    /// `stop_playback` records the true stop position into `play_us`, so the
    /// seek target must be written after it.
    pub(crate) fn seek_to_tick(&mut self, tick: u64, play: bool, cx: &mut Context<Self>) {
        let us = self.doc(|d| d.tempo_map_for(self.sel_track).tick_to_us(tick));
        let was = play || self.playback.is_some();
        if was {
            self.stop_playback();
        }
        self.play_us = us;
        self.play_start_us = us;
        if was {
            self.start_playback();
        }
        cx.notify();
    }

    /// The one transport-level Stop: engine stop, then the return-on-stop
    /// policy. Space, toolbar, menu, palette and MCP all land here — the
    /// same command always produces the same cursor result (#156). A Stop
    /// while already stopped returns to the pass start (Ableton's
    /// double-stop).
    pub(crate) fn transport_stop(&mut self, cx: &mut Context<Self>) {
        self.stop_playback();
        if self.return_to_start_on_stop {
            self.play_us = self.play_start_us;
        }
        cx.notify();
    }

    /// Pause/Continue: while playing, stop in place — the play point stays
    /// where the pass halted so resuming continues from there; while
    /// stopped, resume from the play point (#156).
    pub(crate) fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if self.playback.is_some() {
            self.stop_playback();
        } else {
            self.start_playback();
        }
        cx.notify();
    }

    /// Return to the point where the current transport pass began.
    pub(crate) fn return_to_start(&mut self, cx: &mut Context<Self>) {
        let was = self.playback.is_some();
        if was {
            self.stop_playback();
        }
        self.play_us = self.play_start_us;
        if was {
            self.start_playback();
        }
        cx.notify();
    }

    /// Go to song start (tick 0); restarts the pass there when playing.
    pub(crate) fn go_to_start(&mut self, cx: &mut Context<Self>) {
        let was = self.playback.is_some();
        if was {
            self.stop_playback();
        }
        self.play_us = 0;
        self.play_start_us = 0;
        if was {
            self.start_playback();
        }
        cx.notify();
    }

    pub(crate) fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.playback.is_some() {
            self.transport_stop(cx);
        } else {
            self.start_playback();
            cx.notify();
        }
    }

    pub(crate) fn live_ctx(&self) -> LiveCtx {
        let sh = lock_shared(&self.shared);
        let map: HashMap<usize, usize> = (0..sh.doc.tracks.len())
            .map(|t| (t, sh.dest_of(t)))
            .collect();
        LiveCtx {
            dests: sh.dests.clone(),
            dest_of_track: map,
            muted: sh.muted.clone(),
            soloed: sh.soloed.clone(),
            metronome: sh.metronome,
            met_dest: sh.met_dest,
            default_dest: sh.default_dest,
            // the running pass's hold — set by start_playback, kept on
            // every refresh so a rebuild stays in the parked domain
            countin_us: self.live_countin_us,
            countin_region: self.rec.as_ref().and_then(|r| r.cin_region),
            loop_enabled: sh.loop_enabled,
            loop_start: sh.loop_start,
            loop_end: sh.loop_end,
            chase_sysex: sh.chase_sysex,
            sequential: sh.doc.is_sequential(),
            sxp: sh.sysex_policy,
        }
    }

    fn live_audible<'a>(&'a self, ctx: &'a LiveCtx) -> impl Fn(usize) -> bool + 'a {
        let sel_track = self.sel_track;
        move |tr: usize| {
            if ctx.sequential && ctx.soloed.is_empty() {
                // format 2: only the viewed sequence plays — sequences are
                // independent patterns, not lanes of one song
                tr == sel_track && !ctx.muted.contains(&tr)
            } else {
                track_audible(tr, &ctx.muted, &ctx.soloed)
            }
        }
    }

    /// Destinations the audible timeline actually touches — the set of
    /// sinks playback needs open.
    fn needed_dests(&self, ctx: &LiveCtx) -> BTreeSet<usize> {
        let audible = self.live_audible(ctx);
        let dest_of = |t: usize| ctx.dest_of_track.get(&t).copied().unwrap_or(0);
        let mut needed: BTreeSet<usize> = self
            .doc(|d| d.timeline_tagged())
            .into_iter()
            .filter(|(_, tr, _)| audible(*tr))
            .map(|(_, tr, _)| dest_of(tr))
            .collect();
        // the click destination gets a sink even when no track routes to it
        if ctx.metronome || ctx.countin_us > 0 {
            needed.insert(ctx.met_dest.unwrap_or(ctx.default_dest));
        }
        needed
    }

    /// Open one sink per needed destination. `loading` collects dest names
    /// whose plugin is still being instantiated — their events drop until
    /// the slot reports Ready (a deferred route refresh picks them up).
    #[allow(clippy::type_complexity)]
    fn open_sinks(
        &mut self,
        ctx: &LiveCtx,
        needed: &BTreeSet<usize>,
    ) -> (
        Vec<Box<dyn EventSink>>,
        HashMap<usize, usize>,
        HashMap<usize, usize>,
        Vec<String>,
    ) {
        let sxp_cfg = midi_io::SysexConfig {
            policy: ctx.sxp,
            ..Default::default()
        };
        let mut sinks: Vec<Box<dyn EventSink>> = Vec::new();
        let mut sink_of: HashMap<usize, usize> = HashMap::new();
        // dest index -> the plugin's transport lane: tempo/meter map events
        // ride the same schedule as notes and apply at block boundaries
        let mut transport_of: HashMap<usize, usize> = HashMap::new();
        let mut loading: Vec<String> = Vec::new();
        self.poll_plugin_events();
        for &d in needed {
            let Some((_, dest)) = ctx.dests.get(d) else {
                continue;
            };
            match dest {
                output::Destination::MidiPort { port_name, ord } => {
                    // bind by (name, ord) — a same-name sibling must never
                    // silently take over this destination
                    match midi_io::Output::open_ord(port_name, *ord) {
                        Ok(out) => {
                            let sink = PortSink::with_config(out, sxp_cfg);
                            self.sysex_stats.push(sink.stats());
                            sink_of.insert(d, sinks.len());
                            sinks.push(Box::new(sink));
                        }
                        Err(e) => self.status = format!("{e}").into(),
                    }
                }
                output::Destination::Plugin { .. } => {
                    self.ensure_plugin(d, false);
                    if matches!(self.plugin_state.get(&d), Some(PluginState::Loading { .. })) {
                        loading.push(ctx.dests[d].0.clone());
                    } else if self.plugin_slots.contains_key(&d)
                        && matches!(self.plugin_state.get(&d), Some(PluginState::Ready { .. }))
                    {
                        let slot = self.plugin_slots.get(&d).expect("slot just loaded");
                        sink_of.insert(d, sinks.len());
                        sinks.push(Box::new(slot.sink.clone()));
                        transport_of.insert(d, sinks.len());
                        sinks.push(Box::new(output::TransportSink::new(slot.plugin.clone())));
                        // #190: blocking lock is deliberate — this runs once
                        // per play start / route refresh and skipping it
                        // would leave the instance not processing for the
                        // whole pass (a silent destination, not a retried
                        // op); the wait is bounded by one audio block.
                        if let Ok(mut p) = slot.plugin.lock() {
                            let _ = p.set_playing(true);
                        }
                    } else if let Some(PluginState::Failed { .. }) = self.plugin_state.get(&d) {
                        self.status =
                            tf("plugin.failed", &[("name", ctx.dests[d].0.as_str())]).into();
                    }
                }
            }
        }
        (sinks, sink_of, transport_of, loading)
    }

    /// Build the routed event schedule for a pass starting at `start_us`
    /// (µs domain): SysEx setup traffic, channel events filtered by
    /// audible tracks and remapped onto open sinks, metronome clicks,
    /// plugin transport lanes, then the chase splice at `start_us`. With an
    /// explicit loop left locator `loop_ls_us` a second chase splices at
    /// that point too, so every loop wrap restores channel+transport state
    /// where the cycle restarts (#130).
    fn build_live_events(
        &self,
        ctx: &LiveCtx,
        start_us: u64,
        loop_ls_us: Option<u64>,
        sink_of: &HashMap<usize, usize>,
        transport_lanes: &[usize],
    ) -> Vec<(u64, usize, Vec<u8>)> {
        let audible = self.live_audible(ctx);
        let dest_of = |t: usize| ctx.dest_of_track.get(&t).copied().unwrap_or(0);
        // explicit FF 20 channel assignment: playback re-channelizes the
        // track's voice messages to it (#221)
        let chan_of = |t: usize| self.doc(|d| d.tracks.get(t).and_then(|tr| tr.explicit_channel()));
        let tagged: Vec<(u64, usize, Vec<u8>)> = self
            .doc(|d| d.timeline_tagged())
            .into_iter()
            .filter(|(_, tr, _)| audible(*tr))
            .collect();
        // SysEx first among same-time events: setup traffic (GM/XG resets,
        // patch dumps) must land before notes struck at the same instant.
        // assemble_events' stable sort keeps sysex < channel < click at
        // equal µs.
        let mut events: Vec<(u64, usize, Vec<u8>)> = route_events(
            self.doc(|d| d.timeline_sysex()),
            &audible,
            dest_of,
            chan_of,
            sink_of,
        );
        events.extend(route_events(tagged, &audible, dest_of, chan_of, sink_of));
        // count-in hold: with `cin` > 0 the song is parked — every event
        // at or after the pass start is deferred by `cin` below, and the
        // hold window is filled by the pre-region's own clicks (#137)
        let cin = ctx.countin_us;
        // explicit click destination (#137): the configured metronome
        // output else the document default — never "first open port"
        let click_d = ctx.met_dest.unwrap_or(ctx.default_dest);
        let click_sink = sink_of.get(&click_d).copied();
        if ctx.metronome {
            if let Some(s) = click_sink {
                // clicks follow the FF58 map: one per `cc` clocks
                // (24 = a quarter) with woodblock 76 on real bar lines —
                // never a fake-PPQ beat or a hard-coded 4/4 accent.
                let td = self.td();
                let mm = self.doc(|d| d.meter_map_for(self.sel_track));
                let end_us = events.iter().map(|e| e.0).max().unwrap_or(0);
                let end_tick = self.doc(|d| d.tempo_map_for(self.sel_track).us_to_tick(end_us));
                // bar-relative click grid (#212): the accent lands on every
                // measure downbeat and subdivisions stay inside their bar —
                // an accumulator walking a fixed interval skipped 7/8 and
                // 5/8 downbeats entirely
                let click_ticks: Vec<u64> = match td {
                    TimeDisplay::Metrical { .. } => metronome_clicks(&mm, end_tick),
                    // one click per displayed second — every click a bar
                    TimeDisplay::Smpte { .. } => {
                        let step = td.click_ticks().max(1);
                        (0..=end_tick / step).map(|i| i * step).collect()
                    }
                };
                for t in click_ticks {
                    let us = self.doc(|d| d.tempo_map_for(self.sel_track).tick_to_us(t));
                    if us > end_us {
                        break;
                    }
                    let accent = match td {
                        TimeDisplay::Metrical { .. } => mm.bar_start_tick(t) == t,
                        TimeDisplay::Smpte { .. } => true,
                    };
                    let note = if accent { 76 } else { 77 };
                    events.push((us, s, vec![0x99, note, 110]));
                    events.push((us + 20_000, s, vec![0x99, note, 0]));
                }
            }
        }
        // transport map -> scheduled updates on each plugin's transport
        // lane, plus the state in effect at the start position (the chase —
        // on loop wrap it replays from the loop point's partition, so the
        // wrap restores loop-start tempo/meter before the next boundary)
        let transport_pts = self.transport_points();
        for &s in transport_lanes {
            for (us, cmd) in &transport_pts {
                events.push((*us, s, output::encode_transport(cmd)));
            }
            for (us, cmd) in output::chase_transport(&transport_pts, start_us) {
                events.push((us, s, output::encode_transport(&cmd)));
            }
        }
        // park the song during the count-in hold (#137): every event at
        // or after the pass start slides `cin` later — the pre-start
        // tail stays skipped by the worker's partition either way; and
        // both chases describe state AT the pass start so they retime to
        // the deferred boundary instead of the skipped tail
        let mut chase = route_events(
            self.doc(|d| d.chase_events(start_us)),
            &audible,
            dest_of,
            chan_of,
            sink_of,
        );
        // opt-in SysEx chase
        let mut chase_sx = if ctx.chase_sysex {
            route_events(
                self.doc(|d| d.chase_sysex(start_us)),
                &audible,
                dest_of,
                chan_of,
                sink_of,
            )
        } else {
            Vec::new()
        };
        let boundary = park_schedule(&mut events, &mut chase, &mut chase_sx, start_us, cin);
        // count-in clicks in the parked domain — appended after the shift
        // so they land inside the hold window, not beyond it
        if let (Some(region), Some(s)) = (ctx.countin_region, click_sink) {
            if cin > 0 {
                events.extend(self.countin_clicks(region, cin, s));
            }
        }
        let mut events = assemble_events(events, chase, chase_sx, boundary);
        // explicit loop locator ≠ the pass start: splice a second chase at
        // the left locator so each wrap re-establishes channel state and
        // plugin transport where the cycle restarts (#130). In a parked
        // pass a locator before the record point folds into the boundary
        // chase — no schedule µs exists for it during the hold.
        if let Some(ls) = loop_ls_us.filter(|&ls| ls != start_us && (cin == 0 || ls > start_us)) {
            let mut lc = route_events(
                self.doc(|d| d.chase_events(ls)),
                &audible,
                dest_of,
                chan_of,
                sink_of,
            );
            if ctx.chase_sysex {
                lc.splice(
                    0..0,
                    route_events(
                        self.doc(|d| d.chase_sysex(ls)),
                        &audible,
                        dest_of,
                        chan_of,
                        sink_of,
                    ),
                );
            }
            for &s in transport_lanes {
                for (us, cmd) in output::chase_transport(&transport_pts, ls) {
                    lc.push((us, s, output::encode_transport(&cmd)));
                }
            }
            let ls_w = ls + cin;
            if cin > 0 {
                for e in &mut lc {
                    e.0 = ls_w;
                }
            }
            lc.sort_by_key(|e| e.0);
            let at = events.partition_point(|e| e.0 < ls_w);
            events.splice(at..at, lc);
        }
        events
    }

    /// Count-in clicks for the viewed track's meter/tempo maps — the
    /// viewed sequence's own map in format 2, like the metronome (#137).
    fn countin_clicks(
        &self,
        region: (u64, u64),
        cin: u64,
        sink: usize,
    ) -> Vec<(u64, usize, Vec<u8>)> {
        self.doc(|d| {
            countin_clicks_of(
                &d.meter_map_for(self.sel_track),
                &d.tempo_map_for(self.sel_track),
                matches!(self.td(), TimeDisplay::Smpte { .. }),
                self.td().click_ticks(),
                region,
                cin,
                sink,
            )
        })
    }

    /// Worker's reported position mapped back to document µs — during a
    /// count-in hold the schedule runs `live_countin_us` ahead of the
    /// document, so `play_us` walks the pre-region while the song is
    /// parked (#137).
    pub(crate) fn live_pos_us(&self, p: &Playback) -> u64 {
        p.position_us().saturating_sub(self.live_countin_us)
    }

    /// Live control-plane update (#140/#141): committed transactions,
    /// mute/solo, loop, metronome and chase toggles rebuild the running
    /// schedule in place instead of restarting playback — the worker
    /// releases only notes that lost their note-off and continues from
    /// the reached position. A routing-map change takes the heavier
    /// `refresh_live_routing` path (new sinks).
    pub(crate) fn refresh_live_schedule(&mut self) {
        let Some(pb) = &self.playback else { return };
        if !pb.is_running() {
            return;
        }
        let ctx = self.live_ctx();
        if ctx.dest_of_track != self.live_dest_of {
            self.refresh_live_routing();
            return;
        }
        let pos = self.live_pos_us(pb);
        let (loop_from, loop_end) = self.loop_range_us(&ctx);
        let events = self.build_live_events(
            &ctx,
            pos,
            loop_from,
            &self.live_sink_of,
            &self.live_transport,
        );
        self.send_live_patch(events, loop_from, loop_end, None);
    }

    /// Heavier live update for destination/routing changes: rebuild the
    /// sink set (open new ports, warm plugins), then patch events + sinks
    /// together — the worker panics the old sinks. A destination still
    /// loading keeps the old route and retries on plugin-ready.
    pub(crate) fn refresh_live_routing(&mut self) {
        if self.playback.as_ref().is_none_or(|p| !p.is_running()) {
            return;
        }
        let ctx = self.live_ctx();
        let needed = self.needed_dests(&ctx);
        let (sinks, sink_of, transport_of, loading) = self.open_sinks(&ctx, &needed);
        if !loading.is_empty() {
            self.live_route_dirty = true;
            return;
        }
        self.live_sink_of = sink_of;
        self.live_transport = transport_of.values().copied().collect();
        self.live_dest_of = ctx.dest_of_track.clone();
        self.live_route_dirty = false;
        let pos = self
            .playback
            .as_ref()
            .map(|p| self.live_pos_us(p))
            .unwrap_or(0);
        let (loop_from, loop_end) = self.loop_range_us(&ctx);
        let events = self.build_live_events(
            &ctx,
            pos,
            loop_from,
            &self.live_sink_of,
            &self.live_transport,
        );
        self.send_live_patch(events, loop_from, loop_end, Some(sinks));
    }

    /// Explicit loop locators → µs bounds for the schedule (#130). Ticks
    /// convert through the viewed track's tempo map. A degenerate or empty
    /// range falls back to the legacy play-start→schedule-end wrap.
    pub(crate) fn loop_range_us(&self, ctx: &LiveCtx) -> (Option<u64>, Option<u64>) {
        if !ctx.loop_enabled {
            return (None, None);
        }
        let (s, e) = self.doc(|d| {
            let tm = d.tempo_map_for(self.sel_track);
            (
                ctx.loop_start.map(|t| tm.tick_to_us(t)),
                ctx.loop_end.map(|t| tm.tick_to_us(t)),
            )
        });
        match (s, e) {
            (Some(a), Some(b)) if b > a => (Some(a), Some(b)),
            // left locator only wraps at the schedule's end
            (Some(a), None) => (Some(a), None),
            // right locator only cycles from song start
            (None, Some(b)) => (Some(0), Some(b)),
            // unset locators keep the implicit play-start→end wrap
            _ => (Some(self.loop_start_us), None),
        }
    }

    fn send_live_patch(
        &self,
        events: Vec<(u64, usize, Vec<u8>)>,
        loop_from_us: Option<u64>,
        loop_end_us: Option<u64>,
        sinks: Option<Vec<Box<dyn EventSink>>>,
    ) {
        if let Some(pb) = &self.playback {
            // locator bounds are doc µs — translate into the running
            // pass's parked domain. During a count-in a left locator
            // before the record point can't be expressed, so the pass
            // cycles from the capture boundary (#137)
            let cin = self.live_countin_us;
            pb.update(midi_io::SchedulePatch {
                events,
                loop_from_us: loop_from_us.map(|v| {
                    (if cin > 0 {
                        v.max(self.play_start_us)
                    } else {
                        v
                    }) + cin
                }),
                loop_end_us: loop_end_us.map(|v| v + cin),
                sinks,
            });
        }
    }

    pub(crate) fn start_playback(&mut self) {
        self.audition_off();
        self.sysex_stats.clear();
        // the point this pass began — Return-to-Start / stop-return anchor
        self.play_start_us = self.play_us;
        let mut ctx = self.live_ctx();
        if ctx.dests.is_empty() {
            self.status = t("status.no_port").into();
            return;
        }
        // record arm with a count-in: the pass parks the song for the
        // lead-in — clicks fill the hold and playback starts exactly at
        // the capture boundary (#137)
        ctx.countin_us = self
            .rec
            .as_ref()
            .filter(|r| r.recording.load(std::sync::atomic::Ordering::Relaxed))
            .map(|r| r.cin_us)
            .unwrap_or(0);
        ctx.countin_region = self.rec.as_ref().and_then(|r| r.cin_region);
        self.live_countin_us = ctx.countin_us;
        let needed = self.needed_dests(&ctx);
        let (sinks, sink_of, transport_of, loading) = self.open_sinks(&ctx, &needed);
        if !loading.is_empty() {
            self.play_pending = true;
            self.status = tf("plugin.waiting", &[("name", loading[0].as_str())]).into();
            return;
        }
        if sinks.is_empty() {
            self.status = t("status.no_port").into();
            return;
        }
        let transport_lanes: Vec<usize> = transport_of.values().copied().collect();
        let (loop_from, loop_end) = self.loop_range_us(&ctx);
        let events =
            self.build_live_events(&ctx, self.play_us, loop_from, &sink_of, &transport_lanes);
        self.loop_start_us = self.play_us;
        self.live_sink_of = sink_of;
        self.live_transport = transport_lanes;
        self.live_dest_of = ctx.dest_of_track.clone();
        self.live_route_dirty = false;
        // locators shift into the parked domain with the schedule; a
        // left locator before the record point folds to the boundary
        let cin = ctx.countin_us;
        self.playback = Some(Playback::start(
            sinks,
            events,
            self.play_us,
            loop_from.map(|v| (if cin > 0 { v.max(self.play_us) } else { v }) + cin),
            loop_end.map(|v| v + cin),
            self.reset_on_stop,
        ));
        // Auto monitoring silences the echo while the transport runs
        self.update_monitor();
    }

    /// MIDI Panic: the user-facing emergency silence — a full CC123/121/120
    /// burst on every configured destination (#161). While transport runs,
    /// the live pass panics its open sinks mid-flight (transport keeps
    /// going); while stopped, each destination is opened just long enough
    /// to deliver the burst — this is what reaches gear stuck by an
    /// earlier crash or unplug.
    pub(crate) fn midi_panic(&mut self) {
        if self.playback.as_ref().is_some_and(|p| p.is_running()) {
            if let Some(pb) = &self.playback {
                pb.panic_now();
            }
        } else {
            let ctx = self.live_ctx();
            for (d, (_, dest)) in ctx.dests.iter().enumerate() {
                match dest {
                    output::Destination::MidiPort { port_name, ord } => {
                        match midi_io::Output::open_ord(port_name, *ord) {
                            Ok(mut out) => out.panic(),
                            Err(e) => self.status = format!("{e}").into(),
                        }
                    }
                    output::Destination::Plugin { .. } => {
                        self.ensure_plugin(d, false);
                        self.poll_plugin_events();
                        if let Some(slot) = self.plugin_slots.get(&d) {
                            slot.sink.clone().panic();
                        }
                    }
                }
            }
        }
        self.status = tf("transport.panicked", &[]).into();
    }

    pub(crate) fn stop_playback(&mut self) {
        self.audition_off();
        self.play_pending = false;
        self.live_sink_of.clear();
        self.live_transport.clear();
        self.live_dest_of.clear();
        self.live_route_dirty = false;
        // the pass is over — its signal is gone; the clip latch stays until
        // the user clears it (#203)
        self.master_peak = 0.0;
        self.master_peak_at = std::time::Instant::now();
        if let Some(mut p) = self.playback.take() {
            self.play_us = self.live_pos_us(&p);
            p.stop();
        }
        self.live_countin_us = 0;
        // surface the long-message diagnostic for the pass that just ended:
        // a dump that was deferred or dropped is silent unless reported
        let (mut inl, mut def, mut drop_n, mut worst) = (0u64, 0u64, 0u64, 0u64);
        for s in self.sysex_stats.drain(..) {
            let (i, d, x, _l, m) = s.snapshot();
            inl += i;
            def += d;
            drop_n += x;
            worst = worst.max(m);
        }
        if drop_n > 0 || def > 0 {
            let (i, d, x, ms) = (
                inl.to_string(),
                def.to_string(),
                drop_n.to_string(),
                (worst / 1000).to_string(),
            );
            self.status = tf(
                "status.sysex_diag",
                &[("i", &i), ("d", &d), ("x", &x), ("ms", &ms)],
            )
            .into();
        }
        // silence every warm instance but keep it loaded — the next Play
        // (and parameter edits made meanwhile) start instantly
        for slot in self.plugin_slots.values() {
            let mut s = slot.sink.clone();
            s.panic();
            if let Ok(mut pl) = slot.plugin.try_lock() {
                let _ = pl.set_playing(false);
            }
        }
        // playback can move plugin state (CC-mapped params); queue a capture
        for d in self.plugin_slots.keys() {
            self.pending_state_capture.insert(*d);
        }
        // transport Stop while recording commits the take (#159); a merely
        // armed input stays armed and keeps listening
        if self.is_recording() {
            self.finish_record();
        }
        // Auto monitoring resumes once the transport halts
        self.update_monitor();
    }

    /// The document's tempo map + meter map as scheduled transport updates
    /// `(µs, TransportCmd)`, sorted by µs. `TransportSink`s drive these into
    /// hosted plugins so their `ProcessContext` follows mid-song changes at
    /// audio block boundaries instead of staying at the head values.
    /// Format-2 documents schedule from the viewed sequence's own maps.
    pub(crate) fn transport_points(&self) -> Vec<(u64, output::TransportCmd)> {
        self.doc(|d| {
            let tr = if d.is_sequential() {
                Some(self.sel_track.min(d.tracks.len().saturating_sub(1)))
            } else {
                None
            };
            transport_points_for(d, tr)
        })
    }

    /// Current playhead position in ticks — where a punch bound lands.
    pub(crate) fn playhead_tick(&self) -> u64 {
        self.doc(|d| d.tempo_map.us_to_tick(self.play_us))
    }

    /// Per-frame master output level poll (#203): max peak across every warm
    /// plugin slot — the running pass's sinks are clones of these, so the
    /// slots read the same live instances (port sinks report nothing). A
    /// contended `try_lock` reads 0, so the held value decays instead of
    /// snapping down; a fresh peak jumps straight up (instant attack).
    /// Also latches the clip LED while any reading leaves sample space.
    pub(crate) fn poll_master_peak(&mut self) {
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.master_peak_at).as_secs_f32();
        self.master_peak_at = now;
        let raw = self
            .plugin_slots
            .values()
            .map(|s| s.sink.level())
            .fold(0.0f32, f32::max);
        if raw >= self.master_peak {
            self.master_peak = raw;
        } else {
            // ~-20 dB/s release toward 0 (amplitude × 0.1 per second)
            self.master_peak = (self.master_peak * 0.1f32.powf(dt)).max(raw);
        }
        if raw > CLIP_LEVEL {
            self.clip_latched = true;
        }
    }

    /// Center the timeline view on the minimap position under window-x
    /// (click or drag on the overview strip).
    pub(crate) fn seek_minimap(&mut self, window_x: f32) {
        let b = self.mini_bounds.get();
        let w = f32::from(b.size.width);
        if w <= 0.0 {
            return;
        }
        let frac = ((window_x - f32::from(b.origin.x)) / w).clamp(0.0, 1.0);
        let t = frac * self.doc_end_ticks() as f32;
        let vw = f32::from(self.roll_bounds.get().size.width) / self.zoom;
        self.scroll_x = ((t - vw / 2.0) * self.zoom).max(0.0);
        self.clamp_scroll();
        // minimap pan counts as a manual scroll — pause follow briefly
        self.follow_hold = Some(std::time::Instant::now() + FOLLOW_HOLD);
    }

    /// Make sure the audition worker holds a sink for destination `d`.
    /// Ports are opened once then owned by the worker; plugins reuse the
    /// already-warm slot (a still-loading plugin simply skips this strike —
    /// the next click works once the slot is ready).
    pub(crate) fn audition_sink(&mut self, d: usize) -> bool {
        if self.aud_ships.contains(&d) {
            return true;
        }
        let dest = lock_shared(&self.shared)
            .dests
            .get(d)
            .map(|(_, dd)| dd.clone());
        match dest {
            Some(output::Destination::MidiPort { port_name, ord }) => {
                match midi_io::Output::open_ord(&port_name, ord) {
                    Ok(out) => {
                        self.audition.set_sink(d, Box::new(PortSink::new(out)));
                        self.aud_ships.insert(d);
                        true
                    }
                    Err(e) => {
                        if self.aud_failed.insert(d) {
                            self.status = tf("status.aud_failed", &[("e", &e.to_string())]).into();
                        }
                        false
                    }
                }
            }
            Some(output::Destination::Plugin { .. }) => {
                self.ensure_plugin(d, false);
                if self.plugin_slots.contains_key(&d)
                    && matches!(self.plugin_state.get(&d), Some(PluginState::Ready { .. }))
                {
                    let sink = self
                        .plugin_slots
                        .get(&d)
                        .expect("slot just loaded")
                        .sink
                        .clone();
                    self.audition.set_sink(d, Box::new(sink));
                    self.aud_ships.insert(d);
                    true
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    /// Preview one pitch through `track`'s destination + `ch`'s bank/program
    /// state at `at_tick`. The worker schedules the note-off itself.
    pub(crate) fn audition_strike(&mut self, track: usize, ch: u8, key: u8, vel: u8, at_tick: u64) {
        if !self.aud_enabled {
            return;
        }
        let (d, setup) = {
            let sh = lock_shared(&self.shared);
            (sh.dest_of(track), sh.doc.channel_setup(track, ch, at_tick))
        };
        if !self.audition_sink(d) {
            return;
        }
        self.audition.setup(d, ch, setup);
        self.audition.strike(d, ch, key, vel, self.aud_ms);
    }

    /// Release every preview note — mouse-up, focus loss, doc swap,
    /// destination change, quit.
    pub(crate) fn audition_off(&mut self) {
        self.scrub_key = None;
        self.audition.all_off();
    }
}

/// Metronome click grid: one accented click per measure downbeat plus
/// unaccented subdivisions at the meter's `cc` click interval, strictly
/// inside each bar (#212). Bar-relative, so odd meters (7/8, 5/8) keep
/// their downbeats instead of drifting past them.
pub(crate) fn metronome_clicks(mm: &document::MeterMap, end_tick: u64) -> Vec<u64> {
    let mut out = Vec::new();
    for bs in mm.bar_starts_between(0, end_tick.saturating_add(1)) {
        if bs > end_tick {
            break;
        }
        out.push(bs);
        let click = mm.click_ticks_at(bs).max(1);
        let bar_end = mm.next_bar_start(bs);
        let mut t = bs.saturating_add(click);
        while t < bar_end && t <= end_tick {
            out.push(t);
            t = t.saturating_add(click);
        }
    }
    out
}

#[cfg(test)]
mod metronome_tests {
    use super::route_events;
    use std::collections::HashMap;

    /// #221 — a track with an explicit FF 20 channel plays back on that
    /// channel; without it each event keeps its recorded channel.
    #[test]
    fn route_events_rechannelizes_to_explicit_track_channel() {
        let tl = vec![
            (0u64, 0usize, vec![0x90, 60, 100]),
            (100u64, 0usize, vec![0xE5, 62, 90]), // channel-6 bend also moves
            (200u64, 1usize, vec![0x90, 64, 80]), // track 1: no FF 20
        ];
        let mut sinks = HashMap::new();
        sinks.insert(0usize, 0usize);
        let out = route_events(
            tl,
            |t| t < 2,
            |_| 0,
            |t| if t == 0 { Some(1u8) } else { None },
            &sinks,
        );
        assert_eq!(
            out,
            vec![
                (0, 0, vec![0x91, 60, 100]),
                (100, 0, vec![0xE1, 62, 90]),
                (200, 0, vec![0x90, 64, 80]),
            ]
        );
    }

    use super::metronome_clicks;
    use document::MeterMap;

    fn mm_of(sig: &[(u64, u8, u8)], ppq: u16) -> MeterMap {
        mm_of_cc(
            &sig.iter()
                .map(|&(t, n, d)| (t, n, d, 24u8))
                .collect::<Vec<_>>(),
            ppq,
        )
    }

    fn mm_of_cc(sig: &[(u64, u8, u8, u8)], ppq: u16) -> MeterMap {
        // one-track doc carrying FF58 events at the given ticks
        let mut events: Vec<document::Event> = sig
            .iter()
            .enumerate()
            .map(|(i, &(tick, num, den, cc))| document::Event {
                id: i as document::EventId,
                tick,
                seq: 0,
                raw_body: None,
                kind: smf_core::EventKind::Meta {
                    meta_type: 0x58,
                    data: bytes::Bytes::from(vec![num, den, cc, 8]),
                },
            })
            .collect();
        events.sort_by_key(|e| e.tick);
        MeterMap::build(
            &[document::Track {
                events,
                name: None,
                out_port: 0,
                out_channel: 0,
            }],
            smf_core::Division::Metrical(ppq),
        )
    }

    #[test]
    fn quarter_meter_clicks_every_beat() {
        // 4/4 @480: accents at 0/1920/3840, unaccented at 480/960/1440…
        let mm = mm_of(&[(0, 4, 4)], 480);
        let clicks = metronome_clicks(&mm, 1920);
        assert_eq!(clicks, vec![0, 480, 960, 1440, 1920]);
    }

    #[test]
    fn seven_eight_keeps_every_downbeat() {
        // #212 repro: 7/8 bars are 1680 ticks; the old accumulator at
        // 480-tick spacing landed on 1920 and skipped the 1680 downbeat
        let mm = mm_of(&[(0, 7, 3)], 480);
        let clicks = metronome_clicks(&mm, 2 * 1680);
        assert_eq!(
            clicks,
            vec![0, 480, 960, 1440, 1680, 2160, 2640, 3120, 3360]
        );
    }

    #[test]
    fn five_eight_and_meter_change() {
        let mm = mm_of(&[(0, 5, 3), (1200, 4, 2)], 480);
        let clicks = metronome_clicks(&mm, 1200 + 1920);
        // bar 1 = [0,1200): clicks at 0,480,960; then 4/4 from 1200
        assert_eq!(clicks, vec![0, 480, 960, 1200, 1680, 2160, 2640, 3120]);
    }

    #[test]
    fn compound_meter_clicks_dotted_quarters() {
        // 6/8 @480: cc=36 → dotted-quarter clicks (720 ticks), 2 per bar
        let mm = mm_of_cc(&[(0, 6, 3, 36)], 480);
        let clicks = metronome_clicks(&mm, 1440);
        assert_eq!(clicks, vec![0, 720, 1440]);
    }

    #[test]
    fn zero_end_is_single_click() {
        let mm = mm_of(&[(0, 4, 4)], 480);
        assert_eq!(metronome_clicks(&mm, 0), vec![0]);
    }
}

#[cfg(test)]
mod meter_tests {
    use super::{meter_frac, CLIP_LEVEL};

    /// #203 — the dB scale anchors: silence is empty, -6 dBFS lands at the
    /// green/amber boundary, 0 dBFS near (but not at) the top, and a clipped
    /// peak still grows the bar instead of pegging at 100%.
    #[test]
    fn meter_scale_anchors() {
        assert_eq!(meter_frac(0.0), 0.0);
        assert_eq!(meter_frac(-1.0), 0.0);
        // -6 dBFS = 0.5 linear → ≈42 dB of the 54 dB span (log10(0.5) is
        // -6.02 dB, not exactly -6, so allow a hair of tolerance)
        let minus6 = meter_frac(0.5);
        assert!((minus6 - 42.0 / 54.0).abs() < 1e-2, "{minus6}");
        // 0 dBFS = 1.0 linear → 48 of 54 dB — headroom remains above
        let zero = meter_frac(1.0);
        assert!((zero - 48.0 / 54.0).abs() < 1e-4, "{zero}");
        // clipped: past 0 dBFS but still inside the bar
        let clip = meter_frac(CLIP_LEVEL * 1.5);
        assert!(clip > zero && clip <= 1.0, "{clip} vs {zero}");
        assert_eq!(meter_frac(100.0), 1.0);
    }

    /// The draw gate is "fraction non-zero": peaks below the -48 dB floor
    /// map to an empty bar, so the meter hides in golden screenshots
    /// (rendered without playback, peak 0.0) and at true silence.
    #[test]
    fn floor_hides_silence() {
        assert_eq!(meter_frac(0.001), 0.0); // -60 dBFS, under the floor
        assert!(meter_frac(0.1) > 0.0); // -20 dBFS, drawn
        assert!(meter_frac(0.004) > 0.0); // ≈ -48 dBFS, just over it
    }
}

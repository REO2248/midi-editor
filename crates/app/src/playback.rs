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

/// Filter a per-track timeline to audible tracks and remap each event's
/// track onto its sink index; events whose destination has no open sink
/// (failed port, unavailable plugin) are dropped.
pub(crate) fn route_events(
    timeline: Vec<(u64, usize, Vec<u8>)>,
    audible: impl Fn(usize) -> bool,
    dest_of: impl Fn(usize) -> usize,
    sink_of: &HashMap<usize, usize>,
) -> Vec<(u64, usize, Vec<u8>)> {
    timeline
        .into_iter()
        .filter(|(_, tr, _)| audible(*tr))
        .filter_map(|(us, tr, b)| sink_of.get(&dest_of(tr)).map(|&s| (us, s, b)))
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
    /// count-in lead-in µs for this pass — clicks always sound inside it
    /// even when the metronome toggle is off (#137); 0 = no count-in
    countin_us: u64,
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
            // set only on the initial pass in start_playback — a mid-run
            // refresh keeps click generation for the metronome only
            countin_us: 0,
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
        let tagged: Vec<(u64, usize, Vec<u8>)> = self
            .doc(|d| d.timeline_tagged())
            .into_iter()
            .filter(|(_, tr, _)| audible(*tr))
            .collect();
        // SysEx first among same-time events: setup traffic (GM/XG resets,
        // patch dumps) must land before notes struck at the same instant.
        // assemble_events' stable sort keeps sysex < channel < click at
        // equal µs.
        let mut events: Vec<(u64, usize, Vec<u8>)> =
            route_events(self.doc(|d| d.timeline_sysex()), &audible, dest_of, sink_of);
        events.extend(route_events(tagged, &audible, dest_of, sink_of));
        if ctx.metronome || ctx.countin_us > 0 {
            // explicit click destination (#137): the configured metronome
            // output else the document default — never "first open port"
            let click_d = ctx.met_dest.unwrap_or(ctx.default_dest);
            let click_sink = sink_of.get(&click_d).copied();
            if let Some(s) = click_sink {
                // clicks follow the FF58 map: one per `cc` clocks
                // (24 = a quarter) with woodblock 76 on real bar lines —
                // never a fake-PPQ beat or a hard-coded 4/4 accent. With
                // the metronome off they cover only the count-in window.
                let td = self.td();
                let mm = self.doc(|d| d.meter_map_for(self.sel_track));
                let end_us = if ctx.metronome {
                    events.iter().map(|e| e.0).max().unwrap_or(0)
                } else {
                    start_us + ctx.countin_us
                };
                let mut t = 0u64;
                loop {
                    let us = self.doc(|d| d.tempo_map_for(self.sel_track).tick_to_us(t));
                    if us > end_us {
                        break;
                    }
                    let accent = match td {
                        TimeDisplay::Metrical { .. } => mm.bar_start_tick(t) == t,
                        // one click per displayed second — every click a bar
                        TimeDisplay::Smpte { .. } => true,
                    };
                    let note = if accent { 76 } else { 77 };
                    events.push((us, s, vec![0x99, note, 110]));
                    events.push((us + 20_000, s, vec![0x99, note, 0]));
                    t += match td {
                        TimeDisplay::Metrical { .. } => mm.click_ticks_at(t),
                        TimeDisplay::Smpte { .. } => td.click_ticks(),
                    };
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
        // globally sort before seek partitioning and the chase splice —
        // transport payloads join the same ordered stream
        events.sort_by_key(|e| e.0);
        let chase = route_events(
            self.doc(|d| d.chase_events(start_us)),
            &audible,
            dest_of,
            sink_of,
        );
        // opt-in SysEx chase
        let chase_sx = if ctx.chase_sysex {
            route_events(
                self.doc(|d| d.chase_sysex(start_us)),
                &audible,
                dest_of,
                sink_of,
            )
        } else {
            Vec::new()
        };
        let mut events = assemble_events(events, chase, chase_sx, start_us);
        // explicit loop locator ≠ the pass start: splice a second chase at
        // the left locator so each wrap re-establishes channel state and
        // plugin transport where the cycle restarts (#130)
        if let Some(ls) = loop_ls_us.filter(|&ls| ls != start_us) {
            let mut lc = route_events(self.doc(|d| d.chase_events(ls)), &audible, dest_of, sink_of);
            if ctx.chase_sysex {
                lc.splice(
                    0..0,
                    route_events(self.doc(|d| d.chase_sysex(ls)), &audible, dest_of, sink_of),
                );
            }
            for &s in transport_lanes {
                for (us, cmd) in output::chase_transport(&transport_pts, ls) {
                    lc.push((us, s, output::encode_transport(&cmd)));
                }
            }
            lc.sort_by_key(|e| e.0);
            let at = events.partition_point(|e| e.0 < ls);
            events.splice(at..at, lc);
        }
        events
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
        let pos = pb.position_us();
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
        let pos = self.playback.as_ref().map(|p| p.position_us()).unwrap_or(0);
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
            pb.update(midi_io::SchedulePatch {
                events,
                loop_from_us,
                loop_end_us,
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
        // record arm with a count-in: the pass carries the lead-in length so
        // click generation covers it on the click destination (#137)
        ctx.countin_us = self
            .rec
            .as_ref()
            .filter(|r| r.recording.load(std::sync::atomic::Ordering::Relaxed))
            .map(|r| r.cin_us)
            .unwrap_or(0);
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
        self.playback = Some(Playback::start(
            sinks,
            events,
            self.play_us,
            loop_from,
            loop_end,
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
        if let Some(mut p) = self.playback.take() {
            self.play_us = p.position_us();
            p.stop();
        }
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

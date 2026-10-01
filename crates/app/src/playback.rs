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
    /// accessibility Increment/Decrement actions).
    pub(crate) fn seek_bars(&mut self, bars: i64, cx: &mut Context<Self>) {
        let step = self.ppq() as i64 * 4;
        let cur = self.doc(|d| d.tempo_map.us_to_tick(self.play_us)) as i64;
        let tick = (cur + bars * step).max(0).min(self.doc_end_ticks() as i64);
        self.play_us = self.doc(|d| d.tempo_map.tick_to_us(tick as u64));
        cx.notify();
    }

    /// Move the playhead to `tick`; `play` (or an already-playing transport)
    /// restarts the engine from there.
    pub(crate) fn seek_to_tick(&mut self, tick: u64, play: bool, cx: &mut Context<Self>) {
        self.play_us = self.doc(|d| d.tempo_map_for(self.sel_track).tick_to_us(tick));
        if play || self.playback.is_some() {
            self.stop_playback();
            self.start_playback();
        }
        cx.notify();
    }

    pub(crate) fn toggle_play(&mut self, cx: &mut Context<Self>) {
        if self.playback.is_some() {
            self.stop_playback();
        } else {
            self.start_playback();
        }
        cx.notify();
    }

    pub(crate) fn start_playback(&mut self) {
        self.audition_off();
        // snapshot routing state so no lock is held while opening sinks
        let (
            dests,
            dest_of_track,
            muted,
            soloed,
            metronome,
            loop_enabled,
            chase_sysex,
            sequential,
            sxp,
        ) = {
            let sh = lock_shared(&self.shared);
            let map: HashMap<usize, usize> = (0..sh.doc.tracks.len())
                .map(|t| (t, sh.dest_of(t)))
                .collect();
            (
                sh.dests.clone(),
                map,
                sh.muted.clone(),
                sh.soloed.clone(),
                sh.metronome,
                sh.loop_enabled,
                sh.chase_sysex,
                sh.doc.is_sequential(),
                sh.sysex_policy,
            )
        };
        let sxp_cfg = midi_io::SysexConfig {
            policy: sxp,
            ..Default::default()
        };
        self.sysex_stats.clear();
        let dest_of = |t: usize| dest_of_track.get(&t).copied().unwrap_or(0);
        if dests.is_empty() {
            self.status = t("status.no_port").into();
            return;
        }
        let sel_track = self.sel_track;
        let audible = |tr: usize| {
            if sequential && soloed.is_empty() {
                // format 2: only the viewed sequence plays — sequences are
                // independent patterns, not lanes of one song
                tr == sel_track && !muted.contains(&tr)
            } else {
                track_audible(tr, &muted, &soloed)
            }
        };
        let tagged: Vec<(u64, usize, Vec<u8>)> = self
            .doc(|d| d.timeline_tagged())
            .into_iter()
            .filter(|(_, tr, _)| audible(*tr))
            .collect();
        // open each destination that at least one event needs
        let needed: BTreeSet<usize> = tagged.iter().map(|(_, tr, _)| dest_of(*tr)).collect();
        let mut sinks: Vec<Box<dyn EventSink>> = Vec::new();
        let mut sink_of: HashMap<usize, usize> = HashMap::new();
        // dest index -> the plugin's transport lane: tempo/meter map events
        // ride the same schedule as notes and apply at block boundaries
        let mut transport_of: HashMap<usize, usize> = HashMap::new();
        self.poll_plugin_events();
        for d in needed {
            let Some((_, dest)) = dests.get(d) else {
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
                        let name = dests[d].0.clone();
                        self.play_pending = true;
                        self.status = tf("plugin.waiting", &[("name", name.as_str())]).into();
                        return;
                    }
                    if self.plugin_slots.contains_key(&d)
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
                        self.status = tf("plugin.failed", &[("name", dests[d].0.as_str())]).into();
                    }
                }
            }
        }
        if sinks.is_empty() {
            self.status = t("status.no_port").into();
            return;
        }
        // SysEx first among same-time events: setup traffic (GM/XG resets,
        // patch dumps) must land before notes struck at the same instant.
        // assemble_events' stable sort keeps sysex < channel < click at
        // equal µs.
        let mut events: Vec<(u64, usize, Vec<u8>)> =
            route_events(self.doc(|d| d.timeline_sysex()), audible, dest_of, &sink_of);
        events.extend(route_events(tagged, audible, dest_of, &sink_of));
        if metronome {
            // prefer a plain MIDI port for clicks; fall back to any sink
            let click_sink = dests
                .iter()
                .enumerate()
                .find(|(_, (_, d))| matches!(d, output::Destination::MidiPort { .. }))
                .and_then(|(d, _)| sink_of.get(&d).copied())
                .or_else(|| sink_of.values().next().copied());
            if let Some(s) = click_sink {
                // one click per beat / per second — never a fake-PPQ beat
                let click = self.td().click_ticks();
                let end_us = events.iter().map(|e| e.0).max().unwrap_or(0);
                let mut beat = 0u64;
                loop {
                    let us = self.doc(|d| d.tempo_map_for(self.sel_track).tick_to_us(beat * click));
                    if us > end_us {
                        break;
                    }
                    let note = if beat.is_multiple_of(4) { 76 } else { 77 };
                    events.push((us, s, vec![0x99, note, 110]));
                    events.push((us + 20_000, s, vec![0x99, note, 0]));
                    beat += 1;
                }
            }
        }
        // chase: re-establish the state the timeline had built up before the
        // play position (CC/program/bend/at, plus notes already sounding) so
        // mid-song starts and loop wraps sound like a continuous pass.
        let start_us = self.play_us;
        // transport map -> scheduled updates on each plugin's transport
        // lane, plus the state in effect at the start position (the chase —
        // on loop wrap it replays from the loop point's partition, so the
        // wrap restores loop-start tempo/meter before the next boundary)
        let transport_pts = self.transport_points();
        for &s in transport_of.values() {
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
            audible,
            dest_of,
            &sink_of,
        );
        // opt-in SysEx chase
        let chase_sx = if chase_sysex {
            route_events(
                self.doc(|d| d.chase_sysex(start_us)),
                audible,
                dest_of,
                &sink_of,
            )
        } else {
            Vec::new()
        };
        let events = assemble_events(events, chase, chase_sx, start_us);
        self.loop_start_us = self.play_us;
        self.playback = Some(Playback::start(
            sinks,
            events,
            self.play_us,
            loop_enabled.then_some(self.loop_start_us),
        ));
    }

    pub(crate) fn stop_playback(&mut self) {
        self.audition_off();
        self.play_pending = false;
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
        self.finish_record();
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

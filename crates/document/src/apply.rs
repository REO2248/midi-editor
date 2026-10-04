//! Transaction engine + derived views over the raw event truth.
//!
//! `Document::apply(Transaction)` is the single mutation path — GUI and
//! MCP edits both funnel through it, and `revert` powers unified undo.
//! Diagnostics/fix-ops, timeline + channel-state chase, the note-pairing
//! derived view, and `DocSnapshot` all live here because they share the
//! same ownership rule: every read is a pure view over the raw events,
//! every write is a Transaction.

use super::*;

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
        // A file that declares format 0 but carries several tracks is
        // self-contradictory (format 0 means exactly one); read it as
        // format 1 — an interpretation of the header, not a data rewrite.
        let format = if f.format == 0 && tracks.len() != 1 {
            1
        } else {
            f.format
        };
        let mut doc = Document {
            format,
            division: f.division,
            tracks,
            revision: 0,
            next_event_id: next_id,
            by_id,
            tempo_map: TempoMap::default(),
            meter_map: MeterMap::default(),
        };
        doc.rebuild_index();
        doc.rebuild_maps();
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

    /// Tempo and meter breakpoints are pure derivations of the event
    /// list — rebuilt wholesale after every mutation, cheap and always
    /// in sync.
    fn rebuild_maps(&mut self) {
        self.tempo_map = TempoMap::build(&self.tracks, self.division);
        self.meter_map = MeterMap::build(&self.tracks, self.division);
    }

    /// The single edit entry point shared by GUI and MCP. Atomic: either
    /// every op applies and the revision advances, or an error leaves the
    /// document (and its id index) exactly as it was.
    ///
    /// Post-conditions enforced here (the "structural invariants"):
    /// every touched track keeps a single End-of-Track as its last event
    /// (re-ticked past the new content), the document never drops to zero
    /// tracks, and the declared SMF format stays consistent with the
    /// track count — callers must use `Op::SetFormat` to convert.
    ///
    /// Normalization is expressed as synthesized ops appended to the
    /// returned `Applied::tx` — reverting THAT transaction restores the
    /// exact pre-edit state (a minted EOT disappears again), and replaying
    /// it reaches the same normalized state deterministically.
    pub fn apply(&mut self, tx: Transaction) -> Result<Applied, ApplyError> {
        if tx.base != self.revision {
            return Err(ApplyError::StaleRevision {
                expected: self.revision,
                got: tx.base,
            });
        }
        let mut format = self.format;
        let mut tracks = self.tracks.clone();
        let mut touched = std::collections::BTreeSet::new();
        let mut resolved: Vec<Vec<usize>> = Vec::with_capacity(tx.ops.len());
        for op in &tx.ops {
            resolved.push(apply_op(&mut format, &mut tracks, op, &mut touched)?);
        }
        if tracks.is_empty() {
            return Err(ApplyError::EmptyDocument);
        }
        if format == 0 && tracks.len() != 1 {
            return Err(ApplyError::FormatTrackMismatch {
                format,
                tracks: tracks.len(),
            });
        }
        // synthesize first (before-images read post-user-op state), then
        // apply — the synthesized ops carry their own before-images, so
        // undo of the effective transaction restores the original bytes.
        let mut synth = Vec::new();
        for &ti in &touched {
            synth.extend(eot_normalize_ops(ti, &tracks[ti], &mut self.next_event_id));
        }
        let mut extra = std::collections::BTreeSet::new();
        let mut synth_res: Vec<Vec<usize>> = Vec::with_capacity(synth.len());
        for op in &synth {
            synth_res.push(apply_op(&mut format, &mut tracks, op, &mut extra)?);
        }
        self.format = format;
        self.tracks = tracks;
        self.revision += 1;
        self.rebuild_index();
        self.rebuild_maps();
        let mut ops = tx.ops;
        resolved.extend(synth_res);
        ops.extend(synth);
        // write apply-time positions into the effective transaction so
        // undo restores exact original slots: UpdateEvent `pos` holds
        // `before`'s index, RemoveEvents entries hold each removed event's
        // index at removal time
        for (op, r) in ops.iter_mut().zip(resolved) {
            match op {
                Op::UpdateEvent { pos, .. } => {
                    if let Some(&p) = r.first() {
                        *pos = p;
                    }
                }
                Op::RemoveEvents { removed, .. } => {
                    for ((i, _), p) in removed.iter_mut().zip(r) {
                        *i = p;
                    }
                }
                _ => {}
            }
        }
        Ok(Applied {
            revision: self.revision,
            tx: Transaction {
                label: tx.label,
                base: tx.base,
                ops,
            },
        })
    }

    /// restore the `before` images of a transaction (undo). Pass the
    /// *effective* transaction from `apply` — its synthesized trailing ops
    /// are what restore the pre-normalization bytes exactly.
    pub fn revert(&mut self, tx: &Transaction) {
        let mut touched = std::collections::BTreeSet::new();
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
                        // restore each event at its resolved index — a
                        // key-based insert would land at an arbitrary slot
                        // among same-(tick,seq) events. usize::MAX marks
                        // an event apply() never removed — don't insert it
                        let mut reinsert: Vec<(usize, &Event)> = removed
                            .iter()
                            .filter(|(i, _)| *i != usize::MAX)
                            .map(|(i, e)| (*i, e))
                            .collect();
                        reinsert.sort_by_key(|(i, _)| *i);
                        for (i, e) in reinsert {
                            t.events.insert(i.min(t.events.len()), e.clone());
                        }
                    }
                }
                Op::UpdateEvent {
                    track, pos, before, ..
                } => {
                    if let Some(t) = self.tracks.get_mut(*track) {
                        if let Some(i) = t.events.iter().position(|x| x.id == before.id) {
                            t.events.remove(i);
                            // `pos` is where `before` sat before the op —
                            // inserting there restores the exact original
                            // order among same-key events
                            let at = if *pos == usize::MAX {
                                t.events
                                    .binary_search_by_key(&(before.tick, before.seq), |x| {
                                        (x.tick, x.seq)
                                    })
                                    .unwrap_or_else(|p| p)
                            } else {
                                (*pos).min(t.events.len())
                            };
                            t.events.insert(at, before.clone());
                        }
                    }
                }
                Op::InsertTrack { index, .. } => {
                    // mirror apply's `min(len)` clamp: the track landed at the
                    // clamped position, so remove it from there
                    if !self.tracks.is_empty() {
                        self.tracks.remove((*index).min(self.tracks.len() - 1));
                    }
                }
                Op::RemoveTrack { index, track } => {
                    self.tracks
                        .insert((*index).min(self.tracks.len()), track.clone());
                }
                Op::UpdateTrack { index, before, .. } => {
                    if let Some(t) = self.tracks.get_mut(*index) {
                        *t = before.clone();
                    }
                }
                Op::SetFormat { before, .. } => {
                    self.format = *before;
                }
            }
            let ti = match op {
                Op::InsertEvents { track, .. }
                | Op::RemoveEvents { track, .. }
                | Op::UpdateEvent { track, .. } => Some(*track),
                Op::InsertTrack { index, .. }
                | Op::RemoveTrack { index, .. }
                | Op::UpdateTrack { index, .. } => Some(*index),
                Op::SetFormat { .. } => None,
            };
            if let Some(ti) = ti {
                refresh_track_meta(&mut self.tracks, ti);
                touched.insert(ti);
            }
        }
        self.revision += 1;
        self.rebuild_index();
        self.rebuild_maps();
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
            let mut first_eot: Option<(usize, &Event)> = None;
            for (ei, e) in t.events.iter().enumerate() {
                if matches!(
                    e.kind,
                    EventKind::Meta {
                        meta_type: 0x2F,
                        ..
                    }
                ) {
                    if eot {
                        out.push(Diagnostic {
                            code: "duplicate-eot",
                            track: ti,
                            tick: e.tick,
                            event: Some(e.id),
                            detail: "track carries more than one End-of-Track".into(),
                        });
                    } else {
                        first_eot = Some((ei, e));
                    }
                    eot = true;
                }
                // tempo maps outside track 0 (format-1 files): legal but
                // most players ignore them — worth flagging
                if self.format == 1
                    && ti != 0
                    && matches!(
                        e.kind,
                        EventKind::Meta {
                            meta_type: 0x51,
                            ..
                        }
                    )
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
            // content past the first EOT is dead space to most players —
            // the terminator is expected to be the last event
            if let Some((ei, eot_e)) = first_eot {
                if let Some(e) = t.events.get(ei + 1) {
                    out.push(Diagnostic {
                        code: "events-after-eot",
                        track: ti,
                        tick: e.tick,
                        event: Some(eot_e.id),
                        detail:
                            "events sit after the End-of-Track marker and may be dropped by players"
                                .into(),
                    });
                }
            }
        }
        let (notes, mut pairing_diags) = self.paired_notes();
        out.append(&mut pairing_diags);
        for n in notes {
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

    /// Build the ops that resolve each diagnostic — one undo step for the
    /// whole batch. `codes` filters by diagnostic code; empty = fix all.
    pub fn fix_ops(&mut self, codes: &[&str]) -> Vec<Op> {
        let diags = self.diagnose();
        let notes = self.notes();
        let mut ops = Vec::new();
        for d in diags {
            if !codes.is_empty() && !codes.contains(&d.code) {
                continue;
            }
            match d.code {
                "missing-eot" => {
                    let id = self.alloc_event_id();
                    ops.push(Op::InsertEvents {
                        track: d.track,
                        events: vec![Event {
                            id,
                            tick: d.tick,
                            // after every existing event at the final tick
                            seq: self.next_seq(d.track, d.tick),
                            raw_body: None,
                            kind: EventKind::Meta {
                                meta_type: 0x2F,
                                data: Bytes::new(),
                            },
                        }],
                    });
                }
                "dangling-noteon" | "zero-length-note" => {
                    // remove the on event plus its paired off (zero-len keeps one)
                    let off_id = notes
                        .iter()
                        .find(|n| Some(n.on_id) == d.event)
                        .and_then(|n| n.off_id);
                    for id in [d.event, off_id].into_iter().flatten() {
                        if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                            let e = self.tracks[ti].events[ei].clone();
                            ops.push(Op::RemoveEvents {
                                track: ti,
                                removed: vec![(ei, e)],
                            });
                        }
                    }
                }
                "duplicate-eot" => {
                    // drop the extra terminator; the first stays
                    if let Some(id) = d.event {
                        if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                            let e = self.tracks[ti].events[ei].clone();
                            ops.push(Op::RemoveEvents {
                                track: ti,
                                removed: vec![(ei, e)],
                            });
                        }
                    }
                }
                "events-after-eot" => {
                    // slide the terminator past the track's last content
                    // tick — a silent tail is preserved, orphaned content
                    // becomes reachable again
                    if let Some(id) = d.event {
                        if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                            let before = self.tracks[ti].events[ei].clone();
                            let mut after = before.clone();
                            after.tick = self.tracks[ti]
                                .events
                                .iter()
                                .map(|e| e.tick)
                                .max()
                                .unwrap_or(0);
                            after.seq = u32::MAX;
                            after.raw_body = None;
                            ops.push(Op::UpdateEvent {
                                pos: usize::MAX,
                                track: ti,
                                before,
                                after,
                            });
                        }
                    }
                }
                "tempo-outside-conductor" => {
                    // move the tempo event into track 0 (same tick)
                    if let Some(id) = d.event {
                        if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                            let e = self.tracks[ti].events[ei].clone();
                            ops.push(Op::RemoveEvents {
                                track: ti,
                                removed: vec![(ei, e.clone())],
                            });
                            let mut moved = e;
                            moved.id = self.alloc_event_id();
                            ops.push(Op::InsertEvents {
                                track: 0,
                                events: vec![moved],
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        ops
    }

    /// Bank+program state a (track, channel) reached by `tick`, as wire
    /// bytes in emit order (CC0, CC32, PC). Audition strikes prefix their
    /// note-on with these so the preview sounds on the patch the track
    /// would actually be playing at that point.
    pub fn channel_setup(&self, track: usize, channel: u8, tick: u64) -> Vec<Vec<u8>> {
        let (mut msb, mut lsb, mut prog) = (None, None, None);
        if let Some(t) = self.tracks.get(track) {
            for e in &t.events {
                if e.tick >= tick {
                    break;
                }
                let EventKind::Channel { status, data, .. } = &e.kind else {
                    continue;
                };
                if status & 0x0F != channel & 0x0F {
                    continue;
                }
                match (status & 0xF0, data[0]) {
                    (0xB0, 0) => msb = Some(data[1]),
                    (0xB0, 32) => lsb = Some(data[1]),
                    (0xC0, p) => prog = Some(p),
                    _ => {}
                }
            }
        }
        let ch = channel & 0x0F;
        let mut out = Vec::new();
        if let Some(v) = msb {
            out.push(vec![0xB0 | ch, 0, v]);
        }
        if let Some(v) = lsb {
            out.push(vec![0xB0 | ch, 32, v]);
        }
        if let Some(p) = prog {
            out.push(vec![0xC0 | ch, p]);
        }
        out
    }

    /// Key signature in effect at `at_tick`: the latest meta 0x59 event at
    /// or before it, else the earliest one in the file (a signature written
    /// mid-song is the best hint before it's reached). Returns (sf, minor):
    /// sf is the signed fifths byte (-7..=7), minor the mi flag.
    pub fn key_signature(&self, at_tick: u64) -> Option<(i8, bool)> {
        let mut best: Option<(u64, i8, bool)> = None;
        let mut earliest: Option<(u64, i8, bool)> = None;
        for t in &self.tracks {
            for e in &t.events {
                let EventKind::Meta {
                    meta_type: 0x59,
                    data,
                } = &e.kind
                else {
                    continue;
                };
                if data.len() < 2 {
                    continue;
                }
                let ks = (e.tick, data[0] as i8, data[1] != 0);
                if e.tick <= at_tick {
                    if best.is_none_or(|(t, ..)| e.tick >= t) {
                        best = Some(ks);
                    }
                } else if earliest.is_none_or(|(t, ..)| e.tick < t) {
                    earliest = Some(ks);
                }
            }
        }
        best.or(earliest).map(|(_, sf, minor)| (sf, minor))
    }

    /// `(absolute µs, source track index, raw channel message)` sorted by time.
    /// The track tag lets playback fan events out to per-track destinations.
    pub fn timeline_tagged(&self) -> Vec<(u64, usize, Vec<u8>)> {
        let mut out = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            // format 2: this sequence's own tempo map, not the merged map
            let map = self.map_scope(ti);
            for e in &t.events {
                if let EventKind::Channel { status, data, len } = &e.kind {
                    let mut b = Vec::with_capacity(3);
                    b.push(*status);
                    b.push(data[0]);
                    if *len == 2 {
                        b.push(data[1]);
                    }
                    out.push((map.tick_to_us(e.tick), ti, b));
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

    /// Complete SysEx messages in wire form (`F0 … F7`) as
    /// `(µs, track, bytes)`, timestamped at the FIRST fragment. SMF allows a
    /// message to be split: an `F0` packet followed by further `F7` escape
    /// packets, with only the last one ending in `F7`; other events may be
    /// interleaved between the fragments. A message still open when a new
    /// `F0` starts (or at end of track) is closed by appending `F7`.
    /// Standalone `F7` escapes are arbitrary non-MIDI bytes and never start
    /// or join a message — they are never emitted.
    pub fn timeline_sysex(&self) -> Vec<(u64, usize, Vec<u8>)> {
        self.sysex_wire()
            .into_iter()
            .map(|(us, ti, b, _)| (us, ti, b))
            .collect()
    }

    /// The last SysEx message per track that the file itself terminated
    /// before `start_us`, retimed to `start_us`. This is the opt-in half of
    /// the chase: a chased GM/GS/XG reset would wipe the channel state
    /// `chase_events` just restored, so the caller gates it behind a
    /// preference (as Logic/Cubase do). Messages we closed by appending `F7`
    /// (abandoned by a new `F0` or end of track) are skipped — re-sending a
    /// truncated dump would only confuse the device.
    pub fn chase_sysex(&self, start_us: u64) -> Vec<(u64, usize, Vec<u8>)> {
        let mut last: HashMap<usize, Vec<u8>> = HashMap::new();
        for (us, ti, b, terminated) in self.sysex_wire() {
            if terminated && us < start_us {
                last.insert(ti, b);
            }
        }
        let mut tis: Vec<usize> = last.keys().copied().collect();
        tis.sort_unstable();
        tis.into_iter()
            .map(|ti| (start_us, ti, last.remove(&ti).unwrap()))
            .collect()
    }

    /// `(µs, track, wire bytes, terminated-in-file)` — the shared joining
    /// walk behind `timeline_sysex` and `chase_sysex`.
    fn sysex_wire(&self) -> Vec<(u64, usize, Vec<u8>, bool)> {
        let mut out = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            let map = self.map_scope(ti);
            let mut open: Option<(u64, Vec<u8>)> = None;
            for e in &t.events {
                match &e.kind {
                    EventKind::SysEx(p) => {
                        if let Some((us, mut b)) = open.take() {
                            b.push(0xF7);
                            out.push((us, ti, b, false));
                        }
                        let us = map.tick_to_us(e.tick);
                        let mut b = Vec::with_capacity(p.len() + 2);
                        b.push(0xF0);
                        b.extend_from_slice(p);
                        if p.last() == Some(&0xF7) {
                            out.push((us, ti, b, true));
                        } else {
                            open = Some((us, b));
                        }
                    }
                    EventKind::Escape(p) if open.is_some() => {
                        let completes = p.last() == Some(&0xF7);
                        open.as_mut().unwrap().1.extend_from_slice(p);
                        if completes {
                            let (us, b) = open.take().unwrap();
                            out.push((us, ti, b, true));
                        }
                    }
                    _ => {}
                }
            }
            if let Some((us, mut b)) = open {
                b.push(0xF7);
                out.push((us, ti, b, false));
            }
        }
        out.sort_by_key(|(us, _, _, _)| *us);
        out
    }

    /// Chase events for starting playback at `start_us`: everything a synth
    /// needs to be in the state it would have reached by playing the timeline
    /// from the beginning. All output carries `us = start_us`; the caller
    /// inserts them at the schedule's seek point so they fire just before the
    /// first real event at/after the position (loop wraps re-send them the
    /// same way). Per (track, channel):
    ///
    /// - bank MSB/LSB, then program change (bank must precede PC)
    /// - last value of each CC 0-119 except the channel-mode range; CC64
    ///   (sustain) lands before the note restrikes below
    /// - the last RPN/NRPN selector followed by data entry (CC 6/38) — data
    ///   before the selector would hit the synth's stale cursor
    /// - pitch bend, channel aftertouch, poly aftertouch
    /// - note-ons of notes still sounding at `start_us` (their real note-offs
    ///   are already in the future part of the timeline — never re-sent)
    /// - notes released earlier but caught by a held pedal: re-struck as
    ///   note-on + note-off pairs AFTER CC64 so the pedal catches them
    ///
    /// Channel-mode messages in the prefix model what the receiver did:
    /// CC120 drops all sounding notes, CC123/124-127 act as note-offs (a
    /// held pedal still catches), CC121 clears controller state
    /// (bank/program survive per RP-015). SysEx is not part of this channel
    /// chase — see `chase_sysex` for the opt-in message chase.
    pub fn chase_events(&self, start_us: u64) -> Vec<(u64, usize, Vec<u8>)> {
        let mut out = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            let map = self.map_scope(ti);
            let mut chans: HashMap<u8, ChaseState> = HashMap::new();
            for e in &t.events {
                if map.tick_to_us(e.tick) >= start_us {
                    break; // events sorted by (tick, seq); tick_to_us is monotonic
                }
                let EventKind::Channel { status, data, .. } = &e.kind else {
                    continue;
                };
                let ch = status & 0x0F;
                let st = chans.entry(ch).or_default();
                match (status & 0xF0, data[0], data[1]) {
                    // corrupt input can carry data bytes with the top bit
                    // set; those can't index the 128-entry key tables
                    (0x90, key, v) if v > 0 && key < 0x80 => st.pending[key as usize].push(v),
                    (0x80, key, _) | (0x90, key, _) if key < 0x80 => {
                        // LIFO pairing, same as the notes() view
                        if let Some(vel) = st.pending[key as usize].pop() {
                            if st.pedal_down {
                                st.sustained.push((key, vel));
                            }
                        }
                    }
                    (0xB0, ctl, v) => match ctl {
                        0 => st.bank_msb = Some(v),
                        32 => st.bank_lsb = Some(v),
                        6 | 38 => {
                            // data entry lands in the active parameter's
                            // slot — every tuned RPN/NRPN is kept, not just
                            // the last-selected one (#215)
                            if let Some(k) = st.active_param() {
                                let e = st.params.entry(k).or_default();
                                if ctl == 6 {
                                    e.0 = Some(v);
                                } else {
                                    e.1 = Some(v);
                                }
                            }
                        }
                        98..=101 => {
                            st.sel_vals[(ctl - 98) as usize] = Some(v);
                            st.sel_nrpn = ctl <= 99;
                        }
                        64 => {
                            st.pedal_down = v >= 64;
                            st.cc[64] = Some(v);
                            if !st.pedal_down {
                                st.sustained.clear();
                            }
                        }
                        120 => {
                            st.pending.iter_mut().for_each(|s| s.clear());
                            st.sustained.clear();
                        }
                        121 => {
                            // RP-015: controllers to default; bank/program survive
                            st.cc = std::array::from_fn(|_| None);
                            st.params.clear();
                            st.sel_vals = std::array::from_fn(|_| None);
                            st.sel_nrpn = false;
                            st.bend = None;
                            st.pressure = None;
                            st.poly = std::array::from_fn(|_| None);
                            st.pedal_down = false;
                            st.sustained.clear();
                        }
                        123..=127 => {
                            // all-notes-off semantics; hold pedal still catches
                            if st.pedal_down {
                                for (key, stack) in st.pending.iter_mut().enumerate() {
                                    for vel in stack.drain(..) {
                                        st.sustained.push((key as u8, vel));
                                    }
                                }
                            } else {
                                st.pending.iter_mut().for_each(|s| s.clear());
                            }
                        }
                        _ if ctl < 120 => st.cc[ctl as usize] = Some(v),
                        _ => {}
                    },
                    (0xC0, prog, _) => st.prog = Some(prog),
                    (0xD0, press, _) => st.pressure = Some(press),
                    (0xE0, lsb, msb) => st.bend = Some([lsb, msb]),
                    (0xA0, key, v) if key < 0x80 => st.poly[key as usize] = Some(v),
                    _ => {}
                }
            }
            // emit per channel, ascending, in the documented order
            let mut ch_list: Vec<u8> = chans.keys().copied().collect();
            ch_list.sort_unstable();
            macro_rules! push {
                ($bytes:expr) => {
                    out.push((start_us, ti, $bytes.to_vec()))
                };
            }
            for ch in ch_list {
                let st = &chans[&ch];
                if let Some(v) = st.bank_msb {
                    push!([0xB0 | ch, 0, v]);
                }
                if let Some(v) = st.bank_lsb {
                    push!([0xB0 | ch, 32, v]);
                }
                if let Some(p) = st.prog {
                    push!([0xC0 | ch, p]);
                }
                // bank (0/32) and data entry (6/38) live in dedicated fields,
                // so this only ever holds plain CC 1-119 (incl. 64 sustain)
                for (ctl, v) in st.cc.iter().enumerate() {
                    if let Some(v) = v {
                        push!([0xB0 | ch, ctl as u8, *v]);
                    }
                }
                // RPN/NRPN: chase every tuned parameter, then null the
                // selector so later data-entry on the synth can't keep
                // writing into the last one (#215)
                for (&(nrpn, m, l), &(dmsb, dlsb)) in &st.params {
                    let (msb_cc, lsb_cc) = if nrpn { (99u8, 98u8) } else { (101, 100) };
                    push!([0xB0 | ch, msb_cc, m]);
                    push!([0xB0 | ch, lsb_cc, l]);
                    if let Some(v) = dmsb {
                        push!([0xB0 | ch, 6, v]);
                    }
                    if let Some(v) = dlsb {
                        push!([0xB0 | ch, 38, v]);
                    }
                }
                if !st.params.is_empty() {
                    // RPN Null (127/127) deactivates the parameter
                    push!([0xB0 | ch, 101, 127]);
                    push!([0xB0 | ch, 100, 127]);
                }
                match st.bend {
                    Some([lsb, msb]) => push!([0xE0 | ch, lsb, msb]),
                    // no bend in the prefix → re-center: a bend left from an
                    // earlier pass must not detune the seek target (#215)
                    None => push!([0xE0 | ch, 0, 64]),
                }
                if let Some(v) = st.pressure {
                    push!([0xD0 | ch, v]);
                }
                for (key, v) in st.poly.iter().enumerate() {
                    if let Some(v) = v {
                        push!([0xA0 | ch, key as u8, *v]);
                    }
                }
                for (key, stack) in st.pending.iter().enumerate() {
                    for vel in stack {
                        push!([0x90 | ch, key as u8, *vel]);
                    }
                }
                for &(key, vel) in &st.sustained {
                    push!([0x90 | ch, key, vel]);
                    push!([0x80 | ch, key, 0]);
                }
            }
        }
        out
    }

    /// How the UI should present this document's timing (bar/beat grid
    /// vs SMPTE timecode) — straight from the division, never a fake PPQ.
    pub fn time_display(&self) -> TimeDisplay {
        TimeDisplay::of(self.division)
    }

    /// Last event tick in one track — a sequence's own duration. Matters
    /// for format 2, where each track is an independent sequence rather
    /// than a lane of one shared timeline.
    pub fn track_end_tick(&self, track: usize) -> u64 {
        self.tracks
            .get(track)
            .and_then(|t| t.events.iter().map(|e| e.tick).max())
            .unwrap_or(0)
    }

    /// Whether this file declares format 2 (independent sequences).
    pub fn is_sequential(&self) -> bool {
        self.format == 2
    }

    /// The tempo map a track's ticks convert through. Format-2 tracks are
    /// independent sequences — each is timed by its own tempo events, so
    /// this builds a map from that track alone. Format 0/1 tracks share
    /// the conductor timeline and get the document-wide map.
    pub fn tempo_map_for(&self, track: usize) -> TempoMap {
        match self.map_scope(track) {
            std::borrow::Cow::Borrowed(m) => m.clone(),
            std::borrow::Cow::Owned(m) => m,
        }
    }

    fn map_scope(&self, track: usize) -> std::borrow::Cow<'_, TempoMap> {
        if self.is_sequential() && track < self.tracks.len() {
            std::borrow::Cow::Owned(TempoMap::build(
                std::slice::from_ref(&self.tracks[track]),
                self.division,
            ))
        } else {
            std::borrow::Cow::Borrowed(&self.tempo_map)
        }
    }

    /// The meter map a track's bars/beats count by. Format-2 tracks are
    /// independent sequences — each counts its own signatures, so this
    /// builds a map from that track alone (same rule as `tempo_map_for`).
    /// Format 0/1 tracks share the conductor map.
    pub fn meter_map_for(&self, track: usize) -> MeterMap {
        if self.is_sequential() && track < self.tracks.len() {
            MeterMap::build(std::slice::from_ref(&self.tracks[track]), self.division)
        } else {
            self.meter_map.clone()
        }
    }

    /// Owned position formatter for `track`'s timeline — metrical
    /// `bar.beat.tick` under that track's real `FF 58` map (format 2: the
    /// sequence's own), SMPTE timecode otherwise. UI snapshot paths
    /// (a11y subtrees, canvas closures) keep the value; immediate reads
    /// can use `format_position_for`.
    pub fn position_format_for(&self, track: usize) -> PositionFormat {
        match self.time_display() {
            TimeDisplay::Smpte { .. } => PositionFormat::Smpte(self.time_display()),
            TimeDisplay::Metrical { .. } => PositionFormat::Bbt(self.meter_map_for(track)),
        }
    }

    /// `tick` as UI position text — see `position_format_for`.
    pub fn format_position_for(&self, track: usize, tick: u64) -> String {
        self.position_format_for(track).fmt(tick)
    }

    pub fn serialize(&self, opts: smf_core::WriteOptions) -> Vec<u8> {
        self.snapshot().serialize(opts)
    }

    /// Clone just the data a save serializes — tracks without the by_id
    /// index or tempo_map. Edits applied after the snapshot bump the live
    /// document's revision, so they can never be marked saved by the write
    /// that serializes this snapshot.
    pub fn snapshot(&self) -> DocSnapshot {
        DocSnapshot {
            format: self.format,
            division: self.division,
            tracks: self.tracks.clone(),
        }
    }
}

/// Owned snapshot of a `Document`'s serializable state. Cheap to take —
/// `Bytes` payloads are refcounted, so cloning tracks is a contiguous
/// copy of event records, not the encode+write `serialize` performs.
#[derive(Debug)]
pub struct DocSnapshot {
    format: u16,
    division: Division,
    tracks: Vec<Track>,
}

impl DocSnapshot {
    /// events across all tracks — e.g. for gating save-progress UI
    pub fn event_count(&self) -> usize {
        self.tracks.iter().map(|t| t.events.len()).sum()
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

/// Apply one op to a working (format, tracks) pair. Fails only on
/// `UnknownTrack` before any partial state is visible —
/// `Document::apply` commits only when every op of the transaction
/// succeeded. Track indexes the op mutates are collected into
/// `touched` for the post-commit EOT normalization.
fn apply_op(
    format: &mut u16,
    tracks: &mut Vec<Track>,
    op: &Op,
    touched: &mut std::collections::BTreeSet<usize>,
) -> Result<Vec<usize>, ApplyError> {
    // resolved apply-time positions written into the effective transaction
    // for position-exact undo: UpdateEvent -> [before's index],
    // RemoveEvents -> each removed event's index at removal time
    // (usize::MAX = the event wasn't found)
    let mut resolved = Vec::new();
    match op {
        Op::InsertEvents { track, events } => {
            let t = tracks
                .get_mut(*track)
                .ok_or(ApplyError::UnknownTrack(*track))?;
            for e in events {
                // insert after any same-(tick, seq) run: an event sharing
                // another's exact key (e.g. a NoteOff on its NoteOn's tick)
                // must follow it, never precede it (#187). The End-of-Track
                // is exempt — it must stay last even under a key tie (EOTs
                // carry seq = u32::MAX, which next_seq can saturate to)
                let mut pos = t
                    .events
                    .binary_search_by_key(&(e.tick, e.seq), |x| (x.tick, x.seq))
                    .unwrap_or_else(|p| p);
                while pos < t.events.len()
                    && (t.events[pos].tick, t.events[pos].seq) == (e.tick, e.seq)
                    && !matches!(
                        t.events[pos].kind,
                        EventKind::Meta {
                            meta_type: 0x2F,
                            ..
                        }
                    )
                {
                    pos += 1;
                }
                t.events.insert(pos, e.clone());
            }
        }
        Op::RemoveEvents { track, removed } => {
            let t = tracks
                .get_mut(*track)
                .ok_or(ApplyError::UnknownTrack(*track))?;
            // resolve every target's position BEFORE removing any — the
            // recorded index is the slot the event occupied when this op
            // applied, which is exactly where undo must put it back
            resolved = removed
                .iter()
                .map(|(_, e)| {
                    t.events
                        .iter()
                        .position(|x| x.id == e.id)
                        .unwrap_or(usize::MAX)
                })
                .collect();
            for (_, e) in removed {
                if let Some(pos) = t.events.iter().position(|x| x.id == e.id) {
                    t.events.remove(pos);
                }
            }
        }
        Op::UpdateEvent { track, after, .. } => {
            let t = tracks
                .get_mut(*track)
                .ok_or(ApplyError::UnknownTrack(*track))?;
            if let Some(pos) = t.events.iter().position(|x| x.id == after.id) {
                let mut after = after.clone();
                // a modified kind must be re-encoded on save; a stale raw_body
                // would silently revert the edit at serialize time
                if after.kind != t.events[pos].kind {
                    after.raw_body = None;
                }
                t.events[pos] = after;
                t.events.sort_by_key(|e| (e.tick, e.seq));
                resolved = vec![pos];
            }
        }
        Op::InsertTrack { index, track } => {
            tracks.insert((*index).min(tracks.len()), track.clone());
        }
        Op::RemoveTrack { index, .. } => {
            if *index >= tracks.len() {
                return Err(ApplyError::UnknownTrack(*index));
            }
            tracks.remove(*index);
        }
        Op::UpdateTrack { index, after, .. } => {
            if let Some(t) = tracks.get_mut(*index) {
                *t = after.clone();
            }
        }
        Op::SetFormat { after, .. } => {
            *format = *after;
        }
    }
    // keep the track's cached conventional metas honest
    let ti = match op {
        Op::InsertEvents { track, .. }
        | Op::RemoveEvents { track, .. }
        | Op::UpdateEvent { track, .. } => Some(*track),
        Op::InsertTrack { index, .. }
        | Op::RemoveTrack { index, .. }
        | Op::UpdateTrack { index, .. } => Some(*index),
        Op::SetFormat { .. } => None,
    };
    if let Some(ti) = ti {
        if ti < tracks.len() {
            refresh_track_meta(tracks, ti);
            touched.insert(ti);
        }
    }
    Ok(resolved)
}

/// Ops that restore a touched track's structural terminator: exactly one
/// End-of-Track, sitting last. A lone EOT keeps its stored tick unless
/// content was appended past it (it may intentionally pad a silent tail);
/// duplicates collapse to the latest (by tick,seq) terminator, which is
/// the one whose raw bytes survive a save; a track left without one gets
/// a fresh event minted at the last content tick.
///
/// The ops carry before-images, so they are appended to the effective
/// transaction and undo restores the pre-normalization state exactly.
fn eot_normalize_ops(ti: usize, t: &Track, next_id: &mut EventId) -> Vec<Op> {
    let is_eot = |e: &Event| {
        matches!(
            e.kind,
            EventKind::Meta {
                meta_type: 0x2F,
                ..
            }
        )
    };
    let last_content = t.events.iter().filter(|e| !is_eot(e)).map(|e| e.tick).max();
    let eot_pos: Vec<usize> = t
        .events
        .iter()
        .enumerate()
        .filter(|(_, e)| is_eot(e))
        .map(|(i, _)| i)
        .collect();
    match (eot_pos.len(), last_content) {
        (0, None) => vec![], // empty track: nothing to terminate
        (0, Some(last)) => {
            let id = *next_id;
            *next_id += 1;
            vec![Op::InsertEvents {
                track: ti,
                events: vec![Event {
                    id,
                    tick: last,
                    seq: u32::MAX,
                    raw_body: None,
                    kind: EventKind::Meta {
                        meta_type: 0x2F,
                        data: Bytes::new(),
                    },
                }],
            }]
        }
        (1, None) => vec![], // lone EOT keeps its tail tick
        (_, _) => {
            // the terminator to keep is the LAST one in (tick, seq)
            // order — for an in-order track that is the trailing meta
            let keep = *eot_pos.iter().max().unwrap();
            let mut ops = Vec::new();
            let removed: Vec<(usize, Event)> = eot_pos
                .iter()
                .filter(|&&i| i != keep)
                .map(|&i| (i, t.events[i].clone()))
                .collect();
            if !removed.is_empty() {
                ops.push(Op::RemoveEvents { track: ti, removed });
            }
            let before = t.events[keep].clone();
            let target = before.tick.max(last_content.unwrap_or(0));
            if before.tick != target || before.seq != u32::MAX {
                let after = Event {
                    tick: target,
                    seq: u32::MAX,
                    raw_body: None,
                    ..before.clone()
                };
                ops.push(Op::UpdateEvent {
                    pos: usize::MAX,
                    track: ti,
                    before,
                    after,
                });
            }
            ops
        }
    }
}

/// Rescan the first name/out-port/out-channel metas after an edit so the
/// `Track` cache fields stay correct without callers doing it.
fn refresh_track_meta(tracks: &mut [Track], ti: usize) {
    if let Some(t) = tracks.get_mut(ti) {
        t.name = None;
        t.out_port = 0;
        t.out_channel = 0;
        for e in &t.events {
            if let EventKind::Meta { meta_type, data } = &e.kind {
                match *meta_type {
                    0x03 if t.name.is_none() => t.name = Some(data.clone()),
                    0x21 if !data.is_empty() => t.out_port = data[0],
                    0x20 if !data.is_empty() => t.out_channel = data[0],
                    _ => {}
                }
            }
        }
    }
}

/// Tuned RPN/NRPN parameter values: (nrpn?, selector msb, selector lsb)
/// → (data msb, data lsb). One entry per selector so chasing restores
/// every tuned parameter, not just the last one selected (#215).
type ParamTuning = std::collections::BTreeMap<(bool, u8, u8), (Option<u8>, Option<u8>)>;

/// Per-(track, channel) state reconstructed by `Document::chase_events` while
/// scanning the prefix before the play position. `None` = never set (or reset
/// by CC121) → nothing is emitted for that slot.
#[derive(Debug)]
struct ChaseState {
    /// plain CC 1-119 values (bank 0/32 and data entry 6/38 live separately)
    cc: [Option<u8>; 128],
    bank_msb: Option<u8>,
    bank_lsb: Option<u8>,
    prog: Option<u8>,
    /// RPN/NRPN parameter values: (nrpn?, msb, lsb) → (data msb, data lsb).
    /// One map per selector — chasing must restore EVERY tuned parameter,
    /// not just the one whose selectors happened to come last (#215).
    params: ParamTuning,
    /// the parameter data entry currently writes into, and the selector
    /// nibbles seen so far (CC98-101 by `cc - 98`); the active group is
    /// whichever of RPN/NRPN wrote last
    sel_vals: [Option<u8>; 4],
    sel_nrpn: bool,
    bend: Option<[u8; 2]>,
    pressure: Option<u8>,
    poly: [Option<u8>; 128],
    pedal_down: bool,
    /// note-ons still held (key → stack of velocities, LIFO pairing)
    pending: [Vec<u8>; 128],
    /// notes already note-off'd but still ringing under the pedal
    sustained: Vec<(u8, u8)>,
}

impl ChaseState {
    /// The fully-specified parameter data entry writes into, if any.
    fn active_param(&self) -> Option<(bool, u8, u8)> {
        let (msb, lsb) = if self.sel_nrpn {
            (self.sel_vals[1], self.sel_vals[0])
        } else {
            (self.sel_vals[3], self.sel_vals[2])
        };
        match (msb, lsb) {
            (Some(m), Some(l)) => Some((self.sel_nrpn, m, l)),
            _ => None,
        }
    }
}

impl Default for ChaseState {
    fn default() -> Self {
        Self {
            cc: std::array::from_fn(|_| None),
            poly: std::array::from_fn(|_| None),
            pending: std::array::from_fn(|_| Vec::new()),
            sel_vals: std::array::from_fn(|_| None),
            params: ParamTuning::new(),
            bank_msb: None,
            bank_lsb: None,
            prog: None,
            sel_nrpn: false,
            bend: None,
            pressure: None,
            pedal_down: false,
            sustained: Vec::new(),
        }
    }
}

impl Document {
    /// Derived view over the raw event truth — O(events). Call after edits.
    ///
    /// Pairing policy — deterministic, channel-aware, **LIFO**: every
    /// (channel, key) lane keeps a stack of pending note-ons. A `0x90` with
    /// vel>0 pushes; a `0x80` or `0x90`-vel0 pops the NEWEST pending on for
    /// that lane. Overlapping ons for the same key pair their offs
    /// innermost-first (the conventional reading of stacked retriggers);
    /// ons on different channels or different keys never interact. A
    /// retrigger arriving while a previous on is still held is surfaced as
    /// an `overlapping-noteon` diagnostic instead of being silently
    /// interpreted. Leftover ons become `Note`s with `end_tick: None` (the
    /// `dangling-noteon` diagnostic).
    ///
    /// The pairing is a pure derived view — no events are inserted,
    /// removed, or merged — and edits always address the paired
    /// `on_id`/`off_id` EventIds, never key/time heuristics.
    pub fn notes(&self) -> Vec<Note> {
        self.paired_notes().0
    }

    /// `notes()` plus the pairing diagnostics discovered on the same pass
    /// (`overlapping-noteon`) — the two never disagree about which on
    /// paired which off.
    fn paired_notes(&self) -> (Vec<Note>, Vec<Diagnostic>) {
        let mut out = Vec::new();
        let mut diags = Vec::new();
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
                    (0x90, v) if v > 0 && key < 0x80 => {
                        // an on arriving while a previous on for this lane
                        // is still held is ambiguous — flag it, then keep
                        // stacking (LIFO pairing is the documented answer)
                        if !pending[ch][key].is_empty() {
                            diags.push(Diagnostic {
                                code: "overlapping-noteon",
                                track: ti,
                                tick: e.tick,
                                event: Some(e.id),
                                detail: format!(
                                    "noteOn ch{} key{} retriggered while previous on still held — paired LIFO (newest on takes the next off)",
                                    ch + 1,
                                    key
                                ),
                            });
                        }
                        pending[ch][key].push(on_events.len());
                        on_events.push((e.tick, v, e.id));
                    }
                    (0x80, off_vel) | (0x90, off_vel) if key < 0x80 => {
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
                                off_vel,
                                off_via_on: msg == 0x90,
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
                            off_vel: 0,
                            off_via_on: false,
                        });
                    }
                }
            }
        }
        out.sort_by_key(|n| (n.start_tick, n.key));
        (out, diags)
    }
}

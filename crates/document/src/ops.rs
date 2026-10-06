//! Semantic region transforms — Op builders shared by GUI and MCP.
//!
//! Each method builds the low-level `Op`s for one undoable Transaction;
//! they take `&mut self` only to mint event ids. The real mutation happens
//! in `Document::apply` (see apply.rs), so both frontends see identical
//! semantics and a single undo history.

use super::*;

/// Semantic region transforms. Each method builds the low-level `Op`s for
/// one undoable Transaction — shared by the GUI and the MCP tool surface so
/// both see identical semantics. All take `&mut self` only to mint event ids.
impl Document {
    pub(crate) fn chan_event(status_nibble: u8, channel: u8, d0: u8, d1: u8) -> EventKind {
        EventKind::Channel {
            status: (status_nibble & 0xF0) | (channel & 0x0F),
            data: [d0, d1],
            len: 2,
        }
    }

    /// seq after every existing event at `tick` in `track`
    pub(crate) fn next_seq(&self, track: usize, tick: u64) -> u32 {
        self.tracks
            .get(track)
            .map(|t| {
                t.events
                    .iter()
                    .filter(|e| e.tick == tick)
                    .map(|e| e.seq)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
            .saturating_add(1)
    }

    /// Quantize note starts inside [from,to) to `grid` ticks, with grid
    /// lines laid from each bar's downbeat per the document's meter map
    /// (so meter changes and pickup bars keep their own boundaries).
    /// `strength` 0..=100 interpolates between the original and the grid point;
    /// the note's duration is preserved (on+off shift together).
    pub fn quantize_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        grid: u64,
        strength: u32,
    ) -> Vec<Op> {
        // i128 intermediates: u64-range ticks and grids never overflow
        let grid = grid.max(1) as i128;
        let str_f = strength.min(100) as f64 / 100.0;
        // grid lines are laid from each bar's downbeat (#217): absolute
        // tick-0 math misses measure boundaries once a meter change or
        // pickup bar shifts the bar lines off the global grid
        let mm = self.meter_map_for(track);
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let start = n.start_tick as i128;
            let bar = mm.bar_start_tick(n.start_tick) as i128;
            let snapped = bar + (((start - bar) + grid / 2) / grid) * grid;
            let new_start = (start as f64 + (snapped - start) as f64 * str_f).round() as i128;
            let delta = new_start - start;
            if delta == 0 {
                continue;
            }
            let delta = delta.clamp(i64::MIN as i128, i64::MAX as i128) as i64;
            let ids: Vec<EventId> = [Some(n.on_id), n.off_id].into_iter().flatten().collect();
            for id in ids {
                if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                    let mut after = self.tracks[ti].events[ei].clone();
                    after.tick = after.tick.saturating_add_signed(delta);
                    ops.push(Op::UpdateEvent {
                        pos: usize::MAX,
                        track: ti,
                        before: self.tracks[ti].events[ei].clone(),
                        after,
                    });
                }
            }
        }
        ops
    }

    /// Transpose all notes starting inside [from,to) by `semitones`
    /// (clamped to 0..=127; notes that would leave the range are skipped).
    pub fn transpose_ops(&mut self, track: usize, from: u64, to: u64, semitones: i32) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let Some(new_key) = (n.key as i32 + semitones)
                .try_into()
                .ok()
                .filter(|k: &u8| *k <= 127)
            else {
                continue;
            };
            for id in [Some(n.on_id), n.off_id].into_iter().flatten() {
                if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                    let before = self.tracks[ti].events[ei].clone();
                    let mut after = before.clone();
                    if let EventKind::Channel { data, .. } = &mut after.kind {
                        data[0] = new_key;
                    }
                    ops.push(Op::UpdateEvent {
                        pos: usize::MAX,
                        track: ti,
                        before,
                        after,
                    });
                }
            }
        }
        ops
    }

    /// Multiply noteOn velocities inside [from,to) by `factor` (clamped 1..127).
    pub fn scale_velocity_ops(&mut self, track: usize, from: u64, to: u64, factor: f64) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let nv = ((n.vel as f64 * factor).round() as i64).clamp(1, 127) as u8;
            if nv == n.vel {
                continue;
            }
            if let Some((ti, ei)) = self.by_id.get(&n.on_id).copied() {
                let before = self.tracks[ti].events[ei].clone();
                let mut after = before.clone();
                if let EventKind::Channel { data, .. } = &mut after.kind {
                    data[1] = nv;
                }
                ops.push(Op::UpdateEvent {
                    pos: usize::MAX,
                    track: ti,
                    before,
                    after,
                });
            }
        }
        ops
    }

    /// Deterministic pseudo-random jitter for note starts/velocities in
    /// [from,to). `timing` = max |tick shift|, `vel` = max |velocity delta|.
    /// Seeded by event id — same document produces the same take (undo and
    /// MCP diffs stay reproducible).
    pub fn humanize_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        timing: i64,
        vel: i32,
        seed: u64,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            // deterministic per (note, seed): same seed and settings always
            // produce the same deltas
            let mut r = n
                .on_id
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(seed ^ 0xA076_1D64_78BD_642F);
            let mut next = || {
                r ^= r << 13;
                r ^= r >> 7;
                r ^= r << 17;
                r
            };
            let dt = if timing > 0 {
                // i64-wide math: the modulo result can exceed i64::MAX/2
                let t = timing.min(i64::MAX / 2);
                (next() % (t as u64 * 2 + 1)) as i64 - t
            } else {
                0
            };
            let dv = if vel > 0 {
                // u64 span can't overflow (vel <= i32::MAX), but the modulo
                // result does exceed i32::MAX — subtract in i64, then narrow
                ((next() % (vel as u64 * 2 + 1)) as i64 - vel as i64) as i32
            } else {
                0
            };
            if dt == 0 && dv == 0 {
                continue;
            }
            for id in [Some(n.on_id), n.off_id].into_iter().flatten() {
                if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                    let before = self.tracks[ti].events[ei].clone();
                    let mut after = before.clone();
                    after.tick = after.tick.saturating_add_signed(dt);
                    if id == n.on_id && dv != 0 {
                        if let EventKind::Channel { data, .. } = &mut after.kind {
                            data[1] = (data[1] as i32 + dv).clamp(1, 127) as u8;
                        }
                    }
                    ops.push(Op::UpdateEvent {
                        pos: usize::MAX,
                        track: ti,
                        before,
                        after,
                    });
                }
            }
        }
        ops
    }

    /// Extend each note's end to `next.start - gap` ticks before the next
    /// note ON THE SAME KEY AND CHANNEL (per-pitch legato — chord voicings
    /// stay intact). `gap > 0` leaves space, `gap < 0` overlaps into the
    /// next note. Notes already reaching the next start are left alone.
    pub fn legato_ops(&mut self, track: usize, from: u64, to: u64, gap: i64) -> Vec<Op> {
        let mut notes: Vec<Note> = self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
            .collect();
        notes.sort_by_key(|n| (n.key, n.channel, n.start_tick));
        let mut ops = Vec::new();
        for w in notes.windows(2) {
            let (cur, nxt) = (&w[0], &w[1]);
            if cur.key != nxt.key || cur.channel != nxt.channel {
                continue;
            }
            let Some(off_id) = cur.off_id else { continue };
            let cur_end = cur.end_tick.unwrap_or(cur.start_tick);
            if nxt.start_tick <= cur_end {
                continue;
            }
            let new_end = (nxt.start_tick as i64 - gap).max(cur.start_tick as i64 + 1) as u64;
            if let Some((ti, ei)) = self.by_id.get(&off_id).copied() {
                let before = self.tracks[ti].events[ei].clone();
                let mut after = before.clone();
                after.tick = new_end;
                ops.push(Op::UpdateEvent {
                    pos: usize::MAX,
                    track: ti,
                    before,
                    after,
                });
            }
        }
        ops
    }

    /// Swing: notes whose start snaps to an odd `grid` index — counted
    /// from their bar's downbeat, so downbeats are never swung — are
    /// pushed later by `amount`% of one grid cell (0..=100). On and off
    /// move together so durations hold; notes more than a grid cell from
    /// the swung line, or already swung, are left alone.
    pub fn swing_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        grid: u64,
        amount: u32,
    ) -> Vec<Op> {
        let grid = grid.max(1) as i64;
        let shift = (grid * amount.min(100) as i64 / 100).min(grid - 1);
        if shift == 0 {
            return Vec::new();
        }
        // parity counts grid cells from the bar's downbeat (#217): after
        // an odd-length bar the next downbeat sits on an odd absolute
        // index, and tick-0 parity would delay it while leaving the real
        // off-beats on the grid — inverting the groove
        let mm = self.meter_map_for(track);
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let start = n.start_tick as i64;
            let bar = mm.bar_start_tick(n.start_tick) as i64;
            let idx = (start - bar) / grid;
            if idx % 2 == 0 {
                continue;
            }
            let swung = bar + idx * grid + shift;
            let delta = swung - start;
            if delta == 0 || delta.abs() > grid {
                continue;
            }
            for id in [Some(n.on_id), n.off_id].into_iter().flatten() {
                if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                    let before = self.tracks[ti].events[ei].clone();
                    let mut after = before.clone();
                    after.tick = after.tick.saturating_add_signed(delta);
                    ops.push(Op::UpdateEvent {
                        pos: usize::MAX,
                        track: ti,
                        before,
                        after,
                    });
                }
            }
        }
        ops
    }

    /// Split the notes whose `on_id` is in `on_ids` at `at_tick`: the
    /// original keeps its NoteOn and ends at `at`, a fresh NoteOn+NoteOff
    /// pair carries the tail. Only notes strictly spanning `at` split —
    /// boundaries and dangling NoteOns are left alone.
    pub fn split_ids_ops(
        &mut self,
        on_ids: &std::collections::BTreeSet<EventId>,
        at: u64,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self.notes() {
            if !on_ids.contains(&n.on_id) {
                continue;
            }
            let Some(off_id) = n.off_id else { continue };
            let end = n.end_tick.unwrap_or(n.start_tick);
            if n.start_tick >= at || end <= at {
                continue;
            }
            let Some(&(ti, ei)) = self.by_id.get(&n.on_id) else {
                continue;
            };
            let Some(&(oti, oei)) = self.by_id.get(&off_id) else {
                continue;
            };
            let on_before = self.tracks[ti].events[ei].clone();
            let off_before = self.tracks[oti].events[oei].clone();
            let mut off_after = off_before.clone();
            off_after.tick = at;
            ops.push(Op::UpdateEvent {
                pos: usize::MAX,
                track: oti,
                before: off_before.clone(),
                after: off_after,
            });
            let mut new_on = on_before;
            new_on.id = self.alloc_event_id();
            new_on.tick = at;
            // note pairing is a per-(channel,key) LIFO stack: the new on at
            // `at` must sort AFTER the shortened off, or the off would close
            // the new on and make a zero-length note
            new_on.seq = off_before.seq.saturating_add(1);
            let mut new_off = off_before;
            new_off.id = self.alloc_event_id();
            new_off.tick = end;
            ops.push(Op::InsertEvents {
                track: n.track,
                events: vec![new_on, new_off],
            });
        }
        ops
    }

    /// Split every note in `track` starting inside `[from,to)` that spans
    /// `at` — the MCP/range form of [`Self::split_ids_ops`].
    pub fn split_ops(&mut self, track: usize, from: u64, to: u64, at: u64) -> Vec<Op> {
        let ids: std::collections::BTreeSet<EventId> = self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
            .map(|n| n.on_id)
            .collect();
        self.split_ids_ops(&ids, at)
    }

    /// Join runs of same-(key, channel) notes that overlap or touch into one
    /// note. Explicit policy: the earliest NoteOn of each run survives with
    /// its event id; the surviving NoteOff is the run's longest end moved
    /// onto the earliest possible off event; every other event of the run is
    /// removed. Notes are matched by start inside `[from,to)` — the merge
    /// itself is by intervals, so notes never straddle into chains they
    /// only touch at the range boundary.
    pub fn join_ops(&mut self, track: usize, from: u64, to: u64) -> Vec<Op> {
        let mut notes: Vec<Note> = self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
            .collect();
        notes.sort_by_key(|n| (n.key, n.channel, n.start_tick));
        let mut ops = Vec::new();
        let mut i = 0;
        while i < notes.len() {
            let head = notes[i].clone();
            let mut end = head.end_tick.unwrap_or(head.start_tick);
            let mut j = i + 1;
            while j < notes.len()
                && notes[j].key == head.key
                && notes[j].channel == head.channel
                && notes[j].start_tick <= end
            {
                end = end.max(notes[j].end_tick.unwrap_or(notes[j].start_tick));
                j += 1;
            }
            if j > i + 1 {
                // the head's off survives when present, else the last off in
                // the run — a run of dangling NoteOns cannot join
                let keep_off = head
                    .off_id
                    .or_else(|| notes[i + 1..j].iter().rev().find_map(|n| n.off_id));
                if let Some(off_id) = keep_off {
                    if let Some(&(ti, ei)) = self.by_id.get(&off_id) {
                        let before = self.tracks[ti].events[ei].clone();
                        if before.tick != end {
                            let mut after = before.clone();
                            after.tick = end;
                            ops.push(Op::UpdateEvent {
                                pos: usize::MAX,
                                track: ti,
                                before,
                                after,
                            });
                        }
                    }
                }
                let mut removed = Vec::new();
                for n in &notes[i..j] {
                    for id in [Some(n.on_id), n.off_id].into_iter().flatten() {
                        if id == head.on_id || Some(id) == keep_off {
                            continue;
                        }
                        if let Some(&(ti, ei)) = self.by_id.get(&id) {
                            removed.push((ei, self.tracks[ti].events[ei].clone()));
                        }
                    }
                }
                if !removed.is_empty() {
                    ops.push(Op::RemoveEvents {
                        track: head.track,
                        removed,
                    });
                }
            }
            i = j;
        }
        ops
    }

    /// Shorten any note whose end reaches past the next same-(key, channel)
    /// note's start so the two no longer overlap. Only the offending
    /// NoteOff's tick changes — every event keeps its id.
    pub fn fix_overlaps_ops(&mut self, track: usize, from: u64, to: u64) -> Vec<Op> {
        let mut notes: Vec<Note> = self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
            .collect();
        notes.sort_by_key(|n| (n.key, n.channel, n.start_tick));
        let mut ops = Vec::new();
        for w in notes.windows(2) {
            let (cur, nxt) = (&w[0], &w[1]);
            if cur.key != nxt.key || cur.channel != nxt.channel {
                continue;
            }
            let Some(off_id) = cur.off_id else { continue };
            let cur_end = cur.end_tick.unwrap_or(cur.start_tick);
            if cur_end <= nxt.start_tick {
                continue;
            }
            let new_end = nxt.start_tick.max(cur.start_tick + 1);
            let Some(&(ti, ei)) = self.by_id.get(&off_id) else {
                continue;
            };
            let before = self.tracks[ti].events[ei].clone();
            let mut after = before.clone();
            after.tick = new_end;
            // pairing is a per-(channel,key) LIFO stack: the moved off must
            // sort BEFORE the next on at the same tick or it closes the
            // wrong note (a zero-length one) instead of `cur`
            if new_end == nxt.start_tick {
                if let Some(&(nti, nei)) = self.by_id.get(&nxt.on_id) {
                    after.seq = self.tracks[nti].events[nei].seq.saturating_sub(1);
                }
            }
            ops.push(Op::UpdateEvent {
                pos: usize::MAX,
                track: ti,
                before,
                after,
            });
        }
        ops
    }
    /// Set every note in [from,to) to exactly `ticks` long.
    pub fn set_length_ops(&mut self, track: usize, from: u64, to: u64, ticks: u64) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let Some(off_id) = n.off_id else { continue };
            if let Some((ti, ei)) = self.by_id.get(&off_id).copied() {
                let new_end = n.start_tick.saturating_add(ticks.max(1));
                let before = self.tracks[ti].events[ei].clone();
                if before.tick == new_end {
                    continue;
                }
                let mut after = before.clone();
                after.tick = new_end;
                ops.push(Op::UpdateEvent {
                    pos: usize::MAX,
                    track: ti,
                    before,
                    after,
                });
            }
        }
        ops
    }

    /// Set every noteOn velocity in [from,to) to `vel`.
    pub fn set_velocity_ops(&mut self, track: usize, from: u64, to: u64, vel: u8) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            if n.vel == vel {
                continue;
            }
            if let Some((ti, ei)) = self.by_id.get(&n.on_id).copied() {
                let before = self.tracks[ti].events[ei].clone();
                let mut after = before.clone();
                if let EventKind::Channel { data, .. } = &mut after.kind {
                    data[1] = vel.clamp(1, 127);
                }
                ops.push(Op::UpdateEvent {
                    pos: usize::MAX,
                    track: ti,
                    before,
                    after,
                });
            }
        }
        ops
    }

    /// Set every note-OFF's release velocity in [from,to) to `vel`.
    /// `vel > 0` upgrades a NoteOn-vel0 off to a real 0x80 NoteOff — the
    /// 0x90 form has nowhere to carry release data. `vel = 0` keeps the
    /// stored form (an 0x80 stays 0x80, a 0x90v0 stays 0x90v0).
    pub fn set_release_velocity_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        vel: u8,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        for n in self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
        {
            let Some(off_id) = n.off_id else { continue };
            // 0x90-vel0 at vel 0 is already what would be written — no-op
            if vel == 0 && n.off_via_on {
                continue;
            }
            if vel == n.off_vel && !n.off_via_on {
                continue;
            }
            if let Some((ti, ei)) = self.by_id.get(&off_id).copied() {
                let before = self.tracks[ti].events[ei].clone();
                let mut after = before.clone();
                if let EventKind::Channel { status, data, .. } = &mut after.kind {
                    if vel > 0 {
                        *status = (*status & 0x0F) | 0x80;
                    }
                    data[1] = vel;
                }
                ops.push(Op::UpdateEvent {
                    pos: usize::MAX,
                    track: ti,
                    before,
                    after,
                });
            }
        }
        ops
    }

    /// Retarget every channel event in [from,to) to `channel` (0-indexed).
    pub fn set_channel_ops(&mut self, track: usize, from: u64, to: u64, channel: u8) -> Vec<Op> {
        let mut ops = Vec::new();
        let t = match self.tracks.get(track) {
            Some(t) => t,
            None => return ops,
        };
        for e in &t.events {
            if e.tick < from || e.tick >= to {
                continue;
            }
            if let EventKind::Channel { status, .. } = &e.kind {
                if status & 0x0F == channel & 0x0F {
                    continue;
                }
                let mut after = e.clone();
                if let EventKind::Channel { status, .. } = &mut after.kind {
                    *status = (*status & 0xF0) | (channel & 0x0F);
                }
                ops.push(Op::UpdateEvent {
                    pos: usize::MAX,
                    track,
                    before: e.clone(),
                    after,
                });
            }
        }
        ops
    }

    /// Insert a program change (with optional bank select CC0/CC32 first) —
    /// classic "pick a patch" as raw events.
    pub fn set_program_ops(
        &mut self,
        track: usize,
        tick: u64,
        channel: u8,
        program: u8,
        bank_msb: Option<u8>,
        bank_lsb: Option<u8>,
    ) -> Vec<Op> {
        let mut seq = self.next_seq(track, tick);
        let mut mk = |kind: EventKind| Event {
            id: self.alloc_event_id(),
            tick,
            seq: {
                let s = seq;
                seq = seq.saturating_add(1);
                s
            },
            raw_body: None,
            kind,
        };
        let mut events = Vec::new();
        if let Some(m) = bank_msb {
            events.push(mk(Self::chan_event(0xB0, channel, 0, m)));
        }
        if let Some(l) = bank_lsb {
            events.push(mk(Self::chan_event(0xB0, channel, 32, l)));
        }
        events.push(mk(EventKind::Channel {
            status: 0xC0 | (channel & 0x0F),
            data: [program & 0x7F, 0],
            len: 1,
        }));
        vec![Op::InsertEvents { track, events }]
    }

    /// Insert (or replace at same tick) one CC point.
    pub fn set_cc_ops(
        &mut self,
        track: usize,
        tick: u64,
        channel: u8,
        cc: u8,
        value: u8,
    ) -> Vec<Op> {
        let seq = self.next_seq(track, tick);
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick,
                seq,
                raw_body: None,
                kind: Self::chan_event(0xB0, channel, cc & 0x7F, value & 0x7F),
            }],
        }]
    }

    /// Pitch bend point (0..16383, center 8192).
    pub fn set_pitch_bend_ops(
        &mut self,
        track: usize,
        tick: u64,
        channel: u8,
        value: u16,
    ) -> Vec<Op> {
        let v = value.min(16383);
        let seq = self.next_seq(track, tick);
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick,
                seq,
                raw_body: None,
                kind: Self::chan_event(0xE0, channel, (v & 0x7F) as u8, (v >> 7) as u8),
            }],
        }]
    }

    /// Channel pressure (aftertouch, 0xD0) point — single data byte, 0..127.
    pub fn set_channel_pressure_ops(
        &mut self,
        track: usize,
        tick: u64,
        channel: u8,
        value: u8,
    ) -> Vec<Op> {
        let seq = self.next_seq(track, tick);
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick,
                seq,
                raw_body: None,
                kind: EventKind::Channel {
                    status: 0xD0 | (channel & 0x0F),
                    data: [value & 0x7F, 0],
                    len: 1,
                },
            }],
        }]
    }

    /// Polyphonic key pressure (0xA0) point — key + value, both 0..127.
    pub fn set_poly_pressure_ops(
        &mut self,
        track: usize,
        tick: u64,
        channel: u8,
        key: u8,
        value: u8,
    ) -> Vec<Op> {
        let seq = self.next_seq(track, tick);
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick,
                seq,
                raw_body: None,
                kind: EventKind::Channel {
                    status: 0xA0 | (channel & 0x0F),
                    data: [key & 0x7F, value & 0x7F],
                    len: 2,
                },
            }],
        }]
    }

    /// Remove arbitrary events by id (lane deletes, event-list deletes,
    /// meta removal). Returns one RemoveEvents op per source track.
    pub fn remove_events_ops(&mut self, ids: &[EventId]) -> Vec<Op> {
        let mut by_track: std::collections::BTreeMap<usize, Vec<(usize, Event)>> =
            std::collections::BTreeMap::new();
        for &id in ids {
            if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                by_track
                    .entry(ti)
                    .or_default()
                    .push((ei, self.tracks[ti].events[ei].clone()));
            }
        }
        by_track
            .into_iter()
            .map(|(track, removed)| Op::RemoveEvents { track, removed })
            .collect()
    }

    /// Derive the RPN/NRPN write sequence for every track: each completed
    /// selector pair opens an entry at that tick and following CC6/CC38 events
    /// on the same channel attach to it until the next selector pair (or null
    /// selector) opens a new entry. Read-only — raw ordering is untouched.
    pub fn rpn_entries(&self) -> Vec<RpnEntry> {
        let mut out = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            // per channel: pending half-selectors (cc-98 slot → (tick,id,val))
            // and the index of the open entry receiving data entry CCs
            let mut pend: [[Option<(u64, EventId, u8)>; 4]; 16] = [[None; 4]; 16];
            let mut open: [Option<usize>; 16] = [None; 16];
            for e in &t.events {
                let (ch, cc, val) = match e.kind {
                    EventKind::Channel { status, data, len }
                        if status & 0xF0 == 0xB0 && len == 2 =>
                    {
                        (status & 0x0F, data[0], data[1])
                    }
                    _ => continue,
                };
                if (98..=101).contains(&cc) {
                    let slot = (cc - 98) as usize;
                    let nrpn = slot < 2;
                    // a selector event for the other group doesn't reset this
                    // one: hardware writes the two halves however it likes
                    pend[ch as usize][slot] = Some((e.tick, e.id, val));
                    let (msb_slot, lsb_slot) = if nrpn { (1, 0) } else { (3, 2) };
                    let (m, l) = (pend[ch as usize][msb_slot], pend[ch as usize][lsb_slot]);
                    if let (Some((mt, mid, mv)), Some((lt, lid, lv))) = (m, l) {
                        let mut sel_ids = Vec::new();
                        if mt <= lt {
                            sel_ids.push(mid);
                            sel_ids.push(lid);
                        } else {
                            sel_ids.push(lid);
                            sel_ids.push(mid);
                        }
                        pend[ch as usize][msb_slot] = None;
                        pend[ch as usize][lsb_slot] = None;
                        out.push(RpnEntry {
                            track: ti,
                            channel: ch,
                            nrpn,
                            param_msb: mv,
                            param_lsb: lv,
                            tick: mt.min(lt),
                            sel_ids,
                            data_msb: None,
                            data_lsb: None,
                            data_msb_id: None,
                            data_lsb_id: None,
                            extra_data_ids: Vec::new(),
                        });
                        let idx = out.len() - 1;
                        open[ch as usize] = if out[idx].is_null() { None } else { Some(idx) };
                    }
                } else if cc == 6 || cc == 38 {
                    if let Some(i) = open[ch as usize] {
                        let ent = &mut out[i];
                        if cc == 6 && ent.data_msb_id.is_none() {
                            ent.data_msb = Some(val);
                            ent.data_msb_id = Some(e.id);
                        } else if cc == 38 && ent.data_lsb_id.is_none() {
                            ent.data_lsb = Some(val);
                            ent.data_lsb_id = Some(e.id);
                        } else {
                            ent.extra_data_ids.push(e.id);
                        }
                    }
                }
            }
        }
        out
    }

    /// First recognized mode-reset SysEx anywhere in the file (GM1/GM2/GS/XG).
    /// A display hint only — detection never edits bytes.
    pub fn synth_mode(&self) -> Option<smf_core::ModeHint> {
        self.tracks
            .iter()
            .flat_map(|t| t.events.iter())
            .find_map(|e| match &e.kind {
                EventKind::SysEx(p) => smf_core::reset_hint(p),
                _ => None,
            })
    }

    /// Program changes with their effective bank-select context: CC0 (bank
    /// MSB) and CC32 (bank LSB) state at the moment of each PC event, tracked
    /// per (track, channel). Purely derived — raw bytes untouched.
    pub fn program_changes(&self) -> Vec<ProgramChange> {
        let mut out = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            let mut bank = [(0u8, 0u8); 16];
            for e in &t.events {
                if let EventKind::Channel { status, data, len } = e.kind {
                    match status & 0xF0 {
                        0xB0 if len == 2 && data[0] == 0 => {
                            bank[(status & 0x0F) as usize].0 = data[1];
                        }
                        0xB0 if len == 2 && data[0] == 32 => {
                            bank[(status & 0x0F) as usize].1 = data[1];
                        }
                        0xC0 if len == 1 => {
                            let (msb, lsb) = bank[(status & 0x0F) as usize];
                            out.push(ProgramChange {
                                track: ti,
                                channel: status & 0x0F,
                                tick: e.tick,
                                id: e.id,
                                bank_msb: msb,
                                bank_lsb: lsb,
                                program: data[0],
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        out
    }

    /// Find the RPN/NRPN entry containing `id` (selector or data event).
    pub fn rpn_entry_containing(&self, id: EventId) -> Option<RpnEntry> {
        self.rpn_entries()
            .into_iter()
            .find(|e| e.ids().contains(&id))
    }

    /// Canonical RPN/NRPN write: selector MSB, selector LSB, data entry MSB,
    /// optional data entry LSB — inserted at `tick` with consecutive seqs.
    /// `param_msb`/`param_lsb` 0x7F/0x7F writes the null selector reset.
    #[allow(clippy::too_many_arguments)]
    pub fn set_rpn_ops(
        &mut self,
        track: usize,
        tick: u64,
        channel: u8,
        nrpn: bool,
        param_msb: u8,
        param_lsb: u8,
        data_msb: u8,
        data_lsb: Option<u8>,
    ) -> Vec<Op> {
        let seq = self.next_seq(track, tick);
        let (sel_msb, sel_lsb) = if nrpn { (99u8, 98u8) } else { (101u8, 100u8) };
        let mut events = Vec::new();
        let mut push = |i: u32, cc: u8, v: u8, events: &mut Vec<Event>| {
            events.push(Event {
                id: self.alloc_event_id(),
                tick,
                seq: seq + i,
                raw_body: None,
                kind: Self::chan_event(0xB0, channel, cc, v & 0x7F),
            });
        };
        push(0, sel_msb, param_msb, &mut events);
        push(1, sel_lsb, param_lsb, &mut events);
        // null selector is a pure reset — no data entry follows it
        let null = param_msb == 0x7F && param_lsb == 0x7F;
        if !null {
            push(2, 6, data_msb, &mut events);
            if let Some(l) = data_lsb {
                push(3, 38, l, &mut events);
            }
        }
        vec![Op::InsertEvents { track, events }]
    }

    /// Rewrite the data-entry bytes of a parsed entry. Missing data events are
    /// inserted right after the selector pair; passing `data_lsb: None` with
    /// an existing LSB removes it (7-bit entry). Selectors are untouched.
    pub fn update_rpn_value_ops(
        &mut self,
        entry: &RpnEntry,
        data_msb: u8,
        data_lsb: Option<u8>,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        let track = entry.track;
        let ch = entry.channel;
        // inserts for missing data events go at the selector tick, after every
        // event already there (i.e. behind the selector pair)
        let mut ins: Vec<(u8, u8)> = Vec::new();
        match entry.data_msb_id {
            Some(id) => {
                if let Some(&(ti, ei)) = self.by_id.get(&id) {
                    let mut after = self.tracks[ti].events[ei].clone();
                    if let EventKind::Channel { ref mut data, .. } = after.kind {
                        data[1] = data_msb & 0x7F;
                    }
                    ops.push(Op::UpdateEvent {
                        pos: usize::MAX,
                        track: ti,
                        before: self.tracks[ti].events[ei].clone(),
                        after,
                    });
                }
            }
            None => ins.push((6, data_msb)),
        }
        match (entry.data_lsb_id, data_lsb) {
            (Some(id), Some(l)) => {
                if let Some(&(ti, ei)) = self.by_id.get(&id) {
                    let mut after = self.tracks[ti].events[ei].clone();
                    if let EventKind::Channel { ref mut data, .. } = after.kind {
                        data[1] = l & 0x7F;
                    }
                    ops.push(Op::UpdateEvent {
                        pos: usize::MAX,
                        track: ti,
                        before: self.tracks[ti].events[ei].clone(),
                        after,
                    });
                }
            }
            (Some(id), None) => {
                if let Some(&(ti, ei)) = self.by_id.get(&id) {
                    let before = self.tracks[ti].events[ei].clone();
                    ops.push(Op::RemoveEvents {
                        track: ti,
                        removed: vec![(ei, before)],
                    });
                }
            }
            (None, Some(l)) => ins.push((38, l)),
            (None, None) => {}
        }
        if !ins.is_empty() {
            let seq = self.next_seq(track, entry.tick);
            let events = ins
                .into_iter()
                .enumerate()
                .map(|(i, (cc, v))| Event {
                    id: self.alloc_event_id(),
                    tick: entry.tick,
                    seq: seq + i as u32,
                    raw_body: None,
                    kind: Self::chan_event(0xB0, ch, cc, v & 0x7F),
                })
                .collect();
            ops.push(Op::InsertEvents { track, events });
        }
        ops
    }

    /// Rewrite the selector bytes of a parsed entry (new parameter number).
    /// Selector order and grouping are preserved; data events untouched.
    pub fn update_rpn_param_ops(
        &mut self,
        entry: &RpnEntry,
        param_msb: u8,
        param_lsb: u8,
    ) -> Vec<Op> {
        let mut ops = Vec::new();
        // each selector event keeps its own CC number; the msb-numbered one
        // (101/99) takes param_msb, the lsb-numbered one (100/98) param_lsb
        let msb_cc = if entry.nrpn { 99 } else { 101 };
        for &id in &entry.sel_ids {
            let Some(&(ti, ei)) = self.by_id.get(&id) else {
                continue;
            };
            let Some(e) = self.tracks[ti].events.get(ei) else {
                continue;
            };
            let val = match e.kind {
                EventKind::Channel { data, .. } if data[0] == msb_cc => param_msb,
                _ => param_lsb,
            };
            let mut after = e.clone();
            if let EventKind::Channel { ref mut data, .. } = after.kind {
                data[1] = val & 0x7F;
            }
            ops.push(Op::UpdateEvent {
                pos: usize::MAX,
                track: ti,
                before: e.clone(),
                after,
            });
        }
        ops
    }

    /// Remove an entire parsed entry (selectors + every data event) so the
    /// whole parameter write is deleted as one semantic unit.
    pub fn remove_rpn_entry_ops(&mut self, entry: &RpnEntry) -> Vec<Op> {
        let mut by_track: BTreeMap<usize, Vec<(usize, Event)>> = BTreeMap::new();
        for &id in &entry.ids() {
            if let Some(&(ti, ei)) = self.by_id.get(&id) {
                by_track
                    .entry(ti)
                    .or_default()
                    .push((ei, self.tracks[ti].events[ei].clone()));
            }
        }
        by_track
            .into_iter()
            .map(|(track, removed)| Op::RemoveEvents { track, removed })
            .collect()
    }

    /// Friendly name for a program change under the file's detected mode.
    /// Only banks we can name unambiguously resolve — GM-family melodic bank
    /// (0,0) maps to the GM table on every mode; percussion kits resolve via
    /// `smf_core::kit_name`. Unknown banks stay numeric (None).
    pub fn program_name(&self, pc: &ProgramChange) -> Option<String> {
        let mode = self.synth_mode();
        if pc.channel == 9 {
            let m = mode.unwrap_or(smf_core::ModeHint::Gm1);
            return smf_core::kit_name(m, pc.bank_msb).map(|k| format!("{k} #{}", pc.program));
        }
        if pc.bank_msb == 0 && pc.bank_lsb == 0 {
            let name = smf_core::gm_program_name(pc.program);
            return Some(match mode {
                Some(m) => format!("{}: {name}", m.label()),
                None => name.to_string(),
            });
        }
        None
    }

    /// Set/replace the tempo at `tick` on `track` (the conductor is
    /// track 0 for format 0/1; a format-2 sequence owns its own tempo).
    pub fn set_tempo_ops(&mut self, track: usize, tick: u64, bpm: f64) -> Vec<Op> {
        let mpq = (60_000_000.0 / bpm.max(1.0))
            .round()
            .clamp(1.0, 0xFF_FFFF as f64) as u32;
        let data = Bytes::copy_from_slice(&mpq.to_be_bytes()[1..]);
        // replace an existing tempo event at the same tick
        if let Some(e) = self
            .tracks
            .get(track)
            .and_then(|t| {
                t.events.iter().find(|e| {
                    e.tick == tick
                        && matches!(
                            e.kind,
                            EventKind::Meta {
                                meta_type: 0x51,
                                ..
                            }
                        )
                })
            })
            .cloned()
        {
            let mut after = e.clone();
            after.kind = EventKind::Meta {
                meta_type: 0x51,
                data,
            };
            return vec![Op::UpdateEvent {
                pos: usize::MAX,
                track,
                before: e,
                after,
            }];
        }
        self.insert_single_ops(
            track,
            tick,
            EventKind::Meta {
                meta_type: 0x51,
                data,
            },
        )
    }

    /// Shared tail for single-meta builders: inserts the event on `track`,
    /// creating track 0 first when the document is empty and the target is
    /// the conductor track — otherwise the transaction fails UnknownTrack(0).
    fn insert_single_ops(&mut self, track: usize, tick: u64, kind: EventKind) -> Vec<Op> {
        let mut ops = Vec::new();
        if self.tracks.is_empty() && track == 0 {
            ops.push(Op::InsertTrack {
                index: 0,
                track: Track {
                    name: None,
                    out_port: 0,
                    out_channel: 0,
                    events: vec![],
                },
            });
        }
        ops.push(Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick,
                seq: self.next_seq(track, tick),
                raw_body: None,
                kind,
            }],
        });
        ops
    }

    /// Set/replace the time signature at `tick` on `track` (denominator
    /// given as the actual value — 4, 8, … — encoded to the SMF
    /// power-of-two form). Rewriting nn/dd alone preserves the stored
    /// `cc`/`bb` bytes; a brand-new signature gets the conventional
    /// quarter-note click (36 clocks in compound meter) and 8 32nds.
    pub fn set_time_sig_ops(&mut self, track: usize, tick: u64, num: u8, den: u8) -> Vec<Op> {
        let dd = (den.max(1) as f64).log2().round() as u8;
        let (cc, bb) = match self
            .tracks
            .get(track)
            .and_then(|t| {
                t.events.iter().find(|e| {
                    e.tick == tick
                        && matches!(
                            e.kind,
                            EventKind::Meta {
                                meta_type: 0x58,
                                ..
                            }
                        )
                })
            })
            .map(|e| match &e.kind {
                EventKind::Meta { data, .. } if data.len() >= 4 => (data[2], data[3]),
                _ => (24, 8),
            }) {
            Some(cb) => cb,
            None => (MeterEvent::default_click_clocks(num, dd), 8),
        };
        self.set_time_sig_full_ops(track, tick, num, den, cc, bb)
    }

    /// Set/replace a time signature with the complete `nn dd cc bb`
    /// payload — every byte the caller wants stored is written verbatim.
    pub fn set_time_sig_full_ops(
        &mut self,
        track: usize,
        tick: u64,
        num: u8,
        den: u8,
        cc: u8,
        bb: u8,
    ) -> Vec<Op> {
        let dd = (den.max(1) as f64).log2().round() as u8;
        let data = Bytes::copy_from_slice(&[num, dd, cc, bb]);
        if let Some(e) = self
            .tracks
            .get(track)
            .and_then(|t| {
                t.events.iter().find(|e| {
                    e.tick == tick
                        && matches!(
                            e.kind,
                            EventKind::Meta {
                                meta_type: 0x58,
                                ..
                            }
                        )
                })
            })
            .cloned()
        {
            let mut after = e.clone();
            after.kind = EventKind::Meta {
                meta_type: 0x58,
                data,
            };
            return vec![Op::UpdateEvent {
                pos: usize::MAX,
                track,
                before: e,
                after,
            }];
        }
        self.insert_single_ops(
            track,
            tick,
            EventKind::Meta {
                meta_type: 0x58,
                data,
            },
        )
    }

    /// Set the track's output channel meta (`FF 20`): update the existing
    /// marker or insert one at tick 0.
    pub fn set_track_channel_ops(&mut self, track: usize, channel: u8) -> Vec<Op> {
        if let Some(e) = self
            .tracks
            .get(track)
            .and_then(|t| {
                t.events.iter().find(|e| {
                    matches!(
                        e.kind,
                        EventKind::Meta {
                            meta_type: 0x20,
                            ..
                        }
                    )
                })
            })
            .cloned()
        {
            let mut after = e.clone();
            after.kind = EventKind::Meta {
                meta_type: 0x20,
                data: Bytes::copy_from_slice(&[channel & 0x0F]),
            };
            return vec![Op::UpdateEvent {
                pos: usize::MAX,
                track,
                before: e,
                after,
            }];
        }
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick: 0,
                seq: 0,
                raw_body: None,
                kind: EventKind::Meta {
                    meta_type: 0x20,
                    data: Bytes::copy_from_slice(&[channel & 0x0F]),
                },
            }],
        }]
    }

    /// Rewrite every channel event's status channel in `track` to
    /// `channel` — the destructive counterpart of playback-time
    /// re-channelization (#221): it edits the events themselves, so it
    /// survives export to other tools. Events already on the channel
    /// produce no op; metas/SysEx are untouched.
    pub fn rechannelize_ops(&mut self, track: usize, channel: u8) -> Vec<Op> {
        let mut ops = Vec::new();
        let Some(t) = self.tracks.get(track) else {
            return ops;
        };
        for e in &t.events {
            if let EventKind::Channel { status, .. } = &e.kind {
                let new_status = (*status & 0xF0) | (channel & 0x0F);
                if new_status == *status {
                    continue;
                }
                let mut after = e.clone();
                if let EventKind::Channel { status, .. } = &mut after.kind {
                    *status = new_status;
                }
                ops.push(Op::UpdateEvent {
                    pos: usize::MAX,
                    track,
                    before: e.clone(),
                    after,
                });
            }
        }
        ops
    }

    /// Clone every channel event in [from,to) shifted to start at `to`.
    /// Notes keep their NoteOff even when it sits outside the range — a
    /// duplicated note must not hang. (Meta events stay behind — duplicating
    /// tempo/EOT would corrupt the structure.)
    pub fn duplicate_range_ops(&mut self, track: usize, from: u64, to: u64) -> Vec<Op> {
        let span = to.saturating_sub(from);
        if span == 0 {
            return vec![];
        }
        let t = match self.tracks.get(track) {
            Some(t) => t,
            None => return vec![],
        };
        let mut events: Vec<Event> = t
            .events
            .iter()
            .filter(|e| e.tick >= from && e.tick < to)
            .filter(|e| matches!(e.kind, EventKind::Channel { .. }))
            .cloned()
            .collect();
        // grab the NoteOff of any note whose start is in range — its tick may
        // lie past `to`, in which case the in-range filter missed it
        let have: std::collections::BTreeSet<EventId> = events.iter().map(|e| e.id).collect();
        let extra_offs: Vec<Event> = self
            .notes()
            .into_iter()
            .filter(|n| n.track == track && n.start_tick >= from && n.start_tick < to)
            .filter_map(|n| n.off_id)
            .filter(|id| !have.contains(id))
            .filter_map(|id| {
                self.by_id
                    .get(&id)
                    .map(|&(ti, ei)| self.tracks[ti].events[ei].clone())
            })
            .collect();
        events.extend(extra_offs);
        for e in &mut events {
            e.tick = e.tick.saturating_add(span);
            e.id = self.alloc_event_id();
        }
        if events.is_empty() {
            return vec![];
        }
        vec![Op::InsertEvents { track, events }]
    }

    /// Delete every channel event in [from,to) plus the matching NoteOff of
    /// any note that starts inside the range (notes delete whole).
    pub fn delete_range_ops(&mut self, track: usize, from: u64, to: u64) -> Vec<Op> {
        self.delete_range_channel_ops(track, from, to, &(0u8..16).collect())
    }

    /// `delete_range_ops` limited to channel events on `channels`
    /// (status low nibble) — replace-mode recording erases only the
    /// channels the take actually carries, never the whole range.
    pub fn delete_range_channel_ops(
        &mut self,
        track: usize,
        from: u64,
        to: u64,
        channels: &std::collections::BTreeSet<u8>,
    ) -> Vec<Op> {
        let t = match self.tracks.get(track) {
            Some(t) => t,
            None => return vec![],
        };
        // Notes whose release lands inside the range but whose onset lies
        // before it straddle the cut: deleting the Note-Off alone leaves an
        // unterminated Note-On ringing forever. Truncate those releases to
        // `from` so the sounded portion before the cut survives (#210).
        let straddling_offs: std::collections::BTreeSet<EventId> = self
            .notes()
            .into_iter()
            .filter(|n| {
                n.track == track
                    && n.start_tick < from
                    && n.end_tick.is_some_and(|e| e >= from && e < to)
                    && channels.contains(&n.channel)
            })
            .filter_map(|n| n.off_id)
            .collect();
        let mut ids: std::collections::BTreeSet<EventId> = t
            .events
            .iter()
            .filter(|e| e.tick >= from && e.tick < to)
            .filter(|e| match e.kind {
                EventKind::Channel { status, .. } => channels.contains(&(status & 0x0F)),
                _ => false,
            })
            .filter(|e| !straddling_offs.contains(&e.id))
            .map(|e| e.id)
            .collect();
        for n in self.notes().into_iter().filter(|n| {
            n.track == track
                && n.start_tick >= from
                && n.start_tick < to
                && channels.contains(&n.channel)
        }) {
            ids.insert(n.on_id);
            if let Some(o) = n.off_id {
                ids.insert(o);
            }
        }
        let mut ops: Vec<Op> = ids
            .into_iter()
            .filter_map(|id| {
                self.by_id
                    .get(&id)
                    .copied()
                    .map(|(ti, ei)| Op::RemoveEvents {
                        track: ti,
                        removed: vec![(ei, self.tracks[ti].events[ei].clone())],
                    })
            })
            .collect();
        for id in straddling_offs {
            if let Some((ti, ei)) = self.by_id.get(&id).copied() {
                let mut after = self.tracks[ti].events[ei].clone();
                if after.tick != from {
                    after.tick = from;
                    ops.push(Op::UpdateEvent {
                        pos: usize::MAX,
                        track: ti,
                        before: self.tracks[ti].events[ei].clone(),
                        after,
                    });
                }
            }
        }
        ops
    }

    /// Append a fresh track (EOT at tick 0) and optionally a name meta.
    /// `enc` selects the name's write encoding (`None` = UTF-8, #176).
    pub fn add_track_ops(
        &mut self,
        name: Option<&str>,
        enc: Option<smf_core::TextEncoding>,
    ) -> Vec<Op> {
        let enc = enc.unwrap_or(smf_core::TextEncoding::Utf8);
        let name_bytes = |n: &str| Bytes::copy_from_slice(&smf_core::encode_text(n, enc));
        let mut events = vec![Event {
            id: self.alloc_event_id(),
            tick: 0,
            seq: 0,
            raw_body: None,
            kind: EventKind::Meta {
                meta_type: 0x2F,
                data: Bytes::new(),
            },
        }];
        if let Some(n) = name {
            events.insert(
                0,
                Event {
                    id: self.alloc_event_id(),
                    tick: 0,
                    seq: 0,
                    raw_body: None,
                    kind: EventKind::Meta {
                        meta_type: 0x03,
                        data: name_bytes(n),
                    },
                },
            );
        }
        let mut ops = vec![Op::InsertTrack {
            index: self.tracks.len(),
            track: Track {
                name: name.map(name_bytes),
                out_port: 0,
                out_channel: 0,
                events,
            },
        }];
        // a format-0 file already holding a track becomes format 1 —
        // declared explicitly in the transaction, never by the serializer
        if self.format == 0 && !self.tracks.is_empty() {
            ops.push(Op::SetFormat {
                before: 0,
                after: 1,
            });
        }
        ops
    }

    /// Remove track `index` entirely (undo restores it wholesale).
    /// The last track cannot be removed — a document keeps at least one.
    pub fn remove_track_ops(&mut self, index: usize) -> Vec<Op> {
        if self.tracks.len() <= 1 {
            return vec![];
        }
        match self.tracks.get(index) {
            Some(t) => vec![Op::RemoveTrack {
                index,
                track: t.clone(),
            }],
            None => vec![],
        }
    }

    /// Explicit format conversion transaction content (e.g. format 1 →
    /// format 2, or 1 → 0 when a single-track file is saved back).
    /// `Document::apply` enforces the resulting invariant.
    pub fn set_format_ops(&mut self, format: u16) -> Vec<Op> {
        if format == self.format {
            return vec![];
        }
        vec![Op::SetFormat {
            before: self.format,
            after: format,
        }]
    }

    /// Set/replace the track name meta (0x03) at tick 0. `enc` selects the
    /// write encoding (`None` = UTF-8); callers pass the editor's override
    /// or the file's own charset hint so a legacy Shift-JIS file keeps its
    /// encoding instead of being silently rewritten as UTF-8 (#176).
    pub fn set_track_name_ops(
        &mut self,
        track: usize,
        name: &str,
        enc: Option<smf_core::TextEncoding>,
    ) -> Vec<Op> {
        let enc = enc.unwrap_or(smf_core::TextEncoding::Utf8);
        let data = Bytes::copy_from_slice(&smf_core::encode_text(name, enc));
        if let Some(e) = self
            .tracks
            .get(track)
            .and_then(|t| {
                t.events.iter().find(|e| {
                    matches!(
                        e.kind,
                        EventKind::Meta {
                            meta_type: 0x03,
                            ..
                        }
                    )
                })
            })
            .cloned()
        {
            let mut after = e.clone();
            after.kind = EventKind::Meta {
                meta_type: 0x03,
                data,
            };
            return vec![Op::UpdateEvent {
                pos: usize::MAX,
                track,
                before: e,
                after,
            }];
        }
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick: 0,
                seq: 0,
                raw_body: None,
                kind: EventKind::Meta {
                    meta_type: 0x03,
                    data,
                },
            }],
        }]
    }

    /// Create or overwrite a text-family meta (FF 01–0F: text, copyright,
    /// track name, instrument, lyric, marker, cue, …). With `id` naming an
    /// existing event of that type only its payload is rewritten — metas
    /// stay byte-preserved unless explicitly edited. Otherwise a new event
    /// is inserted at `tick`. `enc` is the requested write encoding
    /// (`None` = UTF-8); the editor's encoding override is passed through
    /// by callers, never guessed.
    pub fn set_meta_text_ops(
        &mut self,
        track: usize,
        tick: u64,
        meta_type: u8,
        id: EventId,
        text: &str,
        enc: Option<smf_core::TextEncoding>,
    ) -> Vec<Op> {
        let enc = enc.unwrap_or(smf_core::TextEncoding::Utf8);
        let data = Bytes::copy_from_slice(&smf_core::encode_text(text, enc));
        let existing = self
            .tracks
            .get(track)
            .and_then(|t| {
                t.events.iter().find(|e| {
                    e.id == id
                        && matches!(e.kind, EventKind::Meta { meta_type: m, .. } if m == meta_type)
                })
            })
            .cloned();
        if let Some(e) = existing {
            let mut after = e.clone();
            after.kind = EventKind::Meta { meta_type, data };
            return vec![Op::UpdateEvent {
                pos: usize::MAX,
                track,
                before: e,
                after,
            }];
        }
        vec![Op::InsertEvents {
            track,
            events: vec![Event {
                id: self.alloc_event_id(),
                tick,
                seq: self.next_seq(track, tick),
                raw_body: None,
                kind: EventKind::Meta { meta_type, data },
            }],
        }]
    }

    /// Delete a single meta event by id — exact removal of one row, nothing
    /// else is touched.
    pub fn remove_meta_ops(&mut self, track: usize, id: EventId) -> Vec<Op> {
        let Some((idx, e)) = self.tracks.get(track).and_then(|t| {
            t.events
                .iter()
                .enumerate()
                .find(|(_, e)| e.id == id && matches!(e.kind, EventKind::Meta { .. }))
                .map(|(i, e)| (i, e.clone()))
        }) else {
            return vec![];
        };
        vec![Op::RemoveEvents {
            track,
            removed: vec![(idx, e)],
        }]
    }

    /// Set the song's key signature (FF 59 `sf`/`mi`, track 0 like tempo):
    /// update the first existing key sig in place, else insert at `tick`.
    pub fn set_key_sig_ops(&mut self, tick: u64, sf: i8, mi: u8) -> Vec<Op> {
        let data = Bytes::copy_from_slice(&[sf as u8, mi.min(1)]);
        if let Some(e) = self
            .tracks
            .first()
            .and_then(|t| {
                t.events.iter().find(|e| {
                    matches!(
                        e.kind,
                        EventKind::Meta {
                            meta_type: 0x59,
                            ..
                        }
                    )
                })
            })
            .cloned()
        {
            let mut after = e.clone();
            after.kind = EventKind::Meta {
                meta_type: 0x59,
                data,
            };
            return vec![Op::UpdateEvent {
                pos: usize::MAX,
                track: 0,
                before: e,
                after,
            }];
        }
        self.insert_single_ops(
            0,
            tick,
            EventKind::Meta {
                meta_type: 0x59,
                data,
            },
        )
    }
}

impl Document {
    /// #162 — explicit Format 0 → Format 1 conversion splitting channel
    /// events into one track per used channel. Non-channel content (tempo,
    /// signatures, text, SysEx, EOT) stays in track 0 — it is never
    /// duplicated or moved. Returns empty unless the document is format 0
    /// with more than one channel in use, so accidental single-channel
    /// files never get spurious tracks.
    pub fn split_fmt0_by_channel_ops(&self) -> Vec<Op> {
        if self.format != 0 || self.tracks.len() != 1 {
            return Vec::new();
        }
        let tr = &self.tracks[0];
        let chans: std::collections::BTreeSet<u8> = tr
            .events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Channel { status, .. } => Some(status & 0x0F),
                _ => None,
            })
            .collect();
        if chans.len() <= 1 {
            return Vec::new();
        }
        let mut ops = vec![Op::SetFormat {
            before: 0,
            after: 1,
        }];
        let removed: Vec<(usize, Event)> = tr
            .events
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e.kind, EventKind::Channel { .. }))
            .map(|(i, e)| (i, e.clone()))
            .collect();
        ops.push(Op::RemoveEvents { track: 0, removed });
        for ch in chans {
            let events: Vec<Event> = tr
                .events
                .iter()
                .filter(
                    |e| matches!(&e.kind, EventKind::Channel { status, .. } if status & 0x0F == ch),
                )
                .cloned()
                .collect();
            ops.push(Op::InsertTrack {
                index: usize::MAX,
                track: Track {
                    name: Some(format!("Channel {}", ch + 1).into_bytes().into()),
                    out_port: tr.out_port,
                    out_channel: ch,
                    events,
                },
            });
        }
        ops
    }
}

impl Document {
    /// Channels (0-15) present in the document's channel events — the
    /// basis of the Format-0 multichannel import check (#162).
    pub fn channels_used(&self) -> std::collections::BTreeSet<u8> {
        self.tracks
            .iter()
            .flat_map(|t| t.events.iter())
            .filter_map(|e| match &e.kind {
                EventKind::Channel { status, .. } => Some(status & 0x0F),
                _ => None,
            })
            .collect()
    }
}

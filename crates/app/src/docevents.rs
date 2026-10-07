//! Document lifecycle: new/open/save/save-as, undo/redo, external-change
//! and save-conflict dialogs, restore-on-launch, diagnostics export, and
//! the file-system watcher that feeds them. All writes go through
//! `write_to`/`apply_tx`; the byte-exact round-trip invariant lives here.

use super::*;

pub(crate) fn empty_doc() -> Document {
    let f = smf_core::File {
        format: 1,
        division: Division::Metrical(480),
        tracks: vec![smf_core::Track { events: vec![] }],
        warnings: vec![],
    };
    Document::from_file(f)
}

/// Ask whether to restore a crash/autosave snapshot for this startup, with
/// the snapshot's provenance so the decision is informed. Restore swaps the
/// recovered document in and marks it dirty; the original file is only ever
/// written by an explicit Save. Runs as a window task so it doesn't block
/// app startup.
pub(crate) fn maybe_prompt_restore(
    view: Entity<EditorView>,
    argv_path: Option<PathBuf>,
    window: &mut Window,
    cx: &mut App,
) {
    // bounded retention — stale/overflow snapshots die on startup
    recovery::cleanup_stale(
        &recovery::recovery_dir(),
        recovery::KEEP_MAX,
        recovery::MAX_AGE,
        std::time::SystemTime::now(),
    );
    let Some((snap_path, meta, payload)) =
        recovery::find_candidate(&recovery::recovery_dir(), argv_path.as_deref())
    else {
        return;
    };
    window
        .spawn(cx, async move |wcx| {
            let src = meta
                .source_path
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| t("recovery.untitled").to_string());
            let mut shown_details = false;
            loop {
                let detail = if shown_details {
                    tf(
                        "recovery.inspect_detail",
                        &[
                            ("src", &src),
                            ("saved", &meta.saved_revision.to_string()),
                            ("rev", &meta.current_revision.to_string()),
                            ("size", &meta.payload_len.to_string()),
                            ("ver", &meta.app_version),
                            ("ago", &fmt_rel_time(meta.timestamp)),
                        ],
                    )
                } else {
                    tf(
                        "recovery.found",
                        &[("src", &src), ("ago", &fmt_rel_time(meta.timestamp))],
                    )
                };
                let idx = wcx
                    .prompt(
                        PromptLevel::Warning,
                        t("recovery.title"),
                        Some(&detail),
                        &[
                            PromptButton::Ok(t("recovery.restore").into()),
                            PromptButton::Other(t("recovery.discard").into()),
                            PromptButton::Cancel(
                                if shown_details {
                                    t("recovery.later")
                                } else {
                                    t("recovery.inspect")
                                }
                                .into(),
                            ),
                        ],
                    )
                    .await
                    .unwrap_or(usize::MAX);
                match idx {
                    // Restore — swap the snapshot's document into the view
                    0 => {
                        wcx.update(|_w, app| {
                            view.update(app, |v, cx| {
                                v.restore_snapshot(&meta, &payload, cx);
                            })
                        })
                        .ok();
                        break;
                    }
                    // Discard — explicit discard clears the snapshot
                    1 => {
                        let _ = std::fs::remove_file(&snap_path);
                        break;
                    }
                    // Inspect — one expansion, then the same three fates
                    _ if !shown_details => shown_details = true,
                    // Later/dismissed — keep the snapshot, nothing happens;
                    // the next verified save or discard clears it
                    _ => break,
                }
            }
        })
        .detach();
}

/// "3 minutes ago" style label for snapshot timestamps.
pub(crate) fn fmt_rel_time(unix_ts: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ago = now.saturating_sub(unix_ts);
    if ago < 60 {
        tf("time.sec_ago", &[("n", &ago.to_string())])
    } else if ago < 3600 {
        tf("time.min_ago", &[("n", &(ago / 60).to_string())])
    } else if ago < 86400 {
        tf("time.hr_ago", &[("n", &(ago / 3600).to_string())])
    } else {
        tf("time.day_ago", &[("n", &(ago / 86400).to_string())])
    }
}

/// Poll the shared doc's notify counter so MCP-driven edits repaint the UI
/// even while the user is idle. `pending_opens` (Windows primary only)
/// carries file paths handed over by secondary instances (#197); each tick
/// drains them through the same discard-guarded open drag/drop uses and
/// brings the window to the foreground.
pub(crate) fn spawn_doc_watch(
    cx: &mut Context<EditorView>,
    shared: SharedDoc,
    pending_opens: Option<crate::single_instance::PendingOpens>,
) {
    cx.spawn(async move |this, cx| {
        let mut last = 0u64;
        let mut last_tx = 0u64;
        let mut tick = 0u32;
        // autosave: the last revision a snapshot captured and the last
        // write attempt (started one debounce early so the first dirty
        // revision snapshots without an artificial delay)
        let mut last_snap_rev: Option<u64> = None;
        let mut last_snap_write =
            std::time::Instant::now() - recovery::DEBOUNCE;
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(150))
                .await;
            {
                let sh = lock_shared(&shared);
                let rev = sh.doc.revision();
                // save-point aware (#177): undoing back to the saved state
                // stops snapshotting just like a real save does
                let doc_dirty = sh.is_dirty();
                drop(sh);
                if doc_dirty
                    && last_snap_rev != Some(rev)
                    && last_snap_write.elapsed() >= recovery::DEBOUNCE
                {
                    last_snap_write = std::time::Instant::now();
                    let dir = recovery::recovery_dir();
                    if recovery::write_snapshot(&shared, &dir, std::time::SystemTime::now()).is_ok() {
                        last_snap_rev = Some(rev);
                    }
                }
            }
            let (cur, reqs, mcp_tx) = {
                let mut sh = lock_shared(&shared);
                (
                    sh.gui_notify.load(std::sync::atomic::Ordering::Relaxed),
                    std::mem::take(&mut sh.transport_req),
                    sh.last_mcp_tx.clone(),
                )
            };
            let dirty = cur != last || !reqs.is_empty();
            if cur != last {
                last = cur;
            }
            // surface the newest agent-originated transaction in the status bar
            let mcp_label = mcp_tx
                .as_ref()
                .filter(|r| r.revision > last_tx)
                .map(|r| {
                    last_tx = r.revision;
                    r.label.clone()
                });
            if let Some(view) = this.upgrade() {
                view.update(cx, |v, cx| {
                    // hotplug reconcile ~every 2 s: fresh endpoints join the
                    // catalog, vanished ones stay visible but marked offline,
                    // and an armed recording's input reconnects on return
                    tick += 1;
                    let ports_changed = tick.is_multiple_of(13) && v.reconcile_ports();
                    // MCP transport requests -> real playback actions
                    for r in reqs {
                        match r {
                            mcp_server::TransportReq::Play if v.playback.is_none() => {
                                v.start_playback();
                            }
                            mcp_server::TransportReq::Stop => v.transport_stop(cx),
                            mcp_server::TransportReq::SetLoop { start, end } => {
                                {
                                    let mut sh = crate::lock_shared(&v.shared);
                                    sh.loop_start = start;
                                    sh.loop_end = end;
                                }
                                v.persist();
                                v.refresh_live_schedule();
                            }
                            mcp_server::TransportReq::Seek { tick } => {
                                v.play_us = v.doc(|d| {
                                    d.tempo_map_for(v.sel_track).tick_to_us(tick)
                                });
                                v.play_start_us = v.play_us;
                                if v.playback.is_some() {
                                    v.stop_playback();
                                    v.start_playback();
                                }
                            }
                            _ => {}
                        }
                    }
                    let plugin_changed = v.poll_plugin_events();
                    if let Some(rx) = &v.scan_rx {
                        if let Ok(report) = rx.try_recv() {
                            v.scan_rx = None;
                            v.apply_catalog(report);
                            cx.notify();
                        }
                    }
                    if let Some(rx) = &v.save_rx {
                        if let Ok(done) = rx.try_recv() {
                            v.save_rx = None;
                            match done {
                                // committed=false means the document was
                                // swapped mid-save — its state governs
                                Ok(out) if out.committed => {
                                    tracing::info!(path = %out.path.display(), rev = out.revision, "document saved");
                                    // the canonical path moves only after a
                                    // durable write — Save-As to a failed
                                    // target never steals it (#158)
                                    // clear the snapshots of the identity the
                                    // document had while dirty: its own
                                    // pre-save path, or the untitled lineage
                                    // it was just saved out of (#174)
                                    let old = lock_shared(&v.shared).path.clone();
                                    v.adopt_saved_path(&out.path);
                                    v.status = t("status.saved").into();
                                    v.persist();
                                    recovery::clear_recovery_for(old.as_deref());
                                    // the just-written file is the new
                                    // identity baseline for external-watch
                                    v.file_stamp = watch::stat_file(&out.path);
                                    v.ext_prompted = false;
                                }
                                Ok(_) => {}
                                Err(e) => v.status = e.into(),
                            }
                            cx.notify();
                        }
                    }
                    if let Some((rx, saving)) = &v.dlg_rx {
                        if let Ok(done) = rx.try_recv() {
                            let saving = *saving;
                            v.dlg_rx = None;
                            if let Some(path) = done {
                                v.finish_dialog(path, saving, cx);
                            }
                        }
                    }
                    if dirty {
                        v.refresh_derived();
                        // cover routing changes that came from MCP tools —
                        // also warms any newly-assigned VST3 destination
                        v.persist();
                        v.refresh_plugins();
                        // a preview ringing on a route MCP just changed
                        // must not keep sounding into the wrong place
                        v.audition_off();
                        // MCP-originated edits/routing reach the running
                        // pass through the same live-update path (#140/#141)
                        v.refresh_live_schedule();
                    }
                    // watch the backing .mid for external modification /
                    // deletion (only the MIDI file — the sidecar doesn't count)
                    v.check_external_change(cx);
                    if let Some(l) = mcp_label {
                        v.status = tf("status.mcp_edit", &[("label", &l)]).into();
                    }
                    // repaint while playing so the playhead/counter advance;
                    // also while a plugin editor is open so its native event
                    // queue gets serviced even when the app is idle
                    // keep repainting while a plugin editor is open so its
                    // platform events get pumped and user-close is noticed
                    let mut editor_resync = None;
                    if let Some(pw) = &v.plugin_window {
                        let _ = pw.service_platform_events();
                        // live param sync: forward the editor's edits into
                        // the playing instance (best-effort each tick).
                        // #190: only drain when the playing instance's
                        // mutex is free — `take_parameter_edits` empties
                        // the editor's queue, so draining before a failed
                        // try_lock would DROP the edits instead of
                        // deferring them; left queued they simply retry
                        // on the next tick.
                        if let Some((d, editor)) = v.editor_plugin.clone() {
                            let mut applied_edits = 0usize;
                            if let Some(slot) = v.plugin_slots.get(&d) {
                                if let Ok(mut p) = slot.plugin.try_lock() {
                                    let edits = editor
                                        .lock()
                                        .map(|mut e| e.take_parameter_edits())
                                        .unwrap_or_default();
                                    applied_edits = edits.len();
                                    for ed in edits {
                                        if let Some(val) = ed.value {
                                            let _ = p.set_parameter(ed.id, val);
                                        }
                                    }
                                }
                            }
                            if applied_edits > 0 {
                                // the playing slot's state changed — queue a
                                // capture; the throttled flush below bounds
                                // the write rate while a knob is dragged
                                v.pending_state_capture.insert(d);
                            }
                            // the editor instance's own restartComponent drain:
                            // a preset/state change made inside the plugin's
                            // GUI arrives as kParamValuesChanged, not as
                            // parameter edits — push the editor's state into
                            // the playing instance so they agree. (Drained
                            // with take_restart_flags, not serviced: a
                            // GUI-only instance has no processing lifecycle.
                            // This is its control thread — the UI thread that
                            // loaded it.)
                            let flags = editor
                                .lock()
                                .map(|mut e| e.take_restart_flags())
                                .unwrap_or_default();
                            let mut resync = false;
                            for note in output::restart_notes(flags) {
                                match note {
                                    output::RestartNote::ParamValues
                                    | output::RestartNote::ParamTitles => resync = true,
                                    _ => {
                                        if v
                                            .restart_logged
                                            .entry(d)
                                            .or_default()
                                            .first_seen(note)
                                        {
                                            tracing::info!(
                                                "editor instance (dest {d}): {} noted",
                                                note.name()
                                            );
                                        }
                                    }
                                }
                            }
                            if resync {
                                editor_resync = Some((d, editor));
                            }
                        }
                        if pw.closed_by_user() {
                            v.plugin_window = None;
                            v.sync_editor_state_into_slot();
                        }
                    }
                    if let Some((d, editor)) = editor_resync {
                        v.push_editor_state(d, &editor);
                    }
                    if !v.pending_state_capture.is_empty() {
                        v.flush_plugin_states(false);
                    }
                    if dirty
                        || plugin_changed
                        || ports_changed
                        || v.playback.is_some()
                        || v.plugin_window.is_some()
                    {
                        cx.notify();
                    }
                });
                // #197: file paths a secondary instance forwarded. Outside
                // the update above so `update_in` can borrow the window:
                // each path goes through the same guarded open drag/drop
                // uses (the discard prompt applies), then the window is
                // foregrounded. An empty queue costs nothing.
                let forwarded = match &pending_opens {
                    Some(q) => {
                        let mut q = q.lock().unwrap_or_else(|e| e.into_inner());
                        std::mem::take(&mut *q)
                    }
                    None => Vec::new(),
                };
                if !forwarded.is_empty() {
                    let opened = this.update_in(cx, |v, w, cx| {
                        for p in forwarded {
                            v.confirm_discard_or_save(PendingAction::OpenPath(p), w, cx);
                        }
                        // best-effort foreground; activate cannot fail
                        w.activate_window();
                    });
                    if opened.is_err() {
                        tracing::warn!("single-instance hand-off arrived after the window closed");
                    }
                }
            } else {
                break;
            }
        }
    })
    .detach();
}

impl EditorView {
    /// New untitled document in place.
    pub(crate) fn new_file(&mut self, cx: &mut Context<Self>) {
        // an armed recording belongs to the document being replaced — drop
        // it with a warning instead of silently losing the take
        let rec_discarded = self.rec.take().is_some();
        // the outgoing document's identity, captured before the swap — its
        // snapshots stop applying once the doc is replaced (#174)
        let outgoing = lock_shared(&self.shared).path.clone();
        self.stop_playback();
        // the outgoing song keeps its plugin state — flush before the path
        // and the state table are dropped with the document
        self.flush_plugin_states(true);
        self.plugin_states = plugin_state::PluginStateStore::default();
        self.state_file_dirty = false;
        self.state_path_written = None;
        self.pending_state_capture.clear();
        mcp_server::service::swap_document(&self.shared, empty_doc(), None);
        self.last_take = None;
        self.punch_in = None;
        self.punch_out = None;
        self.doc_epoch += 1; // the fresh document reports revision 0 again
        self.selection.clear();
        self.drag = None;
        self.erase_ids.clear();
        self.mouse_pos = None;
        self.sel_track = 0;
        self.enc_override = None;
        self.play_us = 0;
        self.play_start_us = 0;
        self.refresh_derived();
        self.reset_view_to_content();
        // clear the replaced document's snapshots — but only its own:
        // snapshots belonging to other songs (e.g. a "Later" dismissal at
        // startup) must survive (#174). An untitled outgoing doc keeps its
        // lineage — it cannot be told apart from other untitled snapshots,
        // and conserving them is the safe side.
        if let Some(p) = &outgoing {
            recovery::clear_recovery_for(Some(p));
        }
        // untitled has no backing file to watch
        self.file_stamp = None;
        self.ext_prompted = false;
        self.status = if rec_discarded {
            format!("{} — {}", t("status.new_doc"), t("status.rec_discarded")).into()
        } else {
            t("status.new_doc").into()
        };
        cx.notify();
    }

    /// Adopt a recovery snapshot in place: swaps in its document, keeps the
    /// source path so an explicit Save writes back to it — but marks the
    /// doc dirty via an unreachable saved_revision so nothing is written
    /// until the user says so.
    pub(crate) fn restore_snapshot(
        &mut self,
        meta: &recovery::SnapshotMeta,
        payload: &[u8],
        cx: &mut Context<Self>,
    ) {
        match smf_core::parse_with_limits(payload, &smf_core::Limits::from_env()) {
            Ok(file) => {
                let rec_discarded = self.rec.take().is_some();
                self.stop_playback();
                {
                    let mut sh = lock_shared(&self.shared);
                    sh.doc = Document::from_file(file);
                    sh.undo = UndoStack::new(512);
                    if sh.path.is_none() {
                        sh.path = meta.source_path.clone();
                    }
                    // an unreachable marker: dirty until a verified save,
                    // matching "recovery never overwrites without Save"
                    sh.saved_revision = u64::MAX;
                }
                self.reset_view_for_new_doc();
                if let Some(src) = &meta.source_path {
                    self.apply_prefs(src);
                }
                let mut status = t("recovery.restored").to_string();
                if rec_discarded {
                    status = format!("{status} — {}", t("status.rec_discarded"));
                }
                self.status = status.into();
            }
            // corrupt payload — fail safely, keep the file for diagnosis
            Err(e) => self.status = tf("recovery.failed", &[("e", &e.to_string())]).into(),
        }
        cx.notify();
    }

    pub(crate) fn set_enc(&mut self, enc: Option<smf_core::TextEncoding>, cx: &mut Context<Self>) {
        self.enc_override = enc;
        self.ev_key = (u64::MAX, u64::MAX, usize::MAX, u64::MAX); // force event-row rebuild
        self.refresh_derived();
        self.persist();
        cx.notify();
    }

    pub(crate) fn pick_default_track(&self) -> usize {
        self.doc(|d| {
            d.tracks
                .iter()
                .position(|t| {
                    t.events
                        .iter()
                        .any(|e| matches!(e.kind, EventKind::Channel { .. }))
                })
                .unwrap_or(0)
        })
    }

    pub(crate) fn undo(&mut self, cx: &mut Context<Self>) {
        let arc = self.shared.clone();
        let mut sh = lock_shared(&arc);
        // unified undo (#204): document transactions and session changes
        // (routing/mute/solo) revert in the order the user made them
        if let Some((session, label)) = sh.undo_any() {
            self.status = tf("status.undo", &[("label", &label)]).into();
            self.selection.clear();
            if session {
                // a sidecar change: rewrite it and refresh the live mix
                drop(sh);
                self.persist();
            } else {
                self.refresh_derived_sh(&mut sh);
                drop(sh);
            }
            // undo reaches the running pass too (#141)
            self.refresh_live_schedule();
            cx.notify();
        }
    }

    pub(crate) fn redo(&mut self, cx: &mut Context<Self>) {
        let arc = self.shared.clone();
        let mut sh = lock_shared(&arc);
        if let Some((session, label)) = sh.redo_any() {
            self.status = tf("status.redo", &[("label", &label)]).into();
            self.selection.clear();
            if session {
                drop(sh);
                self.persist();
            } else {
                self.refresh_derived_sh(&mut sh);
                drop(sh);
            }
            self.refresh_live_schedule();
            cx.notify();
        }
    }

    pub(crate) fn save(&mut self, cx: &mut Context<Self>) {
        if self.save_rx.is_some() {
            return; // one save in flight — a second Ctrl+S isn't queued
        }
        // check the backing file's identity before touching it — a save
        // must never silently overwrite somebody else's changes
        let path = lock_shared(&self.shared).path.clone();
        if let Some(p) = &path {
            match watch::check_file(p, self.file_stamp) {
                watch::FileEvent::Unchanged => {}
                // timestamp/size drifted but content is identical — safe
                // to write, just adopt the fresh stamp
                watch::FileEvent::Touched(s) => self.file_stamp = Some(s),
                ev => {
                    self.prompt_save_conflict(p.clone(), ev, cx);
                    return;
                }
            }
        }
        self.write_to(path.as_deref(), cx);
    }

    /// The actual write shared by save() and the conflict resolutions —
    /// the stamp re-check is skipped because the caller already decided
    /// to write (Overwrite/Recreate resolutions).
    pub(crate) fn write_file(&mut self, p: &std::path::Path, cx: &mut Context<Self>) {
        self.write_to(Some(p), cx)
    }

    /// The same persistence core MCP save uses: snapshot under a short
    /// lock, then serialize+write on a worker so a slow save never
    /// freezes the UI. `file_stamp` re-baselines only after the durable
    /// replace lands, so a failed write can't make the next conflict
    /// check blind. `p == None` falls back to the document's own path.
    pub(crate) fn write_to(&mut self, p: Option<&std::path::Path>, cx: &mut Context<Self>) {
        let req = mcp_server::service::SaveRequest {
            path: p,
            ..Default::default()
        };
        match mcp_server::service::begin_save(&self.shared, req) {
            Ok(ticket) => {
                // non-modal progress only when the save can be perceptible
                if ticket.event_count() > 100_000 {
                    self.status = t("status.saving").into();
                }
                let shared = self.shared.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx
                        .send(mcp_server::service::finish_save(ticket).map_err(|e| e.to_string()));
                    drop(shared);
                });
                self.save_rx = Some(rx);
            }
            Err(mcp_server::service::SaveError::NoPath) => {
                self.save_as(cx);
                return;
            }
            Err(e) => {
                tracing::error!(path = ?p, error = %e, "save failed");
                self.status = format!("{e}").into();
            }
        }
        cx.notify();
    }

    /// Synchronous save to the current backing path through the same shared
    /// save core — used by the discard guard, which must know the write
    /// finished before it proceeds. Returns false — leaving the document
    /// dirty — when there is no path (callers route through a Save-As
    /// prompt) or the write fails; the caller still sees the error in the
    /// status line.
    pub(crate) fn try_save(&mut self, cx: &mut Context<Self>) -> bool {
        let path = lock_shared(&self.shared).path.clone();
        let ok = match mcp_server::service::save_document(&self.shared, Default::default()) {
            Ok(_) => {
                self.status = t("status.saved").into();
                self.persist();
                // a verified save resolves this document's own snapshots
                // only — other songs' stay (#174)
                recovery::clear_recovery_for(path.as_deref());
                if let Some(p) = &path {
                    self.file_stamp = watch::stat_file(p);
                }
                self.ext_prompted = false;
                true
            }
            Err(mcp_server::service::SaveError::NoPath) => false,
            Err(e) => {
                self.status = format!("{e}").into();
                false
            }
        };
        cx.notify();
        ok
    }

    /// Synchronous save to an explicit target — the guard's picked
    /// Save-As path. The canonical path adopts only after the write
    /// lands; a failed Save-As leaves the old one alone (#158).
    pub(crate) fn try_save_to(&mut self, path: &std::path::Path, cx: &mut Context<Self>) -> bool {
        // the identity the document had while dirty — captured before the
        // Save-As adopts the new path (#174)
        let old = lock_shared(&self.shared).path.clone();
        let ok = match mcp_server::service::save_document(
            &self.shared,
            mcp_server::service::SaveRequest {
                path: Some(path),
                ..Default::default()
            },
        ) {
            Ok(out) => {
                self.adopt_saved_path(&out.path);
                self.status = t("status.saved").into();
                self.persist();
                recovery::clear_recovery_for(old.as_deref());
                self.file_stamp = watch::stat_file(&out.path);
                self.ext_prompted = false;
                true
            }
            Err(e) => {
                self.status = format!("{e}").into();
                false
            }
        };
        cx.notify();
        ok
    }

    /// The guard's save step — `NeedsPath` sends the flow through a
    /// Save-As prompt instead of writing silently.
    pub(crate) fn save_for_guard(&mut self, cx: &mut Context<Self>) -> guard::SaveOutcome {
        if lock_shared(&self.shared).path.is_none() {
            return guard::SaveOutcome::NeedsPath;
        }
        if self.try_save(cx) {
            guard::SaveOutcome::Saved
        } else {
            guard::SaveOutcome::Failed
        }
    }

    /// Run an action the discard guard cleared (or that never needed it).
    pub(crate) fn perform_pending(&mut self, action: PendingAction, cx: &mut Context<Self>) {
        match action {
            PendingAction::NewFile => self.new_file(cx),
            PendingAction::OpenDialog => self.open_dialog(cx),
            PendingAction::OpenPath(p) => self.open(p, cx),
            PendingAction::CloseWindow => {
                self.close_confirmed = true;
                if let Some(wh) = self.window_handle {
                    wh.update(cx, |_, w, _app| w.remove_window()).ok();
                }
            }
        }
    }

    /// The save-time conflict prompt: Reload (take the disk version,
    /// discarding local changes), Save As (keep local under a new path),
    /// Overwrite (explicit — destroy the external change), or Cancel.
    pub(crate) fn prompt_save_conflict(
        &mut self,
        p: PathBuf,
        ev: watch::FileEvent,
        cx: &mut Context<Self>,
    ) {
        if self.prompt_active {
            self.status = t("watch.save_blocked").into();
            cx.notify();
            return;
        }
        let Some(wh) = self.window_handle else {
            self.status = t("watch.save_blocked").into();
            cx.notify();
            return;
        };
        let name = p.display().to_string();
        let (title, detail, answers) = match ev {
            watch::FileEvent::Missing => (
                t("watch.missing_title").to_string(),
                tf("watch.missing_detail", &[("p", &name)]),
                vec![
                    PromptButton::Ok(t("watch.save_as").into()),
                    PromptButton::Other(t("watch.recreate").into()),
                    PromptButton::Cancel(t("watch.cancel").into()),
                ],
            ),
            _ => (
                t("watch.changed_title").to_string(),
                tf("watch.changed_detail", &[("p", &name)]),
                vec![
                    PromptButton::Other(t("watch.reload").into()),
                    PromptButton::Ok(t("watch.save_as").into()),
                    PromptButton::Other(t("watch.overwrite").into()),
                    PromptButton::Cancel(t("watch.cancel").into()),
                ],
            ),
        };
        self.prompt_active = true;
        // open the prompt from a deferred task: save() is invoked inside
        // the window's own event-handler update, and nesting a window
        // update there is rejected — spawning moves it outside the handler
        cx.spawn(async move |this, cx| {
            let rx = wh
                .update(cx, |_, w, app| {
                    w.prompt(PromptLevel::Warning, &title, Some(&detail), &answers, app)
                })
                .ok();
            let Some(rx) = rx else {
                this.update(cx, |v, cx| {
                    v.prompt_active = false;
                    v.status = t("watch.save_blocked").into();
                    cx.notify();
                })
                .ok();
                return;
            };
            let idx = rx.await.unwrap_or(usize::MAX);
            this.update(cx, |v, cx| {
                v.prompt_active = false;
                v.resolve_save_conflict(&p, ev, idx, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Apply the save-conflict answer. Index follows the button order built
    /// in `prompt_save_conflict`; any unexpected index is Cancel.
    pub(crate) fn resolve_save_conflict(
        &mut self,
        p: &Path,
        ev: watch::FileEvent,
        idx: usize,
        cx: &mut Context<Self>,
    ) {
        match ev {
            // [Reload, Save As, Overwrite, Cancel]
            watch::FileEvent::Modified => match idx {
                // Reload = the only path that resets undo/history — and it
                // only happens through this explicit choice
                0 => self.open(p.to_path_buf(), cx),
                1 => self.save_as(cx),
                2 => self.write_file(p, cx),
                _ => {
                    self.status = t("watch.save_cancelled").into();
                    cx.notify();
                }
            },
            // [Save As, Recreate, Cancel]
            watch::FileEvent::Missing => match idx {
                0 => self.save_as(cx),
                1 => self.write_file(p, cx),
                _ => {
                    self.status = t("watch.save_cancelled").into();
                    cx.notify();
                }
            },
            _ => {}
        }
    }

    /// While-open poll, called from the doc-watch loop (~2s cadence).
    /// Surfaces external modification or deletion with a prompt; `Touched`
    /// just re-baselines silently. One prompt per episode — `ext_prompted`
    /// releases only when the stamp is re-baselined by open/save.
    pub(crate) fn check_external_change(&mut self, cx: &mut Context<Self>) {
        if self.last_ext_check.elapsed() < std::time::Duration::from_secs(2) {
            return;
        }
        self.last_ext_check = std::time::Instant::now();
        if self.ext_prompted || self.prompt_active {
            return;
        }
        let Some(p) = lock_shared(&self.shared).path.clone() else {
            return;
        };
        match watch::check_file(&p, self.file_stamp) {
            watch::FileEvent::Unchanged => {}
            watch::FileEvent::Touched(s) => self.file_stamp = Some(s),
            ev => {
                self.ext_prompted = true;
                self.prompt_ext_change(p, ev, cx);
            }
        }
    }

    /// The while-open notice. Clean doc: Enter=Reload is safe (nothing is
    /// lost). Dirty doc: Enter=Keep Editing — Reload stays available but
    /// can't be triggered by a reflex Enter.
    pub(crate) fn prompt_ext_change(
        &mut self,
        p: PathBuf,
        ev: watch::FileEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(wh) = self.window_handle else {
            self.ext_prompted = false;
            return;
        };
        let dirty = {
            let sh = lock_shared(&self.shared);
            sh.is_dirty()
        };
        let name = p.display().to_string();
        let (title, detail, answers) = match (ev, dirty) {
            (watch::FileEvent::Missing, _) => (
                t("watch.missing_title").to_string(),
                tf("watch.missing_open_detail", &[("p", &name)]),
                vec![
                    PromptButton::Ok(t("watch.keep").into()),
                    PromptButton::Other(t("watch.save_as").into()),
                ],
            ),
            (watch::FileEvent::Modified, false) => (
                t("watch.changed_title").to_string(),
                tf("watch.changed_open_detail", &[("p", &name)]),
                vec![
                    PromptButton::Ok(t("watch.reload").into()),
                    PromptButton::Other(t("watch.keep").into()),
                ],
            ),
            _ => (
                t("watch.changed_title").to_string(),
                tf("watch.changed_dirty_detail", &[("p", &name)]),
                vec![
                    PromptButton::Ok(t("watch.keep").into()),
                    PromptButton::Other(t("watch.reload").into()),
                ],
            ),
        };
        let Ok(rx) = wh.update(cx, |_, w, app| {
            w.prompt(PromptLevel::Warning, &title, Some(&detail), &answers, app)
        }) else {
            self.ext_prompted = false;
            return;
        };
        self.prompt_active = true;
        cx.spawn(async move |this, cx| {
            let idx = rx.await.unwrap_or(usize::MAX);
            this.update(cx, |v, cx| {
                v.prompt_active = false;
                v.resolve_ext_change(&p, ev, dirty, idx, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Index meaning depends on the button order chosen by
    /// `prompt_ext_change`: Missing = [Keep, Save As]; clean-modified =
    /// [Reload, Keep]; dirty-modified = [Keep, Reload].
    pub(crate) fn resolve_ext_change(
        &mut self,
        p: &Path,
        ev: watch::FileEvent,
        dirty: bool,
        idx: usize,
        cx: &mut Context<Self>,
    ) {
        match ev {
            watch::FileEvent::Missing => {
                if idx == 1 {
                    self.save_as(cx);
                }
            }
            watch::FileEvent::Modified => {
                let reload = if dirty { idx == 1 } else { idx == 0 };
                if reload {
                    self.open(p.to_path_buf(), cx);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn save_as(&mut self, cx: &mut Context<Self>) {
        // Windows document conventions (#158): the dialog opens in the
        // current document's folder (else the last-used one) and suggests
        // the current file name — not a fresh "untitled".
        let (dir, name) = self.save_dialog_start();
        #[cfg(windows)]
        {
            // real "MIDI files / All files" filter + default extension —
            // gpui's prompt API cannot express a filter list
            let hwnd = self.dialog_hwnd(cx);
            self.dlg_rx = Some((crate::filedlg::save_path(hwnd, dir, name), true));
            cx.notify();
        }
        #[cfg(not(windows))]
        {
            let rx = cx.prompt_for_new_path(&dir, Some(&name));
            cx.spawn(async move |this, cx| {
                if let Ok(Ok(Some(path))) = rx.await {
                    if let Some(this) = this.upgrade() {
                        this.update(cx, |v, cx| {
                            let mut path = path;
                            if path.extension().is_none() {
                                path.set_extension("mid");
                            }
                            // write to the picked target WITHOUT adopting
                            // it — the canonical path only moves when the
                            // write actually commits (adopt_saved_path)
                            v.write_to(Some(&path), cx);
                        });
                    }
                }
            })
            .detach();
        }
    }

    /// Owner HWND for the native file dialogs, as a Send-able usize.
    pub(crate) fn dialog_hwnd(&mut self, cx: &mut Context<Self>) -> usize {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        self.window_handle
            .and_then(|wh| {
                wh.update(cx, |_, w, _| match w.window_handle().map(|h| h.as_raw()) {
                    Ok(RawWindowHandle::Win32(h)) => h.hwnd.get() as usize,
                    _ => 0,
                })
                .ok()
            })
            .unwrap_or(0)
    }

    /// Consume a native-dialog result picked in `save_as`/`open_dialog`.
    pub(crate) fn finish_dialog(
        &mut self,
        path: std::path::PathBuf,
        saving: bool,
        cx: &mut Context<Self>,
    ) {
        if saving {
            let mut path = path;
            if path.extension().is_none() {
                path.set_extension("mid");
            }
            // write to the picked target WITHOUT adopting it — the
            // canonical path only moves when the write commits (#158)
            self.write_to(Some(&path), cx);
        } else {
            self.open(path, cx);
        }
    }

    /// Save-As dialog conventions (#158): the document's folder (else the
    /// last-used one) and its current file name — the guard's prompt uses
    /// the same start point.
    pub(crate) fn save_dialog_start(&self) -> (std::path::PathBuf, String) {
        let sh = lock_shared(&self.shared);
        let dir = sh
            .path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .or_else(|| self.last_dir.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let name = sh
            .path
            .as_ref()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "untitled.mid".to_string());
        (dir, name)
    }

    /// Adopt a just-committed save target as the document's canonical path
    /// and remember its folder for the next dialog (#158).
    pub(crate) fn adopt_saved_path(&mut self, p: &std::path::Path) {
        {
            let mut sh = lock_shared(&self.shared);
            sh.path = Some(p.to_path_buf());
        }
        if let Some(dir) = p.parent() {
            if self.last_dir.as_deref() != Some(dir) {
                self.last_dir = Some(dir.to_path_buf());
                self.save_global();
            }
        }
    }

    /// #162 — Format 0 multichannel import choice. Keep is the default
    /// (non-destructive, byte preservation untouched); split runs one
    /// explicit document transaction that converts to Format 1 with one
    /// track per used channel. The prompt is deferred off the open handler
    /// like the save-conflict prompt — `window.prompt` cannot nest inside
    /// a view update.
    fn prompt_fmt0_split(&mut self, cx: &mut Context<Self>) {
        if self.prompt_active {
            return;
        }
        let Some(wh) = self.window_handle else {
            return;
        };
        self.prompt_active = true;
        cx.spawn(async move |this, cx| {
            let rx = wh
                .update(cx, |_, w, app| {
                    w.prompt(
                        PromptLevel::Info,
                        t("import.fmt0_title"),
                        Some(t("import.fmt0_detail")),
                        &[
                            PromptButton::Ok(t("import.fmt0_keep").into()),
                            PromptButton::Other(t("import.fmt0_split").into()),
                            PromptButton::Cancel(t("guard.cancel").into()),
                        ],
                        app,
                    )
                })
                .ok();
            let idx = match rx {
                Some(rx) => rx.await.unwrap_or(usize::MAX),
                None => usize::MAX,
            };
            if idx == 1 {
                this.update(cx, |v, cx| {
                    let before = v.doc(|d| d.tracks.len());
                    let ops = v.doc(|d| d.split_fmt0_by_channel_ops());
                    if !ops.is_empty() {
                        v.apply_tx("split channels to Format 1", ops);
                        // each split track edits its own channel by default
                        let chans: Vec<u8> = v.doc(|d| {
                            (before..d.tracks.len())
                                .map(|i| d.tracks[i].out_channel)
                                .collect()
                        });
                        for (off, ch) in chans.into_iter().enumerate() {
                            v.edit_ch.insert(before + off, ch);
                        }
                        v.sel_track = 1;
                        v.persist();
                        cx.notify();
                    }
                })
                .ok();
            }
            this.update(cx, |v, _cx| v.prompt_active = false).ok();
        })
        .detach();
    }

    /// Help → Open Logs: reveal the rolling log directory in Explorer.
    pub(crate) fn open_logs(&mut self, cx: &mut Context<Self>) {
        let dir = diagnostics::log_dir();
        if std::fs::create_dir_all(&dir).is_ok() {
            diagnostics::open_in_explorer(&dir);
            self.status = tf("status.logs_dir", &[("p", &dir.display().to_string())]).into();
        } else {
            self.status = t("status.logs_open_failed").into();
        }
        cx.notify();
    }

    /// Help → Export Diagnostics Bundle: sanitized env facts + redacted
    /// tails of the retained logs, one attachable text file.
    pub(crate) fn export_diagnostics(&mut self, cx: &mut Context<Self>) {
        let (dests, mcp_auth) = {
            let sh = lock_shared(&self.shared);
            (
                sh.dests
                    .iter()
                    .map(|(_, d)| format!("{d:?}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                std::env::var("MIDI_MCP_TOKEN").is_ok(),
            )
        };
        let hd = output::host_diag();
        let host_lines = format!(
            "app_version={}\naudio_device={:?}\nhelper={:?}\nprobe={:?}\ndests={}\nmcp_auth={}\ncount_in={} midi_in={}",
            env!("CARGO_PKG_VERSION"),
            hd.audio_device,
            hd.helper,
            hd.probe,
            dests,
            mcp_auth,
            self.count_in_bars,
            self.midi_in,
        );
        let dir = diagnostics::app_data_dir().join("diagnostics");
        match diagnostics::export_bundle(&dir, &host_lines) {
            Ok(p) => {
                self.status =
                    tf("status.bundle_written", &[("p", &p.display().to_string())]).into();
                diagnostics::reveal_file(&p);
            }
            Err(e) => {
                tracing::error!(error = %e, "diagnostics bundle export failed");
                self.status = tf("status.bundle_failed", &[("e", &e.to_string())]).into();
            }
        }
        cx.notify();
    }

    /// Open dialog start folder (#158): the document's folder, else the
    /// last-used one, else the working directory.
    pub(crate) fn open_dialog_start(&self) -> std::path::PathBuf {
        let sh = lock_shared(&self.shared);
        sh.path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .or_else(|| self.last_dir.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
    }

    pub(crate) fn open_dialog(&mut self, cx: &mut Context<Self>) {
        #[cfg(windows)]
        {
            // native IFileOpenDialog: MIDI filter + start folder (#158)
            let hwnd = self.dialog_hwnd(cx);
            let dir = self.open_dialog_start();
            self.dlg_rx = Some((crate::filedlg::open_path(hwnd, dir), false));
            cx.notify();
        }
        #[cfg(not(windows))]
        {
            let rx = cx.prompt_for_paths(PathPromptOptions {
                files: true,
                directories: false,
                multiple: false,
                prompt: None,
            });
            cx.spawn(async move |this, cx| {
                if let Ok(Ok(Some(paths))) = rx.await {
                    if let Some(p) = paths.into_iter().next() {
                        if let Some(this) = this.upgrade() {
                            this.update(cx, |v, cx| v.open(p, cx));
                        }
                    }
                }
            })
            .detach();
        }
    }

    /// Post-swap view reset shared by open() and snapshot restore:
    /// everything that referenced the old document is cleared or rebuilt.
    pub(crate) fn reset_view_for_new_doc(&mut self) {
        // the new document also reports revision 0 — bump the epoch
        // so revision-keyed derived views cannot stay stale
        self.doc_epoch += 1;
        self.sel_track = self.pick_default_track();
        self.selection.clear();
        self.drag = None;
        self.erase_ids.clear();
        self.mouse_pos = None;
        self.enc_override = None;
        self.play_us = 0;
        self.play_start_us = 0;
        {
            let mut sh = lock_shared(&self.shared);
            sh.muted.clear();
            sh.soloed.clear();
            sh.track_dest.clear();
        }
        // rebuild the derived views, then land the view on the new content
        self.refresh_derived();
        self.reset_view_to_content();
    }

    pub(crate) fn open(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        match mcp_server::service::load_document(&path) {
            Ok((d, load_warnings)) => {
                tracing::info!(
                    path = %path.display(),
                    warnings = load_warnings.len(),
                    "document opened"
                );
                // an armed recording belongs to the previous document —
                // drop it with a warning instead of silently losing the take
                let rec_discarded = self.rec.take().is_some();
                // the replaced document's identity, captured before the swap —
                // its snapshots stop applying once it is replaced (#174)
                let outgoing = lock_shared(&self.shared).path.clone();
                self.stop_playback();
                // flush plugin state while the outgoing song's path (and its
                // state file) is still the active one — `apply_prefs` loads
                // the incoming song's table after the swap
                self.flush_plugin_states(true);
                self.last_take = None;
                // sidecar-less file: a stale punch from the previous doc
                // must not carry over (apply_prefs early-returns then)
                self.punch_in = None;
                self.punch_out = None;
                // #162: a format-0 file holding several channels gets an
                // explicit keep/split choice after the swap settles
                let fmt0_multi = d.format == 0 && d.channels_used().len() > 1;
                // swap the document in place — the MCP server holds this same Arc
                mcp_server::service::swap_document(&self.shared, d, Some(path.clone()));
                // the new document also reports revision 0 — bump the epoch
                // so revision-keyed derived views cannot stay stale
                self.doc_epoch += 1;
                self.sel_track = self.pick_default_track();
                self.selection.clear();
                self.drag = None;
                self.erase_ids.clear();
                self.mouse_pos = None;
                self.enc_override = None;
                self.play_us = 0;
                self.play_start_us = 0;
                // rebuild the derived views, then land the view on the new
                // content — a saved per-file sidecar (applied next) overrides
                self.refresh_derived();
                self.reset_view_to_content();
                // the freshly-opened file is the new identity baseline
                self.file_stamp = watch::stat_file(&path);
                self.ext_prompted = false;
                let pref_diags = self.apply_prefs(&path);
                self.push_recent(&path);
                // the opened file supersedes its own stale snapshots and the
                // replaced document's — other songs' snapshots survive (#174).
                // An untitled outgoing doc keeps its lineage: it cannot be
                // told apart from other untitled snapshots, and conserving
                // them is the safe side.
                if let Some(prev) = &outgoing {
                    recovery::clear_recovery_for(Some(prev.as_path()));
                }
                recovery::clear_recovery_for(Some(path.as_path()));
                let mut status = if load_warnings.is_empty() {
                    t("status.loaded").to_string()
                } else {
                    tf(
                        "status.loaded_warn",
                        &[
                            ("n", &load_warnings.len().to_string()),
                            ("w", &load_warnings.join("; ")),
                        ],
                    )
                };
                if !pref_diags.is_empty() {
                    status = format!(
                        "{status} — {}",
                        tf("status.prefs_warn", &[("e", &pref_diags.join("; "))])
                    );
                }
                if rec_discarded {
                    status = format!("{status} — {}", t("status.rec_discarded"));
                }
                self.status = status.into();
                // opening adopts the folder as the last-used one (#158)
                if let Some(dir) = path.parent() {
                    if self.last_dir.as_deref() != Some(dir) {
                        self.last_dir = Some(dir.to_path_buf());
                        self.save_global();
                    }
                }
                if fmt0_multi {
                    self.prompt_fmt0_split(cx);
                }
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "open failed");
                self.status = tf("status.load_failed", &[("e", &e.to_string())]).into();
            }
        }
        cx.notify();
    }
}

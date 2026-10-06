//! VST3 plugin hosting on the UI side: scan/rescan, catalog, GUI open,
//! per-destination state capture/restore/flush, and audio routing picks.
//! Live plugin instances always live on the host worker thread — this
//! module only posts requests and drains events on the shared state.

use super::*;

/// Does the companion state file need a write now? Dirty records always
/// write (`plugin_states.save` no-ops for an empty store, so no file is
/// created for songs without plugins). A clean store still writes when its
/// companion file was last written for a different path — Save As renamed
/// the song, and the new name must not silently lose every instrument
/// patch (#175).
fn needs_state_write(dirty: bool, written_for: Option<&Path>, target: &Path) -> bool {
    dirty || written_for != Some(target)
}

pub(crate) enum PluginState {
    Loading {
        path: PathBuf,
        since: std::time::Instant,
    },
    Ready {
        path: PathBuf,
    },
    Failed {
        path: PathBuf,
        phase: &'static str,
        msg: String,
    },
}

/// How a plugin (re)scan treats the persistent scan cache.
pub(crate) enum ScanMode {
    /// Serve unchanged bundles from the cache; probe only new/changed paths.
    Changed,
    /// Ignore the cache entirely and re-probe every bundle found.
    All,
    /// Force-re-probe one bundle (a quarantined plugin the user retried).
    Retry(PathBuf),
}

/// What `ensure_plugin` should do for a destination — the load/unload
/// decision lifted out of the UI so the ordering rules are unit-testable
/// without a plugin (or a window).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PluginPlan {
    /// the wanted bundle is already resident in the slot — nothing to do
    Satisfied,
    /// same bundle still loading, or a failed load we may not retry yet
    Wait,
    /// a load must be issued; `retire` when a warm slot is evicted first
    Open { retire: bool },
}

/// `state` is the tracked lifecycle state for the index, `warm` the bundle
/// currently resident in its slot (if any), `target` the bundle the
/// destination now points at. Order of checks matters: a Ready+resident
/// match short-circuits before the wait guards, and any other mismatch
/// reloads — retiring the stale slot first so the host never holds two
/// instances for one index.
pub(crate) fn plugin_plan(
    state: Option<&PluginState>,
    warm: Option<&Path>,
    target: &Path,
    force: bool,
) -> PluginPlan {
    if let Some(PluginState::Ready { path }) = state {
        if path == target && warm == Some(target) {
            return PluginPlan::Satisfied;
        }
    } else if warm == Some(target) {
        return PluginPlan::Satisfied;
    }
    match state {
        Some(PluginState::Loading { path, .. }) if path == target => PluginPlan::Wait,
        Some(PluginState::Failed { path, .. }) if path == target && !force => PluginPlan::Wait,
        _ => PluginPlan::Open {
            retire: warm.is_some(),
        },
    }
}

impl EditorView {
    /// If dest `d` is a VST3 bundle not already loaded/loading, ask the host
    /// thread to warm it. Called when a destination is assigned and from
    /// `refresh_plugins` — Play then never pays the load stall.
    pub(crate) fn ensure_plugin(&mut self, d: usize, force: bool) {
        let path = {
            let sh = lock_shared(&self.shared);
            match sh.dests.get(d).map(|(_, dest)| dest) {
                Some(output::Destination::Plugin { plugin_path, .. }) => PathBuf::from(plugin_path),
                _ => return,
            }
        };
        let PluginPlan::Open { retire } = plugin_plan(
            self.plugin_state.get(&d),
            self.plugin_slots.get(&d).map(|s| s.path.as_path()),
            &path,
            force,
        ) else {
            return;
        };
        // index now points at a different bundle — retire the old instance,
        // capturing its state first so a re-point doesn't lose the patch.
        // #190: blocking capture is deliberate (once-only retire of a slot
        // dropped right after — a skip here would lose the patch, #175)
        if retire && self.plugin_slots.contains_key(&d) {
            self.capture_plugin_state(d);
            self.state_restored.remove(&d);
            self.restart_logged.remove(&d);
            self.plugin_slots.remove(&d);
            let _ = self.plugin_req.send(output::PluginReq::Drop(d));
            // an audition sink bound to that slot is stale too
            if self.aud_ships.remove(&d) {
                self.audition.drop_sink(d);
            }
        }
        self.plugin_state.insert(
            d,
            PluginState::Loading {
                path: path.clone(),
                since: std::time::Instant::now(),
            },
        );
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.status = tf("plugin.loading", &[("name", name.as_str())]).into();
        let _ = self
            .plugin_req
            .send(output::PluginReq::Open(d, path, self.audio_sel.clone()));
    }

    /// Warm instances for every VST3 destination a track or the default
    /// currently resolves to. Cheap to call often — no-ops once satisfied.
    pub(crate) fn refresh_plugins(&mut self) {
        let idxs: Vec<usize> = {
            let sh = lock_shared(&self.shared);
            let mut v: Vec<usize> = sh.track_dest.values().copied().collect();
            v.push(sh.default_dest);
            v
        };
        for d in idxs {
            self.ensure_plugin(d, false);
        }
    }

    /// Apply + persist a new audio configuration for hosted plugins. Every
    /// live instance is reopened onto it (state is preserved across the
    /// reopen, so a rate/buffer/device change doesn't lose the program);
    /// loading/failed slots pick it up on their next open attempt.
    pub(crate) fn apply_audio_selection(&mut self, sel: output::AudioSelection) {
        self.audio_sel = sel;
        self.save_global();
        for d in self.plugin_slots.keys().copied().collect::<Vec<_>>() {
            self.ensure_plugin(d, true);
        }
    }

    pub(crate) fn poll_plugin_events(&mut self) -> bool {
        let mut changed = false;
        let now = std::time::Instant::now();
        // hot-plug: re-enumerate output devices every few seconds so the
        // audio settings panel tracks additions/removals while it's open
        if now.duration_since(self.audio_devices_at) >= std::time::Duration::from_secs(3) {
            self.audio_devices = output::output_devices();
            self.audio_devices_at = now;
        }
        let timed_out: Vec<usize> = self.plugin_state.iter().filter_map(|(&d, s)| {
            matches!(s, PluginState::Loading { since, .. } if now.duration_since(*since) >= std::time::Duration::from_secs(20)).then_some(d)
        }).collect();
        for d in timed_out {
            if let Some(PluginState::Loading { path, .. }) = self.plugin_state.remove(&d) {
                tracing::warn!(dest = d, path = %path.display(), "plugin load timed out");
                self.plugin_state.insert(
                    d,
                    PluginState::Failed {
                        path,
                        phase: "load",
                        msg: t("plugin.timeout").to_string(),
                    },
                );
                let _ = self.plugin_req.send(output::PluginReq::Drop(d));
                if self.aud_ships.remove(&d) {
                    self.audition.drop_sink(d);
                }
                changed = true;
            }
        }
        while let Ok(event) = self.plugin_evt.try_recv() {
            let Some(PluginState::Loading { path, .. }) = self.plugin_state.get(&event.dest) else {
                continue;
            };
            if path != &event.path {
                continue;
            }
            match event.result {
                Ok(slot) => {
                    let name = slot
                        .path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    tracing::info!(dest = event.dest, plugin = %name, "plugin ready");
                    // fresh instance → fresh notification log
                    self.restart_logged.remove(&event.dest);
                    self.state_restored.remove(&event.dest);
                    self.plugin_slots.insert(event.dest, slot);
                    // saved sidecar state goes in before Ready — playback may
                    // start as soon as this slot reports ready
                    self.restore_plugin_state(event.dest);
                    self.plugin_state
                        .insert(event.dest, PluginState::Ready { path: event.path });
                    self.status = tf("plugin.ready", &[("name", name.as_str())]).into();
                }
                Err(e) => {
                    let phase = match e {
                        output::PluginError::Host(_) => "host",
                        output::PluginError::Load(_) => "load",
                        output::PluginError::Audio(_) => "audio",
                    };
                    let name = event
                        .path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    tracing::warn!(
                        dest = event.dest,
                        plugin = %name,
                        phase,
                        error = %e,
                        "plugin load failed"
                    );
                    self.state_restored.remove(&event.dest);
                    self.plugin_state.insert(
                        event.dest,
                        PluginState::Failed {
                            path: event.path,
                            phase,
                            msg: e.to_string(),
                        },
                    );
                    self.status = tf("plugin.failed", &[("name", name.as_str())]).into();
                }
            }
            changed = true;
        }
        // VST3 restart notifications (IComponentHandler::restartComponent):
        // serviced per ready slot each frame. service_host_requests runs the
        // VST3-required stop/deactivate/reactivate lifecycle on the isolated
        // helper's control thread and returns every flag raised; the audit
        // then reacts where the host tracks state (latency re-read, bus
        // re-query, component reload) and logs the rest once per instance.
        // try_lock keeps a busy audio block from stalling a UI frame — the
        // next frame picks the flags up.
        let mut drained: Vec<(usize, vst3_host::RestartFlags)> = Vec::new();
        // dest indexes whose stream errored mid-flight (device unplugged) —
        // reopened below, throttled so a device that never comes up doesn't
        // hot-loop stream builds
        let mut lost: Vec<(usize, String)> = Vec::new();
        for (d, slot) in &self.plugin_slots {
            if !matches!(self.plugin_state.get(d), Some(PluginState::Ready { .. })) {
                continue;
            }
            if let Some(err) = slot.take_stream_error() {
                lost.push((*d, err));
            }
            if let Some(flags) = slot
                .plugin
                .try_lock()
                .ok()
                .and_then(|mut p| p.service_host_requests().ok())
            {
                if !flags.is_empty() {
                    drained.push((*d, flags));
                }
            }
        }
        let mut reloads = Vec::new();
        for (d, flags) in drained {
            changed |= self.apply_restart_flags(d, flags, &mut reloads);
        }
        for d in reloads {
            self.ensure_plugin(d, true);
            changed = true;
        }
        // device loss: reopen — the backend falls back to the current
        // default when the selected device is gone, so this recovers onto
        // whatever output still exists instead of staying dead
        for (d, err) in lost {
            let cooled = self
                .audio_retry
                .get(&d)
                .map(|t| t.elapsed() >= std::time::Duration::from_secs(5))
                .unwrap_or(true);
            if !cooled {
                continue;
            }
            self.audio_retry.insert(d, std::time::Instant::now());
            self.status = tf("audio.device_lost", &[("e", err.as_str())]).into();
            self.ensure_plugin(d, true);
            // a running pass still holds the dead sink — flag the live
            // routing dirty so the pass relinks onto the recovered slot
            // below instead of playing into a closed stream until a manual
            // Stop/Play cycle (#191)
            if self.playback.is_some() {
                self.live_route_dirty = true;
            }
            changed = true;
        }
        if self.play_pending {
            let needed_loading = {
                let sh = lock_shared(&self.shared);
                sh.doc.tracks.iter().enumerate().any(|(t, _)| {
                    let d = sh.dest_of(t);
                    matches!(self.plugin_state.get(&d), Some(PluginState::Loading { .. }))
                })
            };
            if !needed_loading {
                self.play_pending = false;
                self.start_playback();
                changed = true;
            }
        }
        // a live routing refresh deferred while a plugin loaded — retry once
        // nothing needed is still on the way up
        if self.live_route_dirty && self.playback.is_some() {
            let still_loading = self
                .plugin_state
                .values()
                .any(|s| matches!(s, PluginState::Loading { .. }));
            if !still_loading {
                self.live_route_dirty = false;
                self.refresh_live_routing();
                changed = true;
            }
        }
        changed
    }

    /// Route one plugin's drained `restartComponent` flags through the
    /// notification audit (`output::restart_notes` → `restart_action`).
    /// Reactions needing a `&mut self` follow-up after the drain loop
    /// (instance reloads) are queued on `reloads` for the caller.
    pub(crate) fn apply_restart_flags(
        &mut self,
        d: usize,
        flags: vst3_host::RestartFlags,
        reloads: &mut Vec<usize>,
    ) -> bool {
        let mut changed = false;
        let name = self
            .plugin_slots
            .get(&d)
            .and_then(|s| s.path.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .unwrap_or_default();
        for note in output::restart_notes(flags) {
            match output::restart_action(note) {
                output::RestartAction::RefreshLatency => {
                    if let Some(slot) = self.plugin_slots.get(&d) {
                        let n = slot.refresh_latency();
                        tracing::info!("{name} (dest {d}): kLatencyChanged -> {n} samples");
                    }
                    changed = true;
                }
                output::RestartAction::RequeryIo => {
                    // the lifecycle already ran inside service_host_requests;
                    // nothing caches the layout, so re-query it for the log
                    let ch = self
                        .plugin_slots
                        .get(&d)
                        .and_then(|s| s.plugin.try_lock().ok().map(|p| p.output_channel_count()));
                    tracing::info!("{name} (dest {d}): kIoChanged -> {ch:?} output channel(s)");
                }
                output::RestartAction::Reload => {
                    tracing::info!("{name} (dest {d}): kReloadComponent -> reopening");
                    reloads.push(d);
                    changed = true;
                }
                output::RestartAction::LogOnly => {
                    if self.restart_logged.entry(d).or_default().first_seen(note) {
                        tracing::info!(
                            "{name} (dest {d}): {} noted — no host state to rebuild",
                            note.name()
                        );
                    }
                }
            }
        }
        changed
    }

    pub(crate) fn rescan_plugins(&mut self, mode: ScanMode) {
        self.status = t("status.scanning").into();
        let (tx, rx) = std::sync::mpsc::channel();
        self.scan_rx = Some(rx);
        let cache_file = scan_cache_path();
        let timeout = std::time::Duration::from_secs(self.probe_timeout_secs);
        let handle = std::thread::spawn(move || {
            let (all, retry) = match &mode {
                ScanMode::All => (true, None),
                ScanMode::Retry(p) => (false, Some(p.as_path())),
                ScanMode::Changed => (false, None),
            };
            let _ = tx.send(output::discover_plugins_cached(
                Some(&cache_file),
                timeout,
                all,
                retry,
            ));
        });
        self.shutdown.track_scan(handle);
    }

    pub(crate) fn apply_catalog(&mut self, report: output::ScanReport) {
        self.scan_probe_used = Some(report.probe_used);
        self.scan_cached = report.cached_ok;
        self.quarantined = report.quarantined.clone();
        self.plugin_meta = report
            .plugins
            .iter()
            .cloned()
            .map(|p| (p.path.to_string_lossy().into_owned(), p))
            .collect();
        let fmt_skip = |(p, r): &(PathBuf, String)| {
            format!(
                "{} — {}",
                p.file_stem()
                    .map(|s| s.to_string_lossy())
                    .unwrap_or_default(),
                r
            )
        };
        let mut note: Vec<String> = report.skipped.iter().map(fmt_skip).collect();
        note.extend(
            report
                .quarantined
                .iter()
                .map(|s| format!("{}: {}", t("output.quarantined_short"), fmt_skip(s))),
        );
        self.scan_note = if note.is_empty() {
            None
        } else {
            Some(note.join("; "))
        };
        let fresh = build_dest_catalog(&report.plugins);
        tracing::info!(
            dests = fresh.len(),
            skipped = report.skipped.len(),
            probe_used = report.probe_used,
            "destination catalog applied"
        );
        let mut sh = lock_shared(&self.shared);
        if fresh == sh.dests {
            let ns = sh.dests.len().to_string();
            drop(sh);
            self.status = tf("status.rescan", &[("n", ns.as_str())]).into();
            return;
        }
        let old_default = sh.dests.get(sh.default_dest).map(|(_, d)| d.clone());
        let old_tracks: Vec<(usize, midi_io::Destination)> = sh
            .track_dest
            .iter()
            .filter_map(|(t, i)| sh.dests.get(*i).map(|(_, d)| (*t, d.clone())))
            .collect();
        sh.dests = fresh;
        sh.default_dest = old_default
            .map(|d| {
                // identity-aware lookup (path or component id), then re-add
                // offline entries rather than dropping the assignment —
                // they play again once replugged
                match sh.dests.iter().position(|(_, dd)| dd.same_identity(&d)) {
                    Some(i) => i,
                    None => sh.ensure_dest(&dest_label(&d), d),
                }
            })
            .unwrap_or(0);
        sh.track_dest = old_tracks
            .into_iter()
            .map(|(t, d)| {
                let i = match sh.dests.iter().position(|(_, dd)| dd.same_identity(&d)) {
                    Some(i) => i,
                    None => sh.ensure_dest(&dest_label(&d), d),
                };
                (t, i)
            })
            .collect();
        let n = sh.dests.len();
        drop(sh);
        if let Some(mut pw) = self.plugin_window.take() {
            pw.close();
        }
        self.editor_plugin = None;
        // dest indices were just remapped — every slot is stale; capture
        // their state first so a rescan doesn't lose dialed-in patches
        self.capture_all_plugin_states();
        self.state_restored.clear();
        self.plugin_slots.clear();
        self.restart_logged.clear();
        self.plugin_state.clear();
        // audition sinks key on the same indexes — rebuild on next strike
        self.aud_ships.clear();
        self.aud_failed.clear();
        self.audition.clear_sinks();
        let _ = self.plugin_req.send(output::PluginReq::Clear);
        self.refresh_plugins();
        let ns = n.to_string();
        self.status = tf("status.rescan", &[("n", ns.as_str())]).into();
    }

    /// Toggle the selected destination's plugin editor. Process-isolated
    /// plugins cannot host a GUI on Windows (the helper's GUI loop is
    /// macOS-only), so the editor always loads a separate in-process
    /// instance inside a `PluginWindow` — a standalone Win32 window that
    /// hosts the plugin's editor view.
    pub(crate) fn open_plugin_gui(&mut self) {
        if let Some(mut pw) = self.plugin_window.take() {
            pw.close();
            self.sync_editor_state_into_slot();
            self.editor_plugin = None;
            return;
        }
        let (d, path) = {
            let sh = lock_shared(&self.shared);
            let d = sh.dest_of(self.sel_track);
            let p = sh.dests.get(d).and_then(|(_, dd)| match dd {
                output::Destination::Plugin { plugin_path, .. } => Some(PathBuf::from(plugin_path)),
                _ => None,
            });
            (d, p)
        };
        let Some(path) = path else { return };
        match output::load_for_gui(&path) {
            Ok(a) => {
                // adopt the live state of the playing instance so the editor
                // shows what's actually being heard. #190: blocking lock is
                // fine here — a once-per-open user gesture, and skipping
                // would show the editor a stale patch.
                if let Some(slot) = self.plugin_slots.get(&d).filter(|s| s.path == path) {
                    let state = slot.plugin.lock().ok().and_then(|p| p.save_state().ok());
                    if let Some(data) = state {
                        if let Ok(mut e) = a.lock() {
                            let _ = e.load_state(&data);
                        }
                    }
                }
                let mut pw = vst3_host::PluginWindow::new(a.clone());
                match pw.open() {
                    Ok(()) => {
                        self.plugin_window = Some(pw);
                        self.editor_plugin = Some((d, a));
                    }
                    Err(e) => {
                        self.status = tf("plugin.gui_open_failed", &[("e", &e.to_string())]).into()
                    }
                }
            }
            Err(e) => self.status = format!("{}: {e}", t("plugin.gui_failed")).into(),
        }
    }

    /// Copy the in-process editor's full state into the playing instance —
    /// covers program/bank changes parameter-edit draining can't see.
    ///
    /// #190: blocking lock is deliberate — this whole-state transfer runs
    /// once per editor close / preset change (a user gesture, not a tick),
    /// and skipping it would silently drop those program/bank changes from
    /// the playing instance (patch loss, #175).
    pub(crate) fn push_editor_state(
        &mut self,
        d: usize,
        editor: &std::sync::Arc<std::sync::Mutex<vst3_host::Plugin>>,
    ) {
        let Some(slot) = self.plugin_slots.get(&d) else {
            return;
        };
        let data = editor.lock().ok().and_then(|e| e.save_state().ok());
        if let Some(data) = data {
            if let Ok(mut p) = slot.plugin.lock() {
                let _ = p.load_state(&data);
            }
            // the playing instance's state just changed wholesale — persist it
            self.pending_state_capture.insert(d);
            self.flush_plugin_states(true);
        }
    }

    /// Locking discipline for the shared plugin mutex (#190): the audio
    /// callback owns `slot.plugin` for the duration of every audio block;
    /// the GUI is a try-lock guest on anything that runs periodically
    /// (state drains, restart servicing) and skips a tick under contention.
    /// Blocking locks are reserved for once-only points — teardown
    /// (doc swap / new file / rescan retire, where `stop_playback` has
    /// already closed the pass and panic'd the sinks, so the wait is at
    /// most one audio block) and data-transfer moments (restore on load,
    /// editor open/close, destination re-point) where skipping could lose
    /// a user's patch. State loss is worse than a stalled frame (#175).
    ///
    /// Capture one warm slot's component+controller state into the per-song
    /// store. The blob is keyed by the loaded plugin's own class uid so the
    /// record follows the component when the bundle path moves; while a uid
    /// is unknown the bundle path is the key. No-op without a readable slot.
    ///
    /// Blocking (teardown semantics): the slot is about to be retired or the
    /// document is going away — wait for the plugin mutex so the patch
    /// cannot be lost (#175). See `capture_plugin_state_try` for the
    /// contention-safe periodic variant.
    pub(crate) fn capture_plugin_state(&mut self, d: usize) {
        self.capture_plugin_state_impl(d, true);
    }

    /// Contention-safe capture (#190): takes the plugin mutex only when the
    /// audio callback isn't holding it (a block boundary in progress). On
    /// contention returns `false` WITHOUT capturing — the caller must keep
    /// the destination queued so a later tick retries. A skip never loses
    /// state because nothing was drained; it merely defers it.
    pub(crate) fn capture_plugin_state_try(&mut self, d: usize) -> bool {
        self.capture_plugin_state_impl(d, false)
    }

    /// Shared capture body. `blocking` = teardown semantics (wait for the
    /// mutex — runs at most once, so a bounded stall is the right trade);
    /// `false` = periodic drain (skip when contended, retry next tick).
    /// Returns `true` when the record is safely in the store (or there was
    /// nothing to capture), `false` only for a contended non-blocking call.
    fn capture_plugin_state_impl(&mut self, d: usize, blocking: bool) -> bool {
        let Some(slot) = self.plugin_slots.get(&d) else {
            return true;
        };
        // #190 locking discipline (audio = owner, GUI = try-lock guest):
        // see the note at the top of this capture/restore section
        let p = if blocking {
            slot.plugin
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        } else {
            match slot.plugin.try_lock() {
                Ok(guard) => guard,
                // a poisoned mutex is recoverable (the audio thread panicked
                // earlier) — capture anyway; only a live block is skipped
                Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => return false,
            }
        };
        let (uid, blob) = match p.save_state() {
            Ok(b) => (p.info().uid.clone(), b),
            // save_state failed — record nothing, matching the old
            // behavior (an immediate retry would not succeed either)
            Err(_) => return true,
        };
        let key = slot.path.to_string_lossy().into_owned();
        let meta = self.plugin_meta.get(&key);
        let changed = self.plugin_states.insert(plugin_state::PluginStateRecord {
            uid,
            path: key.clone(),
            vendor: meta.map(|m| m.vendor.clone()).unwrap_or_default(),
            name: meta.map(|m| m.name.clone()).unwrap_or_else(|| {
                slot.path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
            version: meta.map(|m| m.version.clone()).unwrap_or_default(),
            saved_unix_ms: plugin_state::now_unix_ms(),
            state: blob,
        });
        self.state_file_dirty |= changed;
        true
    }

    /// Snapshot every warm slot — used at doc swaps and catalog rebuilds,
    /// where the instances the state belongs to are about to be retired.
    /// #190: blocking captures — these slots die right after, so a skipped
    /// capture could never be retried (patch loss); on the doc-swap path
    /// `stop_playback` has already closed the pass first.
    pub(crate) fn capture_all_plugin_states(&mut self) {
        let ds: Vec<usize> = self.plugin_slots.keys().copied().collect();
        for d in ds {
            self.capture_plugin_state(d);
        }
    }

    /// Drain pending captures into the store and write the companion file
    /// when records changed. `force` does double duty (#190): it bypasses
    /// the ~1 s write throttle used by the periodic tick AND selects
    /// blocking captures — teardown points (persist, doc swap, rescan,
    /// editor close) always force so a quick exit can't strand state, and
    /// on those paths playback has already been stopped first (see
    /// `open`/`new_file`), so the wait is bounded and safe. The periodic
    /// (`force = false`) tick captures with `try_lock` instead: a
    /// destination whose audio callback holds the mutex stays queued in
    /// `pending_state_capture` and retries next tick — a skip defers, it
    /// never drops.
    pub(crate) fn flush_plugin_states(&mut self, force: bool) {
        let pending = std::mem::take(&mut self.pending_state_capture);
        let mut skipped = Vec::new();
        for d in pending {
            let captured = if force {
                self.capture_plugin_state(d);
                true
            } else {
                self.capture_plugin_state_try(d)
            };
            if !captured {
                skipped.push(d);
            }
        }
        self.pending_state_capture.extend(skipped);
        let doc_path = lock_shared(&self.shared).path.clone();
        let Some(doc_path) = doc_path else {
            // untitled document: keep records in memory until Save As gives
            // the song (and its sidecars) a home
            return;
        };
        let target = plugin_state::state_path(&doc_path);
        if !needs_state_write(
            self.state_file_dirty,
            self.state_path_written.as_deref(),
            &target,
        ) {
            return;
        }
        if !force && self.last_state_write.elapsed() < std::time::Duration::from_secs(1) {
            return;
        }
        match self.plugin_states.save(&target) {
            Ok(_) => {
                self.state_file_dirty = false;
                self.state_path_written = Some(target);
                self.last_state_write = std::time::Instant::now();
            }
            Err(e) => tracing::warn!("plugin state write failed: {e}"),
        }
    }

    /// Push the song's saved state into a warm or just-loaded slot. Runs once
    /// per (re)load — `state_restored` prevents a double `load_state` when a
    /// doc is reopened over still-warm instances. A non-empty record uid that
    /// disagrees with the loaded plugin's real uid is rejected as
    /// incompatible; any failure is a status line note, never fatal — the
    /// plugin still loads and plays with its defaults.
    ///
    /// #190: the plugin locks below block, deliberately — this is a
    /// once-per-load transfer of the user's saved patch, so skipping on
    /// contention would lose it; the wait is bounded by one audio block.
    pub(crate) fn restore_plugin_state(&mut self, d: usize) {
        if self.state_restored.contains(&d) {
            return;
        }
        let Some(slot) = self.plugin_slots.get(&d) else {
            return;
        };
        let plugin = slot.plugin.clone();
        let path = slot.path.clone();
        let uid = plugin
            .lock()
            .map(|p| p.info().uid.clone())
            .unwrap_or_default();
        let err = {
            let Some(rec) = self.plugin_states.lookup(&uid, &path) else {
                return;
            };
            if !rec.uid.is_empty() && !uid.is_empty() && rec.uid != uid {
                Some("class id mismatch".to_string())
            } else {
                match plugin.lock() {
                    Ok(mut p) => p.load_state(&rec.state).err().map(|e| e.to_string()),
                    Err(_) => None,
                }
            }
        };
        // mark even on failure — a rejected blob should not retry every tick
        self.state_restored.insert(d);
        if let Some(e) = err {
            let name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            self.status = tf(
                "plugin.state_restore_failed",
                &[("name", name.as_str()), ("e", e.as_str())],
            )
            .into();
        }
    }

    /// `push_editor_state` on editor close, then the handle is released.
    pub(crate) fn sync_editor_state_into_slot(&mut self) {
        let Some((d, editor)) = self.editor_plugin.take() else {
            return;
        };
        self.push_editor_state(d, &editor);
    }

    /// Periodic MIDI endpoint reconcile (called ~every 2 s by the doc
    /// watcher): refresh the present-port set, append newly discovered
    /// outputs to the catalog — an assignment targeting a vanished port
    /// keeps its identity and plays again on replug — and reopen an armed
    /// recording's input connection when its endpoint comes back.
    /// Returns true when the catalog or availability changed.
    pub(crate) fn reconcile_ports(&mut self) -> bool {
        let outs = midi_io::list_outputs().unwrap_or_default();
        let present: std::collections::HashSet<(String, usize)> =
            outs.iter().map(|p| (p.name.clone(), p.ord)).collect();
        let mut changed = false;
        {
            let mut sh = lock_shared(&self.shared);
            if sh.port_present != present {
                // brand-new outputs become assignable immediately
                let mut name_counts: HashMap<String, usize> = HashMap::new();
                for p in &outs {
                    *name_counts.entry(p.name.clone()).or_default() += 1;
                }
                for p in &outs {
                    let d = midi_io::Destination::MidiPort {
                        port_name: p.name.clone(),
                        ord: p.ord,
                    };
                    if !sh.dests.iter().any(|(_, dd)| *dd == d) {
                        let label = if name_counts.get(p.name.as_str()).copied().unwrap_or(0) > 1 {
                            format!("{} #{}", p.name, p.ord + 1)
                        } else {
                            p.name.clone()
                        };
                        sh.dests.push((label, d));
                    }
                }
                sh.port_present = present;
                changed = true;
            }
        }
        // armed input whose endpoint returned: swap in a fresh connection —
        // its t=0 restarts, so re-anchor the take's doc-time base at now
        if let Some(rec) = self.rec.as_mut() {
            let (name, ord) = (rec.input.name.clone(), rec.input.ord);
            let found = midi_io::list_inputs()
                .unwrap_or_default()
                .iter()
                .any(|p| p.name == name && p.ord == ord);
            if found && rec.input_lost {
                let buf2 = rec.buf.clone();
                let cb = move |us, b: &[u8]| {
                    buf2.lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push((us, b.to_vec()));
                };
                if let Ok(inp) = midi_io::Input::open_ord(&name, ord, cb) {
                    rec.input = inp;
                    rec.input_lost = false;
                    // the new connection's t=0 restarts — re-anchor the
                    // doc-time base and clear the already-consumed count-in
                    rec.base_us = {
                        let cin = self.live_countin_us;
                        self.playback
                            .as_ref()
                            .map(|p| p.position_us().saturating_sub(cin))
                            .unwrap_or(self.play_us)
                    };
                    rec.cin_us = 0;
                    rec.cin_region = None;
                    tracing::info!("recording input '{name}' reconnected");
                    changed = true;
                }
            } else if !found && !rec.input_lost {
                rec.input_lost = true;
                tracing::warn!("recording input '{name}' disappeared; reconnect on return");
                changed = true;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::needs_state_write;
    use std::path::Path;

    #[test]
    fn state_write_decides_by_dirty_and_path() {
        let a = Path::new(r"C:\songs\a.editor.state");
        let b = Path::new(r"C:\songs\b.editor.state");
        // clean and already written for this path — nothing to do
        assert!(!needs_state_write(false, Some(a), a));
        // dirty — always write
        assert!(needs_state_write(true, Some(a), a));
        // #175: Save As renamed the song — the clean store must still be
        // re-derived under the new name or every patch is lost
        assert!(needs_state_write(false, Some(a), b));
        // no companion file written yet for any path
        assert!(needs_state_write(false, None, a));
    }
}

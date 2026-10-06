use super::*;
use serde_json::json;

fn shared() -> SharedDoc {
    let note = |tick: u64, status: u8, d0: u8, d1: u8| smf_core::Event {
        tick,
        seq: 0,
        raw_body: None,
        kind: EventKind::Channel {
            status,
            data: [d0, d1],
            len: 2,
        },
    };
    let f = smf_core::File {
        format: 1,
        division: smf_core::Division::Metrical(480),
        tracks: vec![
            smf_core::Track { events: vec![] },
            smf_core::Track {
                events: vec![note(0, 0x90, 60, 100), note(480, 0x80, 60, 0)],
            },
        ],
        warnings: vec![],
    };
    Arc::new(Mutex::new(Shared::new(Document::from_file(f))))
}

/// dispatch a tool and decode its (is_error, first text block as JSON)
fn call(shared: &SharedDoc, name: &str, args: serde_json::Value) -> (bool, serde_json::Value) {
    let (is_err, text) = call_text(shared, name, args);
    (is_err, serde_json::from_str(&text).unwrap_or(json!(null)))
}

/// dispatch a tool and decode its (is_error, first text block verbatim)
fn call_text(shared: &SharedDoc, name: &str, args: serde_json::Value) -> (bool, String) {
    match dispatch(name, &args, shared.clone()) {
        CallToolResponse::Complete(r) => {
            let is_err = r.is_error.unwrap_or(false);
            let text = match r.content.first() {
                Some(ContentBlock::Text(t)) => t.text.clone(),
                other => panic!("expected text content, got {other:?}"),
            };
            (is_err, text)
        }
        other => panic!("unexpected response kind: {other:?}"),
    }
}

fn note_count(shared: &SharedDoc) -> usize {
    shared.lock().unwrap().doc.notes().len()
}

/// Destination dedup is by routing identity: a plugin identity arriving
/// with different metadata (moved path, fresh vendor/name) or via MCP
/// (path only, no component id) must reuse the same catalog slot rather
/// than forking it.
#[test]
fn ensure_dest_dedups_by_routing_identity() {
    let mut sh = Shared::new(Document::from_file(smf_core::File {
        format: 1,
        division: smf_core::Division::Metrical(480),
        tracks: vec![],
        warnings: vec![],
    }));
    let catalog_dest = Destination::Plugin {
        plugin_path: r"C:\VST3\Surge.vst3".into(),
        component_id: Some("UID".into()),
        vendor: Some("V".into()),
        plugin_name: Some("Surge".into()),
    };
    let i = sh.ensure_dest("Surge", catalog_dest);
    // MCP-style: same bundle, no metadata at all → same slot
    let j = sh.ensure_dest(
        "Surge",
        Destination::Plugin {
            plugin_path: r"C:\VST3\Surge.vst3".into(),
            component_id: None,
            vendor: None,
            plugin_name: None,
        },
    );
    assert_eq!(i, j);
    // moved bundle, same component ID → same slot
    let k = sh.ensure_dest(
        "Surge",
        Destination::Plugin {
            plugin_path: r"D:\Moved\Surge.vst3".into(),
            component_id: Some("UID".into()),
            vendor: None,
            plugin_name: None,
        },
    );
    assert_eq!(i, k);
    assert_eq!(sh.dests.len(), 1);
    // a different plugin does get its own slot
    let l = sh.ensure_dest(
        "Dexed",
        Destination::Plugin {
            plugin_path: r"C:\VST3\Dexed.vst3".into(),
            component_id: Some("UID2".into()),
            vendor: None,
            plugin_name: None,
        },
    );
    assert_eq!(l, 1);
}

#[test]
fn editor_info_reports_contract() {
    let sh = shared();
    let (err, v) = call(&sh, "editor_info", json!({}));
    assert!(!err);
    assert_eq!(v["name"], "midi-editor");
    assert!(v["version"].as_str().unwrap().contains('.'));
    assert!(v["commit"].as_str().is_some());
    assert_eq!(v["mcp_surface_version"], MCP_SURFACE_VERSION);
    assert_eq!(v["document"]["revision"], 0);
    assert_eq!(v["features"]["editing"]["base_revision"], true);
    // standalone (test) mode: no GUI-hosted features
    assert_eq!(v["features"]["transport"], false);
    // every listed tool is dispatchable and carries contract metadata
    let tools = v["tools"].as_array().unwrap();
    assert_eq!(tools.len(), tool_specs().len());
    for t in tools {
        assert!(t["version"].as_u64().unwrap() >= 1);
        assert!(t["name"].as_str().is_some());
    }
    assert!(tools.iter().any(|t| t["name"] == "apply_patch"));
}

#[test]
fn every_spec_is_listed_and_dispatchable() {
    // the registry is the single source of truth for the tool surface
    let names: Vec<_> = tool_specs().iter().map(|s| s.name).collect();
    assert_eq!(names.len(), {
        let mut n = names.clone();
        n.sort();
        n.dedup();
        n.len()
    });
}

#[test]
fn document_summary_reports_tracks() {
    let sh = shared();
    let (err, v) = call(&sh, "document_summary", json!({}));
    assert!(!err);
    assert_eq!(v["tracks"].as_array().unwrap().len(), 2);
    assert_eq!(v["events"], 2);
}

#[test]
fn remove_track_requires_valid_track() {
    let sh = shared();
    let (err, _) = call(&sh, "remove_track", json!({}));
    assert!(err, "missing track must error, not delete track 0");
    let (err, _) = call(&sh, "remove_track", json!({"track": 99}));
    assert!(err);
    assert_eq!(sh.lock().unwrap().doc.tracks.len(), 2, "nothing deleted");
}

#[test]
fn apply_patch_failure_is_atomic() {
    let sh = shared();
    let rev0 = sh.lock().unwrap().doc.revision();
    // second op targets a nonexistent track — nothing may be applied
    let (err, _) = call(
        &sh,
        "apply_patch",
        json!({"ops": [
            {"op": "insert_note", "track": 1, "key": 64, "start": 480, "dur": 240},
            {"op": "insert_note", "track": 99, "key": 65, "start": 0, "dur": 240},
        ]}),
    );
    assert!(err);
    let shg = sh.lock().unwrap();
    assert_eq!(shg.doc.revision(), rev0, "no revision bump on failure");
    assert_eq!(shg.doc.tracks[1].events.len(), 2, "op 1 not half-applied");
    drop(shg);
    // a valid patch at the same base revision still works
    let (err, v) = call(
        &sh,
        "apply_patch",
        json!({
            "base_revision": rev0,
            "ops": [{"op": "insert_note", "track": 1, "key": 64, "start": 480, "dur": 240}]
        }),
    );
    assert!(!err);
    assert_eq!(v["applied"], true);
    assert_eq!(note_count(&sh), 2);
    // undo (shared with GUI) reverts it
    let (err, _) = call(&sh, "undo", json!({}));
    assert!(!err);
    assert_eq!(note_count(&sh), 1);
}

#[test]
fn insert_note_rejects_unknown_track() {
    let sh = shared();
    let (err, _) = call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_note", "track": 5}]}),
    );
    assert!(err, "unknown track must error before any op applies");
}

#[test]
fn duplicate_range_defaults_to_song_end() {
    let sh = shared();
    // single note 0..480 (off at 480); omitting `to` duplicates to the
    // end of song instead of a u64::MAX span
    let (err, _) = call(&sh, "duplicate_range", json!({"track": 1, "from": 0}));
    assert!(!err);
    let ticks: Vec<(u64, u8)> = sh.lock().unwrap().doc.tracks[1]
        .events
        .iter()
        // the track also carries its structural End-of-Track — meta, not
        // a channel event
        .filter_map(|e| match &e.kind {
            EventKind::Channel { status, .. } => Some((e.tick, *status)),
            _ => None,
        })
        .collect();
    // original on/off at 0/480 plus the copy on/off at 480/960. The
    // copy-on shares tick+seq with the original off and now inserts
    // AFTER it (#187): a boundary-tick NoteOn follows the release it
    // succeeds, so pairing and wire order stay musical.
    assert_eq!(
        ticks,
        vec![(0, 0x90), (480, 0x80), (480, 0x90), (960, 0x80)]
    );
}

#[test]
fn transaction_commit_is_one_undo_step() {
    let sh = shared();
    let rev0 = sh.lock().unwrap().doc.revision();
    let (err, v) = call(&sh, "begin_transaction", json!({"label": "fix chorus"}));
    assert!(!err);
    assert_eq!(v["base_revision"], rev0);
    let tx = v["tx_id"].as_u64().unwrap();
    // stage two separate edits — each carries the batch's tx_id (#185)
    let (err, v) = call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_note", "track": 1, "key": 62, "start": 0, "dur": 240}], "tx_id": tx}),
    );
    assert!(!err);
    assert_eq!(v["staged"], true);
    // reads inside the batch see staged state; the real doc is untouched
    let (_err, v) = call(&sh, "list_notes", json!({}));
    assert_eq!(v["count"], 2);
    assert_eq!(
        sh.lock().unwrap().doc.notes().len(),
        1,
        "real document unchanged while staged"
    );
    let (err, _) = call(
        &sh,
        "set_track_name",
        json!({"track": 1, "name": "Chorus", "tx_id": tx}),
    );
    assert!(!err);
    // commit merges both calls into ONE undo step
    let (err, v) = call(&sh, "commit_transaction", json!({}));
    assert!(!err);
    assert_eq!(v["committed"], true);
    assert_eq!(v["label"], "fix chorus");
    assert_eq!(v["ops"], 2);
    assert_eq!(sh.lock().unwrap().doc.notes().len(), 2);
    let (err, v) = call(&sh, "undo", json!({}));
    assert!(!err);
    assert_eq!(v["undone"], "fix chorus");
    assert_eq!(note_count(&sh), 1, "single undo reverted both edits");
    assert!(sh.lock().unwrap().doc.tracks[1].name.is_none());
}

#[test]
fn rollback_leaves_document_unchanged() {
    let sh = shared();
    let before = sh.lock().unwrap().doc.serialize(smf_core::WriteOptions {
        running_status: false,
    });
    let (_, v) = call(&sh, "begin_transaction", json!({"label": "experiment"}));
    let tx = v["tx_id"].as_u64().unwrap();
    call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_note", "track": 1, "key": 65, "start": 0, "dur": 120}], "tx_id": tx}),
    );
    call(
        &sh,
        "set_tempo",
        json!({"tick": 0, "bpm": 90.0, "tx_id": tx}),
    );
    let (err, v) = call(&sh, "rollback_transaction", json!({}));
    assert!(!err);
    assert_eq!(v["discarded_ops"], 2);
    let shg = sh.lock().unwrap();
    let after = shg.doc.serialize(smf_core::WriteOptions {
        running_status: false,
    });
    assert_eq!(before, after, "byte-for-byte unchanged after rollback");
    assert_eq!(shg.doc.revision(), 0);
}

#[test]
fn commit_reports_stale_conflict_on_concurrent_edit() {
    let sh = shared();
    let (_, v) = call(&sh, "begin_transaction", json!({"label": "agent work"}));
    let tx = v["tx_id"].as_u64().unwrap();
    call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_note", "track": 1, "key": 60, "start": 960, "dur": 120}], "tx_id": tx}),
    );
    // a GUI edit lands on the real document mid-batch
    let ops = sh
        .lock()
        .unwrap()
        .doc
        .add_track_ops(Some("gui track"), None);
    sh.lock().unwrap().apply("gui edit", ops).unwrap();
    let (err, v) = call(&sh, "commit_transaction", json!({}));
    assert!(err);
    assert_eq!(v["error"], "stale_base");
    // the conflicted batch stays open — caller decides (rollback here)
    let (err, v) = call(&sh, "transaction_status", json!({}));
    assert!(!err);
    assert_eq!(v["open"], true);
    call(&sh, "rollback_transaction", json!({}));
}

#[test]
fn dry_run_commit_validates_and_keeps_batch() {
    let sh = shared();
    let (_, v) = call(&sh, "begin_transaction", json!({}));
    let tx = v["tx_id"].as_u64().unwrap();
    call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_note", "track": 1, "key": 60, "start": 0, "dur": 120}], "tx_id": tx}),
    );
    let (err, v) = call(&sh, "commit_transaction", json!({"dry_run": true}));
    assert!(!err);
    assert_eq!(v["valid"], true);
    assert_eq!(v["would_be_revision"], 1);
    assert_eq!(
        sh.lock().unwrap().doc.revision(),
        0,
        "dry run applied nothing"
    );
    let (_err, v) = call(&sh, "transaction_status", json!({}));
    assert_eq!(v["open"], true, "batch still open after dry_run");
    let (err, v) = call(&sh, "commit_transaction", json!({}));
    assert!(!err && v["committed"] == true);
}

#[test]
fn abandoned_batch_expires() {
    let sh = shared();
    call(&sh, "begin_transaction", json!({"label": "forgotten"}));
    // push the checkpoint past its TTL
    sh.lock().unwrap().batch.as_mut().unwrap().last_activity =
        Instant::now() - Duration::from_secs(400);
    let (err, v) = call(&sh, "transaction_status", json!({}));
    assert!(!err);
    assert_eq!(v["open"], false, "idle batch auto-rolled-back");
    assert_eq!(sh.lock().unwrap().doc.revision(), 0);
}

#[test]
fn mutation_replies_carry_change_summary() {
    let sh = shared();
    let (err, v) = call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_note", "track": 1, "key": 64, "start": 480, "dur": 240}]}),
    );
    assert!(!err);
    let s = &v["summary"];
    assert_eq!(s["inserted"], 2, "on + off events");
    assert_eq!(s["notes"]["inserted"], 1);
    assert_eq!(s["tracks_touched"], json!([1]));
    assert_eq!(s["tick_range"], json!([480, 720]));
    // a move is reported as moved, not as delete+insert
    let (_err, v) = call(&sh, "list_notes", json!({}));
    let on_id = v["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["key"] == 64)
        .unwrap()["on_id"]
        .as_u64()
        .unwrap();
    let (err, v) = call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "move_note", "on_id": on_id, "dtick": 240, "dkey": 2}]}),
    );
    assert!(!err);
    assert_eq!(v["summary"]["notes"]["moved"], 1);
    assert_eq!(v["summary"]["notes"]["inserted"], 0);
    assert_eq!(v["summary"]["notes"]["removed"], 0);
}

#[test]
fn history_and_changes_since_revision() {
    let sh = shared();
    call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_note", "track": 1, "key": 64, "start": 480, "dur": 240}]}),
    );
    call(&sh, "set_tempo", json!({"tick": 0, "bpm": 90.0}));
    let (err, v) = call(&sh, "transaction_history", json!({}));
    assert!(!err);
    assert_eq!(v["count"], 2);
    // newest first; origin attribution
    assert_eq!(v["transactions"][0]["label"], "set tempo");
    assert_eq!(v["transactions"][0]["origin"], "mcp");
    // last agent-originated tx is what the GUI status bar surfaces
    assert_eq!(
        sh.lock().unwrap().last_mcp_tx.as_ref().unwrap().label,
        "set tempo"
    );

    let (err, v) = call(&sh, "changes_since_revision", json!({"revision": 1}));
    assert!(!err);
    assert_eq!(v["count"], 1);
    // tempo write + the structural End-of-Track the touched track gains —
    // both meta-class ops are reported
    assert_eq!(v["aggregate"]["meta_changes"], 2);
    assert_eq!(v["truncated"], false);

    // undo is recorded too — revision moves are visible both ways
    call(&sh, "undo", json!({}));
    let (_, v) = call(&sh, "changes_since_revision", json!({"revision": 2}));
    assert_eq!(v["transactions"][0]["kind"], "undo");

    let (err, _) = call(&sh, "changes_since_revision", json!({"revision": 999}));
    assert!(err, "future revision is an error, not an empty diff");
}

#[test]
fn gui_apply_is_not_attributed_to_mcp() {
    let sh = shared();
    let ops = sh.lock().unwrap().doc.add_track_ops(Some("gui"), None);
    sh.lock().unwrap().apply("gui edit", ops).unwrap();
    let (_, v) = call(&sh, "transaction_history", json!({}));
    assert_eq!(v["transactions"][0]["origin"], "gui");
    assert!(sh.lock().unwrap().last_mcp_tx.is_none());
}

#[test]
fn history_is_bounded() {
    let sh = shared();
    for i in 0..(TX_HISTORY_CAP + 8) {
        let (err, _) = call(
            &sh,
            "set_tempo",
            json!({"tick": i as u64 * 1000, "bpm": 100.0 + i as f64}),
        );
        assert!(!err);
    }
    let (_, v) = call(&sh, "transaction_history", json!({"limit": 1000}));
    assert_eq!(v["count"], TX_HISTORY_CAP, "history capped");
    // coverage no longer reaches revision 0 — the flag tells the agent
    // to fall back to a full document query instead of trusting a gap
    let (_, v) = call(&sh, "changes_since_revision", json!({"revision": 0}));
    assert_eq!(v["truncated"], true);
}

/// Walk a paginated tool to exhaustion, returning every row emitted.
fn paged(
    sh: &SharedDoc,
    tool: &str,
    args: serde_json::Value,
    rows: &str,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    let mut cursor = serde_json::Value::Null;
    for _ in 0..100 {
        let mut a = args.clone();
        a["cursor"] = cursor;
        let (err, v) = call(sh, tool, a);
        assert!(!err, "{tool} errored: {v}");
        out.extend(v[rows].as_array().unwrap().clone());
        match v["next_cursor"].as_str() {
            Some(c) => cursor = c.into(),
            None => return out,
        }
    }
    panic!("{tool}: pagination did not terminate");
}

#[test]
fn list_notes_paginates_without_gaps() {
    let sh = shared(); // fixture already has one note (key 60 @0-480)
    let ops: Vec<_> = (1..7)
        .map(|i| {
            json!({"op": "insert_note", "track": 1, "key": 60 + i,
                   "start": i * 480, "dur": 240})
        })
        .collect();
    call(&sh, "apply_patch", json!({"ops": ops}));
    let rows = paged(&sh, "list_notes", json!({"limit": 3}), "notes");
    assert_eq!(rows.len(), 7);
    let ids: std::collections::HashSet<_> =
        rows.iter().map(|n| n["on_id"].as_u64().unwrap()).collect();
    assert_eq!(ids.len(), 7, "no duplicates across pages");
    let starts: Vec<_> = rows.iter().map(|n| n["start"].as_u64().unwrap()).collect();
    let mut sorted = starts.clone();
    sorted.sort();
    assert_eq!(
        starts, sorted,
        "pages stay in (start, key, track, id) order"
    );
}

#[test]
fn query_events_pages_and_field_projection() {
    let sh = shared();
    call(
        &sh,
        "apply_patch",
        json!({"ops": (0..4).map(|i| json!({"op": "insert_note", "track": 1,
            "key": 64, "start": i * 960, "dur": 120})).collect::<Vec<_>>()}),
    );
    // 1 fixture note + 4 inserted = 10 channel events + 1 structural
    // End-of-Track the edited track gains
    let rows = paged(
        &sh,
        "query_events",
        json!({"limit": 4, "fields": ["id", "tick"]}),
        "events",
    );
    assert_eq!(rows.len(), 11);
    let ids: std::collections::HashSet<_> =
        rows.iter().map(|e| e["id"].as_u64().unwrap()).collect();
    assert_eq!(ids.len(), 11);
    for e in &rows {
        let obj = e.as_object().unwrap();
        assert_eq!(obj.len(), 2, "fields projection dropped everything else");
        assert!(obj.contains_key("id") && obj.contains_key("tick"));
    }
}

#[test]
fn stale_cursor_is_reported() {
    let sh = shared();
    call(
        &sh,
        "apply_patch",
        json!({"ops": (0..4).map(|i| json!({"op": "insert_note", "track": 1,
            "key": 64, "start": i * 960, "dur": 120})).collect::<Vec<_>>()}),
    );
    let (_, v) = call(&sh, "query_events", json!({"limit": 2}));
    let cursor = v["next_cursor"].as_str().unwrap().to_string();
    // any mutation bumps the revision the cursor was minted under
    call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_note", "track": 1, "key": 70}]}),
    );
    let (err, v) = call(&sh, "query_events", json!({"cursor": cursor}));
    assert!(err);
    let s = v.to_string();
    assert!(s.contains("stale_cursor") && s.contains("current_revision") && s.contains("hint"));
    let (err, _) = call(&sh, "list_notes", json!({"cursor": "not-a-cursor"}));
    assert!(err, "malformed cursors are rejected, not ignored");
}

#[test]
fn meta_and_cc_reads_paginate() {
    let sh = shared();
    let evs: Vec<_> = (0..5)
        .map(|i| {
            json!({"tick": i * 240, "kind": {"meta": {"type": 3, "data_utf8": format!("m{i}")}}})
        })
        .chain((0..4).map(|i| {
            json!({"tick": i * 120, "kind": {"channel": {"status": 176, "data": [20 + i, i]}}})
        }))
        .collect();
    call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_events", "track": 0, "events": evs}]}),
    );
    let metas = paged(&sh, "get_meta", json!({"meta_type": 3, "limit": 2}), "meta");
    assert_eq!(metas.len(), 5);
    let ids: std::collections::HashSet<_> =
        metas.iter().map(|m| m["id"].as_u64().unwrap()).collect();
    assert_eq!(ids.len(), 5);
    let ccs = paged(&sh, "get_cc", json!({"limit": 2}), "cc");
    assert_eq!(ccs.len(), 4, "one row per (track,channel,cc)");
    // projection drops the per-row heavy field
    let metas = paged(
        &sh,
        "get_meta",
        json!({"meta_type": 3, "limit": 100, "fields": ["id", "tick"]}),
        "meta",
    );
    for m in &metas {
        assert!(m.get("data_hex").is_none() && m.get("text").is_none());
    }
}

#[test]
fn undo_is_blocked_while_batch_open() {
    let sh = shared();
    call(&sh, "begin_transaction", json!({}));
    let (err, _) = call(&sh, "undo", json!({}));
    assert!(err);
}

#[test]
fn query_events_pages_without_materializing_json() {
    let sh = shared();
    let (err, v) = call(&sh, "query_events", json!({"limit": 1, "offset": 1}));
    assert!(!err);
    assert_eq!(v["total"], 2);
    assert_eq!(v["events"].as_array().unwrap().len(), 1);
}

#[test]
fn hex_payloads_are_capped() {
    assert!(hex_to_bytes("ab").is_some());
    assert!(hex_to_bytes(&"ab".repeat(MAX_HEX_BYTES + 1)).is_none());
    assert!(hex_to_bytes("abc").is_none(), "odd length rejected");
    // oversized hex in a request is an error, not a silent empty payload
    let sh = shared();
    let big = "ab".repeat(MAX_HEX_BYTES + 1);
    let (err, _) = call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_events", "track": 0, "events": [
            {"tick": 0, "kind": {"meta": {"type": 1, "data_hex": big}}}
        ]}]}),
    );
    assert!(err);
}

// ---------- issue #10: auth ----------
// ---------- issue #16: file-system scope ----------

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir()
        .join("midi-editor-mcp-tests")
        .join(format!("{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn token_file_provisions_once_then_reuses() {
    let p = tmpdir("tok").join("sub").join("mcp-token");
    ensure_token_file(&p).unwrap();
    let t1 = read_token_file(&p).unwrap();
    assert_eq!(t1.len(), 64, "32 bytes of hex");
    assert!(t1.bytes().all(|b| b.is_ascii_hexdigit()));
    // second launch reuses, doesn't rotate
    ensure_token_file(&p).unwrap();
    assert_eq!(read_token_file(&p).unwrap(), t1);
    // a corrupt file is regenerated rather than trusted
    std::fs::write(&p, "not-a-token\nextra").unwrap();
    ensure_token_file(&p).unwrap();
    let t2 = read_token_file(&p).unwrap();
    assert_eq!(t2.len(), 64);
    assert_ne!(t1, t2);
    let _ = std::fs::remove_dir_all(p.parent().unwrap().parent().unwrap());
}

#[test]
fn token_validation_rejects_bad_values() {
    assert!(token_is_valid("abc123"));
    assert!(!token_is_valid(""));
    assert!(!token_is_valid("has space"));
    assert!(!token_is_valid("line\nbreak"));
    assert!(!token_is_valid(&"x".repeat(257)));
}

#[test]
fn constant_time_compares_exactly() {
    assert!(constant_time_eq("abc", "abc"));
    assert!(!constant_time_eq("abc", "abd"));
    assert!(!constant_time_eq("abc", "abcd"));
    assert!(!constant_time_eq("", "a"));
    assert!(constant_time_eq("", ""));
}

/// Start the real router on an ephemeral port; returns the bound address.
async fn start_http(auth: HttpAuth) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().unwrap().to_string();
    let app = mcp_http_router(shared(), &addr, auth)
        .into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    addr
}

fn mcp_headers(token: Option<&str>) -> Vec<(&'static str, String)> {
    let mut h: Vec<(&'static str, String)> = vec![
        ("Content-Type", "application/json".into()),
        ("Accept", "application/json, text/event-stream".into()),
    ];
    if let Some(t) = token {
        h.push(("Authorization", format!("Bearer {t}")));
    }
    h
}
async fn authed_post(addr: &str, token: Option<&str>) -> u16 {
    let h = mcp_headers(token);
    let pairs: Vec<(&str, &str)> = h.iter().map(|(k, v)| (*k, v.as_str())).collect();
    http_post(addr, None, &pairs, INIT).await
}

#[tokio::test]
async fn bearer_token_required_and_checked() {
    let addr = start_http(HttpAuth::Token(TokenSource::Fixed("s3cret".into()))).await;
    assert_eq!(authed_post(&addr, None).await, 401, "no creds rejected");
    assert_eq!(authed_post(&addr, Some("wrong")).await, 401);
    assert_eq!(authed_post(&addr, Some("s3cret")).await, 200);
}

#[tokio::test]
async fn token_file_source_rotates_without_restart() {
    let dir = tmpdir("rotate");
    let p = dir.join("mcp-token");
    std::fs::write(&p, "tok-a").unwrap();
    let addr = start_http(HttpAuth::Token(TokenSource::File(p.clone()))).await;
    assert_eq!(authed_post(&addr, Some("tok-a")).await, 200);
    // rotate: rewrite the file — next request must require the new token
    std::fs::write(&p, "tok-b").unwrap();
    assert_eq!(
        authed_post(&addr, Some("tok-a")).await,
        401,
        "old token revoked"
    );
    assert_eq!(
        authed_post(&addr, Some("tok-b")).await,
        200,
        "new token live"
    );
    // revoke: delete the file — everything fails closed
    std::fs::remove_file(&p).unwrap();
    assert_eq!(authed_post(&addr, Some("tok-b")).await, 401);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn auth_failures_are_rate_limited() {
    let addr = start_http(HttpAuth::Token(TokenSource::Fixed("s3cret".into()))).await;
    for _ in 0..AUTH_FAIL_MAX {
        assert_eq!(authed_post(&addr, Some("bad")).await, 401);
    }
    // past the limit the client is throttled — even with the right token
    assert_eq!(authed_post(&addr, Some("bad")).await, 429);
    assert_eq!(authed_post(&addr, Some("s3cret")).await, 429);
}

#[test]
fn resolve_prefers_env_then_opt_out_then_file() {
    // env vars are process-global; keep every mutation inside this one
    // test so nothing races with a sibling
    let dir = tmpdir("resolve");
    std::env::set_var("MIDI_MCP_TOKEN", "envtok");
    std::env::set_var("LOCALAPPDATA", &dir);
    std::env::remove_var("MIDI_MCP_ALLOW_INSECURE");
    match resolve_http_auth().unwrap() {
        HttpAuth::Token(TokenSource::Fixed(t)) => assert_eq!(t, "envtok"),
        _ => panic!("env token must win"),
    }
    std::env::remove_var("MIDI_MCP_TOKEN");
    std::env::set_var("MIDI_MCP_ALLOW_INSECURE", "1");
    assert!(matches!(resolve_http_auth().unwrap(), HttpAuth::Insecure));
    std::env::remove_var("MIDI_MCP_ALLOW_INSECURE");
    match resolve_http_auth().unwrap() {
        HttpAuth::Token(TokenSource::File(p)) => {
            assert_eq!(p, dir.join("midi-editor").join("mcp-token"));
            assert!(read_token_file(&p).is_some(), "provisioned on resolve");
        }
        _ => panic!("default must auto-provision a token file"),
    }
    std::env::remove_var("LOCALAPPDATA");
}

/// shared() pointed at a real temp dir as its document path
fn shared_in(dir: &std::path::Path) -> SharedDoc {
    let sh = shared();
    sh.lock().unwrap().path = Some(dir.join("song.mid"));
    sh
}

#[test]
fn save_inside_doc_dir_writes() {
    let dir = tmpdir("inside");
    let sh = shared_in(&dir);
    let out = dir.join("out.mid");
    let (err, v) = call(&sh, "save", json!({"path": out.to_string_lossy()}));
    assert!(!err, "{v}");
    assert!(out.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn summary_reports_auth_mode_not_secret() {
    let sh = shared();
    let (err, v) = call(&sh, "document_summary", json!({}));
    assert!(!err);
    assert_eq!(v["mcp_auth"]["mode"], "stdio");
    let _router = mcp_http_router(
        sh.clone(),
        "127.0.0.1:0",
        HttpAuth::Token(TokenSource::Fixed("dontleak".into())),
    );
    let (err, v) = call(&sh, "document_summary", json!({}));
    assert!(!err);
    assert_eq!(v["mcp_auth"]["mode"], "bearer");
    assert_eq!(v["mcp_auth"]["detail"], "MIDI_MCP_TOKEN");
    assert!(!v.to_string().contains("dontleak"), "token never surfaces");
}

#[test]
fn authority_parsing_accepts_only_wellformed() {
    assert_eq!(
        authority_host("127.0.0.1:7878").as_deref(),
        Some("127.0.0.1")
    );
    assert_eq!(authority_host("LOCALHOST").as_deref(), Some("localhost"));
    assert_eq!(authority_host("[::1]:7878").as_deref(), Some("::1"));
    assert_eq!(authority_host("[::1]").as_deref(), Some("::1"));
    // malformed / hostile spellings
    for bad in [
        "",
        "127.0.0.1:",
        ":7878",
        "a:b:c",
        "127.0.0.1:8x",
        "[::1",
        "::1]",
        "user@127.0.0.1",
        "evil.com@127.0.0.1",
        "127.0.0.1 @evil.com",
    ] {
        assert!(authority_host(bad).is_none(), "{bad:?} must be malformed");
    }
    // parses fine but is not loopback
    assert!(!is_loopback_host(
        &authority_host("127.0.0.1.evil.com").unwrap()
    ));
    assert!(!is_loopback_host(&authority_host("localhost.").unwrap()));
    assert!(!is_loopback_host(&authority_host("127.1").unwrap()));
}

#[test]
fn origin_check_accepts_only_loopback() {
    for good in [
        "http://localhost",
        "https://localhost:6274",
        "http://127.0.0.1:7878",
        "http://[::1]:3000",
        "https://[::1]",
    ] {
        assert!(origin_is_loopback(good), "{good:?} must pass");
    }
    for bad in [
        "null",
        "",
        "https://evil.com",
        "http://127.0.0.1.evil.com",
        "http://evil.com@127.0.0.1",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "http://127.0.0.1@evil.com",
        "localhost",       // missing scheme
        "ftp://localhost", // wrong scheme
    ] {
        assert!(!origin_is_loopback(bad), "{bad:?} must be rejected");
    }
}

/// Start the real router on an ephemeral port; returns the bound address.
async fn start_http_insecure() -> String {
    start_http(HttpAuth::Insecure).await
}

/// Raw HTTP/1.1 POST (no client library needed); returns the status code.
/// `host` overrides the Host header; `headers` may carry any others.
async fn http_post(addr: &str, host: Option<&str>, headers: &[(&str, &str)], body: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let host = host.unwrap_or(addr);
    let mut req = format!(
        "POST /mcp HTTP/1.1\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if !host.is_empty() {
        req += &format!("Host: {host}\r\n");
    }
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    req += "\r\n";
    req += body;
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), s.read_to_end(&mut buf))
        .await
        .expect("response timed out");
    let text = String::from_utf8_lossy(&buf);
    text.split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or_else(|| panic!("no status line in {text:?}"))
}

/// Raw HTTP/1.1 request; returns the status code.
async fn http_req(
    addr: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    req += "\r\n";
    s.write_all(req.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(30), s.read_to_end(&mut buf))
        .await
        .expect("response timed out");
    String::from_utf8_lossy(&buf)
        .split_whitespace()
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0)
}

/// A well-formed initialize request — the request every MCP client starts
/// with. Proves legitimate local clients still get through the guard.
const INIT: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","#,
    r#""params":{"protocolVersion":"2025-03-26","capabilities":{},"#,
    r#""clientInfo":{"name":"t","version":"0"}}}"#,
);
const MCP_HEADERS: &[(&str, &str)] = &[
    ("Content-Type", "application/json"),
    ("Accept", "application/json, text/event-stream"),
];

#[tokio::test]
async fn legitimate_client_initialize_passes() {
    let addr = start_http_insecure().await;
    let status = http_post(&addr, None, MCP_HEADERS, INIT).await;
    assert_eq!(status, 200);
}

#[tokio::test]
async fn hostile_origins_are_rejected() {
    let addr = start_http_insecure().await;
    for origin in [
        "https://evil.com",
        "null",
        "http://127.0.0.1.attacker.tld",
        "http://user@127.0.0.1:7878",
    ] {
        let headers: Vec<_> = MCP_HEADERS
            .iter()
            .cloned()
            .chain([("Origin", origin)])
            .collect();
        let status = http_post(&addr, None, &headers, INIT).await;
        assert_eq!(status, 403, "Origin {origin:?} must be rejected");
    }
}

#[tokio::test]
async fn hostile_hosts_are_rejected() {
    let addr = start_http_insecure().await;
    for host in [
        "evil.com",
        "127.0.0.1.evil.com",
        "localhost.evil.com",
        "user@127.0.0.1",
    ] {
        let status = http_post(&addr, Some(host), MCP_HEADERS, INIT).await;
        assert_eq!(status, 403, "Host {host:?} must be rejected");
    }
}

#[tokio::test]
async fn loopback_ipv4_ipv6_and_loopback_origin_pass() {
    let addr = start_http_insecure().await;
    // Host variants the guard must accept (the socket is IPv4 but the
    // Host header is validated by value, not by interface)
    for host in [
        "localhost:7878",
        "127.0.0.1:7878",
        "[::1]:7878",
        "localhost",
    ] {
        let status = http_post(&addr, Some(host), MCP_HEADERS, INIT).await;
        assert_eq!(status, 200, "Host {host:?} must pass");
    }
    // local browser tooling origins pass too
    for origin in [
        "http://localhost:6274",
        "https://127.0.0.1:3000",
        "http://[::1]:9",
    ] {
        let headers: Vec<_> = MCP_HEADERS
            .iter()
            .cloned()
            .chain([("Origin", origin)])
            .collect();
        let status = http_post(&addr, None, &headers, INIT).await;
        assert_eq!(status, 200, "Origin {origin:?} must pass");
    }
}

#[tokio::test]
async fn diagnostics_reports_http_security_mode() {
    let addr = start_http_insecure().await;
    let sh = shared();
    // mounting the router is what stamps the security report
    let _app = mcp_http_router(
        sh.clone(),
        &addr,
        HttpAuth::Token(TokenSource::Fixed("t0k3n".into())),
    );
    let (err, v) = call(&sh, "diagnostics", json!({}));
    assert!(!err);
    assert_eq!(v["security"]["auth"], "bearer");
    assert!(v["security"]["transport"]
        .as_str()
        .unwrap()
        .contains("streamable-http"));
    // and the report never leaks credential material
    assert!(!v.to_string().contains("t0k3n"));
}

#[test]
fn diagnostics_reports_stdio_mode_by_default() {
    let sh = shared();
    let (err, v) = call(&sh, "diagnostics", json!({}));
    assert!(!err);
    assert_eq!(v["security"]["transport"], "stdio");
}

// ---------- issue #11: limits ----------

#[tokio::test]
async fn normal_request_passes_limits() {
    let addr = start_http_insecure().await;
    assert_eq!(
        http_req(&addr, "POST", "/mcp", MCP_HEADERS, INIT.as_bytes()).await,
        200
    );
}

#[tokio::test]
async fn oversized_body_is_rejected() {
    let addr = start_http_insecure().await;
    let body = vec![b'x'; MAX_HTTP_BODY_BYTES + 1];
    let status = http_req(&addr, "POST", "/mcp", MCP_HEADERS, &body).await;
    assert_eq!(
        status, 413,
        "over {MAX_HTTP_BODY_BYTES} bytes must not reach dispatch"
    );
}

/// A stub endpoint behind `bounded_request` lets the limits be exercised
/// with tiny values instead of the production constants.
async fn stub_limited(slots: usize, timeout: std::time::Duration) -> String {
    use axum::middleware::Next;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let gate = Arc::new(tokio::sync::Semaphore::new(slots));
    let app = axum::Router::new()
        .route(
            "/slow",
            axum::routing::get(|| async {
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                "ok"
            }),
        )
        .layer(axum::middleware::from_fn(
            move |req: axum::extract::Request, next: Next| {
                let gate = gate.clone();
                async move { bounded_request(gate, timeout, req, next).await }
            },
        ));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_requests_are_bounded() {
    let addr = stub_limited(2, std::time::Duration::from_secs(30)).await;
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let addr = addr.clone();
        set.spawn(async move { http_req(&addr, "GET", "/slow", &[], &[]).await });
    }
    let mut codes = Vec::new();
    while let Some(c) = set.join_next().await {
        codes.push(c.unwrap());
    }
    assert!(
        codes.iter().filter(|&&c| c == 429).count() >= 5,
        "slots held by slow requests must reject the flood: {codes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_requests_time_out() {
    let addr = stub_limited(8, std::time::Duration::from_millis(50)).await;
    assert_eq!(http_req(&addr, "GET", "/slow", &[], &[]).await, 504);
}

#[test]
fn apply_patch_ops_array_is_bounded() {
    let sh = shared();
    let ops = vec![serde_json::json!({"op": "insert_note"}); MAX_PATCH_OPS + 1];
    let (err, text) = call_text(&sh, "apply_patch", json!({"ops": ops}));
    assert!(err);
    assert!(text.contains("too large"), "{text}");
    // exactly at the cap the size check must not fire
    let ops = vec![serde_json::json!({"op": "bogus"}); MAX_PATCH_OPS];
    let (err, text) = call_text(&sh, "apply_patch", json!({"ops": ops}));
    assert!(err);
    assert!(!text.contains("too large"), "{text}");
}

#[test]
fn concurrent_saves_do_not_collide_on_temp_name() {
    let dir = std::env::temp_dir()
        .join("midi-editor-mcp-tests")
        .join(format!("saves-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("song.mid");
    std::thread::scope(|s| {
        for tag in ["a", "b"] {
            let p = p.clone();
            s.spawn(move || {
                for i in 0..50 {
                    write_atomic(&p, format!("{tag}{i}").as_bytes())
                        .expect("save must not fail under concurrency");
                }
            });
        }
    });
    let final_bytes = std::fs::read(&p).unwrap();
    assert!(final_bytes == b"a49" || final_bytes == b"b49");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_with_no_arg_uses_doc_path() {
    let dir = tmpdir("noarg");
    let sh = shared_in(&dir);
    // no path arg → document path, always allowed even outside roots
    let (err, v) = call(&sh, "save", json!({}));
    assert!(!err, "{v}");
    assert!(dir.join("song.mid").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn save_traversal_cannot_escape_root() {
    let dir = tmpdir("trav");
    let allowed = dir.join("allowed");
    std::fs::create_dir_all(&allowed).unwrap();
    let sh = shared_in(&allowed);
    // `../..` out of the document dir must be canonicalized then rejected
    let evil = allowed.join("..").join("..").join("evil.mid");
    let (err, _) = call(&sh, "save", json!({"path": evil.to_string_lossy()}));
    assert!(err);
    assert!(
        !dir.parent().unwrap().join("evil.mid").exists(),
        "escaped write happened"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn diagnostics_report_total_and_truncation() {
    let sh = shared();
    let (err, v) = call(&sh, "diagnostics", json!({}));
    assert!(!err);
    assert_eq!(v["total"], v["count"]);
    assert_eq!(v["truncated"], false);
}

#[test]
fn save_outside_all_roots_is_actionable_error() {
    let allowed = tmpdir("scope-a");
    let elsewhere = tmpdir("scope-b");
    let sh = shared_in(&allowed);
    let target = elsewhere.join("x.mid");
    let (err, text) = call_text(&sh, "save", json!({"path": target.to_string_lossy()}));
    assert!(err);
    assert!(text.contains("outside the MCP save scope"), "{text}");
    assert!(text.contains("MIDI_MCP_ALLOWED_ROOTS"), "{text}");
    assert!(!target.exists());
    let _ = (
        std::fs::remove_dir_all(&allowed),
        std::fs::remove_dir_all(&elsewhere),
    );
}

#[test]
fn save_via_reparse_point_cannot_escape() {
    // directory junction needs no privilege on Windows — other
    // platforms can't forge one here, so the test is Windows-only
    #[cfg(windows)]
    {
        let dir = tmpdir("junction");
        let allowed = dir.join("allowed");
        let outside = dir.join("outside");
        std::fs::create_dir_all(&allowed).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(allowed.join("link"))
            .arg(&outside)
            .status()
            .expect("mklink");
        assert!(allowed.join("link").exists());
        let sh = shared_in(&allowed);
        // looks inside the allowed root, resolves outside it
        let via_link = allowed.join("link").join("evil.mid");
        let (err, text) = call_text(&sh, "save", json!({"path": via_link.to_string_lossy()}));
        assert!(err, "junction must be resolved before the root check");
        assert!(text.contains("outside the MCP save scope"), "{text}");
        assert!(!outside.join("evil.mid").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn stdio_scope_also_allows_cwd() {
    let cwd = std::env::current_dir().unwrap();
    // parent must exist for canonicalization — use the cwd itself
    let target = cwd.join("stdio-write.mid");
    // HTTP scope refuses (not under doc dir or extra roots)
    let none: Option<PathBuf> = None;
    assert!(authorize_write(&none, FsScope::Http, &[], &target).is_err());
    // stdio trusts the spawning client: cwd is an implicit root
    assert!(authorize_write(&none, FsScope::Stdio, &[], &target).is_ok());
}

#[test]
fn env_roots_extend_the_scope() {
    let dir = tmpdir("envroots");
    let target = dir.join("ok.mid");
    std::env::set_var("MIDI_MCP_ALLOWED_ROOTS", &dir);
    let roots = roots_from_env("MIDI_MCP_ALLOWED_ROOTS");
    std::env::remove_var("MIDI_MCP_ALLOWED_ROOTS");
    let none: Option<PathBuf> = None;
    assert_eq!(
        authorize_write(&none, FsScope::Http, &roots, &target).unwrap(),
        std::fs::canonicalize(&dir).unwrap().join("ok.mid")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rpn_tools_round_trip() {
    let sh = shared();
    // canonical write: selector msb, lsb, data msb, data lsb
    let (err, _) = call(
        &sh,
        "set_rpn",
        json!({
        "track": 1, "tick": 960, "param_msb": 0, "param_lsb": 0,
        "data_msb": 12, "data_lsb": 30}),
    );
    assert!(!err);
    let (err, v) = call(&sh, "get_rpn", json!({}));
    assert!(!err && v["count"] == 1);
    let e = &v["entries"][0];
    assert_eq!(e["name"], "Pitch Bend Range");
    assert_eq!(e["value"], (12 << 7) | 30);
    let id = e["ids"][0].as_u64().unwrap();

    // value rewrite keeps selector order; 7-bit drops the LSB event
    let (err, _) = call(&sh, "update_rpn_value", json!({"id": id, "data_msb": 5}));
    assert!(!err);
    let (err, v) = call(&sh, "get_rpn", json!({}));
    assert!(!err);
    assert_eq!(v["entries"][0]["value"], 5);
    assert!(v["entries"][0]["data_lsb"].is_null());

    // retarget to coarse tuning; then nrpn + null-selector + bad id paths
    let (err, _) = call(
        &sh,
        "update_rpn_param",
        json!({"id": id, "param_msb": 0, "param_lsb": 2}),
    );
    assert!(!err);
    let (err, v) = call(&sh, "get_rpn", json!({"kind": "rpn"}));
    assert!(!err && v["entries"][0]["name"] == "Coarse Tuning");
    let (err, _) = call(
        &sh,
        "set_rpn",
        json!({
        "track": 1, "tick": 1000, "kind": "nrpn",
        "param_msb": 1, "param_lsb": 2, "data_msb": 9}),
    );
    assert!(!err);
    let (err, v) = call(&sh, "get_rpn", json!({"kind": "nrpn"}));
    assert!(!err && v["count"] == 1 && v["entries"][0]["param14"] == (1 << 7) + 2);
    let (err, _) = call(
        &sh,
        "set_rpn",
        json!({
        "track": 1, "tick": 1100, "param_msb": 127, "param_lsb": 127, "data_msb": 0}),
    );
    assert!(!err);
    let (err, v) = call(&sh, "get_rpn", json!({}));
    assert!(!err && v["count"] == 3 && v["entries"][2]["null"] == true);
    let (err, _) = call(&sh, "update_rpn_value", json!({"id": 9999, "data_msb": 1}));
    assert!(err);
    // undo restores prior state
    let (err, _) = call(&sh, "undo", json!({}));
    assert!(!err);
    let (err, v) = call(&sh, "get_rpn", json!({}));
    assert!(!err && v["count"] == 2);
}

#[test]
fn instruments_report_mode_and_names() {
    let sh = shared();
    // inject: GS reset + bank-select pair + PC on ch1, PC on ch10
    let (err, _) = call(
        &sh,
        "apply_patch",
        json!({"ops": [{"op": "insert_events", "track": 1, "events": [
            {"tick": 0, "kind": {"sysex_hex": "41 10 42 12 40 00 7f 00 41 f7"}},
            {"tick": 0, "kind": {"channel": {"status": 176, "data": [0, 0]}}},
            {"tick": 0, "kind": {"channel": {"status": 176, "data": [32, 0]}}},
            {"tick": 10, "kind": {"channel": {"status": 192, "data": [24]}}},
            {"tick": 20, "kind": {"channel": {"status": 201, "data": [0]}}},
        ]}]}),
    );
    assert!(!err);
    let (err, v) = call(&sh, "get_instruments", json!({}));
    assert!(!err);
    assert_eq!(v["mode"], "GS");
    assert_eq!(v["count"], 2);
    assert_eq!(v["programs"][0]["name"], "GS: Acoustic Guitar (nylon)");
    assert_eq!(v["programs"][1]["name"], "Standard Kit #0");
    assert_eq!(v["programs"][1]["channel"], 10);
}

#[test]
fn write_atomic_replaces_existing_file() {
    let dir = std::env::temp_dir().join("midi-editor-mcp-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("savetest.mid");
    std::fs::write(&p, b"old").unwrap();
    write_atomic(&p, b"new contents").unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), b"new contents");
    // no temp litter left beside the target
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".savetest"))
        .collect();
    assert!(leftovers.is_empty());
    let _ = std::fs::remove_file(&p);
}

#[test]
fn aftertouch_tools_round_trip() {
    let sh = shared();
    let (err, _) = call(
        &sh,
        "set_channel_pressure",
        json!({"track": 1, "points": [{"tick": 240, "value": 90}, {"tick": 480, "value": 64}]}),
    );
    assert!(!err);
    let (err, _) = call(
        &sh,
        "set_poly_pressure",
        json!({"track": 1, "tick": 600, "key": 62, "value": 70}),
    );
    assert!(!err);

    let (err, v) = call(&sh, "get_aftertouch", json!({}));
    assert!(!err);
    let at = v["aftertouch"].as_array().unwrap();
    assert_eq!(at.len(), 3);
    assert!(at
        .iter()
        .any(|r| r["kind"] == "channel" && r["value"] == 90));
    assert!(at
        .iter()
        .any(|r| r["kind"] == "poly" && r["key"] == 62 && r["value"] == 70));

    // kind/key filters
    let (err, v) = call(&sh, "get_aftertouch", json!({"kind": "poly", "key": 62}));
    assert!(!err);
    assert_eq!(v["aftertouch"].as_array().unwrap().len(), 1);
    let (err, v) = call(&sh, "get_aftertouch", json!({"kind": "poly", "key": 60}));
    assert!(!err);
    assert_eq!(v["aftertouch"].as_array().unwrap().len(), 0);

    // remove_events deletes by id (re-query the poly event)
    let (err, v) = call(&sh, "get_aftertouch", json!({"kind": "poly"}));
    assert!(!err);
    let pid = v["aftertouch"][0]["id"].as_u64().unwrap();
    let (err, _) = call(&sh, "remove_events", json!({"ids": [pid]}));
    assert!(!err);
    let (err, v) = call(&sh, "get_aftertouch", json!({"kind": "poly"}));
    assert!(!err);
    assert_eq!(v["count"], 0);
    // unknown ids error instead of silently applying nothing
    let (err, _) = call(&sh, "remove_events", json!({"ids": [999999]}));
    assert!(err);
    // undo restores — edits ride the shared undo stack
    let (err, _) = call(&sh, "undo", json!({}));
    assert!(!err);
    let (err, v) = call(&sh, "get_aftertouch", json!({"kind": "poly"}));
    assert!(!err);
    assert_eq!(v["count"], 1);
}

#[test]
fn meta_tools_create_update_remove() {
    let sh = shared();
    // create a marker on track 0 at tick 120
    let (e, r) = call(
        &sh,
        "set_meta",
        json!({"track": 0, "tick": 120, "meta_type": 6, "text": "Verse"}),
    );
    assert!(!e, "{r}");
    let (_, m) = call(&sh, "get_meta", json!({"track": 0, "meta_type": 6}));
    assert_eq!(m["count"], 1);
    let id = m["meta"][0]["id"].as_u64().unwrap();
    assert_eq!(m["meta"][0]["text"], "Verse");
    // update by id — same event, new text
    let (e, r) = call(
        &sh,
        "set_meta",
        json!({"track": 0, "tick": 120, "meta_type": 6, "text": "Chorus", "id": id}),
    );
    assert!(!e, "{r}");
    let (_, m) = call(&sh, "get_meta", json!({"track": 0, "meta_type": 6}));
    assert_eq!(m["meta"][0]["text"], "Chorus");
    // sjis bytes stay sjis on write (enc is explicit)
    let (e, _) = call(
        &sh,
        "set_meta",
        json!({"track": 0, "tick": 240, "meta_type": 5, "text": "歌", "enc": "sjis"}),
    );
    assert!(!e);
    let (_, m) = call(&sh, "get_meta", json!({"track": 0, "meta_type": 5}));
    assert_eq!(m["meta"][0]["text"], "歌");
    // key signature: insert once then update in place
    call(&sh, "set_key_signature", json!({"sf": -3, "mi": 1}));
    call(&sh, "set_key_signature", json!({"sf": 2, "mi": 0}));
    let (_, m) = call(&sh, "get_meta", json!({"track": 0, "meta_type": 89}));
    assert_eq!(m["count"], 1);
    // non-text types are rejected from set_meta
    let (e, _) = call(
        &sh,
        "set_meta",
        json!({"track": 0, "tick": 0, "meta_type": 0x59, "text": "x"}),
    );
    assert!(e);
    // remove only the named event
    let (e, _) = call(&sh, "remove_meta", json!({"track": 0, "id": id}));
    assert!(!e);
    let (_, m) = call(&sh, "get_meta", json!({"track": 0, "meta_type": 6}));
    assert_eq!(m["count"], 0);
}

#[test]
fn token_fallback_path_is_per_user() {
    use crate::http::token_file_path_with;
    let la = token_file_path_with(
        Some(r"C:\Users\re\AppData\Local".into()),
        None,
        Some("re".into()),
    );
    assert_eq!(
        la,
        std::path::PathBuf::from(r"C:\Users\re\AppData\Local")
            .join("midi-editor")
            .join("mcp-token")
    );
    let fb = token_file_path_with(None, None, Some("re".into()));
    assert!(fb.to_string_lossy().contains("midi-editor-mcp-token-re"));
    // hostile username chars can't traverse
    let evil = token_file_path_with(None, None, Some("../admin".into()));
    assert!(evil.to_string_lossy().contains("admin"));
    assert!(!evil.to_string_lossy().contains(".."));
}

#[test]
fn token_file_is_exclusive_and_owner_only() {
    use crate::http::{ensure_token_file, read_token_file};
    let dir = std::env::temp_dir().join(format!("midi-token-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mcp-token");
    ensure_token_file(&path).unwrap();
    assert!(read_token_file(&path).is_some(), "token written");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "owner-only from creation");
    }
    // a pre-planted file at the path is NOT followed or overwritten by
    // provisioning — but a corrupt one is regenerated
    std::fs::write(
        &path,
        b"corrupt !!
",
    )
    .unwrap(); // space+newline: invalid token text
    ensure_token_file(&path).unwrap();
    let t = read_token_file(&path).expect("corrupt token regenerated");
    assert_ne!(
        t,
        "corrupt !!
"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// #186 — edit tools must compute ops against the staged copy inside an
/// open transaction: notes inserted earlier in the same uncommitted batch
/// are visible to every later edit tool.
#[test]
fn edit_tools_see_staged_events_inside_transaction() {
    let sh = shared();
    let (err, v) = call(
        &sh,
        "begin_transaction",
        json!({"label": "staged edit chain"}),
    );
    assert!(!err);
    let tx = v["tx_id"].as_u64().unwrap();
    let (err, _) = call(
        &sh,
        "apply_patch",
        json!({"ops": [
            {"op": "insert_note", "track": 1, "key": 62, "start": 0, "dur": 960},
            {"op": "insert_note", "track": 1, "key": 64, "start": 480, "dur": 480},
        ], "tx_id": tx}),
    );
    assert!(!err);
    // set_length + legato against the still-uncommitted notes
    let (err, _) = call(
        &sh,
        "set_length",
        json!({"track": 1, "from": 480, "to": 960, "ticks": 240, "tx_id": tx}),
    );
    assert!(!err, "set_length must see staged notes");
    let (err, _) = call(
        &sh,
        "legato",
        json!({"track": 1, "from": 0, "to": 960, "gap": 0, "tx_id": tx}),
    );
    assert!(!err, "legato must see staged notes");
    // the staged view exposes the edited note before commit
    let (err, v) = call(&sh, "list_notes", json!({"track": 1}));
    assert!(!err);
    let notes = v["notes"].as_array().cloned().unwrap_or_default();
    // fixture key-60 + the two staged keys
    assert_eq!(notes.len(), 3, "read tools see the staged copy");
    let span = |key: u64| {
        notes
            .iter()
            .find(|n| n["key"].as_u64() == Some(key))
            .map(|n| n["end"].as_u64().unwrap_or(0) - n["start"].as_u64().unwrap_or(0))
    };
    assert_eq!(span(64), Some(240), "set_length edited the staged note");
    assert!(
        span(62).is_some_and(|s| s > 240),
        "legato modified the staged note (span {})",
        span(62).unwrap_or(0)
    );
    // commit folds everything into one revision bump
    let rev0 = sh.lock().unwrap().doc.revision();
    let (err, _) = call(&sh, "commit_transaction", json!({}));
    assert!(!err);
    let rev1 = sh.lock().unwrap().doc.revision();
    assert_eq!(rev1, rev0 + 1, "one transaction = one revision step");
}

/// #185 — a stale session's open batch must not absorb another client's
/// standalone edits, and lifecycle calls carrying a foreign tx_id are
/// refused instead of committing someone else's staging area.
#[test]
fn stale_batch_cannot_hijack_standalone_edits() {
    let sh = shared();
    // session A opens a transaction and dies without committing
    let (_, v) = call(&sh, "begin_transaction", json!({"label": "A"}));
    let a_tx = v["tx_id"].as_u64().unwrap();
    // session B's standalone edit (no tx_id) commits directly — it is
    // NOT diverted into A's batch
    let (err, v) = call(&sh, "set_tempo", json!({"tick": 0, "bpm": 100.0}));
    assert!(!err);
    assert!(
        v["staged"].is_null(),
        "standalone edit committed directly, was not staged"
    );
    assert_eq!(v["revision"], 1, "committed on the real document");
    // a lifecycle call with a foreign tx_id is refused
    let (err, v) = call(&sh, "commit_transaction", json!({"tx_id": a_tx + 999}));
    assert!(err, "foreign tx_id must not commit someone else's batch");
    assert_eq!(v["error"], "wrong_transaction");
    // the batch survived; committing it now reports the real conflict
    let (err, v) = call(&sh, "commit_transaction", json!({"tx_id": a_tx}));
    assert!(err);
    assert_eq!(v["error"], "stale_base");
    // matching tx on rollback works and closes the batch
    let (err, v) = call(&sh, "rollback_transaction", json!({"tx_id": a_tx}));
    assert!(!err);
    assert_eq!(v["rolled_back"], true);
}

#[test]
fn undo_back_to_saved_state_is_clean() {
    // #177: revision climbs on undo too, so `revision != saved_revision`
    // would report a byte-identical document as dirty forever. The save
    // marker in the undo stack decides instead.
    let sh = shared();
    let insert = |sh: &SharedDoc, tick: u64| {
        let mut g = sh.lock().unwrap();
        let ev = document::Event {
            id: g.doc.alloc_event_id(),
            tick,
            seq: 0,
            raw_body: None,
            kind: EventKind::Channel {
                status: 0x90,
                data: [72, 100],
                len: 2,
            },
        };
        let ops = vec![document::Op::InsertEvents {
            track: 1,
            events: vec![ev],
        }];
        g.apply("edit", ops).unwrap();
    };
    insert(&sh, 960);
    // save at this point (through the shared save core, like the GUI guard)
    let file = std::env::temp_dir().join(format!("dirty-undo-{}.mid", std::process::id()));
    let _ = std::fs::remove_file(&file);
    service::save_document(
        &sh,
        service::SaveRequest {
            path: Some(&file),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!sh.lock().unwrap().is_dirty());
    // edit 2 → dirty
    insert(&sh, 1440);
    assert!(sh.lock().unwrap().is_dirty());
    // undo edit 2 → content equals the saved state, so clean (#177)
    let mut g = sh.lock().unwrap();
    let Shared { doc, undo, .. } = &mut *g;
    undo.undo(doc);
    drop(g);
    assert!(
        !sh.lock().unwrap().is_dirty(),
        "undo back to the saved state must be clean"
    );
    // redo → dirty again
    let mut g = sh.lock().unwrap();
    let Shared { doc, undo, .. } = &mut *g;
    undo.redo(doc);
    drop(g);
    assert!(sh.lock().unwrap().is_dirty());
}

#[test]
fn set_track_name_writes_file_hint_encoding() {
    // #176: with no explicit enc, a file carrying the Shift-JIS hint (FF 09
    // "JP" marker) keeps its charset when renamed
    let mk_meta = |mt: u8, data: &[u8]| smf_core::Event {
        tick: 0,
        seq: 0,
        raw_body: None,
        kind: EventKind::Meta {
            meta_type: mt,
            data: bytes::Bytes::copy_from_slice(data),
        },
    };
    let f = smf_core::File {
        format: 1,
        division: smf_core::Division::Metrical(480),
        tracks: vec![smf_core::Track {
            events: vec![
                mk_meta(0x09, b"JP"),
                mk_meta(0x03, "Shift-JIS曲名".as_bytes()),
            ],
        }],
        warnings: vec![],
    };
    let sh = Arc::new(Mutex::new(Shared::new(Document::from_file(f))));
    let (err, v) = call(
        &sh,
        "set_track_name",
        json!({"track": 0, "name": "新しい名前"}),
    );
    assert!(!err, "set_track_name failed: {v}");
    let g = sh.lock().unwrap();
    let name_event = g.doc.tracks[0]
        .events
        .iter()
        .find(|e| {
            matches!(
                e.kind,
                EventKind::Meta {
                    meta_type: 0x03,
                    ..
                }
            )
        })
        .unwrap();
    let expected = smf_core::encode_text("新しい名前", smf_core::TextEncoding::ShiftJis);
    assert!(
        matches!(&name_event.kind, EventKind::Meta { data, .. } if data.as_ref() == expected.as_slice()),
        "hinted file must keep Shift-JIS bytes"
    );
}

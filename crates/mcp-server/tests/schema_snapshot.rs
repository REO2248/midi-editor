//! Schema snapshot test — the MCP tool surface (tool names, per-tool
//! `version`, deprecation metadata, descriptions, input schemas, and the
//! global `MCP_SURFACE_VERSION`) is serialized and compared against the
//! checked-in `schema_snapshot.json`.
//!
//! Any diff fails CI so a breaking change can never land by accident:
//!   - intentional breaking change → bump the tool's `version` (and
//!     `MCP_SURFACE_VERSION` when the contract is incompatible), mark the
//!     old name `deprecated` first when removing/renaming, then regenerate:
//!         MCP_UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p mcp-server --test schema_snapshot
//!   - additive change (new tool / optional arg / response field) → just
//!     regenerate the snapshot the same way.

use mcp_server::{tool_specs, MCP_SURFACE_VERSION};

const SNAPSHOT: &str = include_str!("schema_snapshot.json");
const SNAPSHOT_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/schema_snapshot.json");

/// Re-emit a value with every object's keys sorted. `serde_json::Map` is
/// order-preserving only when some workspace dep enables `preserve_order`,
/// so the raw emission order differs between `-p mcp-server` and
/// `--workspace` builds — canonicalize or the snapshot is flaky.
fn canon(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().cloned().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for k in keys {
                let v = canon(&m[&k]);
                out.insert(k, v);
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(canon).collect()),
        other => other.clone(),
    }
}

fn current_surface() -> String {
    let mut tools: Vec<serde_json::Value> = tool_specs()
        .iter()
        .map(|s| {
            canon(&serde_json::json!({
                "name": s.name,
                "version": s.version,
                "deprecated": s.deprecated,
                "description": s.tool.description.as_deref().unwrap_or_default(),
                "input_schema": *s.tool.input_schema,
            }))
        })
        .collect();
    tools.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    serde_json::to_string_pretty(&canon(&serde_json::json!({
        "mcp_surface_version": MCP_SURFACE_VERSION,
        "tools": tools,
    })))
    .unwrap()
        + "\n"
}

/// First 1-based line index where two texts differ (for the panic message).
fn first_diff_line(a: &str, b: &str) -> usize {
    for (i, (la, lb)) in a.lines().zip(b.lines()).enumerate() {
        if la != lb {
            return i + 1;
        }
    }
    a.lines().count().max(b.lines().count())
}

#[test]
fn tool_surface_matches_snapshot() {
    let current = current_surface();
    // Windows checkouts may materialize the file as CRLF; compare on LF text.
    let expected = SNAPSHOT.replace("\r\n", "\n");
    if current == expected {
        return;
    }
    if std::env::var("MCP_UPDATE_SCHEMA_SNAPSHOT").is_ok() {
        std::fs::write(SNAPSHOT_PATH, &current).unwrap();
        return;
    }
    let actual = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/schema_snapshot.actual.json");
    std::fs::write(actual, &current).unwrap();
    panic!(
        "MCP tool surface changed (first diff at line {} of the snapshot).\n\
         If this change is intentional: bump the changed tools' `version` \
         (and `MCP_SURFACE_VERSION` when the change is breaking), then run\n\
         \x20   MCP_UPDATE_SCHEMA_SNAPSHOT=1 cargo test -p mcp-server --test schema_snapshot\n\
         and commit schema_snapshot.json. The new surface was also written to\n\
         \x20   {actual}",
        first_diff_line(&current, &expected),
    );
}

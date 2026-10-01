//! Embedded MCP server.
//!
//! Topology:
//!   - the GUI app hosts Streamable-HTTP on 127.0.0.1, sharing one `SharedDoc`
//!     with the editor. HTTP requests are Bearer-authenticated by default:
//!     `MIDI_MCP_TOKEN` wins, otherwise a random token is provisioned into
//!     `%LOCALAPPDATA%\midi-editor\mcp-token` on first launch and re-read per
//!     request (rotation/revocation need no restart). Unauthenticated mode
//!     requires the explicit `MIDI_MCP_ALLOW_INSECURE=1` opt-out.
//!   - `mcp-bridge` connects to that endpoint as an rmcp client and re-serves
//!     it over stdio so stdio-only clients (Claude Desktop etc.) can reach it
//!     (it discovers the provisioned token file automatically). With `--file`
//!     it can also serve a standalone document without the app.
//!
//! Every mutation goes through `Document::apply(Transaction)` on the shared
//! doc — the exact same path GUI edits take — so undo is unified.
//!
//! ## Threat model: loopback HTTP transport
//!
//! The embedded endpoint binds to loopback only, so remote machines cannot
//! reach it. The residual risks are *local*: a hostile web page in the user's
//! browser (CSRF / DNS rebinding), and other processes running as the same
//! user.
//!
//! - **DNS rebinding / CSRF**: a browser page can issue cross-origin requests
//!   to `127.0.0.1`. Defence: requests carrying an `Origin` header must name a
//!   loopback origin (`http(s)://localhost|127.0.0.1|[::1]:<any port>`), and a
//!   present `Host` header must be a loopback host — enforced by
//!   `loopback_guard` *and* rmcp's own allowlists (defence in depth).
//!   Non-browser MCP clients send no `Origin` and are unaffected.
//! - **Unauthenticated local access**: when `MIDI_MCP_TOKEN` is unset, any
//!   local process can call mutating tools. This is accepted for convenience
//!   in the desktop app; setting `MIDI_MCP_TOKEN` enables Bearer auth. The
//!   effective mode is reported in the `diagnostics` tool output and on
//!   stderr at startup.
//! - **Web content never gains filesystem access** beyond what the MCP tools
//!   themselves expose; the guard only rejects browser-origin *requests*, it
//!   is not a substitute for authentication.

use bytes::Bytes;
use commands::UndoStack;
use document::{ApplyError, Document, Event, EventId, Op, Transaction};
use midi_io::Destination;
use rmcp::model::*;
use rmcp::service::{RequestContext, ServiceExt};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use smf_core::EventKind;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub mod service;
pub use persist::write_atomic;

/// A transport action the MCP side requests and the GUI poller drains —
/// playback itself lives in the app process (owns sinks/audio), MCP just asks.
#[derive(Debug, Clone, PartialEq)]
pub enum TransportReq {
    Play,
    Stop,
    Seek { tick: u64 },
}

/// MCP authentication posture of the transport serving `Shared`. Written by
/// `serve_http` at startup; `Stdio` is the default for stdio/frontends. The
/// status bar renders it so an unauthenticated endpoint is never invisible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAuthMode {
    /// process-local stdio — inherits the spawning client's trust
    Stdio,
    /// `Authorization: Bearer` required
    Bearer,
    /// serving HTTP without any credential check (explicit opt-out)
    Open,
}

impl McpAuthMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Stdio => "stdio",
            Self::Bearer => "bearer",
            Self::Open => "open",
        }
    }
}

/// The effective MCP security posture, reported by the `diagnostics` tool and
/// renderable by the GUI. Lives in `Shared` (not on the service) so the stdio
/// frontend, the in-app HTTP server, and the UI all see the same facts.
/// Never carries credential material — only the mode names and the
/// credential's provenance.
#[derive(Debug, Clone)]
pub struct SecurityReport {
    /// e.g. "streamable-http 127.0.0.1:7878" or "stdio"
    pub transport: String,
    /// typed auth mode — the status bar keys on this
    pub auth_mode: McpAuthMode,
    /// e.g. "auto-provisioned token file (…\mcp-token)" — tooltips/logs
    pub auth_detail: String,
    /// which Host header values are accepted
    pub host_policy: String,
    /// which Origin header values are accepted
    pub origin_policy: String,
}

impl SecurityReport {
    /// stdio transports inherit the trust of the spawning process — no HTTP
    /// attack surface exists, so Host/Origin checks do not apply.
    pub fn stdio() -> Self {
        Self {
            transport: "stdio".into(),
            auth_mode: McpAuthMode::Stdio,
            auth_detail: "process-local (no HTTP)".into(),
            host_policy: "n/a".into(),
            origin_policy: "n/a".into(),
        }
    }

    pub fn http(bind: &str, auth_mode: McpAuthMode, auth_detail: String) -> Self {
        Self {
            transport: format!("streamable-http {bind}"),
            auth_mode,
            auth_detail,
            host_policy: "loopback only (localhost/127.0.0.1/[::1])".into(),
            origin_policy: "absent or loopback only".into(),
        }
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "transport": self.transport,
            "auth": self.auth_mode.as_str(),
            "auth_detail": self.auth_detail,
            "host_policy": self.host_policy,
            "origin_policy": self.origin_policy,
        })
    }
}

/// Shared editor state. The GUI owns one `Arc`; MCP handlers hold clones and
/// lock briefly per request. `gui_notify` is bumped on every MCP-side edit so
/// the UI can poll and repaint (the GUI has no push channel into views).
///
/// The destination/track state lives here too (not in the view) so MCP tools
/// can see and route the same destinations the GUI offers, and so the GUI's
/// sidecar persist() covers MCP-originated routing changes.
mod shared;
pub use shared::*;

mod tools;
pub use tools::*;

mod http;
pub use http::*;

#[cfg(test)]
mod tests;

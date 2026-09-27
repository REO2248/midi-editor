//! Embedded MCP server.
//!
//! Spike scope: a stdio-served `ServerHandler` with a `ping` tool to prove
//! the rmcp wiring. The real surface (apply_patch/query_events/resources)
//! lands on `Document::apply` shared with the GUI.
//!
//! Production topology (see docs/research/mcp-rust.md):
//!   app hosts Streamable-HTTP on 127.0.0.1 + Bearer token, and ships the
//!   same binary with an `mcp-bridge` subcommand that bridges stdio<->HTTP
//!   for clients like Claude Desktop that can only spawn local stdio servers.

use rmcp::model::*;
use rmcp::service::{RequestContext, ServiceExt};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use std::future::Future;
use std::sync::{Arc, Mutex};

/// Shared handle to the live document — the same object the GUI edits.
/// In the app this is an `Arc<RwLock<Document>>`-style shared cell fed by a
/// command queue; here we keep the type minimal for the spike.
pub type SharedDoc = Arc<Mutex<Option<document::Document>>>;

#[derive(Clone)]
pub struct MidiService {
    doc: SharedDoc,
}

impl MidiService {
    pub fn new(doc: SharedDoc) -> Self {
        Self { doc }
    }
}

fn tool(name: &'static str, description: &'static str) -> Tool {
    Tool::new(
        name,
        description,
        Arc::new(
            serde_json::json!({ "type": "object", "properties": {} })
                .as_object()
                .unwrap()
                .clone(),
        ),
    )
}

impl ServerHandler for MidiService {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("midi-editor", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Pure-SMF MIDI editor. All edits go through Document::apply transactions; \
                 one tool call = one undo step.",
            )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        match name {
            "ping" => Some(tool("ping", "Liveness check — returns \"pong\"")),
            "document_summary" => Some(tool(
                "document_summary",
                "JSON summary of the open SMF document (format/division/track/event counts, revision)",
            )),
            _ => None,
        }
    }

    fn list_tools(
        &self,
        _req: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + '_ {
        std::future::ready(Ok(ListToolsResult::with_all_items(vec![
            self.get_tool("ping").unwrap(),
            self.get_tool("document_summary").unwrap(),
        ])))
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, McpError>> + '_ {
        let name = request.name.as_ref();
        let doc = self.doc.clone();
        std::future::ready(match name {
            "ping" => Ok(CallToolResult::success(vec![ContentBlock::text("pong")]).into()),
            "document_summary" => {
                let body = match doc.lock().unwrap().as_ref() {
                    Some(d) => serde_json::json!({
                        "format": d.format,
                        "division": format!("{:?}", d.division),
                        "tracks": d.tracks.len(),
                        "events": d.tracks.iter().map(|t| t.events.len()).sum::<usize>(),
                        "revision": d.revision(),
                    })
                    .to_string(),
                    None => "no document open".to_string(),
                };
                Ok(CallToolResult::success(vec![ContentBlock::text(body)]).into())
            }
            _ => Err(McpError::method_not_found::<CallToolRequestMethod>()),
        })
    }
}

/// Serve over stdio (what `app.exe mcp-bridge` / a local MCP client expects).
pub async fn serve_stdio(doc: SharedDoc) -> anyhow::Result<()> {
    let service = MidiService::new(doc)
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

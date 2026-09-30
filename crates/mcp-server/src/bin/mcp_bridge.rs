//! `mcp-bridge` — stdio MCP frontend.
//!
//! Default: proxy to the running editor's in-app Streamable-HTTP endpoint
//! (`--url`, `--token`), so stdio-only clients (Claude Desktop etc.) can use
//! it as a spawned local server:
//!     mcp-bridge --url http://127.0.0.1:7878/mcp --token <MIDI_MCP_TOKEN>
//!
//! Standalone mode: `--file song.mid` loads the file into a local document
//! and serves it directly over stdio — no app needed. Edits stay in memory;
//! `save` writes the file.

use mcp_server::SharedDoc;
use rmcp::model::*;
use rmcp::service::{Peer, RequestContext, RoleClient, ServiceExt};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use std::future::Future;
use std::path::PathBuf;

#[derive(Clone)]
struct ForwardService {
    peer: Peer<RoleClient>,
    upstream_name: String,
}

impl ServerHandler for ForwardService {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("mcp-bridge", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!("stdio bridge -> {}", self.upstream_name))
    }

    fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, McpError>> + '_ {
        let peer = self.peer.clone();
        async move {
            peer.list_tools(request)
                .await
                .map_err(|e| McpError::internal_error(format!("upstream: {e}"), None))
        }
    }

    fn call_tool(
        &self,
        request: CallToolRequestParams,
        _ctx: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, McpError>> + '_ {
        let peer = self.peer.clone();
        async move {
            peer.call_tool(request)
                .await
                .map(|r| r.into())
                .map_err(|e| McpError::internal_error(format!("upstream: {e}"), None))
        }
    }
}

/// Standalone `--file` mode uses the same persistence core as the app:
/// parse via `service::load_document`, save via `service::save_document`.
fn load_file(path: &std::path::Path) -> anyhow::Result<SharedDoc> {
    Ok(mcp_server::service::open_shared(path)?)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let mut url = "http://127.0.0.1:7878/mcp".to_string();
    let mut token = std::env::var("MIDI_MCP_TOKEN").ok();
    let mut file: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--url" => url = args.next().unwrap_or(url),
            // an empty bearer token is never valid — a missing value must
            // fail loudly, not silently send "Bearer "
            "--token" => match args.next() {
                Some(t) if !t.is_empty() => token = Some(t),
                _ => anyhow::bail!("--token requires a value"),
            },
            "--file" => file = args.next().map(PathBuf::from),
            _ => {}
        }
    }

    if let Some(f) = file {
        let doc = load_file(&f)?;
        return mcp_server::serve_stdio(doc).await;
    }

    use rmcp::transport::streamable_http_client::{
        StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
    };
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(url.clone());
    if let Some(t) = token {
        cfg = cfg.auth_header(t);
    }
    // from_config is the reqwest-client impl (feature transport-streamable-http-client-reqwest)
    let transport = StreamableHttpClientTransport::from_config(cfg);
    let client = rmcp::service::serve_client((), transport)
        .await
        .map_err(|e| anyhow::anyhow!("cannot reach {url}: {e}"))?;
    let svc = ForwardService {
        peer: client.peer().clone(),
        upstream_name: url,
    };
    svc.serve(rmcp::transport::stdio()).await?.waiting().await?;
    Ok(())
}

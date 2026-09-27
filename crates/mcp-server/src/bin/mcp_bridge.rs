//! spike: stdio MCP server entrypoint.
//! In production this subcommand bridges stdio <-> the app's localhost HTTP
//! endpoint; for the spike it serves the handler directly on stdio.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let doc: mcp_server::SharedDoc = Default::default();
    mcp_server::serve_stdio(doc).await
}

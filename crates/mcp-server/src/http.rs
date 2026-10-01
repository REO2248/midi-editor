//! HTTP transport hardening for the embedded MCP endpoint.
//!
//! The endpoint binds loopback only; the residual risk is *local* — a
//! hostile browser page (CSRF / DNS rebinding) or a same-user process.
//! This module owns `Host`/`Origin` loopback checks, Bearer-token
//! provisioning/validation, auth-failure rate limiting, body-size and
//! concurrency limits, and the axum router that layers them.
//! It never sees the credential beyond comparing it; tokens live on disk
//! under %LOCALAPPDATA% with user-only ACLs.

use super::*;

/// Loopback hostnames accepted in `Host`/`Origin` authority checks.
const LOOPBACK_HOSTS: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// `Origin` values the endpoint accepts: any port on a loopback host, either
/// HTTP scheme. rmcp matches `(scheme, host, port)` tuples where an absent
/// allowlist port is a wildcard, so these six entries cover local browser
/// tooling (e.g. MCP Inspector) while rejecting every remote origin.
const LOOPBACK_ORIGINS: &[&str] = &[
    "http://localhost",
    "https://localhost",
    "http://127.0.0.1",
    "https://127.0.0.1",
    "http://[::1]",
    "https://[::1]",
];

pub(crate) fn is_loopback_host(host: &str) -> bool {
    LOOPBACK_HOSTS.contains(&host.to_ascii_lowercase().as_str())
}

/// Host part of a `host[:port]` / `[v6][:port]` authority, lowercased.
/// Anything surprising — userinfo, whitespace, unbalanced brackets, a stray
/// colon, a non-numeric port — is malformed, not loopback.
pub(crate) fn authority_host(authority: &str) -> Option<String> {
    if authority.is_empty() || authority.contains('@') || authority.chars().any(char::is_whitespace)
    {
        return None;
    }
    if let Some(rest) = authority.strip_prefix('[') {
        let (v6, tail) = rest.split_once(']')?;
        match tail.strip_prefix(':') {
            None if tail.is_empty() => {}
            Some(port) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {}
            _ => return None,
        }
        return Some(v6.to_ascii_lowercase());
    }
    match authority.split(':').collect::<Vec<_>>().as_slice() {
        [host] if !host.is_empty() => Some(host.to_ascii_lowercase()),
        [host, port]
            if !host.is_empty() && !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) =>
        {
            Some(host.to_ascii_lowercase())
        }
        _ => None,
    }
}

/// An `Origin` header value is acceptable iff it is `http(s)://<loopback>`
/// with any port. `Origin: null`, remote hosts, userinfo tricks, and
/// malformed values all fail — non-browser clients simply omit the header.
pub(crate) fn origin_is_loopback(origin: &str) -> bool {
    let origin = origin.trim();
    let rest = origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"));
    let Some(rest) = rest else { return false };
    let authority = rest.split('/').next().unwrap_or("");
    authority_host(authority).is_some_and(|h| is_loopback_host(&h))
}

fn forbidden(reason: &'static str) -> axum::response::Response {
    axum::response::Response::builder()
        .status(axum::http::StatusCode::FORBIDDEN)
        .body(axum::body::Body::from(reason))
        .expect("static response")
}

/// Explicit browser-origin / DNS-rebinding guard for the loopback endpoint —
/// the primary check described by the MCP transport guidance for local HTTP
/// servers. A present `Host` must name a loopback host; a present `Origin`
/// must name a loopback origin. Non-browser MCP clients send neither and are
/// unaffected; browser `fetch`/`XHR` from a rebinded or remote page always
/// carries a hostile `Origin` and is rejected before tool dispatch.
async fn loopback_guard(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    if let Some(host) = req.headers().get(axum::http::header::HOST) {
        let ok = host
            .to_str()
            .ok()
            .and_then(authority_host)
            .is_some_and(|h| is_loopback_host(&h));
        if !ok {
            tracing::warn!(host = ?host, "mcp http: rejected non-loopback Host");
            return forbidden("forbidden host");
        }
    }
    if let Some(origin) = req.headers().get(axum::http::header::ORIGIN) {
        if !origin.to_str().ok().is_some_and(origin_is_loopback) {
            tracing::warn!(origin = ?origin, "mcp http: rejected non-loopback Origin");
            return forbidden("forbidden origin");
        }
    }
    next.run(req).await
}

// ---------- HTTP authentication ----------

/// Where the bearer token comes from. `File` is re-read on every request so
/// rotating or revoking the credential (rewrite/delete the file) takes effect
/// without restarting the app.
pub enum TokenSource {
    /// `MIDI_MCP_TOKEN` / `--token`: fixed for the server's lifetime.
    Fixed(String),
    /// Auto-provisioned per-user token file.
    File(PathBuf),
}

impl TokenSource {
    fn token(&self) -> Option<String> {
        match self {
            Self::Fixed(t) => Some(t.clone()),
            Self::File(p) => read_token_file(p),
        }
    }

    fn describe(&self) -> String {
        match self {
            Self::Fixed(_) => "MIDI_MCP_TOKEN".into(),
            Self::File(p) => format!("auto-provisioned token file ({})", p.display()),
        }
    }
}

/// Authentication posture for the HTTP endpoint.
pub enum HttpAuth {
    /// `Authorization: Bearer <token>` required.
    Token(TokenSource),
    /// Explicit opt-out (`MIDI_MCP_ALLOW_INSECURE`) — unauthenticated loopback.
    Insecure,
}

/// Per-user token file: `%LOCALAPPDATA%\midi-editor\mcp-token` normally —
/// a directory only the owning user can read, so other local accounts cannot
/// steal the credential.
pub fn token_file_path() -> PathBuf {
    for var in ["LOCALAPPDATA", "APPDATA"] {
        if let Ok(d) = std::env::var(var) {
            if !d.is_empty() {
                return PathBuf::from(d).join("midi-editor").join("mcp-token");
            }
        }
    }
    std::env::temp_dir().join("midi-editor-mcp-token")
}

/// Generated tokens are 64 lowercase hex; a file/env-provided token just has
/// to be a single line of printable ASCII.
pub(crate) fn token_is_valid(t: &str) -> bool {
    !t.is_empty() && t.len() <= 256 && t.bytes().all(|b| b.is_ascii_graphic())
}

pub(crate) fn read_token_file(path: &std::path::Path) -> Option<String> {
    let t = std::fs::read_to_string(path).ok()?.trim().to_string();
    token_is_valid(&t).then_some(t)
}

/// Provision the token file on first launch; reuse it afterwards. Never
/// overwrites a healthy file, and regenerates a corrupt/empty one.
pub(crate) fn ensure_token_file(path: &std::path::Path) -> std::io::Result<()> {
    if read_token_file(path).is_some() {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(std::io::Error::other)?;
    write_atomic(path, bytes_hex(&raw).as_bytes()).map_err(std::io::Error::other)
}

/// The stored auto-provisioned token, for `mcp-bridge` to pass through when
/// no `--token`/`MIDI_MCP_TOKEN` was given — keeps stdio clients ergonomic.
pub fn read_stored_token() -> Option<String> {
    read_token_file(&token_file_path())
}

/// Resolve the effective HTTP auth posture, most explicit wins:
///   1. `MIDI_MCP_TOKEN` (non-empty)     → fixed bearer token
///   2. `MIDI_MCP_ALLOW_INSECURE` truthy → explicit unauthenticated opt-out
///   3. otherwise                        → auto-provisioned token file
///
/// Errors instead of silently serving unauthenticated when provisioning
/// fails — failing open would hand mutating tools to any local process.
pub fn resolve_http_auth() -> anyhow::Result<HttpAuth> {
    if let Ok(t) = std::env::var("MIDI_MCP_TOKEN") {
        if token_is_valid(&t) {
            return Ok(HttpAuth::Token(TokenSource::Fixed(t)));
        }
    }
    let insecure = std::env::var("MIDI_MCP_ALLOW_INSECURE")
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if insecure {
        return Ok(HttpAuth::Insecure);
    }
    let path = token_file_path();
    ensure_token_file(&path).map_err(|e| {
        anyhow::anyhow!("cannot provision MCP auth token at {}: {e}", path.display())
    })?;
    Ok(HttpAuth::Token(TokenSource::File(path)))
}

/// Constant-time string equality — the token isn't length-secret, so early
/// exit on length is fine; the byte loop itself must not short-circuit.
pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Throttle authentication failures per client IP. A local port scanner or
/// hostile page gets a bounded number of guesses, then 429s — and every
/// failure is logged by IP only, never with the presented credential.
struct AuthLimiter {
    fails: Mutex<std::collections::HashMap<std::net::IpAddr, (u32, std::time::Instant)>>,
}

pub(crate) const AUTH_FAIL_MAX: u32 = 10;
const AUTH_FAIL_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);

impl AuthLimiter {
    fn new() -> Self {
        Self {
            fails: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// May this IP attempt another auth check right now?
    fn allow_attempt(&self, ip: std::net::IpAddr) -> bool {
        let mut m = self.fails.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((n, first)) = m.get(&ip) {
            if first.elapsed() < AUTH_FAIL_WINDOW && *n >= AUTH_FAIL_MAX {
                return false;
            }
        }
        // keep the map bounded — loopback space is tiny but don't grow forever
        if m.len() > 1024 {
            m.clear();
        }
        true
    }

    fn record(&self, ip: std::net::IpAddr, ok: bool) {
        let mut m = self.fails.lock().unwrap_or_else(|e| e.into_inner());
        match m.get_mut(&ip) {
            Some((n, first)) if first.elapsed() < AUTH_FAIL_WINDOW => {
                if ok {
                    *n = 0;
                } else {
                    *n += 1;
                }
            }
            _ => {
                m.insert(ip, (if ok { 0 } else { 1 }, std::time::Instant::now()));
            }
        }
    }
}

fn http_error(status: axum::http::StatusCode, msg: &'static str) -> axum::response::Response {
    axum::response::Response::builder()
        .status(status)
        .body(axum::body::Body::from(msg))
        .expect("static response")
}

// ---------- HTTP request limits ----------

/// Structured rejection for transport-level limits — a JSON-RPC-shaped error
/// body so MCP clients surface something actionable instead of raw HTTP text.
fn limit_error(status: axum::http::StatusCode, msg: &'static str) -> axum::response::Response {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": null,
        "error": {"code": -32000, "message": msg},
    })
    .to_string();
    axum::response::Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .expect("static response")
}

/// Bound a request's concurrency slot and total time-to-response. Dropping
/// the future on client disconnect cancels the work — axum/tokio do that for
/// free; the permit releases either way.
pub(crate) async fn bounded_request(
    gate: Arc<tokio::sync::Semaphore>,
    timeout: std::time::Duration,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Ok(_permit) = gate.try_acquire_owned() else {
        return limit_error(
            axum::http::StatusCode::TOO_MANY_REQUESTS,
            "MCP concurrency limit reached — retry later",
        );
    };
    match tokio::time::timeout(timeout, next.run(req)).await {
        Ok(resp) => resp,
        Err(_) => limit_error(axum::http::StatusCode::GATEWAY_TIMEOUT, "request timed out"),
    }
}

/// Build the `/mcp` router with the full HTTP security posture: the explicit
/// `loopback_guard` (outermost layer), bounded concurrency/time, Bearer
/// auth middleware (with per-IP failure throttling), body-size cap and rmcp's
/// own Host/Origin allowlists configured explicitly rather than left at
/// library defaults. Split out of [`serve_http`] so tests can mount it on an
/// ephemeral port.
pub fn mcp_http_router(doc: SharedDoc, addr: &str, auth: HttpAuth) -> axum::Router {
    use axum::middleware::Next;
    use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };
    use std::net::SocketAddr;

    let (auth_mode, auth_detail) = match &auth {
        HttpAuth::Token(src) => (McpAuthMode::Bearer, src.describe()),
        HttpAuth::Insecure => (McpAuthMode::Open, "MIDI_MCP_ALLOW_INSECURE".into()),
    };
    doc.lock().unwrap_or_else(|e| e.into_inner()).mcp_security =
        SecurityReport::http(addr, auth_mode, auth_detail);

    let factory = {
        let doc = doc.clone();
        move || -> Result<MidiService, std::io::Error> { Ok(MidiService::new(doc.clone())) }
    };
    // Explicit allowlists: the library default disables Origin validation,
    // which is exactly the DNS-rebinding gap this guards. Loopback Hosts and
    // loopback Origins only; bodies are capped at MAX_HTTP_BODY_BYTES.
    let config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(LOOPBACK_HOSTS.iter().copied())
        .with_allowed_origins(LOOPBACK_ORIGINS.iter().copied())
        .with_max_request_body_bytes(MAX_HTTP_BODY_BYTES);
    let service =
        StreamableHttpService::new(factory, Arc::new(LocalSessionManager::default()), config);

    let mut app = axum::Router::new().route_service("/mcp", service);
    if let HttpAuth::Token(src) = auth {
        let src = Arc::new(src);
        let limiter = Arc::new(AuthLimiter::new());
        app = app.layer(axum::middleware::from_fn(
            move |axum::extract::ConnectInfo(peer): axum::extract::ConnectInfo<SocketAddr>,
                  req: axum::extract::Request,
                  next: Next| {
                let (src, limiter) = (src.clone(), limiter.clone());
                async move {
                    let ip = peer.ip();
                    if !limiter.allow_attempt(ip) {
                        tracing::warn!(ip = %ip, "mcp http: auth attempts throttled");
                        return http_error(
                            axum::http::StatusCode::TOO_MANY_REQUESTS,
                            "too many failed auth attempts — retry later",
                        );
                    }
                    let presented = req
                        .headers()
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.strip_prefix("Bearer "));
                    let expected = src.token();
                    let ok = match (presented, expected) {
                        (Some(p), Some(e)) => constant_time_eq(p, &e),
                        _ => false,
                    };
                    limiter.record(ip, ok);
                    if ok {
                        next.run(req).await
                    } else {
                        // log the attempt, never the credential
                        tracing::warn!(ip = %ip, "mcp http: auth failed");
                        http_error(axum::http::StatusCode::UNAUTHORIZED, "unauthorized")
                    }
                }
            },
        ));
    }
    // cheap limit checks run before auth/body work, but after the loopback
    // guard so hostile Host/Origin is rejected even under a saturated gate
    let gate = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_REQUESTS));
    let app = app.layer(axum::middleware::from_fn(
        move |req: axum::extract::Request, next: Next| {
            let gate = gate.clone();
            async move { bounded_request(gate, REQUEST_TIMEOUT, req, next).await }
        },
    ));
    // guard is applied last so it is the outermost layer: hostile Host/Origin
    // requests are rejected before auth and before tool dispatch.
    app.layer(axum::middleware::from_fn(loopback_guard))
}

/// Serve Streamable-HTTP on `addr` (e.g. "127.0.0.1:7878") at path `/mcp`.
/// `auth` comes from [`resolve_http_auth`] — Bearer by default, explicitly
/// opted-out `Insecure` otherwise.
/// `shutdown`: resolve to stop accepting connections and drain in-flight
/// requests (axum graceful shutdown — open connections finish their work).
pub async fn serve_http(
    doc: SharedDoc,
    addr: &str,
    auth: HttpAuth,
    shutdown: tokio::sync::oneshot::Receiver<()>,
) -> anyhow::Result<()> {
    let app = mcp_http_router(doc, addr, auth);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("mcp http listening on {addr}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown.await;
    })
    .await?;
    Ok(())
}

//! Local HTTP(S) forward-proxy listener. Speaks the two request shapes a
//! client sends when it's configured with `HTTPS_PROXY`/`HTTP_PROXY`/a
//! browser proxy setting: `CONNECT host:port` (used for HTTPS — we hand
//! back a raw byte tunnel) and absolute-form requests (`GET http://host/path`
//! — forwarded to the upstream VPN proxy with our own Proxy-Authorization
//! injected). Origin-form requests (bare `GET /path`, no scheme+host) aren't
//! supported — a client only sends those to an origin server, never to a
//! proxy, so a conformant proxy client never triggers this.

use anyhow::{anyhow, Result};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use internxt_core::vpn::VpnLocation;

use super::relay;
use crate::session_creds::SharedCreds;

const MAX_HEADER_BYTES: usize = 16 * 1024;

pub async fn run(host: &str, port: u16, location: Arc<VpnLocation>, creds: Arc<SharedCreds>, verbose: bool) -> Result<()> {
    let listener =
        TcpListener::bind((host, port)).await.map_err(|e| anyhow!("binding VPN HTTP(S) proxy on {host}:{port}: {e}"))?;
    loop {
        let (stream, peer) = listener.accept().await?;
        stream.set_nodelay(true).ok();
        let creds = creds.clone();
        let location = location.clone();
        tokio::spawn(async move {
            // Always logged, unlike the per-request access log below (gated
            // on `verbose`) — a wrong location/token otherwise fails every
            // single connection with zero visibility unless `-v` happens to
            // be on.
            if let Err(e) = handle(stream, location, creds, verbose).await {
                eprintln!("[vpn/https] {peer}: {e:#}");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, location: Arc<VpnLocation>, creds: Arc<SharedCreds>, verbose: bool) -> Result<()> {
    let Some((head, leftover)) = read_head(&mut stream).await? else {
        // Closed without sending a single byte — a browser/OS speculatively
        // opening (and abandoning) a connection, a health check, a
        // connection-pool probe, etc. Routine, not an error worth logging.
        return Ok(());
    };
    let text = String::from_utf8_lossy(&head);
    let mut lines = text.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();

    let token = creds.get().token.clone();

    if method.eq_ignore_ascii_case("CONNECT") {
        if verbose {
            eprintln!("[vpn/https] CONNECT {target}");
        }
        let tunnel = match relay::connect(&location, &token, target).await {
            Ok(t) => t,
            Err(e) => {
                stream.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n").await.ok();
                return Err(e);
            }
        };
        stream.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
        return relay::splice_with_leftover(stream, tunnel, leftover).await;
    }

    let dest = absolute_form_authority(target).ok_or_else(|| {
        anyhow!("unsupported proxy request target {target:?} (only CONNECT and absolute-form http:// are supported)")
    })?;
    if verbose {
        eprintln!("[vpn/https] {method} {target}");
    }
    let mut tunnel = relay::connect(&location, &token, &dest).await?;
    let auth = internxt_core::vpn::proxy_credentials(&location, &token);
    let rewritten = rewrite_request_head(request_line, lines, &auth);
    tunnel.write_all(rewritten.as_bytes()).await?;
    if !leftover.is_empty() {
        tunnel.write_all(&leftover).await?;
    }
    relay::splice(stream, tunnel).await
}

/// Reads request-line + headers up to the blank line, bounded. Returns the
/// header bytes (without the trailing blank line) and any bytes already
/// read past it (the start of a request body, or pipelined data) — or
/// `None` if the peer closed without sending anything at all (see the
/// caller: that's routine, not an error). Closing *mid*-request (some bytes
/// sent, then abandoned) is still reported — that's a more unusual case
/// worth seeing.
async fn read_head(stream: &mut TcpStream) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(pos) = find_double_crlf(&buf) {
            let leftover = buf.split_off(pos + 4);
            buf.truncate(pos);
            return Ok(Some((buf, leftover)));
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(anyhow!("request headers too large"));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            if buf.is_empty() {
                return Ok(None);
            }
            return Err(anyhow!("connection closed before headers completed"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Extracts `host:port` from an absolute-form request target
/// (`http://host[:port]/path`). IPv6 literal hosts (`[::1]`) aren't
/// supported — rare for a proxy target and not worth the extra parsing.
fn absolute_form_authority(target: &str) -> Option<String> {
    let rest = target.strip_prefix("http://")?;
    let authority = rest.split('/').next().unwrap_or(rest);
    if authority.is_empty() {
        return None;
    }
    if authority.contains(':') {
        Some(authority.to_string())
    } else {
        Some(format!("{authority}:80"))
    }
}

/// Rebuilds the request head with any existing Proxy-Authorization/Connection
/// headers stripped and our own injected, plus `Connection: close` — each
/// local connection maps to exactly one upstream request, so there's no
/// keep-alive re-auth to worry about.
fn rewrite_request_head<'a>(request_line: &str, rest: impl Iterator<Item = &'a str>, auth: &str) -> String {
    let mut out = String::new();
    out.push_str(request_line);
    out.push_str("\r\n");
    for line in rest {
        if line.is_empty() {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("proxy-authorization:") || lower.starts_with("connection:") || lower.starts_with("proxy-connection:")
        {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(&format!("Proxy-Authorization: {auth}\r\n"));
    out.push_str("Connection: close\r\n\r\n");
    out
}

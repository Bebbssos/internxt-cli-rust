//! Shared plumbing for talking to the upstream Internxt VPN proxy: open a
//! (plain, unencrypted — see `internxt_core::config::vpn_proxy_host`) TCP
//! connection to it and CONNECT-tunnel to the real destination. Both local
//! listeners (`http_proxy`, `socks5`) hand off to this once they've parsed
//! their own protocol's destination address — from here on it's a pure
//! byte relay, nothing past the CONNECT handshake is inspected.

use anyhow::{anyhow, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

use internxt_core::vpn::{proxy_credentials, proxy_server, VpnLocation};

/// Opens a fresh connection to the upstream VPN proxy and CONNECT-tunnels
/// to `dest` (`host:port`), authenticated for `location`. Every local
/// connection gets its own upstream connection — the proxy validates
/// Proxy-Authorization per-connection, so there's no session to reuse.
pub async fn connect(location: &VpnLocation, token: &str, dest: &str) -> Result<TcpStream> {
    let server = proxy_server();
    let mut tcp = TcpStream::connect((server.host.as_str(), server.port))
        .await
        .map_err(|e| anyhow!("connecting to VPN proxy {}:{}: {e}", server.host, server.port))?;
    tcp.set_nodelay(true).ok();

    let auth = proxy_credentials(location, token);
    let req =
        format!("CONNECT {dest} HTTP/1.1\r\nHost: {dest}\r\nProxy-Authorization: {auth}\r\nProxy-Connection: Keep-Alive\r\n\r\n");
    tcp.write_all(req.as_bytes()).await?;

    let status_line = read_status_line(&mut tcp).await?;
    if !status_line.contains(" 200 ") {
        return Err(anyhow!("VPN proxy refused CONNECT {dest}: {}", status_line.trim()));
    }
    Ok(tcp)
}

/// Reads the upstream's CONNECT response up to the blank line, discarding
/// the headers (we only need the status line). Bounded so a
/// misbehaving/dead upstream can't stall this or exhaust memory.
async fn read_status_line(tcp: &mut TcpStream) -> Result<String> {
    let mut buf = Vec::with_capacity(512);
    let mut byte = [0u8; 1];
    loop {
        if buf.len() > 8192 {
            return Err(anyhow!("VPN proxy sent an oversized CONNECT response"));
        }
        let n = tcp.read(&mut byte).await?;
        if n == 0 {
            return Err(anyhow!("VPN proxy closed the connection during CONNECT"));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).lines().next().unwrap_or_default().to_string())
}

/// Splices `local` and the already-established upstream `tunnel`
/// bidirectionally until either side closes.
pub async fn splice<S>(local: S, tunnel: TcpStream) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut lr, mut lw) = tokio::io::split(local);
    let (mut tr, mut tw) = tokio::io::split(tunnel);
    let up = tokio::io::copy(&mut lr, &mut tw);
    let down = tokio::io::copy(&mut tr, &mut lw);
    tokio::select! {
        r = up => { r?; }
        r = down => { r?; }
    }
    Ok(())
}

/// Same as [`splice`], but first forwards `leftover` bytes the caller
/// already read off `local` past the header block it parsed (a request
/// body, or pipelined bytes) before starting the raw relay.
pub async fn splice_with_leftover<S>(local: S, mut tunnel: TcpStream, leftover: Vec<u8>) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if !leftover.is_empty() {
        tunnel.write_all(&leftover).await?;
    }
    splice(local, tunnel).await
}

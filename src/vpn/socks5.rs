//! Local SOCKS5 listener (RFC 1928, CONNECT only, no auth — same trust
//! model as the HTTP(S) listener: anything that can reach the bound
//! host:port can use it). Domain-name targets are forwarded to the upstream
//! VPN proxy as-is rather than resolved locally, so DNS happens
//! server-side — the "h" in socks5h, always on, no separate mode needed.

use anyhow::{anyhow, Result};
use std::net::Ipv6Addr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use internxt_core::vpn::VpnLocation;

use super::relay;
use crate::session_creds::SharedCreds;

pub async fn run(host: &str, port: u16, location: Arc<VpnLocation>, creds: Arc<SharedCreds>, verbose: bool) -> Result<()> {
    let listener =
        TcpListener::bind((host, port)).await.map_err(|e| anyhow!("binding VPN SOCKS5 proxy on {host}:{port}: {e}"))?;
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
                eprintln!("[vpn/socks5] {peer}: {e:#}");
            }
        });
    }
}

async fn handle(mut stream: TcpStream, location: Arc<VpnLocation>, creds: Arc<SharedCreds>, verbose: bool) -> Result<()> {
    greeting(&mut stream).await?;
    let dest = read_connect_request(&mut stream).await?;
    let token = creds.get().token.clone();
    if verbose {
        eprintln!("[vpn/socks5] CONNECT {dest}");
    }
    let tunnel = match relay::connect(&location, &token, &dest).await {
        Ok(t) => t,
        Err(e) => {
            reply(&mut stream, 0x05).await.ok(); // general SOCKS server failure
            return Err(e);
        }
    };
    reply(&mut stream, 0x00).await?; // succeeded
    relay::splice(stream, tunnel).await
}

/// `[VER, NMETHODS, METHODS...] -> [VER, METHOD]`. We only offer "no auth"
/// (0x00); a client that requires anything else gets 0xFF and disconnects.
async fn greeting(stream: &mut TcpStream) -> Result<()> {
    let mut head = [0u8; 2];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        return Err(anyhow!("unsupported SOCKS version {}", head[0]));
    }
    let mut methods = vec![0u8; head[1] as usize];
    stream.read_exact(&mut methods).await?;
    if methods.contains(&0x00) {
        stream.write_all(&[0x05, 0x00]).await?;
        Ok(())
    } else {
        stream.write_all(&[0x05, 0xFF]).await?;
        Err(anyhow!("client offered no acceptable SOCKS5 auth method"))
    }
}

/// `[VER, CMD, RSV, ATYP, DST.ADDR, DST.PORT] -> "host:port"`. Only
/// CMD=CONNECT (0x01) is supported — this is a proxy relay, not a full
/// SOCKS5 server (no BIND/UDP ASSOCIATE; the upstream gost endpoint doesn't
/// support them either, see [`crate::vpn`] module docs).
async fn read_connect_request(stream: &mut TcpStream) -> Result<String> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    let cmd = head[1];
    let atyp = head[3];
    if cmd != 0x01 {
        reply(stream, 0x07).await.ok(); // command not supported
        return Err(anyhow!("unsupported SOCKS5 command {cmd} (only CONNECT is supported)"));
    }
    let host = match atyp {
        0x01 => {
            let mut ip = [0u8; 4];
            stream.read_exact(&mut ip).await?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            stream.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| anyhow!("non-UTF8 SOCKS5 domain name"))?
        }
        0x04 => {
            let mut ip = [0u8; 16];
            stream.read_exact(&mut ip).await?;
            Ipv6Addr::from(ip).to_string()
        }
        other => {
            reply(stream, 0x08).await.ok(); // address type not supported
            return Err(anyhow!("unsupported SOCKS5 address type {other}"));
        }
    };
    let mut port = [0u8; 2];
    stream.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);
    Ok(format!("{host}:{port}"))
}

/// Sends a SOCKS5 reply with the given REP code and a placeholder
/// 0.0.0.0:0 bind address — real clients only check REP, and gost doesn't
/// give us a meaningful bound address to report back anyway.
async fn reply(stream: &mut TcpStream, rep: u8) -> Result<()> {
    stream.write_all(&[0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;
    Ok(())
}

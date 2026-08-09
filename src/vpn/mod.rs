//! `vpn locations` / `vpn proxy` — list Internxt VPN locations and run a
//! local HTTP(S)/SOCKS5 proxy that tunnels through it.
//!
//! The VPN is not a tunnel on the wire: Internxt runs one shared HTTP(S)
//! forward proxy (gost) for every location, picked per-connection via the
//! Proxy-Authorization *username* (see internxt-core's `vpn` module, which
//! this reverse-engineering is based on — no official CLI ports this). This
//! module is the front-end half: it owns the local listeners (like `serve`
//! owns its backends) and speaks the outbound TLS+CONNECT relay to gost.
//! Neither listener resolves DNS or a full SOCKS5 UDP/BIND surface — gost
//! itself is CONNECT-only, so there's nothing to bridge those to.

mod http_proxy;
mod relay;
mod socks5;

use anyhow::{anyhow, Result};
use std::str::FromStr;
use std::sync::Arc;

use internxt_core::vpn::{VpnApi, VpnLocation};

use crate::auth;
use crate::output;
use crate::session_creds::{spawn_refresh, SharedCreds};

pub async fn locations() -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = VpnApi::new();
    let zones = api.available_locations(&creds.token).await?;

    if output::is_json() {
        let list: Vec<_> = zones.iter().map(|l| serde_json::json!({ "code": l.code(), "label": l.label() })).collect();
        output::emit("", serde_json::json!({ "success": true, "list": { "locations": list } }));
        return Ok(());
    }

    if zones.is_empty() {
        output::status("No VPN locations available on your current plan.");
        return Ok(());
    }
    // `label()` is `None` for a zone code this build doesn't have a name
    // for yet (server added a location we don't know about) — shown as
    // "-" rather than dropping the row, so it's still usable via its code.
    let rows: Vec<Vec<String>> =
        zones.iter().map(|l| vec![l.code().to_string(), l.label().unwrap_or("-").to_string()]).collect();
    crate::drive_ops::print_table(&["Code", "Location"], &rows);
    Ok(())
}

/// Starts the listeners named in `protocols` (comma-separated, known:
/// `https`, `socks5` — mirrors `serve`'s `PROTOCOLS` argument) and runs
/// until Ctrl-C or a listener dies.
#[allow(clippy::too_many_arguments)]
pub async fn proxy(
    protocols: &str,
    location: &str,
    https_host: &str,
    https_port: u16,
    socks5_host: &str,
    socks5_port: u16,
    verbose: bool,
) -> Result<()> {
    let location = Arc::new(VpnLocation::from_str(location)?);

    let mut run_https = false;
    let mut run_socks5 = false;
    for p in protocols.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match p.to_ascii_lowercase().as_str() {
            "https" | "http" => run_https = true,
            "socks5" | "socks" => run_socks5 = true,
            other => return Err(anyhow!("unknown vpn proxy protocol {other:?} (known: https, socks5)")),
        }
    }
    if !run_https && !run_socks5 {
        return Err(anyhow!("no protocols given (known: https, socks5)"));
    }

    let creds = auth::get_auth_details().await?;

    // Best-effort pre-flight: a typo'd/unauthorized location otherwise
    // starts up fine and then silently fails every single connection (the
    // upstream refusal only surfaces per-connection, in the log line above
    // handle()'s error return). The server is still the real authority —
    // this is just so the user finds out immediately instead of via a pile
    // of failed requests. Doesn't block startup if the check itself fails.
    if let Ok(available) = VpnApi::new().available_locations(&creds.token).await {
        if !available.iter().any(|l| l.code() == location.code()) {
            let known: Vec<&str> = available.iter().map(|l| l.code()).collect();
            output::status_err(&format!(
                "Warning: location {} isn't in your plan's available VPN locations ({}) — every connection will likely be refused upstream.",
                location.code(),
                if known.is_empty() { "none".to_string() } else { known.join(", ") }
            ));
        }
    }

    let shared = Arc::new(SharedCreds::new(creds));
    spawn_refresh(shared.clone());

    // `label()` is `None` for a location code this build doesn't have a
    // name for (see `VpnLocation::Other`) — fall back to the code itself
    // rather than printing "None".
    let location_desc = location.label().unwrap_or_else(|| location.code());

    let mut tasks = tokio::task::JoinSet::new();
    if run_https {
        let shared = shared.clone();
        let location = location.clone();
        let host = https_host.to_string();
        output::status(&format!("VPN HTTP(S) proxy listening on {host}:{https_port} — location {} ({location_desc})", location.code()));
        tasks.spawn(async move { http_proxy::run(&host, https_port, location, shared, verbose).await });
    }
    if run_socks5 {
        let shared = shared.clone();
        let location = location.clone();
        let host = socks5_host.to_string();
        output::status(&format!("VPN SOCKS5 proxy listening on {host}:{socks5_port} — location {} ({location_desc})", location.code()));
        tasks.spawn(async move { socks5::run(&host, socks5_port, location, shared, verbose).await });
    }

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        Some(res) = tasks.join_next() => {
            res??;
        }
    }
    Ok(())
}

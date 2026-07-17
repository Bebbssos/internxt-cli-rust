//! SMB/CIFS server mode. Serves an Internxt Drive (or active workspace, or a
//! chosen subfolder) as an SMB2/3 share, mountable by Windows (`net use`),
//! Linux (`mount -t cifs`), macOS and any SMB client.
//!
//! Runs in the foreground like the other `serve` backends: it binds, then
//! blocks until the orchestrator's shutdown signal (single Ctrl-C for all
//! backends). The SMB2/3 wire protocol, NTLM auth and message signing come from
//! the vendored `smb-server` crate (see `crates/smb-server/VENDORED.md`); this
//! module only supplies the Drive-backed `ShareBackend`/`Handle` in `fs.rs` and
//! wires the server's lifecycle to the shared credentials/cache/upload limiter.

mod fs;

use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use smb_server::{Access, Share, SmbServer};

use internxt_core::config;

/// Runtime configuration for the SMB backend (built from CLI flags). The
/// credential holder, folder cache, upload limiter and root folder come from the
/// `serve` orchestrator (shared with any sibling backends), so they are not part
/// of this struct.
pub struct SmbConfig {
    /// Bind + advertise host (e.g. `127.0.0.1`, `0.0.0.0`). `0.0.0.0` accepts
    /// LAN clients.
    pub host: String,
    /// TCP port. SMB's well-known port is 445; that needs root/admin, so the
    /// default is an unprivileged port.
    pub port: u16,
    /// Exported share name (`\\host\<share>`).
    pub share_name: String,
    /// Username required from clients. Ignored when `password` is `None`.
    pub username: String,
    /// Password required from clients. `None` = anonymous/guest share (no auth).
    pub password: Option<String>,
    /// Delete files permanently instead of moving them to trash.
    pub delete_permanently: bool,
    /// Serve read-only: reject every mutating operation.
    pub read_only: bool,
    /// Directory for per-write temp buffers. `None` = system temp dir.
    pub spool_dir: Option<PathBuf>,
    /// Max read/write payload size advertised to clients, in bytes.
    pub max_transfer_size: u32,
}

/// Serve the Drive over SMB until `shutdown` resolves. Credentials, the folder
/// cache, the upload limiter and the root folder come from the `serve`
/// orchestrator and are shared with any sibling backends.
pub async fn serve(
    shared: Arc<crate::serve::run::Shared>,
    config: SmbConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    if let Some(dir) = &config.spool_dir {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow!("spool directory {} is not usable: {e}", dir.display()))?;
    }

    let addr = (config.host.as_str(), config.port)
        .to_socket_addrs()
        .map_err(|e| anyhow!("failed to resolve {}:{}: {e}", config.host, config.port))?
        .next()
        .ok_or_else(|| anyhow!("no address resolved for {}:{}", config.host, config.port))?;

    let access = if config.read_only {
        Access::Read
    } else {
        Access::ReadWrite
    };

    let inner = Arc::new(fs::Inner::new(
        shared.creds.clone(),
        shared.cache.clone(),
        shared.root_folder.clone(),
        shared.root_updated_at.clone(),
        config.delete_permanently,
        config.read_only,
        config.spool_dir.clone(),
        shared.upload_sem.clone(),
        shared.upload_limit,
    ));
    let backend = fs::DriveBackend::new(inner);

    let mut builder = SmbServer::builder()
        .listen(addr)
        .netbios_name("INTERNXT")
        .max_read_size(config.max_transfer_size)
        .max_write_size(config.max_transfer_size);

    // Authenticated (username/password) vs anonymous (public) share. The
    // backend's own `capabilities().is_read_only` also clamps writes, so the
    // read-only public case is belt-and-suspenders.
    let share = match &config.password {
        Some(pw) => {
            builder = builder.user(&config.username, pw);
            Share::new(&config.share_name, backend).user(&config.username, access)
        }
        None => {
            let s = Share::new(&config.share_name, backend);
            if config.read_only {
                s.public_read_only()
            } else {
                s.public()
            }
        }
    };

    let server = builder
        .share(share)
        .build()
        .map_err(|e| anyhow!("failed to build SMB server: {e}"))?;

    let banner = format!(
        "Internxt {} SMB server (experimental) listening at smb://{}:{}/{}{}{}",
        config::client_version(),
        config.host,
        config.port,
        config.share_name,
        if config.password.is_some() {
            " (with authentication)"
        } else {
            " (anonymous)"
        },
        if config.read_only { " (read-only)" } else { "" },
    );

    // `serve()` consumes the server; grab a shutdown handle first, then race the
    // accept loop against the orchestrator's shutdown signal.
    let shutdown_handle = server.shutdown_handle();
    crate::output::status(&banner);

    let mut serve_task = tokio::spawn(async move { server.serve().await });
    tokio::select! {
        joined = &mut serve_task => {
            return joined
                .map_err(|e| anyhow!("SMB server task panicked: {e}"))?
                .map_err(|e| anyhow!("SMB server error: {e}"));
        }
        _ = shutdown => {}
    }

    crate::output::status("\nShutting down SMB server.");
    shutdown_handle.shutdown();
    // Let in-flight connections wind down (best-effort).
    let _ = serve_task.await;
    Ok(())
}

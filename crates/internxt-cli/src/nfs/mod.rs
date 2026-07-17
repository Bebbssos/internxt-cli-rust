//! NFSv3 server mode. Serves an Internxt Drive (or active workspace, or a chosen
//! subfolder) as an NFSv3 export, mountable by Linux (`mount -t nfs`), macOS and
//! any NFS client.
//!
//! Runs in the foreground like the other `serve` backends: it binds, then blocks
//! until the orchestrator's shutdown signal (single Ctrl-C for all backends).
//! The NFSv3 wire protocol, mount and portmap come from the `nfsserve` crate;
//! this module only supplies the Drive-backed `NFSFileSystem` in `fs.rs` (the
//! NFS analog of `fuser::Filesystem`) and wires the server's lifecycle to the
//! shared credentials / cache / upload limiter.
//!
//! NFSv3 has no open/close for data, so writes can't finalize on a `close` the
//! way FUSE/SMB/SFTP do. Instead each written file is buffered to a temp file
//! and a background sweeper uploads it once writes have gone idle (and evicts the
//! buffer once it has been quiet for a while); a final flush runs on shutdown.

mod fs;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use nfsserve::tcp::{NFSTcp, NFSTcpListener};

use internxt_core::config;

/// How often the sweeper checks write buffers.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Runtime configuration for the NFS backend (built from CLI flags). The
/// credential holder, folder cache, upload limiter and root folder come from the
/// `serve` orchestrator (shared with any sibling backends), so they are not part
/// of this struct.
pub struct NfsConfig {
    /// Bind + advertise host (e.g. `127.0.0.1`, `0.0.0.0`). `0.0.0.0` accepts
    /// LAN clients.
    pub host: String,
    /// TCP port. NFS's well-known port is 2049; that needs root/admin, so the
    /// default is an unprivileged port (mount with `-o port=…,mountport=…`).
    pub port: u16,
    /// Delete files permanently instead of moving them to trash.
    pub delete_permanently: bool,
    /// Serve read-only: reject every mutating operation.
    pub read_only: bool,
    /// Directory for per-file write buffers. `None` = system temp dir.
    pub spool_dir: Option<std::path::PathBuf>,
}

/// Serve the Drive over NFSv3 until `shutdown` resolves. Credentials, the folder
/// cache, the upload limiter and the root folder come from the `serve`
/// orchestrator and are shared with any sibling backends.
pub async fn serve(
    shared: Arc<crate::serve::run::Shared>,
    config: NfsConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    if let Some(dir) = &config.spool_dir {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow!("spool directory {} is not usable: {e}", dir.display()))?;
    }

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

    let ipstr = format!("{}:{}", config.host, config.port);
    let listener = NFSTcpListener::bind(&ipstr, fs::DriveNfs::new(inner.clone()))
        .await
        .map_err(|e| anyhow!("failed to bind NFS server on {ipstr}: {e}"))?;

    let banner = format!(
        "Internxt {} NFSv3 server (experimental) listening at nfs://{}:{}/{}",
        config::client_version(),
        config.host,
        config.port,
        if config.read_only { " (read-only)" } else { "" },
    );
    crate::output::status(&banner);

    // Background sweeper: flush idle write buffers, evict long-quiet ones.
    let sweeper_inner = inner.clone();
    let sweeper = tokio::spawn(async move {
        loop {
            tokio::time::sleep(SWEEP_INTERVAL).await;
            sweeper_inner.sweep().await;
        }
    });

    // `handle_forever` loops accepting connections; race it against shutdown.
    let mut serve_task = tokio::spawn(async move { listener.handle_forever().await });
    tokio::select! {
        joined = &mut serve_task => {
            sweeper.abort();
            return joined
                .map_err(|e| anyhow!("NFS server task panicked: {e}"))?
                .map_err(|e| anyhow!("NFS server error: {e}"));
        }
        _ = shutdown => {}
    }

    crate::output::status("\nShutting down NFS server; flushing pending writes.");
    serve_task.abort();
    sweeper.abort();
    // Best-effort upload of anything still buffered.
    inner.flush_all().await;
    Ok(())
}

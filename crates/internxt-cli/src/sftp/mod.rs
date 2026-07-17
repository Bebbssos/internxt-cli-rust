//! SFTP server mode. Serves an Internxt Drive (or active workspace, or a chosen
//! subfolder) as an SFTP share, reachable with `sftp`, `scp -s`, WinSCP,
//! FileZilla, or a `sshfs` mount.
//!
//! Runs in the foreground like the other `serve` backends: it binds, then blocks
//! until the orchestrator's shutdown signal (single Ctrl-C for all backends).
//! The SSH transport, key exchange and auth come from `russh`; the SFTP
//! subsystem wire protocol from `russh-sftp`. This module only supplies the
//! Drive-backed `russh_sftp::server::Handler` in `fs.rs` (the SFTP analog of
//! `fuser::Filesystem`) and wires the SSH server's lifecycle to the shared
//! credentials / cache / upload limiter.

mod fs;

use std::net::ToSocketAddrs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use russh::keys::{Algorithm, PrivateKey};
use russh::server::{Config as RusshConfig, Server as _};

use internxt_core::config;

/// Runtime configuration for the SFTP backend (built from CLI flags). The
/// credential holder, folder cache, upload limiter and root folder come from the
/// `serve` orchestrator (shared with any sibling backends), so they are not part
/// of this struct.
pub struct SftpConfig {
    /// Bind + advertise host (e.g. `127.0.0.1`, `0.0.0.0`). `0.0.0.0` accepts
    /// LAN clients.
    pub host: String,
    /// TCP port. SSH's well-known port is 22; that needs root/admin, so the
    /// default is an unprivileged port.
    pub port: u16,
    /// Username required from clients.
    pub username: String,
    /// Password required from clients. `None` = accept any password (the
    /// username is still required).
    pub password: Option<String>,
    /// SSH host private key file (OpenSSH/PEM). `None` = generate an ephemeral
    /// key on each start.
    pub host_key: Option<PathBuf>,
    /// Delete files permanently instead of moving them to trash.
    pub delete_permanently: bool,
    /// Serve read-only: reject every mutating operation.
    pub read_only: bool,
    /// Directory for per-write temp buffers. `None` = system temp dir.
    pub spool_dir: Option<PathBuf>,
}

/// Load (or, on first use, generate + persist) the default SFTP host key at
/// `~/.internxt-cli/sftp_host_key`. Persisting it keeps the server's fingerprint
/// stable across restarts so clients don't reject a changed host key.
fn default_host_key() -> Result<PrivateKey> {
    use russh::keys::ssh_key::LineEnding;

    let path = crate::auth::data_dir().join("sftp_host_key");
    if path.exists() {
        return PrivateKey::read_openssh_file(&path)
            .map_err(|e| anyhow!("failed to read SSH host key {}: {e}", path.display()));
    }

    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .map_err(|e| anyhow!("failed to generate an SSH host key: {e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| anyhow!("failed to create {}: {e}", parent.display()))?;
    }
    key.write_openssh_file(&path, LineEnding::LF)
        .map_err(|e| anyhow!("failed to save SSH host key {}: {e}", path.display()))?;
    // Private key → owner-only on Unix.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    crate::output::status(&format!("Generated persistent SSH host key at {}", path.display()));
    Ok(key)
}

/// Serve the Drive over SFTP until `shutdown` resolves. Credentials, the folder
/// cache, the upload limiter and the root folder come from the `serve`
/// orchestrator and are shared with any sibling backends.
pub async fn serve(
    shared: Arc<crate::serve::run::Shared>,
    config: SftpConfig,
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

    // Host key: an explicit `--sftp-host-key` path, else a persistent key in the
    // CLI data dir (generated once, reused after) so clients don't see a changed
    // fingerprint on every restart.
    let host_key = match &config.host_key {
        Some(path) => russh::keys::load_secret_key(path, None)
            .map_err(|e| anyhow!("failed to load SSH host key {}: {e}", path.display()))?,
        None => default_host_key()?,
    };

    let inner = Arc::new(fs::SftpInner::new(
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

    let russh_config = Arc::new(RusshConfig {
        auth_rejection_time: Duration::from_secs(2),
        auth_rejection_time_initial: Some(Duration::from_secs(0)),
        keys: vec![host_key],
        ..Default::default()
    });

    let mut server = fs::SshServer::new(inner, config.username.clone(), config.password.clone());

    let banner = format!(
        "Internxt {} SFTP server (experimental) listening at sftp://{}@{}:{}/{}{}",
        config::client_version(),
        config.username,
        config.host,
        config.port,
        if config.password.is_some() {
            " (with password)"
        } else {
            " (any password)"
        },
        if config.read_only { " (read-only)" } else { "" },
    );
    crate::output::status(&banner);

    // `run_on_address` binds and accepts forever; race it against the
    // orchestrator's shutdown signal and drop the server to stop accepting.
    let mut serve_task = tokio::spawn(async move {
        server.run_on_address(russh_config, addr).await
    });
    tokio::select! {
        joined = &mut serve_task => {
            return joined
                .map_err(|e| anyhow!("SFTP server task panicked: {e}"))?
                .map_err(|e| anyhow!("SFTP server error: {e}"));
        }
        _ = shutdown => {}
    }

    crate::output::status("\nShutting down SFTP server.");
    serve_task.abort();
    let _ = serve_task.await;
    Ok(())
}

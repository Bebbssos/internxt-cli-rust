//! `serve` orchestrator: run one or more Drive-exposing backends (WebDAV, FUSE,
//! …) in a single foreground process, sharing one credential holder (+ refresh
//! task), one folder-listing cache, and one global upload semaphore between them.
//!
//! The CLI picks backends with a comma-separated positional list
//! (`serve webdav,fuse`); shared knobs (`--cache-ttl`, `--folder-uuid`,
//! `--delete-permanently`, `--spool-dir`, `--max-concurrent-uploads`) are bare
//! flags, protocol-specific knobs are prefixed (`--webdav-port`,
//! `--fuse-mountpoint`, …). A single Ctrl-C tears every backend down.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use tokio::sync::watch;
use tokio::task::JoinSet;

use internxt_core::api::DriveApi;
use internxt_core::models::Credentials;
use crate::serve::cache::FolderCache;
use crate::serve::creds::{spawn_refresh, SharedCreds};

/// A backend protocol selected on the `serve` command line. Variants exist only
/// when the corresponding feature is compiled in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    #[cfg(feature = "webdav")]
    Webdav,
    #[cfg(all(unix, feature = "fuse"))]
    Fuse,
    #[cfg(feature = "smb")]
    Smb,
}

impl Protocol {
    pub fn name(self) -> &'static str {
        match self {
            #[cfg(feature = "webdav")]
            Protocol::Webdav => "webdav",
            #[cfg(all(unix, feature = "fuse"))]
            Protocol::Fuse => "fuse",
            #[cfg(feature = "smb")]
            Protocol::Smb => "smb",
        }
    }
}

/// Parse one protocol token, erroring with a helpful message when the name is
/// unknown, not yet implemented, or valid but compiled out / unavailable here.
pub fn parse_protocol(token: &str) -> Result<Protocol> {
    match token.trim().to_ascii_lowercase().as_str() {
        "" => Err(anyhow!("empty protocol name in the protocol list")),
        "webdav" => {
            #[cfg(feature = "webdav")]
            {
                Ok(Protocol::Webdav)
            }
            #[cfg(not(feature = "webdav"))]
            {
                Err(anyhow!(
                    "protocol `webdav` is unavailable: this binary was built without the `webdav` feature"
                ))
            }
        }
        "fuse" => {
            #[cfg(all(unix, feature = "fuse"))]
            {
                Ok(Protocol::Fuse)
            }
            #[cfg(not(all(unix, feature = "fuse")))]
            {
                Err(anyhow!(
                    "protocol `fuse` is unavailable: FUSE mounting is Unix-only and requires the `fuse` feature"
                ))
            }
        }
        "smb" => {
            #[cfg(feature = "smb")]
            {
                Ok(Protocol::Smb)
            }
            #[cfg(not(feature = "smb"))]
            {
                Err(anyhow!(
                    "protocol `smb` is unavailable: this binary was built without the `smb` feature"
                ))
            }
        }
        "sftp" => Err(anyhow!("protocol `{token}` is not implemented yet")),
        other => Err(anyhow!(
            "unknown protocol `{other}` (known protocols: webdav, fuse, smb)"
        )),
    }
}

/// Parse and validate the positional protocol list (`webdav,fuse`). Splits on
/// commas, trims, drops empties, de-duplicates (keeping order), and errors if
/// nothing is left.
pub fn parse_protocols(list: &str) -> Result<Vec<Protocol>> {
    let mut out: Vec<Protocol> = Vec::new();
    for token in list.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        let proto = parse_protocol(token)?;
        if !out.contains(&proto) {
            out.push(proto);
        }
    }
    if out.is_empty() {
        return Err(anyhow!(
            "no protocols given (usage: `serve webdav,smb` — known: webdav, fuse, smb)"
        ));
    }
    Ok(out)
}

/// Resources shared by every backend in one `serve` process.
pub struct Shared {
    /// Refreshable credentials holder (one background refresh task feeds it).
    pub creds: Arc<SharedCreds>,
    /// Folder-listing TTL cache, shared so a mutation via one backend is seen
    /// by the others.
    pub cache: Arc<FolderCache>,
    /// Global upload concurrency limiter (`--max-concurrent-uploads`). `None`
    /// when unlimited. Shared so the cap is process-wide, not per-backend.
    pub upload_sem: Option<Arc<tokio::sync::Semaphore>>,
    /// Per-file upload size cap, resolved once (flags/env/plan) for every backend.
    pub upload_limit: crate::upload_limit::UploadLimit,
    /// Folder uuid exposed as the root of every backend (`--folder-uuid`, or the
    /// account / workspace root).
    pub root_folder: String,
    /// The root folder's `updatedAt`, fetched once for all backends.
    pub root_updated_at: String,
}

/// Runtime configuration for a `serve` run, assembled from the CLI in `main`.
pub struct ServeConfig {
    /// Backends to launch (validated, de-duplicated, non-empty).
    pub protocols: Vec<Protocol>,
    /// Shared root selector; `None` = account / workspace root.
    pub folder_uuid: Option<String>,
    /// Shared folder-listing cache TTL in seconds (already resolved: `--no-cache`
    /// collapses to 0).
    pub cache_ttl: u64,
    /// Shared upload concurrency cap (0 = unlimited).
    pub max_concurrent_uploads: usize,
    /// Per-file upload size limit flags (resolved in `run`).
    pub upload_limit: crate::upload_limit::UploadLimitArgs,
    /// WebDAV backend config (present iff `webdav` is in `protocols`).
    #[cfg(feature = "webdav")]
    pub webdav: Option<crate::webdav::WebdavConfig>,
    /// FUSE backend config (present iff `fuse` is in `protocols`).
    #[cfg(all(unix, feature = "fuse"))]
    pub fuse: Option<crate::fuse::MountConfig>,
    /// SMB backend config (present iff `smb` is in `protocols`).
    #[cfg(feature = "smb")]
    pub smb: Option<crate::smb::SmbConfig>,
}

/// Build a shutdown future for one backend from a shared watch channel: it
/// resolves the moment the orchestrator flips the flag to `true`.
fn shutdown_future(rx: watch::Receiver<bool>) -> impl std::future::Future<Output = ()> + Send {
    let mut rx = rx;
    async move {
        let _ = rx.wait_for(|v| *v).await;
    }
}

/// Launch every selected backend, share credentials/cache/upload-limit between
/// them, and block until Ctrl-C (or a backend exiting on its own), then tear
/// them all down.
pub async fn run(config: ServeConfig) -> Result<()> {
    // Fetch credentials once and derive the shared root.
    let creds = crate::auth::get_auth_details().await?;
    let root_folder = config
        .folder_uuid
        .clone()
        .unwrap_or_else(|| creds.root_folder().to_string());
    let root_updated_at = fetch_folder_updated_at(&creds, &root_folder).await;
    let upload_limit = crate::upload_limit::resolve(&config.upload_limit, &creds).await?;

    let upload_sem = (config.max_concurrent_uploads > 0)
        .then(|| Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_uploads)));
    let shared = Arc::new(Shared {
        creds: Arc::new(SharedCreds::new(creds)),
        cache: Arc::new(FolderCache::new(config.cache_ttl)),
        upload_sem,
        upload_limit,
        root_folder,
        root_updated_at,
    });

    // One background refresh task keeps the shared token fresh for everyone.
    spawn_refresh(shared.creds.clone());

    let names: Vec<&str> = config.protocols.iter().map(|p| p.name()).collect();
    crate::output::status(&format!("Starting serve backends: {}", names.join(", ")));

    // Broadcast shutdown to all backends via a watch flag flipped to `true`.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut set: JoinSet<Result<()>> = JoinSet::new();

    #[cfg(feature = "webdav")]
    let mut webdav_cfg = config.webdav;
    #[cfg(all(unix, feature = "fuse"))]
    let mut fuse_cfg = config.fuse;
    #[cfg(feature = "smb")]
    let mut smb_cfg = config.smb;

    for proto in &config.protocols {
        match proto {
            #[cfg(feature = "webdav")]
            Protocol::Webdav => {
                let cfg = webdav_cfg
                    .take()
                    .ok_or_else(|| anyhow!("internal: webdav selected but no webdav config"))?;
                let shared = shared.clone();
                let shutdown = shutdown_future(shutdown_rx.clone());
                set.spawn(async move { crate::webdav::serve(shared, cfg, shutdown).await });
            }
            #[cfg(all(unix, feature = "fuse"))]
            Protocol::Fuse => {
                let cfg = fuse_cfg
                    .take()
                    .ok_or_else(|| anyhow!("internal: fuse selected but no fuse config"))?;
                let shared = shared.clone();
                let shutdown = shutdown_future(shutdown_rx.clone());
                set.spawn(async move { crate::fuse::serve(shared, cfg, shutdown).await });
            }
            #[cfg(feature = "smb")]
            Protocol::Smb => {
                let cfg = smb_cfg
                    .take()
                    .ok_or_else(|| anyhow!("internal: smb selected but no smb config"))?;
                let shared = shared.clone();
                let shutdown = shutdown_future(shutdown_rx.clone());
                set.spawn(async move { crate::smb::serve(shared, cfg, shutdown).await });
            }
        }
    }

    // Run until Ctrl-C, or until a backend exits on its own (bind failure, mount
    // failure, …). Either way, flip the shutdown flag so the rest wind down.
    let mut first_err: Option<anyhow::Error> = None;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            crate::output::status("\nShutting down serve backends.");
        }
        Some(joined) = set.join_next() => {
            match joined {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    eprintln!("[SERVE] backend exited with error: {e:#}");
                    first_err = Some(e);
                }
                Err(e) => eprintln!("[SERVE] backend task panicked: {e}"),
            }
        }
    }
    let _ = shutdown_tx.send(true);

    // Drain the remaining backends' shutdowns.
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("[SERVE] backend exited with error: {e:#}");
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
            Err(e) => eprintln!("[SERVE] backend task panicked: {e}"),
        }
    }

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Best-effort fetch of a folder's `updatedAt` for the shared root's attributes.
async fn fetch_folder_updated_at(creds: &Credentials, uuid: &str) -> String {
    let api = DriveApi::for_credentials(creds);
    match api.get_folder_meta(&creds.token, uuid).await {
        Ok(v) => v
            .get("updatedAt")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(now_rfc3339),
        Err(_) => now_rfc3339(),
    }
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

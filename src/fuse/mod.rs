//! FUSE mount mode. Mounts an Internxt Drive (or active workspace, or a chosen
//! subfolder) as a local filesystem on Unix (Linux / macOS / FreeBSD) via the
//! `fuser` crate, so the Drive can be browsed and edited with ordinary tools.
//!
//! Runs in the foreground like `serve webdav`: it mounts, then blocks until
//! Ctrl-C, at which point the mount is torn down. Reuses the shared Drive-tree
//! walk, folder-listing cache and refreshable credentials from `crate::serve`,
//! and the streaming upload/download helpers from `crate::commands`.
//!
//! Whole-file model: Internxt has no partial update, so a write buffers the file
//! to a temp file and uploads it in full when the last handle is released.

mod fs;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};
use fuser::{MountOption, SessionACL};

use crate::api::DriveApi;
use crate::serve::cache::FolderCache;
use crate::serve::creds::{spawn_refresh, SharedCreds};

/// Runtime configuration for a `mount` run (built from CLI flags).
pub struct MountConfig {
    /// Local directory to mount onto (must exist and be empty-ish).
    pub mountpoint: PathBuf,
    /// Drive folder uuid to expose as the mount root. `None` = the account/
    /// workspace root folder.
    pub folder_uuid: Option<String>,
    /// TTL (seconds) for the folder-listing cache *and* the kernel attribute /
    /// entry cache. 0 disables both (always live, slower).
    pub cache_ttl: u64,
    /// Delete files permanently instead of moving them to trash (unlink/replace).
    pub delete_permanently: bool,
    /// Directory for the per-write temp buffers. `None` = system temp dir.
    pub spool_dir: Option<PathBuf>,
    /// Max file uploads running the network transfer at once. 0 = unlimited.
    pub max_concurrent_uploads: usize,
    /// Mount read-only (reject all mutations at the kernel level).
    pub read_only: bool,
    /// Allow other users (and root) to access the mount (`allow_other`). Needed
    /// for e.g. serving the mount to another daemon; requires `user_allow_other`
    /// in /etc/fuse.conf on Linux.
    pub allow_other: bool,
}

/// Mount the Drive and block until interrupted (Ctrl-C), then unmount.
pub async fn run(config: MountConfig) -> Result<()> {
    if !config.mountpoint.is_dir() {
        return Err(anyhow!(
            "mountpoint {} does not exist or is not a directory",
            config.mountpoint.display()
        ));
    }
    if let Some(dir) = &config.spool_dir {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow!("spool directory {} is not usable: {e}", dir.display()))?;
    }

    let creds = crate::auth::get_auth_details().await?;
    let root_uuid = config
        .folder_uuid
        .clone()
        .unwrap_or_else(|| creds.root_folder().to_string());
    let root_updated_at = fetch_folder_updated_at(&creds, &root_uuid).await;

    let cache = Arc::new(FolderCache::new(config.cache_ttl));
    let upload_sem = (config.max_concurrent_uploads > 0)
        .then(|| Arc::new(tokio::sync::Semaphore::new(config.max_concurrent_uploads)));
    let shared = Arc::new(SharedCreds::new(creds));
    spawn_refresh(shared.clone());

    let read_only = config.read_only;
    let allow_other = config.allow_other;
    let mountpoint = config.mountpoint.clone();

    let inner = Arc::new(fs::Inner::new(
        shared,
        cache,
        root_uuid,
        root_updated_at,
        upload_sem,
        config,
    ));
    let filesystem = fs::InxtFs::new(inner, tokio::runtime::Handle::current());

    // Mount options. We keep the kernel enforcing permissions against the attrs
    // we report (all owned by the mounting user). `allow_other` widens access to
    // other users; only then do we also add `AutoUnmount` (it requires an
    // allow-other-class ACL or it fails to mount).
    let mut opts = fuser::Config::default();
    opts.mount_options = vec![
        MountOption::FSName("internxt".to_string()),
        MountOption::Subtype("internxt".to_string()),
        MountOption::DefaultPermissions,
    ];
    if read_only {
        opts.mount_options.push(MountOption::RO);
    }
    if allow_other {
        opts.acl = SessionACL::All;
        opts.mount_options.push(MountOption::AutoUnmount);
    }

    // `spawn_mount2` establishes the mount and runs the session on a background
    // thread, whose callbacks bridge into this tokio runtime via the stored
    // handle. Dropping the returned session unmounts.
    let session = fuser::spawn_mount2(filesystem, &mountpoint, &opts)
        .map_err(|e| anyhow!("failed to mount at {}: {e}", mountpoint.display()))?;

    crate::output::status(&format!(
        "Internxt Drive mounted at {}{}",
        mountpoint.display(),
        if read_only { " (read-only)" } else { "" }
    ));
    crate::output::status("Press Ctrl-C to unmount.");

    let _ = tokio::signal::ctrl_c().await;
    crate::output::status("\nUnmounting (Ctrl-C again to force).");

    // Unmounting drops the background session, which unmounts and joins the
    // session thread. If an in-flight kernel op is wedged that can block, so we
    // race it against a second Ctrl-C and a timeout, force-exiting if either
    // fires. A forced exit may leave a stale mount needing `fusermount3 -u`.
    let unmount = tokio::task::spawn_blocking(move || drop(session));
    tokio::select! {
        _ = unmount => {}
        _ = tokio::signal::ctrl_c() => {
            eprintln!("Forced exit; run `fusermount3 -u <mountpoint>` if the mount lingers.");
            std::process::exit(1);
        }
        _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {
            eprintln!("Unmount timed out; forcing exit. Run `fusermount3 -u <mountpoint>` if the mount lingers.");
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Best-effort fetch of a folder's `updatedAt` for the mount root's attributes.
async fn fetch_folder_updated_at(creds: &crate::models::Credentials, uuid: &str) -> String {
    let api = DriveApi::for_credentials(creds);
    match api.get_folder_meta(&creds.token, uuid).await {
        Ok(v) => v
            .get("updatedAt")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(fs::now_rfc3339),
        Err(_) => fs::now_rfc3339(),
    }
}

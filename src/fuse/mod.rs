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

/// Runtime configuration for the FUSE backend (built from CLI flags). The
/// credential holder, folder cache, upload limiter and root folder are supplied
/// by the `serve` orchestrator (shared with any sibling backends), so they are
/// not part of this struct.
pub struct MountConfig {
    /// Local directory to mount onto (must exist and be empty-ish).
    pub mountpoint: PathBuf,
    /// TTL (seconds) for the folder-listing cache *and* the kernel attribute /
    /// entry cache. 0 disables both (always live, slower).
    pub cache_ttl: u64,
    /// Delete files permanently instead of moving them to trash (unlink/replace).
    pub delete_permanently: bool,
    /// Directory for the per-write temp buffers. `None` = system temp dir.
    pub spool_dir: Option<PathBuf>,
    /// Mount read-only (reject all mutations at the kernel level).
    pub read_only: bool,
    /// Bytes of trailing-stream retention for the read path (see
    /// `serve::recent_window`). `0` disables it: every non-sequential read
    /// restarts the download stream instead of possibly hitting memory.
    pub recent_window: u64,
    /// Allow other users (and root) to access the mount (`allow_other`). Needed
    /// for e.g. serving the mount to another daemon; requires `user_allow_other`
    /// in /etc/fuse.conf on Linux.
    pub allow_other: bool,
}

/// Mount the Drive and run until `shutdown` resolves, then unmount. Credentials,
/// the folder cache, the upload limiter and the root folder come from the
/// `serve` orchestrator and are shared with any sibling backends.
pub async fn serve(
    shared: Arc<crate::serve::run::Shared>,
    config: MountConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
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

    let read_only = config.read_only;
    let allow_other = config.allow_other;
    let mountpoint = config.mountpoint.clone();

    let inner = Arc::new(fs::Inner::new(
        shared.creds.clone(),
        shared.cache.clone(),
        shared.root_folder.clone(),
        shared.root_updated_at.clone(),
        shared.upload_sem.clone(),
        shared.upload_limit,
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

    // Wait for the orchestrator's shutdown signal (single Ctrl-C for all
    // backends), then unmount.
    shutdown.await;
    crate::output::status(&format!("\nUnmounting {}.", mountpoint.display()));

    // Unmounting drops the background session, which unmounts and joins the
    // session thread. If an in-flight kernel op is wedged that can block, so we
    // race it against a timeout, force-exiting if it fires. A forced exit may
    // leave a stale mount needing `fusermount3 -u`.
    let unmount = tokio::task::spawn_blocking(move || drop(session));
    tokio::select! {
        _ = unmount => {}
        _ = tokio::time::sleep(std::time::Duration::from_secs(15)) => {
            eprintln!("Unmount of {} timed out; forcing exit. Run `fusermount3 -u <mountpoint>` if the mount lingers.", mountpoint.display());
            std::process::exit(1);
        }
    }
    Ok(())
}

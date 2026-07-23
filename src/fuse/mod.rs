//! FUSE/WinFSP mount mode. Mounts an Internxt Drive (or active workspace, or a
//! chosen subfolder) as a local filesystem, so the Drive can be browsed and
//! edited with ordinary tools. One Cargo feature (`fuse`) everywhere; the
//! backend library is picked per target_os — `fuser` on Unix (`unix.rs`,
//! Linux/macOS/FreeBSD/…), `winfsp_wrs`/WinFSP on Windows (`windows.rs`).
//! `MountConfig` is the shared, OS-agnostic entry point both back ends take.
//!
//! Runs in the foreground like `serve webdav`: it mounts, then blocks until
//! Ctrl-C, at which point the mount is torn down. Reuses the shared Drive-tree
//! walk, folder-listing cache and refreshable credentials from `crate::serve`,
//! and the streaming upload/download helpers from `internxt_core::transfer`.
//!
//! Whole-file model: Internxt has no partial update, so a write buffers the file
//! to a temp file and uploads it in full when the last handle is released.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

/// Runtime configuration for the FUSE backend (built from CLI flags). The
/// credential holder, folder cache, upload limiter and root folder are supplied
/// by the `serve` orchestrator (shared with any sibling backends), so they are
/// not part of this struct.
pub struct MountConfig {
    /// Mount target: a directory on Unix, a drive letter (`X:`) or an empty
    /// directory on Windows.
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
    /// Allow other users (and root) to access the mount (`allow_other`). Unix
    /// only — no WinFSP equivalent, ignored on Windows. Needed for e.g. serving
    /// the mount to another daemon; requires `user_allow_other` in
    /// /etc/fuse.conf on Linux.
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
    #[cfg(unix)]
    {
        unix::serve(shared, config, shutdown).await
    }
    #[cfg(windows)]
    {
        windows::serve(shared, config, shutdown).await
    }
}

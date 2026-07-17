//! Best-effort thumbnail upload shared by the serve backends (WebDAV / FUSE /
//! SMB / NFS / SFTP). After a backend has written a whole file to a temp path and
//! created/replaced its Drive entry, it calls [`upload_thumbnail_best_effort`] to
//! register a 300x300 PNG preview — exactly like `upload-file`. Failures are
//! logged and swallowed; a thumbnail must never break a filesystem write.

use std::path::Path;

use internxt_core::api::DriveApi;
use internxt_core::network::NetworkApi;

/// Generate + upload a thumbnail for the file just written to `temp_path`, if it
/// is a thumbnailable image and thumbnails are enabled. Never returns an error —
/// problems go to the serve warn log tagged with `tag` (e.g. `"webdav"`).
#[allow(clippy::too_many_arguments)]
pub async fn upload_thumbnail_best_effort(
    net: &NetworkApi,
    api: &DriveApi,
    token: &str,
    bucket: &str,
    mnemonic: &str,
    file_uuid: &str,
    file_type: &str,
    temp_path: &Path,
    size: u64,
    tag: &str,
) {
    if !internxt_core::config::thumbnails_enabled() {
        return;
    }
    match internxt_core::thumbnail::try_upload_thumbnail_from_path(
        net, api, token, bucket, mnemonic, file_uuid, file_type, temp_path, size,
    )
    .await
    {
        Ok(_) => {}
        Err(e) => crate::serve::log::warn(&format!("[{tag}] thumbnail failed: {e:#}")),
    }
}

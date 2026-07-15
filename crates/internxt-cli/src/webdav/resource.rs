//! Turning a WebDAV request URL into a Drive item.
//!
//! The protocol-agnostic Drive-tree walk (item types, `list_folders` /
//! `list_files` / `resolve_folder`) now lives in `crate::serve::tree` so the
//! FUSE backend can share it. This module keeps the WebDAV-specific URL parsing
//! (`Resource`, trailing-slash-as-folder-hint) and `resolve_item`, which layers
//! WebDAV's file-then-folder lookup semantics on top of that walk.

use anyhow::Result;

use super::cache::FolderCache;
use internxt_core::api::DriveApi;
// Re-export the shared tree types/functions so existing `resource::…` references
// throughout the webdav module keep resolving.
pub use crate::serve::tree::{list_files, list_folders, resolve_folder, DriveItem, FolderItem};

/// A parsed request URL. `url` is percent-decoded and always starts with `/`.
pub struct Resource {
    /// Decoded full path, e.g. `/dir/file.txt` (trailing `/` preserved).
    pub url: String,
    /// Last path segment (empty for the root).
    pub name: String,
    /// Normalized parent path (leading + trailing `/`).
    pub parent_path: String,
    /// Path components with empty segments removed.
    pub components: Vec<String>,
    /// Whether the URL had a trailing slash (a folder hint).
    pub is_dir_hint: bool,
}

/// Percent-decode a URL path (UTF-8). Invalid escapes are passed through.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

impl Resource {
    /// Parse a raw request path (as delivered by the HTTP layer, still encoded).
    pub fn parse(raw_path: &str) -> Resource {
        let decoded = percent_decode(raw_path).replace("/./", "/");
        let mut url = decoded;
        if url.is_empty() {
            url = "/".to_string();
        }
        let is_dir_hint = url.ends_with('/');
        let components: Vec<String> = url
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        let name = components.last().cloned().unwrap_or_default();
        let parent_path = {
            let mut p = components.clone();
            p.pop();
            if p.is_empty() {
                "/".to_string()
            } else {
                format!("/{}/", p.join("/"))
            }
        };
        Resource {
            url,
            name,
            parent_path,
            components,
            is_dir_hint,
        }
    }
}

/// Resolve a full resource to a file or folder. Mirrors og
/// `WebDavUtils.getDriveItemFromResource`: trailing-slash paths must be folders;
/// otherwise a file is tried first, then a folder.
pub async fn resolve_item(
    api: &DriveApi,
    token: &str,
    root: &str,
    root_updated_at: &str,
    resource: &Resource,
    cache: &FolderCache,
) -> Result<Option<DriveItem>> {
    if resource.components.is_empty() {
        return Ok(Some(DriveItem::Folder(FolderItem {
            uuid: root.to_string(),
            plain_name: String::new(),
            updated_at: root_updated_at.to_string(),
        })));
    }

    // Resolve the parent chain (all but the last component).
    let parent_components = &resource.components[..resource.components.len() - 1];
    let parent =
        match resolve_folder(api, token, root, root_updated_at, parent_components, cache).await? {
            Some(p) => p,
            None => return Ok(None),
        };
    let last = resource.components.last().unwrap();

    if resource.is_dir_hint {
        let folders = list_folders(api, token, &parent.uuid, cache).await?;
        return Ok(folders
            .into_iter()
            .find(|f| &f.plain_name == last)
            .map(DriveItem::Folder));
    }

    // Try file first.
    let files = list_files(api, token, &parent.uuid).await?;
    if let Some(f) = files.into_iter().find(|f| &f.display_name() == last) {
        return Ok(Some(DriveItem::File(f)));
    }
    // Then folder.
    let folders = list_folders(api, token, &parent.uuid, cache).await?;
    Ok(folders
        .into_iter()
        .find(|f| &f.plain_name == last)
        .map(DriveItem::Folder))
}

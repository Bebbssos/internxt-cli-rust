//! Drive-tree navigation shared by the serve backends (WebDAV, FUSE, future
//! sftp/smb). Resolves a path (a list of name components) to a Drive item by
//! walking the folder tree from a root using the paginated `subfolders` /
//! `subfiles` listings — workspace-aware (those calls route through
//! `/workspaces/{id}/…`) and needing no local database.
//!
//! The WebDAV-specific URL parsing (`Resource`, trailing-slash semantics) lives
//! in `webdav::resource`; only the protocol-agnostic tree walk is here.

use anyhow::Result;
use serde_json::Value;

use super::cache::FolderCache;
use internxt_core::api::DriveApi;

/// A resolved Drive file.
#[derive(Clone, Debug)]
pub struct FileItem {
    pub uuid: String,
    pub plain_name: String,
    /// Extension without the dot (may be empty).
    pub file_type: String,
    pub size: u64,
    pub bucket: String,
    pub file_id: Option<String>,
    pub updated_at: String,
}

/// A resolved Drive folder.
#[derive(Clone, Debug)]
pub struct FolderItem {
    pub uuid: String,
    pub plain_name: String,
    pub updated_at: String,
}

/// Either kind of Drive item.
#[derive(Clone, Debug)]
pub enum DriveItem {
    File(FileItem),
    Folder(FolderItem),
}

impl DriveItem {
    pub fn uuid(&self) -> &str {
        match self {
            DriveItem::File(f) => &f.uuid,
            DriveItem::Folder(f) => &f.uuid,
        }
    }
    pub fn is_folder(&self) -> bool {
        matches!(self, DriveItem::Folder(_))
    }
}

impl FileItem {
    /// Display name = plainName + ".ext" when a type is present.
    pub fn display_name(&self) -> String {
        if self.file_type.is_empty() {
            self.plain_name.clone()
        } else {
            format!("{}.{}", self.plain_name, self.file_type)
        }
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn size_field(v: &Value) -> u64 {
    match v.get("size") {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

pub(crate) fn parse_folder(v: &Value) -> FolderItem {
    FolderItem {
        uuid: str_field(v, "uuid"),
        plain_name: str_field(v, "plainName"),
        updated_at: str_field(v, "updatedAt"),
    }
}

pub(crate) fn parse_file(v: &Value) -> FileItem {
    FileItem {
        uuid: str_field(v, "uuid"),
        plain_name: str_field(v, "plainName"),
        file_type: str_field(v, "type"),
        size: size_field(v),
        bucket: str_field(v, "bucket"),
        file_id: {
            let id = str_field(v, "fileId");
            if id.is_empty() { None } else { Some(id) }
        },
        updated_at: str_field(v, "updatedAt"),
    }
}

/// Pull the item array from a listing page (personal `.folders`/`.files`,
/// workspace `.result`), keeping only EXISTS entries.
fn page_items(page: &Value, key: &str) -> Vec<Value> {
    page.get(key)
        .or_else(|| page.get("result"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|item| {
            let status = str_field(item, "status");
            status.is_empty() || status == "EXISTS"
        })
        .collect()
}

/// All EXISTS subfolders of a folder, following pagination, live (no cache
/// read or write).
async fn fetch_folders(api: &DriveApi, token: &str, folder_uuid: &str) -> Result<Vec<FolderItem>> {
    let mut out = Vec::new();
    let mut offset: u32 = 0;
    loop {
        let page = api.get_folder_subfolders(token, folder_uuid, offset).await?;
        let items = page_items(&page, "folders");
        let got = items.len() as u32;
        out.extend(items.iter().map(parse_folder));
        if got < 50 {
            break;
        }
        offset += got;
    }
    Ok(out)
}

/// All EXISTS subfolders of a folder, following pagination. Served from `cache`
/// when a fresh entry exists; a miss populates it.
pub async fn list_folders(
    api: &DriveApi,
    token: &str,
    folder_uuid: &str,
    cache: &FolderCache,
) -> Result<Vec<FolderItem>> {
    if let Some(hit) = cache.get(folder_uuid) {
        return Ok(hit);
    }
    let out = fetch_folders(api, token, folder_uuid).await?;
    cache.put(folder_uuid, out.clone());
    Ok(out)
}

/// Find a subfolder of `parent_uuid` by name. Cache-backed like `list_folders`,
/// but a name miss against a *cached* listing doesn't get trusted blindly: the
/// entry may predate a folder created by another client (another device,
/// another `ixr` process, a plain `ixr` upload) since this process cached it,
/// which would otherwise make that folder — and everything under it — look
/// like it doesn't exist until the TTL expires. So a miss against a cache hit
/// forces one live re-fetch before concluding the name really isn't there.
pub async fn find_folder(
    api: &DriveApi,
    token: &str,
    parent_uuid: &str,
    name: &str,
    cache: &FolderCache,
) -> Result<Option<FolderItem>> {
    if let Some(cached) = cache.get(parent_uuid) {
        if let Some(f) = cached.iter().find(|f| f.plain_name == name) {
            return Ok(Some(f.clone()));
        }
        let fresh = fetch_folders(api, token, parent_uuid).await?;
        let found = fresh.iter().find(|f| f.plain_name == name).cloned();
        cache.put(parent_uuid, fresh);
        return Ok(found);
    }
    let out = fetch_folders(api, token, parent_uuid).await?;
    let found = out.iter().find(|f| f.plain_name == name).cloned();
    cache.put(parent_uuid, out);
    Ok(found)
}

/// All EXISTS subfiles of a folder, following pagination.
pub async fn list_files(api: &DriveApi, token: &str, folder_uuid: &str) -> Result<Vec<FileItem>> {
    let mut out = Vec::new();
    let mut offset: u32 = 0;
    loop {
        let page = api.get_folder_subfiles(token, folder_uuid, offset).await?;
        let items = page_items(&page, "files");
        let got = items.len() as u32;
        out.extend(items.iter().map(parse_file));
        if got < 50 {
            break;
        }
        offset += got;
    }
    Ok(out)
}

/// All EXISTS subfiles of a folder, following pagination. Served from `cache`
/// when a fresh entry exists; a miss populates it.
///
/// Opt-in: callers that need read-your-own-writes visibility (a file this
/// process just uploaded must show up immediately) should invalidate the
/// parent's cache entry on their own mutations, same as folder callers already
/// do — see `Inner::finalize_write_inner`/`unlink`/`rename` in the FUSE
/// backend for the pattern.
pub async fn list_files_cached(
    api: &DriveApi,
    token: &str,
    folder_uuid: &str,
    cache: &FolderCache,
) -> Result<Vec<FileItem>> {
    if let Some(hit) = cache.get_files(folder_uuid) {
        return Ok(hit);
    }
    let out = list_files(api, token, folder_uuid).await?;
    cache.put_files(folder_uuid, out.clone());
    Ok(out)
}

/// Find a subfile of `parent_uuid` by display name (`plainName` + `.type`).
/// Cache-backed like `find_folder`: a name miss against a *cached* listing
/// forces one live re-fetch before concluding the file really isn't there, so
/// a file created by another device/process since this process cached the
/// listing doesn't look missing until the TTL expires.
pub async fn find_file(
    api: &DriveApi,
    token: &str,
    parent_uuid: &str,
    name: &str,
    cache: &FolderCache,
) -> Result<Option<FileItem>> {
    if let Some(cached) = cache.get_files(parent_uuid) {
        if let Some(f) = cached.iter().find(|f| f.display_name() == name) {
            return Ok(Some(f.clone()));
        }
        let fresh = list_files(api, token, parent_uuid).await?;
        let found = fresh.iter().find(|f| f.display_name() == name).cloned();
        cache.put_files(parent_uuid, fresh);
        return Ok(found);
    }
    let out = list_files(api, token, parent_uuid).await?;
    let found = out.iter().find(|f| f.display_name() == name).cloned();
    cache.put_files(parent_uuid, out);
    Ok(found)
}

/// Resolve the folder at `components` (walking from `root`). `None` if any
/// segment is missing or is a file rather than a folder.
pub async fn resolve_folder(
    api: &DriveApi,
    token: &str,
    root: &str,
    root_updated_at: &str,
    components: &[String],
    cache: &FolderCache,
) -> Result<Option<FolderItem>> {
    let mut current = FolderItem {
        uuid: root.to_string(),
        plain_name: String::new(),
        updated_at: root_updated_at.to_string(),
    };
    for comp in components {
        match find_folder(api, token, &current.uuid, comp, cache).await? {
            Some(f) => current = f,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
}

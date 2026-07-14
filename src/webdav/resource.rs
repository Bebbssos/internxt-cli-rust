//! Turning a WebDAV request URL into a Drive item.
//!
//! og resolves paths through a `folders/meta?path=` / `files/meta?path=` endpoint
//! backed by a local sqlite cache. We instead walk the folder tree from the root
//! using the paginated `subfolders` / `subfiles` listings we already have — this
//! stays workspace-aware (those calls route through `/workspaces/{id}/…`) and
//! needs no local database.

use anyhow::Result;
use serde_json::Value;

use super::cache::FolderCache;
use crate::api::DriveApi;

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

fn parse_folder(v: &Value) -> FolderItem {
    FolderItem {
        uuid: str_field(v, "uuid"),
        plain_name: str_field(v, "plainName"),
        updated_at: str_field(v, "updatedAt"),
    }
}

fn parse_file(v: &Value) -> FileItem {
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
    cache.put(folder_uuid, out.clone());
    Ok(out)
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
        let folders = list_folders(api, token, &current.uuid, cache).await?;
        match folders.into_iter().find(|f| &f.plain_name == comp) {
            Some(f) => current = f,
            None => return Ok(None),
        }
    }
    Ok(Some(current))
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

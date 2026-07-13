//! WebDAV method handlers. Each mirrors the corresponding og/cli handler's
//! observable behaviour (status codes, headers, XML body).
//!
//! Each handler takes a fresh credentials snapshot (`ctx.creds()`) at its start
//! so a background token refresh never changes the token mid-request.

use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::Response;
use futures_util::TryStreamExt;
use rand::RngExt;
use serde_json::json;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio_util::io::{ReaderStream, StreamReader};

use super::resource::{self, DriveItem, FolderItem, Resource};
use super::xml;
use super::{log, now_rfc3339, status_response, AppError, Ctx};
use crate::api::DriveApi;
use crate::commands;
use crate::network::NetworkApi;

/// Split a filename into (stem, extension-without-dot).
fn split_name(name: &str) -> (String, String) {
    let p = Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name).to_string();
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("").to_string();
    // A leading-dot name like ".env" has no stem/ext split in the WebDAV sense.
    if stem.is_empty() {
        (name.to_string(), String::new())
    } else {
        (stem, ext)
    }
}

/// A UUID-v4-ish random identifier (hex with dashes). Used for etags / lock tokens.
fn random_uuid() -> String {
    let mut b = [0u8; 16];
    rand::rng().fill(&mut b);
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

fn content_type_for(name: &str) -> String {
    mime_guess::from_path(name)
        .first_raw()
        .unwrap_or("application/octet-stream")
        .to_string()
}

// ---- OPTIONS ----

pub fn options(req: Request) -> Result<Response, AppError> {
    let resource = Resource::parse(req.uri().path());
    let allow = if resource.url == "/" {
        "DELETE, GET, HEAD, LOCK, MKCOL, MOVE, OPTIONS, PROPFIND, PUT, UNLOCK"
    } else if resource.is_dir_hint {
        "DELETE, HEAD, LOCK, MKCOL, MOVE, OPTIONS, PROPFIND, UNLOCK"
    } else {
        "DELETE, GET, HEAD, LOCK, MOVE, OPTIONS, PROPFIND, PUT, UNLOCK"
    };
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Allow", allow)
        .header("DAV", "1, 2, ordered-collections")
        .body(Body::empty())
        .unwrap())
}

// ---- PROPFIND ----

pub async fn propfind(ctx: &Ctx, req: Request) -> Result<Response, AppError> {
    let depth = req
        .headers()
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("1")
        .to_string();
    let resource = Resource::parse(req.uri().path());
    let creds = ctx.creds();
    let token = &creds.token;
    let api = DriveApi::for_credentials(&creds);

    let item = resource::resolve_item(
        &api,
        token,
        &ctx.root_folder,
        &ctx.root_updated_at,
        &resource,
    )
    .await?;
    let item = item.ok_or_else(|| AppError::not_found(""))?;

    let responses = match item {
        DriveItem::File(f) => xml::file_response(
            &resource.url,
            &f.display_name(),
            &content_type_for(&f.display_name()),
            &f.updated_at,
            f.size,
            &random_uuid(),
        ),
        DriveItem::Folder(folder) => {
            let mut out = xml::folder_response(&resource.url, &folder.plain_name, &folder.updated_at);
            if depth != "0" {
                out.push_str(&folder_children(&api, token, &resource.url, &folder).await?);
            }
            out
        }
    };

    Ok(multistatus_response(&xml::multistatus(&responses)))
}

/// Build `<D:response>` nodes for a folder's immediate children.
async fn folder_children(
    api: &DriveApi,
    token: &str,
    base_url: &str,
    folder: &FolderItem,
) -> Result<String> {
    // Ensure the base ends with '/' so child hrefs join cleanly.
    let base = if base_url.ends_with('/') {
        base_url.to_string()
    } else {
        format!("{base_url}/")
    };

    let folders = resource::list_folders(api, token, &folder.uuid).await?;
    let files = resource::list_files(api, token, &folder.uuid).await?;

    let mut out = String::new();
    for f in folders {
        let href = format!("{base}{}/", f.plain_name);
        out.push_str(&xml::folder_response(&href, &f.plain_name, &f.updated_at));
    }
    for f in files {
        let name = f.display_name();
        let href = format!("{base}{name}");
        out.push_str(&xml::file_response(
            &href,
            &name,
            &content_type_for(&name),
            &f.updated_at,
            f.size,
            &random_uuid(),
        ));
    }
    Ok(out)
}

// ---- GET / HEAD ----

pub async fn get(ctx: &Ctx, req: Request, head_only: bool) -> Result<Response, AppError> {
    let range_header = req
        .headers()
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let resource = Resource::parse(req.uri().path());
    let creds = ctx.creds();
    let token = &creds.token;
    let api = DriveApi::for_credentials(&creds);

    let item = resource::resolve_item(
        &api,
        token,
        &ctx.root_folder,
        &ctx.root_updated_at,
        &resource,
    )
    .await?;

    let file = match item {
        Some(DriveItem::File(f)) => f,
        // HEAD on a collection is allowed (200, no body); GET is not.
        Some(DriveItem::Folder(_)) if head_only => return Ok(status_response(StatusCode::OK)),
        Some(DriveItem::Folder(_)) => {
            return Err(AppError::not_found(format!(
                "{} is a collection, use PROPFIND instead.",
                resource.url
            )));
        }
        None => {
            return Err(AppError::not_found(format!(
                "Resource not found on Internxt Drive at {}",
                resource.url
            )));
        }
    };

    let size = file.size;
    let range = range_header.as_deref().and_then(|h| parse_range(h, size));
    let content_length = range.map(|(_, len)| len).unwrap_or(size);

    let builder = Response::builder()
        .status(StatusCode::OK)
        .header("Content-Type", "application/octet-stream")
        .header("Content-Length", content_length.to_string())
        .header("Accept-Ranges", "bytes");

    if head_only || size == 0 {
        return Ok(builder.body(Body::empty()).unwrap());
    }

    let file_id = file
        .file_id
        .ok_or_else(|| AppError::internal("file has no network fileId"))?;
    let bucket = if file.bucket.is_empty() {
        creds.bucket().to_string()
    } else {
        file.bucket.clone()
    };
    let mnemonic = creds.mnemonic().to_string();
    let net = NetworkApi::new(creds.net_user(), creds.net_pass());

    // Producer task decrypts into one half of a duplex pipe; the other half is
    // streamed to the client as the response body. Bounded buffer => bounded RAM.
    let (mut writer, reader) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        if let Err(e) =
            commands::download_file_to_writer(&net, &mnemonic, &bucket, &file_id, &mut writer, range)
                .await
        {
            log(&format!("[GET] download error: {e:#}"));
        }
        let _ = writer.shutdown().await;
    });

    Ok(builder
        .body(Body::from_stream(ReaderStream::new(reader)))
        .unwrap())
}

/// Parse a `Range: bytes=…` header into `(start, len)` clamped to `total`.
/// Only a single range is supported; anything unparseable yields `None`.
fn parse_range(header: &str, total: u64) -> Option<(u64, u64)> {
    if total == 0 {
        return None;
    }
    let spec = header.trim().strip_prefix("bytes=")?;
    // Only the first range of a possible list.
    let first = spec.split(',').next()?.trim();
    let (start_s, end_s) = first.split_once('-')?;
    let (start, end) = if start_s.is_empty() {
        // Suffix range: last N bytes.
        let n: u64 = end_s.parse().ok()?;
        let n = n.min(total);
        (total - n, total - 1)
    } else {
        let start: u64 = start_s.parse().ok()?;
        let end: u64 = if end_s.is_empty() {
            total - 1
        } else {
            end_s.parse::<u64>().ok()?.min(total - 1)
        };
        (start, end)
    };
    if start > end || start >= total {
        return None;
    }
    Some((start, end - start + 1))
}

// ---- PUT ----

pub async fn put(ctx: &Ctx, req: Request) -> Result<Response, AppError> {
    let resource = Resource::parse(req.uri().path());
    if resource.components.is_empty() {
        return Err(AppError::conflict("Cannot PUT to the root collection"));
    }
    let content_length = req
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let creds = ctx.creds();
    let token = &creds.token;
    let api = DriveApi::for_credentials(&creds);
    let parent_components = &resource.components[..resource.components.len() - 1];

    // Resolve (or create) the parent folder.
    let parent = match resource::resolve_folder(
        &api,
        token,
        &ctx.root_folder,
        &ctx.root_updated_at,
        parent_components,
    )
    .await?
    {
        Some(p) => p,
        None => create_parents(ctx, &api, token, parent_components).await?,
    };

    // WebDAV PUT replaces an existing file; error on a same-named folder.
    let existing = resource::resolve_item(
        &api,
        token,
        &ctx.root_folder,
        &ctx.root_updated_at,
        &resource,
    )
    .await?;
    let mut is_replacement = false;
    if let Some(item) = existing {
        match item {
            DriveItem::Folder(_) => {
                return Err(AppError::conflict(
                    "A folder exists on the cloud with the same name",
                ));
            }
            DriveItem::File(f) => {
                is_replacement = true;
                let _ = delete_or_trash(ctx, &api, token, &DriveItem::File(f)).await;
            }
        }
    }

    let (plain, ftype) = split_name(&resource.name);
    let bucket = creds.bucket().to_string();
    let mnemonic = creds.mnemonic();
    let net = NetworkApi::new(creds.net_user(), creds.net_pass());

    // Upload the body to the network, learning the size when it isn't declared.
    let (file_id, size) = match content_length {
        Some(0) => (String::new(), 0),
        Some(sz) => {
            let stream = req
                .into_body()
                .into_data_stream()
                .map_err(std::io::Error::other);
            let reader = StreamReader::new(stream);
            let id =
                commands::upload_stream_to_network(&net, &bucket, mnemonic, reader, sz, None).await?;
            (id, sz)
        }
        None => {
            let tmp = spool_body(req.into_body()).await?;
            if tmp.size == 0 {
                (String::new(), 0)
            } else {
                let id = commands::upload_file_to_network(
                    &net, &bucket, mnemonic, &tmp.path, tmp.size, None,
                )
                .await?;
                (id, tmp.size)
            }
        }
    };

    let now = now_rfc3339();
    api.create_file_entry(
        token,
        &plain,
        &ftype,
        size,
        &parent.uuid,
        &file_id,
        &bucket,
        &now,
        &now,
    )
    .await?;

    let status = if is_replacement {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    };
    Ok(status_response(status))
}

/// A temp file that deletes itself on drop. Used to spool an unknown-length PUT.
struct TempSpool {
    path: std::path::PathBuf,
    size: u64,
}
impl Drop for TempSpool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Copy a request body to a temp file to learn its length (chunked uploads with
/// no Content-Length). Streaming to disk keeps RAM bounded.
async fn spool_body(body: Body) -> Result<TempSpool> {
    let mut rnd = [0u8; 16];
    rand::rng().fill(&mut rnd);
    let path = std::env::temp_dir().join(format!("internxt-webdav-{}.tmp", hex::encode(rnd)));
    let mut file = tokio::fs::File::create(&path).await?;
    let mut spool = TempSpool { path, size: 0 };
    let stream = body.into_data_stream().map_err(std::io::Error::other);
    let mut reader = StreamReader::new(stream);
    let size = tokio::io::copy(&mut reader, &mut file).await?;
    file.flush().await?;
    spool.size = size;
    Ok(spool)
}

// ---- MKCOL ----

pub async fn mkcol(ctx: &Ctx, req: Request) -> Result<Response, AppError> {
    let resource = Resource::parse(req.uri().path());
    if resource.components.is_empty() {
        return Err(AppError::conflict("Root collection already exists"));
    }
    let creds = ctx.creds();
    let token = &creds.token;
    let api = DriveApi::for_credentials(&creds);
    let parent_components = &resource.components[..resource.components.len() - 1];

    let parent = match resource::resolve_folder(
        &api,
        token,
        &ctx.root_folder,
        &ctx.root_updated_at,
        parent_components,
    )
    .await?
    {
        Some(p) => p,
        None => create_parents(ctx, &api, token, parent_components).await?,
    };

    // Already exists? Treat as a no-op success (matches og).
    let existing = resource::list_folders(&api, token, &parent.uuid)
        .await?
        .into_iter()
        .any(|f| f.plain_name == resource.name);
    if existing {
        return Ok(multistatus_response(&xml::multistatus("")));
    }

    api.create_folder(token, &resource.name, &parent.uuid).await?;

    let mut resp = multistatus_response(&xml::multistatus(""));
    *resp.status_mut() = StatusCode::CREATED;
    Ok(resp)
}

// ---- DELETE ----

pub async fn delete(ctx: &Ctx, req: Request) -> Result<Response, AppError> {
    let resource = Resource::parse(req.uri().path());
    let creds = ctx.creds();
    let token = &creds.token;
    let api = DriveApi::for_credentials(&creds);
    let item = resource::resolve_item(
        &api,
        token,
        &ctx.root_folder,
        &ctx.root_updated_at,
        &resource,
    )
    .await?
    .ok_or_else(|| {
        AppError::not_found(format!(
            "Resource not found on Internxt Drive at {}",
            resource.url
        ))
    })?;

    delete_or_trash(ctx, &api, token, &item).await?;
    Ok(status_response(StatusCode::NO_CONTENT))
}

/// Trash or permanently delete a Drive item per the server's config.
async fn delete_or_trash(
    ctx: &Ctx,
    api: &DriveApi,
    token: &str,
    item: &DriveItem,
) -> Result<()> {
    let uuid = item.uuid();
    let type_str = if item.is_folder() { "folder" } else { "file" };
    if ctx.config.delete_permanently {
        if item.is_folder() {
            api.delete_folder(token, uuid).await?;
        } else {
            api.delete_file(token, uuid).await?;
        }
    } else {
        api.trash_items(token, json!([{ "uuid": uuid, "type": type_str }]))
            .await?;
    }
    Ok(())
}

// ---- MOVE ----

pub async fn mv(ctx: &Ctx, req: Request) -> Result<Response, AppError> {
    let destination = req
        .headers()
        .get("destination")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::bad_request("Destination header not received"))?
        .to_string();

    let resource = Resource::parse(req.uri().path());
    let dest = Resource::parse(&strip_destination_host(&destination));
    let creds = ctx.creds();
    let token = &creds.token;
    let api = DriveApi::for_credentials(&creds);

    let item = resource::resolve_item(
        &api,
        token,
        &ctx.root_folder,
        &ctx.root_updated_at,
        &resource,
    )
    .await?
    .ok_or_else(|| {
        AppError::not_found(format!(
            "Resource not found on Internxt Drive at {}",
            resource.url
        ))
    })?;

    let same_dir = resource.parent_path == dest.parent_path;

    if same_dir {
        rename_to(&api, token, &item, &dest.name).await?;
    } else {
        // Resolve (or create) the destination parent, then move + maybe rename.
        let dest_parent_components = &dest.components[..dest.components.len().saturating_sub(1)];
        let dest_parent = match resource::resolve_folder(
            &api,
            token,
            &ctx.root_folder,
            &ctx.root_updated_at,
            dest_parent_components,
        )
        .await?
        {
            Some(p) => p,
            None => create_parents(ctx, &api, token, dest_parent_components).await?,
        };

        match &item {
            DriveItem::Folder(f) => {
                api.move_folder(token, &f.uuid, &dest_parent.uuid).await?;
            }
            DriveItem::File(f) => {
                api.move_file(token, &f.uuid, &dest_parent.uuid).await?;
            }
        }
        // Rename if the destination name differs from the source name.
        let current_name = match &item {
            DriveItem::Folder(f) => f.plain_name.clone(),
            DriveItem::File(f) => f.display_name(),
        };
        if current_name != dest.name {
            rename_to(&api, token, &item, &dest.name).await?;
        }
    }

    Ok(status_response(StatusCode::NO_CONTENT))
}

/// Rename a Drive item to `new_name` (file names split into plainName + type).
async fn rename_to(
    api: &DriveApi,
    token: &str,
    item: &DriveItem,
    new_name: &str,
) -> Result<()> {
    match item {
        DriveItem::Folder(f) => {
            api.rename_folder(token, &f.uuid, new_name).await?;
        }
        DriveItem::File(f) => {
            let (plain, ftype) = split_name(new_name);
            api.rename_file(token, &f.uuid, &plain, &ftype).await?;
        }
    }
    Ok(())
}

/// Strip the scheme + authority from a Destination header, returning the path.
fn strip_destination_host(dest: &str) -> String {
    let lower = dest.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        // Skip past "scheme://authority" to the first '/' of the path.
        if let Some(rest) = dest.splitn(4, '/').nth(3) {
            return format!("/{rest}");
        }
        return "/".to_string();
    }
    dest.to_string()
}

// ---- LOCK / UNLOCK ----

pub fn lock(req: Request) -> Result<Response, AppError> {
    let token = format!("opaquelocktoken:{}", random_uuid());
    let depth = req
        .headers()
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0")
        .to_string();
    let timeout = req
        .headers()
        .get("timeout")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Second-300")
        .to_string();
    let path = req.uri().path().to_string();

    log(&format!("[LOCK] Granted lock token {token} for {path}"));
    let body = xml::lock_discovery(&token, &path, &depth, &timeout);
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("Lock-Token", format!("<{token}>"))
        .header("Content-Type", "application/xml; charset=\"utf-8\"")
        .body(Body::from(body))
        .unwrap())
}

// ---- shared helpers ----

/// Create every folder in `components` under the root, honoring `create_full_path`.
async fn create_parents(
    ctx: &Ctx,
    api: &DriveApi,
    token: &str,
    components: &[String],
) -> Result<FolderItem, AppError> {
    if !ctx.config.create_full_path {
        return Err(AppError::conflict(
            "Parent folders not found on Internxt Drive (createFullPath is disabled)",
        ));
    }
    let mut current = FolderItem {
        uuid: ctx.root_folder.clone(),
        plain_name: String::new(),
        updated_at: ctx.root_updated_at.clone(),
    };
    for comp in components {
        let folders = resource::list_folders(api, token, &current.uuid).await?;
        current = match folders.into_iter().find(|f| &f.plain_name == comp) {
            Some(f) => f,
            None => {
                let v = api.create_folder(token, comp, &current.uuid).await?;
                FolderItem {
                    uuid: v
                        .get("uuid")
                        .and_then(|x| x.as_str())
                        .ok_or_else(|| anyhow!("created folder has no uuid"))?
                        .to_string(),
                    plain_name: comp.clone(),
                    updated_at: now_rfc3339(),
                }
            }
        };
    }
    Ok(current)
}

/// A 207 Multi-Status response with an XML body.
fn multistatus_response(body: &str) -> Response {
    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header("Content-Type", "application/xml; charset=\"utf-8\"")
        .body(Body::from(body.to_string()))
        .unwrap()
}

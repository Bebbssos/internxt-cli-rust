//! WebDAV method handlers. Each mirrors the corresponding og/cli handler's
//! observable behaviour (status codes, headers, XML body).
//!
//! Each handler takes a fresh credentials snapshot (`ctx.creds()`) at its start
//! so a background token refresh never changes the token mid-request.

use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use futures_util::TryStreamExt;
use rand::RngExt;
use serde_json::json;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio_util::io::{ReaderStream, StreamReader};

use super::resource::{self, DriveItem, FolderItem, Resource};
use super::xml;
use super::{log, now_rfc3339, status_response, AppError, Ctx};
use internxt_core::api::DriveApi;

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

// ---- body draining (keep-alive correctness) ----

/// Consume and discard a request body.
///
/// If a handler responds without reading the request body, hyper cannot resync
/// the connection when the client has pipelined the next request behind it, so
/// it closes the connection after the response. WinSCP reuses connections and
/// pipelines (PUT → PROPPATCH → PROPPATCH → PROPFIND), so an undrained PROPPATCH
/// body made the server FIN/RST the socket mid-flight → "connection aborted /
/// Could not send request body". Draining the small bodies of the methods we
/// don't otherwise consume keeps the connection alive.
pub async fn drain_body(body: Body) {
    let mut stream = body.into_data_stream();
    while let Ok(Some(_)) = stream.try_next().await {}
}

/// Drain the request body, then return a status-only error response. Used for
/// methods we don't implement (PROPPATCH / COPY / unknown verbs).
pub async fn unsupported(
    req: Request,
    status: StatusCode,
    message: impl Into<String>,
) -> Result<Response, AppError> {
    drain_body(req.into_body()).await;
    Ok(AppError::new(status, message.into()).into_response())
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
        &ctx.cache,
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
                out.push_str(&folder_children(&api, token, &resource.url, &folder, &ctx.cache).await?);
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
    cache: &super::cache::FolderCache,
) -> Result<String> {
    // Ensure the base ends with '/' so child hrefs join cleanly.
    let base = if base_url.ends_with('/') {
        base_url.to_string()
    } else {
        format!("{base_url}/")
    };

    // Independent calls: run concurrently rather than paying two sequential
    // network round trips for one directory listing.
    let (folders, files) = tokio::try_join!(
        resource::list_folders(api, token, &folder.uuid, cache),
        resource::list_files_cached(api, token, &folder.uuid, cache),
    )?;

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
        &ctx.cache,
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
    // Mirrors node: a Range header is only honored for a non-empty file; a
    // malformed or unsatisfiable range is a client error, not a silent
    // full-file fallback.
    let range = match range_header.as_deref() {
        Some(h) if size > 0 => Some(parse_range(h, size).map_err(|e| e.into_app_error(h, size))?),
        _ => None,
    };
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
    let net = crate::net_client::network_api(creds.net_user(), creds.net_pass());

    // Producer task decrypts into one half of a duplex pipe; the other half is
    // streamed to the client as the response body. Bounded buffer => bounded RAM.
    let (mut writer, reader) = tokio::io::duplex(256 * 1024);
    tokio::spawn(async move {
        if let Err(e) =
            internxt_core::transfer::download_file_to_writer(&net, &mnemonic, &bucket, &file_id, &mut writer, range)
                .await
        {
            crate::serve::log::warn(&format!("[GET] download error: {e:#}"));
        }
        let _ = writer.shutdown().await;
    });

    Ok(builder
        .body(Body::from_stream(ReaderStream::new(reader)))
        .unwrap())
}

/// A `Range` header that couldn't be honored: syntactically invalid, or
/// syntactically fine but outside the file's bounds. Mirrors node's
/// `NetworkUtils.parseRangeHeader` (`range-parser` return codes -2/-1).
enum RangeError {
    Malformed,
    Unsatisfiable,
}

impl RangeError {
    fn into_app_error(self, header: &str, total: u64) -> AppError {
        let detail = format!("{{\"range\":\"{header}\",\"totalFileSize\":{total}}}");
        match self {
            RangeError::Malformed => {
                AppError::bad_request(format!("Malformed Range-Request. {detail}"))
            }
            RangeError::Unsatisfiable => AppError::new(
                StatusCode::RANGE_NOT_SATISFIABLE,
                format!("Unsatisfiable Range-Request. {detail}"),
            ),
        }
    }
}

/// Parse a `Range: bytes=…` header into `(start, len)` clamped to `total`.
/// Only a single range is supported; a comma-separated multi-range is treated
/// as malformed (multi-range responses aren't implemented, same as node).
fn parse_range(header: &str, total: u64) -> Result<(u64, u64), RangeError> {
    let spec = header.trim().strip_prefix("bytes=").ok_or(RangeError::Malformed)?;
    if spec.contains(',') {
        return Err(RangeError::Malformed);
    }
    let first = spec.trim();
    let (start_s, end_s) = first.split_once('-').ok_or(RangeError::Malformed)?;
    let (start, end) = if start_s.is_empty() {
        // Suffix range: last N bytes.
        let n: u64 = end_s.parse().map_err(|_| RangeError::Malformed)?;
        if n == 0 {
            return Err(RangeError::Unsatisfiable);
        }
        let n = n.min(total);
        (total - n, total - 1)
    } else {
        let start: u64 = start_s.parse().map_err(|_| RangeError::Malformed)?;
        let end: u64 = if end_s.is_empty() {
            total - 1
        } else {
            end_s.parse::<u64>().map_err(|_| RangeError::Malformed)?.min(total - 1)
        };
        (start, end)
    };
    if start > end || start >= total {
        return Err(RangeError::Unsatisfiable);
    }
    Ok((start, end - start + 1))
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

    // Fail fast when the declared length already exceeds the cap: reject before
    // resolving folders or reading the body (an oversize upload aborts here).
    if let Some(sz) = content_length {
        if let Err(e) = ctx.upload_limit.check(sz) {
            return Err(AppError::payload_too_large(format!("{e}")));
        }
    }

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
        &ctx.cache,
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
        &ctx.cache,
    )
    .await?;
    // For a same-named existing file, replace its content in place (PUT
    // /files/{uuid}) once the upload succeeds, instead of trashing the old
    // entry up front — see the atomic-replace comment in fuse/fs.rs
    // (finalize_write) for why: delete-then-create leaves a window where a
    // failed upload or create_file_entry call loses the file permanently.
    let replace_uuid: Option<String> = match existing {
        Some(DriveItem::Folder(_)) => {
            return Err(AppError::conflict(
                "A folder exists on the cloud with the same name",
            ));
        }
        Some(DriveItem::File(f)) => Some(f.uuid),
        None => None,
    };
    let is_replacement = replace_uuid.is_some();

    let (plain, ftype) = split_name(&resource.name);
    let bucket = creds.bucket().to_string();
    let mnemonic = creds.mnemonic();
    let net = crate::net_client::network_api(creds.net_user(), creds.net_pass());

    // Gate on the upload-concurrency limit (if any). Held across the whole
    // transfer so `--max-concurrent-uploads 1` serializes uploads: a burst of
    // parallel PUTs queues here instead of all hitting the storage backend at
    // once. Acquiring before the body is read means waiting requests don't yet
    // hold an open storage socket, so nothing idles out while queued.
    let _upload_permit = ctx.acquire_upload().await;

    // Upload the body to network storage. Two strategies:
    //
    // * Default (`--spool` off): stream the live client body straight through,
    //   learning the size from Content-Length. RAM-bounded, no temp disk, lowest
    //   latency — but the storage PUT socket is coupled to the client's pacing.
    // * `--spool` on: spool the request body to a temp file first, then upload
    //   from disk. Costs disk + a little latency, but (a) fully drains the client
    //   body up front so a storage-side failure can't leave an undrained body
    //   (that is what makes hyper abort the TCP connection — WinSCP "Could not
    //   send request body / connection aborted"), and (b) feeds the storage PUT
    //   from local disk continuously, avoiding the S3 "RequestTimeout: socket not
    //   read from or written to within the timeout period" that a stalled /
    //   executor-starved live stream can trip under a concurrent upload burst.
    // A thumbnailable image must be spooled to a temp file even in streaming mode:
    // thumbnail generation needs the bytes on disk after the main upload, and the
    // live streaming path retains nothing. `unwrap_or(1)` keeps unknown-length
    // images eligible (they spool anyway) while still honouring the size cap.
    let thumb_wanted = internxt_core::config::thumbnails_enabled()
        && internxt_core::thumbnail::is_image_thumbnailable(&ftype, content_length.unwrap_or(1));
    let spool_it = ctx.config.spool || content_length.is_none() || thumb_wanted;

    // `retained_tmp` keeps a spooled body alive past the upload so a thumbnail can
    // be built from it; dropped (temp deleted) at the end of the handler.
    let (file_id, size, retained_tmp) = if spool_it {
        let tmp = spool_body(req.into_body(), ctx.config.spool_dir.as_deref()).await?;
        ctx.upload_limit
            .check(tmp.size)
            .map_err(|e| AppError::payload_too_large(format!("{e}")))?;
        if tmp.size == 0 {
            (String::new(), 0, None)
        } else {
            let id = internxt_core::transfer::upload_file_to_network(
                &net, &bucket, mnemonic, &tmp.path, tmp.size, None,
            )
            .await?;
            let size = tmp.size;
            (id, size, Some(tmp))
        }
    } else {
        match content_length {
            Some(0) => (String::new(), 0, None),
            Some(sz) => {
                let stream = req
                    .into_body()
                    .into_data_stream()
                    .map_err(std::io::Error::other);
                let reader = StreamReader::new(stream);
                let id = internxt_core::transfer::upload_stream_to_network(&net, &bucket, mnemonic, reader, sz, None)
                    .await?;
                (id, sz, None)
            }
            // No declared length is handled by `spool_it` above (spooled).
            None => unreachable!("unknown-length bodies are spooled"),
        }
    };

    let now = now_rfc3339();
    let result_uuid = match &replace_uuid {
        Some(old_uuid) => api.replace_file(token, old_uuid, &file_id, size).await?.uuid,
        None => {
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
            .await?
            .uuid
        }
    };

    if let Some(tmp) = &retained_tmp {
        crate::serve::thumbnail::upload_thumbnail_best_effort(
            &net, &api, token, &bucket, mnemonic, &result_uuid, &ftype, &tmp.path, size, "webdav",
        )
        .await;
    }

    // New/replaced file must show up immediately for this process's own
    // subsequent PROPFIND/GET, same as a folder create already invalidates
    // (see `get_or_create_child`).
    ctx.cache.invalidate(&parent.uuid);

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

/// Copy a request body to a temp file. Used to decouple the upload from the
/// client (`--spool`) or to learn the length of a body sent without a
/// Content-Length. Streaming to disk keeps RAM bounded. `dir` overrides the
/// spool location (defaults to the system temp dir).
async fn spool_body(body: Body, dir: Option<&Path>) -> Result<TempSpool> {
    let mut rnd = [0u8; 16];
    rand::rng().fill(&mut rnd);
    let base = dir.map(|d| d.to_path_buf()).unwrap_or_else(std::env::temp_dir);
    let path = base.join(format!("internxt-webdav-{}.tmp", hex::encode(rnd)));
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
        &ctx.cache,
    )
    .await?
    {
        Some(p) => p,
        None => create_parents(ctx, &api, token, parent_components).await?,
    };

    // RFC 4918: MKCOL on an existing resource must fail with 405. Some clients
    // (e.g. QNAP HBS) abort the whole backup job on a 2xx here (matches og).
    let existing = resource::find_folder(&api, token, &parent.uuid, &resource.name, &ctx.cache)
        .await?
        .is_some();
    if existing {
        return Err(AppError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "Folder already exists",
        ));
    }

    // Conflict-tolerant create: a racing MKCOL/PUT may create it first; adopt
    // that instead of surfacing a 409/500 (which would abort the connection).
    get_or_create_child(ctx, &api, token, &parent.uuid, &resource.name).await?;

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
        &ctx.cache,
    )
    .await?
    .ok_or_else(|| {
        AppError::not_found(format!(
            "Resource not found on Internxt Drive at {}",
            resource.url
        ))
    })?;

    delete_or_trash(ctx, &api, token, &item).await?;
    // The removed item's parent listing (folder or file) changed and its
    // uuid isn't known here, so clear the whole cache (deletes are far rarer
    // than reads).
    ctx.cache.clear();
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
        &ctx.cache,
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
            &ctx.cache,
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

    // Moving/renaming changes the source and destination parent listings
    // (folder or file); the affected parent uuids aren't all known here, so
    // clear the whole cache.
    ctx.cache.clear();
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

pub async fn lock(req: Request) -> Result<Response, AppError> {
    let token = format!("opaquelocktoken:{}", random_uuid());
    let (parts, body) = req.into_parts();
    let depth = parts
        .headers
        .get("depth")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("0")
        .to_string();
    let timeout = parts
        .headers
        .get("timeout")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("Second-300")
        .to_string();
    let path = parts.uri.path().to_string();
    // LOCK carries a request body (the lock-info XML); drain it so hyper keeps
    // the connection alive for the client's next (pipelined) request.
    drain_body(body).await;

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
        current = get_or_create_child(ctx, api, token, &current.uuid, comp).await?;
    }
    Ok(current)
}

/// Number of attempts when creating a folder that may be racing another request.
const FOLDER_CREATE_ATTEMPTS: usize = 6;

/// Find the child folder `name` under `parent_uuid`, creating it if missing.
///
/// Concurrent uploads into a not-yet-existing folder (e.g. WinSCP fanning out a
/// batch with `--create-full-path`) all try to create the same folder at once;
/// the backend then answers one with success and the rest with 409/500. Left
/// unhandled those become early PUT errors that drop the (large-bodied) TCP
/// connection, which WinSCP reports as "connection aborted". So on any create
/// failure we re-list (the winner's folder is now there) and adopt it, retrying
/// a few times for transient backend errors.
async fn get_or_create_child(
    ctx: &Ctx,
    api: &DriveApi,
    token: &str,
    parent_uuid: &str,
    name: &str,
) -> Result<FolderItem, AppError> {
    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..FOLDER_CREATE_ATTEMPTS {
        // `find_folder` already bypasses a stale cache hit on a name miss, so a
        // concurrently-created folder is seen without needing to invalidate here.
        if let Some(f) = resource::find_folder(api, token, parent_uuid, name, &ctx.cache).await? {
            return Ok(f);
        }

        match api.create_folder(token, name, parent_uuid).await {
            Ok(v) => {
                ctx.cache.invalidate(parent_uuid);
                return Ok(FolderItem {
                    uuid: v
                        .get("uuid")
                        .and_then(|x| x.as_str())
                        .ok_or_else(|| anyhow!("created folder has no uuid"))?
                        .to_string(),
                    plain_name: name.to_string(),
                    updated_at: now_rfc3339(),
                });
            }
            // Lost the race (already exists) or a transient backend error: loop
            // back, re-list, and adopt the folder the winner created.
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(
                    120 * (attempt as u64 + 1),
                ))
                .await;
            }
        }
    }
    // Exhausted attempts without seeing the folder appear.
    Err(last_err
        .map(AppError::from)
        .unwrap_or_else(|| AppError::internal("failed to create folder")))
}

/// A 207 Multi-Status response with an XML body.
fn multistatus_response(body: &str) -> Response {
    Response::builder()
        .status(StatusCode::MULTI_STATUS)
        .header("Content-Type", "application/xml; charset=\"utf-8\"")
        .body(Body::from(body.to_string()))
        .unwrap()
}

//! On-demand thumbnail management: `thumbnail generate | upload | download`.
//! Complements the automatic thumbnails made during upload — lets you backfill a
//! preview for an existing Drive file, set a custom one, or export the current one.
//!
//! Not part of the node CLI (which only auto-generates on upload).

use anyhow::{anyhow, Result};
use serde_json::json;
use std::path::Path;

use internxt_core::api::DriveApi;
use internxt_core::models::{Credentials, ThumbnailMeta};
use internxt_core::network::NetworkApi;

use crate::auth;

/// Fetch a file's thumbnails + display name. The `/files/{uuid}/meta` endpoint does
/// NOT include thumbnails — they only appear in the folder-content file listing —
/// so this locates the file's folder (from its meta) then pages that folder's file
/// listing to read `files[].thumbnails[]`. Returns `(name, type, thumbnails)`.
async fn fetch_thumbnails(
    api: &DriveApi,
    token: &str,
    file_uuid: &str,
) -> Result<(String, String, Vec<ThumbnailMeta>)> {
    let raw = api.get_file_meta_value(token, file_uuid).await?;
    let name = raw
        .get("plainName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| raw.get("name").and_then(|v| v.as_str()))
        .unwrap_or(file_uuid)
        .to_string();
    let ftype = raw
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let folder_uuid = raw
        .get("folderUuid")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("file meta missing folderUuid"))?;

    let mut offset = 0u32;
    loop {
        let listing = api.get_folder_subfiles(token, folder_uuid, offset).await?;
        let files = match listing.get("files").and_then(|f| f.as_array()) {
            Some(f) if !f.is_empty() => f,
            _ => break,
        };
        let page = files.len();
        for f in files {
            if f.get("uuid").and_then(|u| u.as_str()) == Some(file_uuid) {
                let thumbs = f
                    .get("thumbnails")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                return Ok((name, ftype, thumbs));
            }
        }
        if page < 50 {
            break;
        }
        offset += 50;
    }
    Ok((name, ftype, Vec::new()))
}

/// Resolve a file uuid from either `--id` or `--path`.
async fn resolve_file(
    api: &DriveApi,
    creds: &Credentials,
    id: Option<&str>,
    path: Option<&str>,
) -> Result<String> {
    crate::paths::resolve_opt(
        api,
        &creds.token,
        creds.root_folder(),
        id,
        path,
        crate::paths::Expect::File,
    )
    .await?
    .ok_or_else(|| anyhow!("Provide the file id (--id) or path (--path)"))
}

/// The bucket a file lives in (its own, falling back to the account bucket).
fn file_bucket(creds: &Credentials, meta_bucket: &str) -> String {
    if meta_bucket.is_empty() {
        creds.bucket().to_string()
    } else {
        meta_bucket.to_string()
    }
}

/// Generate a thumbnail for a Drive file from its own image content: download the
/// file, make a 300x300 PNG, upload + register it.
pub async fn generate(id: Option<&str>, path: Option<&str>) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let uuid = resolve_file(&api, &creds, id, path).await?;

    let meta = api.get_file_meta(&creds.token, &uuid).await?;
    let file_type = meta.file_type.clone().unwrap_or_default();
    let size = meta.size.0;
    if !internxt_core::thumbnail::is_image_thumbnailable(&file_type, size) {
        return Err(anyhow!(
            "Not a thumbnailable image (type '{file_type}', {size} bytes). \
             Supported: jpg/png/webp/gif/tiff up to 500MB."
        ));
    }
    let file_id = meta
        .file_id
        .clone()
        .ok_or_else(|| anyhow!("file has no network fileId"))?;
    let bucket = file_bucket(&creds, &meta.bucket);

    let net = NetworkApi::new(creds.net_user(), creds.net_pass());
    crate::output::status("Downloading image...");
    let mut buf: Vec<u8> = Vec::new();
    internxt_core::transfer::download_file_to_writer(
        &net,
        creds.mnemonic(),
        &bucket,
        &file_id,
        &mut buf,
        None,
    )
    .await?;

    crate::output::status("Generating + uploading thumbnail...");
    let thumb = internxt_core::thumbnail::upload_thumbnail_bytes(
        &net,
        &api,
        &creds.token,
        &bucket,
        creds.mnemonic(),
        &uuid,
        &buf,
    )
    .await?
    .ok_or_else(|| anyhow!("thumbnail generation produced no image"))?;

    crate::output::emit(
        &format!("Thumbnail generated ({} bytes)", thumb.size.0),
        json!({ "success": true, "thumbnail": { "id": thumb.id, "size": thumb.size.0 } }),
    );
    Ok(())
}

/// Upload a custom thumbnail image for a Drive file. By default the image is
/// resized to a 300x300 PNG (like the automatic path); `--raw` uploads the bytes
/// as-is, recording the image's real dimensions (may not render in Drive web if
/// it isn't the expected 300x300 PNG).
pub async fn upload(id: Option<&str>, path: Option<&str>, file: &str, raw: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let uuid = resolve_file(&api, &creds, id, path).await?;

    let src = Path::new(file);
    if !src.is_file() {
        return Err(anyhow!("Not a file: {file}"));
    }
    let bytes = tokio::fs::read(src).await?;
    if bytes.is_empty() {
        return Err(anyhow!("Thumbnail image is empty: {file}"));
    }

    let meta = api.get_file_meta(&creds.token, &uuid).await?;
    let bucket = file_bucket(&creds, &meta.bucket);
    let net = NetworkApi::new(creds.net_user(), creds.net_pass());

    let thumb = if raw {
        // Upload the provided bytes unchanged, recording the source extension +
        // real dimensions (falling back to the standard 300x300 when unknown).
        let ttype = src
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("png")
            .to_ascii_lowercase();
        let (w, h) = internxt_core::thumbnail::image_dimensions(&bytes)
            .unwrap_or((internxt_core::thumbnail::MAX_WIDTH, internxt_core::thumbnail::MAX_HEIGHT));
        let size = bytes.len() as u64;
        crate::output::status("Uploading custom thumbnail...");
        let bucket_file = internxt_core::transfer::upload_stream_to_network(
            &net,
            &bucket,
            creds.mnemonic(),
            std::io::Cursor::new(bytes),
            size,
            None,
        )
        .await?;
        api.create_thumbnail_entry(
            &creds.token,
            &uuid,
            &ttype,
            size,
            w,
            h,
            &bucket,
            &bucket_file,
        )
        .await?
    } else {
        crate::output::status("Resizing + uploading thumbnail...");
        internxt_core::thumbnail::upload_thumbnail_bytes(
            &net,
            &api,
            &creds.token,
            &bucket,
            creds.mnemonic(),
            &uuid,
            &bytes,
        )
        .await?
        .ok_or_else(|| anyhow!("could not decode {file} as an image"))?
    };

    crate::output::emit(
        &format!("Custom thumbnail uploaded ({} bytes)", thumb.size.0),
        json!({ "success": true, "thumbnail": { "id": thumb.id, "size": thumb.size.0 } }),
    );
    Ok(())
}

/// Download a Drive file's current thumbnail to a local file.
pub async fn download(
    id: Option<&str>,
    path: Option<&str>,
    directory: Option<&str>,
    overwrite: bool,
    index: Option<usize>,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let uuid = resolve_file(&api, &creds, id, path).await?;

    let (name, _ftype, thumbnails) = fetch_thumbnails(&api, &creds.token, &uuid).await?;
    if thumbnails.is_empty() {
        return Err(anyhow!(
            "File has no thumbnail. Run `internxt thumbnail generate` first."
        ));
    }
    let idx = index.unwrap_or(0);
    let thumb = thumbnails.get(idx).ok_or_else(|| {
        anyhow!(
            "thumbnail index {idx} out of range (file has {})",
            thumbnails.len()
        )
    })?;
    if thumb.bucket_file.is_empty() {
        return Err(anyhow!("thumbnail has no network file id"));
    }
    let bucket = file_bucket(&creds, &thumb.bucket_id);

    let ext = if thumb.thumbnail_type.is_empty() {
        "png".to_string()
    } else {
        thumb.thumbnail_type.clone()
    };
    let filename = format!("{name}-thumbnail.{ext}");
    let dir = directory.filter(|d| !d.trim().is_empty()).unwrap_or(".");
    let out = Path::new(dir).join(&filename);
    if out.exists() && !overwrite {
        return Err(anyhow!(
            "File already exists, use --overwrite to overwrite: {}",
            out.display()
        ));
    }

    let net = NetworkApi::new(creds.net_user(), creds.net_pass());
    crate::output::status("Downloading thumbnail...");
    let mut f = tokio::fs::File::create(&out).await?;
    internxt_core::transfer::download_file_to_writer(
        &net,
        creds.mnemonic(),
        &bucket,
        &thumb.bucket_file,
        &mut f,
        None,
    )
    .await?;
    use tokio::io::AsyncWriteExt;
    f.flush().await?;

    crate::output::emit(
        &format!("Thumbnail downloaded successfully to {}", out.display()),
        json!({ "success": true, "path": out.display().to_string() }),
    );
    Ok(())
}

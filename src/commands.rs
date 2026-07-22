//! upload-file and download-file. Fully streaming — never holds a whole file in RAM.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use rand::RngExt;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use crate::auth;
use internxt_core::api::DriveApi;
use internxt_core::config;
use internxt_core::crypto::{self, Ctr};
use internxt_core::models::DriveFileData;
use internxt_core::network::NetworkApi;
use internxt_core::transfer::{
    create_folder_with_retry, upload_file_to_network, upload_stream_to_network,
};

const MAX_CONCURRENT_FILE_UPLOADS: usize = 10;

fn to_rfc3339(t: SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.to_rfc3339()
}

/// Upload a local file to Internxt Drive (streaming; single-part or multipart).
/// With `use_stdin`, the body is read from stdin instead of `file_path`; `name`
/// supplies the Drive name (required) and `size_hint` the byte length (optional).
#[allow(clippy::too_many_arguments)]
pub async fn upload_file(
    file_path: Option<&str>,
    destination: Option<&str>,
    dest_path: Option<&str>,
    use_stdin: bool,
    name: Option<&str>,
    size_hint: Option<u64>,
    limit_args: &crate::upload_limit::UploadLimitArgs,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let limit = crate::upload_limit::resolve(limit_args, &creds).await?;

    let folder_uuid = {
        let api = DriveApi::for_credentials(&creds);
        crate::paths::resolve_destination_opt(
            &api,
            &creds.token,
            creds.root_folder(),
            destination,
            dest_path,
            crate::paths::Expect::Folder,
        )
        .await?
        .unwrap_or_else(|| creds.root_folder().to_string())
    };

    if use_stdin {
        return upload_from_stdin(&creds, name, size_hint, &folder_uuid, limit).await;
    }

    let file_path = file_path.ok_or_else(|| anyhow!("Provide --file <PATH> or --stdin"))?;
    let path = Path::new(file_path);
    let meta = std::fs::metadata(path).map_err(|_| anyhow!("File not found: {file_path}"))?;
    if !meta.is_file() {
        return Err(anyhow!("Not a file: {file_path}"));
    }
    let size = meta.len();
    limit.check(size)?;

    let name_path = name.map(Path::new).unwrap_or(path);
    let stem = name_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let file_type = name_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let mut file_id = String::new();

    if size > 0 {
        let net = crate::net_client::network_api(creds.net_user(), creds.net_pass());
        crate::output::status("Preparing network...");
        let pb = crate::output::progress_bar(size, "Uploading");
        file_id = upload_file_to_network(
            &net,
            creds.bucket(),
            creds.mnemonic(),
            path,
            size,
            Some(crate::output::bar_sink(&pb)),
        )
        .await?;
        pb.finish_and_clear();
    }

    let creation = to_rfc3339(meta.created().unwrap_or_else(|_| SystemTime::now()));
    let modification = to_rfc3339(meta.modified().unwrap_or_else(|_| SystemTime::now()));

    let drive_file = finish_file_entry(
        &creds,
        &stem,
        &file_type,
        size,
        &folder_uuid,
        &file_id,
        &creation,
        &modification,
    )
    .await?;

    try_upload_thumbnail(&creds, &drive_file.uuid, &file_type, path, size).await;

    emit_upload_success(&creds, &drive_file);
    Ok(())
}

/// Best-effort thumbnail: generate + upload a preview for thumbnailable images.
/// Failures are logged and swallowed — a thumbnail must never fail the upload
/// (mirrors og's `tryUploadThumbnail`).
async fn try_upload_thumbnail(
    creds: &internxt_core::models::Credentials,
    file_uuid: &str,
    file_type: &str,
    path: &Path,
    size: u64,
) {
    if !config::thumbnails_enabled() || !internxt_core::thumbnail::is_image_thumbnailable(file_type, size)
    {
        return;
    }
    let net = crate::net_client::network_api(creds.net_user(), creds.net_pass());
    let api = DriveApi::for_credentials(creds);
    match internxt_core::thumbnail::try_upload_thumbnail_from_path(
        &net,
        &api,
        &creds.token,
        creds.bucket(),
        creds.mnemonic(),
        file_uuid,
        file_type,
        path,
        size,
    )
    .await
    {
        Ok(_) => {}
        Err(e) => crate::output::status(&format!("Thumbnail skipped: {e}")),
    }
}

/// Upload a file whose body comes from stdin. Filename is required (stdin has none).
/// If `size_hint` is provided the body streams straight through (single-part, true
/// streaming); otherwise stdin is spooled to a temp file to learn its length first.
async fn upload_from_stdin(
    creds: &internxt_core::models::Credentials,
    name: Option<&str>,
    size_hint: Option<u64>,
    folder_uuid: &str,
    limit: crate::upload_limit::UploadLimit,
) -> Result<()> {
    let name = name.ok_or_else(|| anyhow!("--name <NAME> is required with --stdin"))?;
    let np = Path::new(name);
    let stem = np
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let file_type = np
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let net = crate::net_client::network_api(creds.net_user(), creds.net_pass());

    let (file_id, size) = match size_hint {
        Some(0) => (String::new(), 0),
        Some(sz) => {
            limit.check(sz)?;
            crate::output::status(&format!("Uploading {sz} bytes from stdin..."));
            let pb = crate::output::progress_bar(sz, "Uploading");
            let id = upload_stream_to_network(
                &net,
                creds.bucket(),
                creds.mnemonic(),
                tokio::io::stdin(),
                sz,
                Some(crate::output::bar_sink(&pb)),
            )
            .await?;
            pb.finish_and_clear();
            (id, sz)
        }
        None => {
            crate::output::status("Buffering stdin...");
            let tmp = spool_stdin_to_temp().await?;
            limit.check(tmp.size)?;
            if tmp.size == 0 {
                (String::new(), 0)
            } else {
                crate::output::status(&format!("Uploading {} bytes from stdin...", tmp.size));
                let pb = crate::output::progress_bar(tmp.size, "Uploading");
                let id = upload_file_to_network(
                    &net,
                    creds.bucket(),
                    creds.mnemonic(),
                    &tmp.path,
                    tmp.size,
                    Some(crate::output::bar_sink(&pb)),
                )
                .await?;
                pb.finish_and_clear();
                (id, tmp.size)
            }
            // tmp dropped here -> temp file deleted
        }
    };

    let now = to_rfc3339(SystemTime::now());
    let drive_file =
        finish_file_entry(creds, &stem, &file_type, size, folder_uuid, &file_id, &now, &now).await?;
    emit_upload_success(creds, &drive_file);
    Ok(())
}

/// Create the drive file entry. Shared by both upload paths; the caller handles
/// any thumbnail and emits the success line.
#[allow(clippy::too_many_arguments)]
async fn finish_file_entry(
    creds: &internxt_core::models::Credentials,
    stem: &str,
    file_type: &str,
    size: u64,
    folder_uuid: &str,
    file_id: &str,
    creation: &str,
    modification: &str,
) -> Result<internxt_core::models::DriveFileData> {
    let api = DriveApi::for_credentials(creds);
    let drive_file = api
        .create_file_entry(
            &creds.token,
            stem,
            file_type,
            size,
            folder_uuid,
            file_id,
            creds.bucket(),
            creation,
            modification,
        )
        .await?;
    Ok(drive_file)
}

/// Emit the "File uploaded successfully" line (human + JSON).
fn emit_upload_success(
    creds: &internxt_core::models::Credentials,
    drive_file: &internxt_core::models::DriveFileData,
) {
    let ws_suffix = creds
        .workspace_id()
        .map(|id| format!("?workspaceid={id}"))
        .unwrap_or_default();
    crate::output::emit(
        &format!(
            "File uploaded successfully, view it at {}/file/{}{ws_suffix}",
            config::drive_web_url(),
            drive_file.uuid
        ),
        serde_json::json!({ "success": true, "file": { "uuid": drive_file.uuid } }),
    );
}

/// A temp file that deletes itself on drop. Used to spool unknown-length stdin.
struct TempSpool {
    path: PathBuf,
    size: u64,
}

impl Drop for TempSpool {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Copy all of stdin into a uniquely-named temp file, returning its path + size.
async fn spool_stdin_to_temp() -> Result<TempSpool> {
    let mut rnd = [0u8; 16];
    rand::rng().fill(&mut rnd);
    let path = std::env::temp_dir().join(format!("internxt-stdin-{}.tmp", hex::encode(rnd)));
    let mut file = tokio::fs::File::create(&path).await?;
    // Guard owns the path so a mid-copy error still cleans up.
    let mut spool = TempSpool { path, size: 0 };
    let mut stdin = tokio::io::stdin();
    let size = tokio::io::copy(&mut stdin, &mut file).await?;
    file.flush().await?;
    spool.size = size;
    Ok(spool)
}

/// Download + decrypt a file by uuid. Writes to a file under `directory` (default),
/// or to stdout when `to_stdout` is set (binary-clean: all chatter goes to stderr).
///
/// By default, writes go to a temp sibling file (`.{name}.inxt-{rand}.part`) that is
/// renamed into place only on full success, and removed on any error — mirroring
/// `sync::download_one` — so a failed download never leaves a truncated file at the
/// destination path. `legacy_write` restores the old behavior: writing directly to
/// the destination with no cleanup on error.
pub async fn download_file(
    id: Option<&str>,
    path: Option<&str>,
    directory: Option<&str>,
    overwrite: bool,
    to_stdout: bool,
    legacy_write: bool,
) -> Result<()> {
    if to_stdout && crate::output::is_json() {
        return Err(anyhow!("--stdout cannot be combined with --json"));
    }
    // When piping to stdout, status/progress must not pollute the data stream.
    let note = |m: &str| {
        if to_stdout {
            crate::output::status_err(m);
        } else {
            crate::output::status(m);
        }
    };

    let creds = auth::get_auth_details().await?;

    let api = DriveApi::for_credentials(&creds);
    let uuid = crate::paths::resolve_opt(
        &api,
        &creds.token,
        creds.root_folder(),
        id,
        path,
        crate::paths::Expect::File,
    )
    .await?
    .ok_or_else(|| anyhow!("Provide the file id (--id) or path (--path)"))?;
    let uuid = uuid.as_str();
    note("Getting file metadata...");
    let meta: DriveFileData = api.get_file_meta(&creds.token, uuid).await?;

    let name = meta
        .plain_name
        .clone()
        .or_else(|| meta.name.clone())
        .unwrap_or_else(|| uuid.to_string());
    let filename = match &meta.file_type {
        Some(t) if !t.is_empty() => format!("{name}.{t}"),
        _ => name.clone(),
    };

    // Resolve the output target. None == stdout.
    let out_path = if to_stdout {
        None
    } else {
        let dir = directory.filter(|d| !d.trim().is_empty()).unwrap_or(".");
        let p = Path::new(dir).join(&filename);
        if p.exists() && !overwrite {
            return Err(anyhow!(
                "File already exists, use --overwrite to overwrite: {}",
                p.display()
            ));
        }
        Some(p)
    };

    let size = meta.size.0;
    if size == 0 {
        if let Some(p) = &out_path {
            std::fs::write(p, b"")?;
            crate::output::emit(
                &format!("File downloaded successfully to {}", p.display()),
                serde_json::json!({ "success": true, "path": p.display().to_string() }),
            );
        } else {
            note("File downloaded (0 bytes).");
        }
        return Ok(());
    }

    let file_id = meta
        .file_id
        .clone()
        .ok_or_else(|| anyhow!("file has no network fileId"))?;
    let bucket = if meta.bucket.is_empty() {
        creds.bucket().to_string()
    } else {
        meta.bucket.clone()
    };

    note("Preparing network...");
    let net = crate::net_client::network_api(creds.net_user(), creds.net_pass());
    let links = net.get_download_links(&bucket, &file_id).await?;
    if matches!(links.version, None | Some(1)) {
        return Err(anyhow!("File version 1 not supported"));
    }

    let index = hex::decode(&links.index)?;
    let iv = &index[0..16];
    let key = crypto::generate_file_key(creds.mnemonic(), &bucket, &index)?;

    let mut shards = links.shards.clone();
    shards.sort_by_key(|s| s.index);

    // Safe (non-legacy) writes to a file land in a temp sibling first, renamed into
    // place only on success (mirrors `sync::download_one`); `--legacy-write` writes
    // straight to the destination, matching the old (and official CLI's) behavior.
    let tmp_path: Option<PathBuf> = match &out_path {
        Some(p) if !legacy_write => {
            let mut rnd = [0u8; 8];
            rand::rng().fill(&mut rnd);
            Some(p.with_file_name(format!(
                ".{}.inxt-{}.part",
                p.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
                hex::encode(rnd)
            )))
        }
        _ => None,
    };
    let write_path = tmp_path.as_deref().or(out_path.as_deref());

    let mut ctr = Ctr::new(&key, iv);
    let mut out: Box<dyn AsyncWrite + Unpin + Send> = match write_path {
        Some(p) => Box::new(tokio::fs::File::create(p).await?),
        None => Box::new(tokio::io::stdout()),
    };
    // Progress bar draws on stderr, so it never mixes into a piped download.
    let pb = crate::output::progress_bar(size, "Downloading");

    let result: Result<()> = async {
        for shard in &shards {
            let resp = net.download_shard_stream(&shard.url).await?;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let mut bytes = chunk?.to_vec();
                ctr.apply(&mut bytes);
                out.write_all(&bytes).await?;
                pb.inc(bytes.len() as u64);
            }
        }
        out.flush().await?;
        Ok(())
    }
    .await;

    if let Err(e) = result {
        pb.finish_and_clear();
        if let Some(tmp) = &tmp_path {
            let _ = tokio::fs::remove_file(tmp).await;
        }
        return Err(e);
    }
    pb.finish_and_clear();

    if let Some(tmp) = &tmp_path {
        // Safe to unwrap: tmp_path is only set when out_path is Some.
        let dest = out_path.as_ref().unwrap();
        if let Err(e) = tokio::fs::rename(tmp, dest).await {
            let _ = tokio::fs::remove_file(tmp).await;
            return Err(e.into());
        }
    }

    match &out_path {
        Some(p) => crate::output::emit(
            &format!("File downloaded successfully to {}", p.display()),
            serde_json::json!({ "success": true, "path": p.display().to_string() }),
        ),
        None => note("File downloaded to stdout."),
    }
    Ok(())
}

// ---- upload-folder ----

/// One scanned filesystem entry. `rel` is relative to the parent of the upload root,
/// so the upload root itself has `rel == <root basename>` (mirrors node's `relative`).
struct ScanNode {
    /// File stem (files) or directory basename (folders) — used as the Drive name.
    name: String,
    abs: PathBuf,
    rel: PathBuf,
    size: u64,
}

/// Node's `path.dirname` semantics: a single-component path → "root level" (None here),
/// otherwise the parent path. Used to map an item to its parent folder.
fn parent_key(rel: &Path) -> Option<PathBuf> {
    match rel.parent() {
        Some(p) if !p.as_os_str().is_empty() => Some(p.to_path_buf()),
        _ => None,
    }
}

/// Recursively scan a directory (pre-order: parent folder pushed before its children),
/// skipping symlinks. Empty (0-byte) files are included by default — Internxt's
/// free/legacy plans reject them server-side (HTTP 402), which now surfaces as a
/// normal per-file failure rather than silent loss — but `exclude_empty` (the
/// `--exclude-empty-files` flag) skips them client-side instead, for accounts
/// where that rejection is expected and not worth reporting. Returns total bytes.
fn scan_dir(
    current: &Path,
    parent: &Path,
    exclude_empty: bool,
    folders: &mut Vec<ScanNode>,
    files: &mut Vec<ScanNode>,
) -> u64 {
    let meta = match std::fs::symlink_metadata(current) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    let rel = current.strip_prefix(parent).unwrap_or(current).to_path_buf();

    if meta.is_file() {
        if exclude_empty && meta.len() == 0 {
            return 0;
        }
        let name = current
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("file")
            .to_string();
        files.push(ScanNode {
            name,
            abs: current.to_path_buf(),
            rel,
            size: meta.len(),
        });
        return meta.len();
    }

    if meta.is_dir() {
        let name = current
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("folder")
            .to_string();
        folders.push(ScanNode {
            name,
            abs: current.to_path_buf(),
            rel,
            size: 0,
        });
        let mut total = 0;
        if let Ok(entries) = std::fs::read_dir(current) {
            for entry in entries.flatten() {
                total += scan_dir(&entry.path(), parent, exclude_empty, folders, files);
            }
        }
        return total;
    }
    0
}

/// Network-upload + create-entry for a single scanned file.
async fn upload_one_file(
    net: &NetworkApi,
    api: &DriveApi,
    token: &str,
    bucket: &str,
    mnemonic: &str,
    file: &ScanNode,
    parent_uuid: &str,
    pb: &indicatif::ProgressBar,
    limit: crate::upload_limit::UploadLimit,
) -> Result<()> {
    let meta = std::fs::metadata(&file.abs)?;
    let size = meta.len();
    limit.check(size)?;
    let file_type = file
        .abs
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let mut file_id = String::new();
    if size > 0 {
        file_id = upload_file_to_network(
            net,
            bucket,
            mnemonic,
            &file.abs,
            size,
            Some(crate::output::bar_sink(pb)),
        )
        .await?;
    }

    let creation = to_rfc3339(meta.created().unwrap_or_else(|_| SystemTime::now()));
    let modification = to_rfc3339(meta.modified().unwrap_or_else(|_| SystemTime::now()));
    let drive_file = api
        .create_file_entry(
            token,
            &file.name,
            &file_type,
            size,
            parent_uuid,
            &file_id,
            bucket,
            &creation,
            &modification,
        )
        .await?;

    // Best-effort thumbnail; never fails the folder upload (silent — the shared
    // progress bar owns the terminal here).
    if config::thumbnails_enabled() && internxt_core::thumbnail::is_image_thumbnailable(&file_type, size)
    {
        let _ = internxt_core::thumbnail::try_upload_thumbnail_from_path(
            net, api, token, bucket, mnemonic, &drive_file.uuid, &file_type, &file.abs, size,
        )
        .await;
    }
    Ok(())
}

/// Recursively upload a local folder tree to Internxt Drive.
pub async fn upload_folder(
    local_path: &str,
    destination: Option<&str>,
    dest_path: Option<&str>,
    exclude_empty_files: bool,
    limit_args: &crate::upload_limit::UploadLimitArgs,
) -> Result<()> {
    let creds = Arc::new(auth::get_auth_details().await?);
    let limit = crate::upload_limit::resolve(limit_args, &creds).await?;

    let root = Path::new(local_path);
    let md = std::fs::metadata(root).map_err(|_| anyhow!("Not a directory: {local_path}"))?;
    if !md.is_dir() {
        return Err(anyhow!("Not a directory: {local_path}"));
    }
    let dest = {
        let api = DriveApi::for_credentials(&creds);
        crate::paths::resolve_destination_opt(
            &api,
            &creds.token,
            creds.root_folder(),
            destination,
            dest_path,
            crate::paths::Expect::Folder,
        )
        .await?
        .unwrap_or_else(|| creds.root_folder().to_string())
    };
    let bucket = creds.bucket().to_string();
    let mnemonic = creds.mnemonic().to_string();

    let parent = root.parent().unwrap_or_else(|| Path::new(""));
    let mut folders = Vec::new();
    let mut files = Vec::new();
    let total_bytes = scan_dir(root, parent, exclude_empty_files, &mut folders, &mut files);

    let timer = Instant::now();
    crate::output::status("Preparing network...");
    let net = crate::net_client::network_api(creds.net_user(), creds.net_pass());
    let api = DriveApi::for_credentials(&creds);

    // Independent of `pb.println` (which indicatif silently drops when stderr
    // isn't a terminal — piped/scripted/CI runs): tracked here so a partial
    // failure — folder or file — is never invisible. Declared before the folder
    // loop so both loops share it: a folder-tree problem (a name collision our
    // own lookup couldn't resolve, a missing parent, an unretryable create error)
    // no longer aborts the whole upload — it skips just the affected subtree
    // (its files fall through to "Parent folder not found, skipping" below) and
    // is reported alongside file failures in the final summary/exit code.
    let failed: Arc<std::sync::Mutex<Vec<(String, String)>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let total_folders = folders.len();

    // 1. Recreate the folder tree (sequential — children need their parent's uuid).
    let mut folder_map: HashMap<PathBuf, String> = HashMap::new();
    for f in &folders {
        let parent_uuid = match parent_key(&f.rel) {
            None => dest.clone(),
            Some(p) => match folder_map.get(&p) {
                Some(u) => u.clone(),
                None => {
                    let msg = "parent folder not found (its own creation failed earlier)";
                    crate::output::status(&format!("{}: {}, skipping...", f.rel.display(), msg));
                    failed.lock().unwrap().push((f.rel.display().to_string(), msg.to_string()));
                    continue;
                }
            },
        };
        // internxt-core 0.1.2+: on an "already exists" conflict, this looks up
        // and returns the *existing* folder's uuid instead of giving up, so a
        // name collision is transparently reused rather than treated as a
        // failure — `Ok(None)` is now reserved for the rarer case where even
        // that lookup couldn't find a match (e.g. a race where the folder was
        // renamed/deleted in between) or non-conflict retries were exhausted.
        match create_folder_with_retry(&api, &creds.token, &f.name, &parent_uuid).await {
            Ok(Some(uuid)) => {
                crate::output::status(&format!("Folder ready: {}", f.name));
                folder_map.insert(f.rel.clone(), uuid);
            }
            Ok(None) => {
                let msg = "could not create or find this folder at the destination";
                crate::output::status(&format!(
                    "Failed to resolve folder \"{}\" ({}): {}, skipping its contents...",
                    f.name,
                    f.rel.display(),
                    msg
                ));
                failed.lock().unwrap().push((f.rel.display().to_string(), msg.to_string()));
            }
            Err(e) => {
                let reason = format!("{e:#}");
                crate::output::status(&format!(
                    "Failed to create folder \"{}\" ({}): {reason}, skipping its contents...",
                    f.name,
                    f.rel.display()
                ));
                failed.lock().unwrap().push((f.rel.display().to_string(), reason));
            }
        }
    }
    // Mitigates upstream PB-1446 (folder not immediately consistent after creation).
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 2. Upload files with bounded concurrency.
    let uploaded = Arc::new(AtomicU64::new(0));
    let folder_map = Arc::new(folder_map);
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FILE_UPLOADS));
    // One shared overall bar across all files (concurrent per-file bars would clash).
    let pb = crate::output::progress_bar(total_bytes, "Uploading");
    let mut handles = Vec::new();
    let total_files = files.len();

    for file in files {
        let parent_uuid = match parent_key(&file.rel) {
            None => dest.clone(),
            Some(p) => match folder_map.get(&p) {
                Some(u) => u.clone(),
                None => {
                    let msg = "parent folder not found (its creation failed earlier)";
                    crate::output::status(&format!("{}: {}, skipping...", file.rel.display(), msg));
                    failed.lock().unwrap().push((file.rel.display().to_string(), msg.to_string()));
                    continue;
                }
            },
        };
        let permit = sem.clone().acquire_owned().await.unwrap();
        let net = net.clone();
        let api = DriveApi::for_credentials(&creds);
        let token = creds.token.clone();
        let bucket = bucket.clone();
        let mnemonic = mnemonic.clone();
        let uploaded = uploaded.clone();
        let pb = pb.clone();
        let failed = failed.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            match upload_one_file(
                &net, &api, &token, &bucket, &mnemonic, &file, &parent_uuid, &pb, limit,
            )
            .await
            {
                Ok(()) => {
                    uploaded.fetch_add(file.size, Ordering::Relaxed);
                    pb.println(format!("Uploaded {}", file.name));
                }
                Err(e) => {
                    pb.println(format!("Failed to upload {}: {e}", file.name));
                    failed.lock().unwrap().push((file.rel.display().to_string(), format!("{e:#}")));
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    pb.finish_and_clear();

    let root_name = root
        .file_name()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(local_path));
    let root_folder_id = folder_map.get(&root_name).cloned().unwrap_or_default();
    let elapsed_ms = timer.elapsed().as_millis();
    let total_uploaded = uploaded.load(Ordering::Relaxed);
    let _ = total_bytes;

    let folder_url = format!("{}/folder/{}", config::drive_web_url(), root_folder_id);
    let failed = failed.lock().unwrap().clone();
    if !failed.is_empty() {
        let detail = failed
            .iter()
            .map(|(rel, err)| format!("{rel}: {err}"))
            .collect::<Vec<_>>()
            .join("; ");
        return Err(anyhow!(
            "{} of {} folder(s)/file(s) failed ({} of {} bytes uploaded, folder: {folder_url}): {detail}",
            failed.len(),
            total_folders + total_files,
            total_uploaded,
            total_bytes,
        ));
    }

    crate::output::emit(
        &format!(
            "Folder uploaded in {elapsed_ms}ms, view it at {folder_url} ({total_uploaded} bytes)"
        ),
        serde_json::json!({
            "success": true,
            "folder": { "uuid": root_folder_id },
            "totalBytes": total_uploaded,
            "uploadTimeMs": elapsed_ms,
        }),
    );
    Ok(())
}

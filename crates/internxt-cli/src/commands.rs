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
pub async fn upload_file(
    file_path: Option<&str>,
    destination: Option<&str>,
    use_stdin: bool,
    name: Option<&str>,
    size_hint: Option<u64>,
    limit_args: &crate::upload_limit::UploadLimitArgs,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let limit = crate::upload_limit::resolve(limit_args, &creds).await?;

    let folder_uuid = match destination {
        Some(d) if !d.trim().is_empty() => d.to_string(),
        _ => creds.root_folder().to_string(),
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

    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let file_type = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let mut file_id = String::new();

    if size > 0 {
        let net = NetworkApi::new(creds.net_user(), creds.net_pass());
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

    finish_file_entry(
        &creds,
        &stem,
        &file_type,
        size,
        &folder_uuid,
        &file_id,
        &creation,
        &modification,
    )
    .await
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

    let net = NetworkApi::new(creds.net_user(), creds.net_pass());

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
    finish_file_entry(creds, &stem, &file_type, size, folder_uuid, &file_id, &now, &now).await
}

/// Create the drive file entry and emit the success line. Shared by both upload paths.
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
) -> Result<()> {
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
    Ok(())
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
pub async fn download_file(
    uuid: &str,
    directory: Option<&str>,
    overwrite: bool,
    to_stdout: bool,
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
    let net = NetworkApi::new(creds.net_user(), creds.net_pass());
    let links = net.get_download_links(&bucket, &file_id).await?;
    if matches!(links.version, None | Some(1)) {
        return Err(anyhow!("File version 1 not supported"));
    }

    let index = hex::decode(&links.index)?;
    let iv = &index[0..16];
    let key = crypto::generate_file_key(creds.mnemonic(), &bucket, &index)?;

    let mut shards = links.shards.clone();
    shards.sort_by_key(|s| s.index);

    let mut ctr = Ctr::new(&key, iv);
    let mut out: Box<dyn AsyncWrite + Unpin + Send> = match &out_path {
        Some(p) => Box::new(tokio::fs::File::create(p).await?),
        None => Box::new(tokio::io::stdout()),
    };
    // Progress bar draws on stderr, so it never mixes into a piped download.
    let pb = crate::output::progress_bar(size, "Downloading");

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
    pb.finish_and_clear();
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
/// skipping symlinks and zero-byte files. Returns total bytes.
fn scan_dir(
    current: &Path,
    parent: &Path,
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
        if meta.len() == 0 {
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
                total += scan_dir(&entry.path(), parent, folders, files);
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
    api.create_file_entry(
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
    Ok(())
}

/// Recursively upload a local folder tree to Internxt Drive.
pub async fn upload_folder(
    local_path: &str,
    destination: Option<&str>,
    limit_args: &crate::upload_limit::UploadLimitArgs,
) -> Result<()> {
    let creds = Arc::new(auth::get_auth_details().await?);
    let limit = crate::upload_limit::resolve(limit_args, &creds).await?;

    let root = Path::new(local_path);
    let md = std::fs::metadata(root).map_err(|_| anyhow!("Not a directory: {local_path}"))?;
    if !md.is_dir() {
        return Err(anyhow!("Not a directory: {local_path}"));
    }
    let dest = match destination {
        Some(d) if !d.trim().is_empty() => d.to_string(),
        _ => creds.root_folder().to_string(),
    };
    let bucket = creds.bucket().to_string();
    let mnemonic = creds.mnemonic().to_string();

    let parent = root.parent().unwrap_or_else(|| Path::new(""));
    let mut folders = Vec::new();
    let mut files = Vec::new();
    let total_bytes = scan_dir(root, parent, &mut folders, &mut files);

    let timer = Instant::now();
    crate::output::status("Preparing network...");
    let net = NetworkApi::new(creds.net_user(), creds.net_pass());
    let api = DriveApi::for_credentials(&creds);

    // 1. Recreate the folder tree (sequential — children need their parent's uuid).
    let mut folder_map: HashMap<PathBuf, String> = HashMap::new();
    for f in &folders {
        let parent_uuid = match parent_key(&f.rel) {
            None => dest.clone(),
            Some(p) => match folder_map.get(&p) {
                Some(u) => u.clone(),
                None => {
                    crate::output::status(&format!(
                        "Parent folder not found for {}, skipping...",
                        f.rel.display()
                    ));
                    continue;
                }
            },
        };
        if let Some(uuid) =
            create_folder_with_retry(&api, &creds.token, &f.name, &parent_uuid).await?
        {
            crate::output::status(&format!("Created folder {}", f.name));
            folder_map.insert(f.rel.clone(), uuid);
        }
    }
    if folder_map.is_empty() {
        return Err(anyhow!("Failed to create folders, cannot upload files"));
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

    for file in files {
        let parent_uuid = match parent_key(&file.rel) {
            None => dest.clone(),
            Some(p) => match folder_map.get(&p) {
                Some(u) => u.clone(),
                None => {
                    crate::output::status(&format!(
                        "Parent folder not found for {}, skipping...",
                        file.rel.display()
                    ));
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

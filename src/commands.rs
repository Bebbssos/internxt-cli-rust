//! upload-file and download-file. Fully streaming — never holds a whole file in RAM.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use rand::RngExt;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::auth;
use crate::config;
use crate::crypto::{self, Ctr};
use crate::network::{NetworkApi, PartRef};
use crate::{api::DriveApi, models::DriveFileData};

const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024; // 100MB
const PART_SIZE: usize = 15 * 1024 * 1024; // 15MB
const READ_CHUNK: usize = 1024 * 1024; // 1MB stream granularity
const UPLOAD_CONCURRENCY: usize = 10;
const MAX_CONCURRENT_FILE_UPLOADS: usize = 10;
const FOLDER_CREATE_RETRIES: usize = 2;
const RETRY_DELAYS_MS: [u64; 3] = [500, 1000, 2000];

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
) -> Result<()> {
    let creds = auth::get_auth_details().await?;

    let folder_uuid = match destination {
        Some(d) if !d.trim().is_empty() => d.to_string(),
        _ => creds.root_folder().to_string(),
    };

    if use_stdin {
        return upload_from_stdin(&creds, name, size_hint, &folder_uuid).await;
    }

    let file_path = file_path.ok_or_else(|| anyhow!("Provide --file <PATH> or --stdin"))?;
    let path = Path::new(file_path);
    let meta = std::fs::metadata(path).map_err(|_| anyhow!("File not found: {file_path}"))?;
    if !meta.is_file() {
        return Err(anyhow!("Not a file: {file_path}"));
    }
    let size = meta.len();

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
        file_id = upload_file_to_network(&net, creds.bucket(), creds.mnemonic(), path, size).await?;
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
    creds: &crate::models::Credentials,
    name: Option<&str>,
    size_hint: Option<u64>,
    folder_uuid: &str,
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
            crate::output::status(&format!("Uploading {sz} bytes from stdin..."));
            let id = upload_stream_to_network(
                &net,
                creds.bucket(),
                creds.mnemonic(),
                tokio::io::stdin(),
                sz,
            )
            .await?;
            (id, sz)
        }
        None => {
            crate::output::status("Buffering stdin...");
            let tmp = spool_stdin_to_temp().await?;
            if tmp.size == 0 {
                (String::new(), 0)
            } else {
                crate::output::status(&format!("Uploading {} bytes from stdin...", tmp.size));
                let id = upload_file_to_network(
                    &net,
                    creds.bucket(),
                    creds.mnemonic(),
                    &tmp.path,
                    tmp.size,
                )
                .await?;
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
    creds: &crate::models::Credentials,
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

/// Encrypt + upload a file's bytes to the network, returning the network file id.
/// Picks single-part or multipart based on size. Shared by upload-file / upload-folder.
pub async fn upload_file_to_network(
    net: &NetworkApi,
    bucket: &str,
    mnemonic: &str,
    path: &Path,
    size: u64,
) -> Result<String> {
    let mut index = [0u8; 32];
    rand::rng().fill(&mut index);
    let iv = index[0..16].to_vec();
    let key = crypto::generate_file_key(mnemonic, bucket, &index)?;

    if size > MULTIPART_THRESHOLD {
        upload_multipart(net, bucket, size, path, &key, &iv, &index).await
    } else {
        let file = tokio::fs::File::open(path).await?;
        upload_single(net, bucket, size, file, &key, &iv, &index).await
    }
}

/// Encrypt + upload `size` bytes from an arbitrary reader (e.g. stdin), returning
/// the network file id. Always single-part: the source isn't seekable, so multipart
/// (which re-slices a buffered stream) doesn't apply — the size must be known.
pub async fn upload_stream_to_network<R>(
    net: &NetworkApi,
    bucket: &str,
    mnemonic: &str,
    reader: R,
    size: u64,
) -> Result<String>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut index = [0u8; 32];
    rand::rng().fill(&mut index);
    let iv = index[0..16].to_vec();
    let key = crypto::generate_file_key(mnemonic, bucket, &index)?;
    upload_single(net, bucket, size, reader, &key, &iv, &index).await
}

/// Single presigned-URL upload, body streamed straight from a reader through CTR.
async fn upload_single<R>(
    net: &NetworkApi,
    bucket: &str,
    size: u64,
    reader: R,
    key: &[u8; 32],
    iv: &[u8],
    index: &[u8],
) -> Result<String>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let start = net.start_upload(bucket, size, 1).await?;
    let slot = start
        .uploads
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no upload slot returned"))?;
    let url = slot.url.ok_or_else(|| anyhow!("no upload url returned"))?;

    let hasher = Arc::new(Mutex::new(Sha256::new()));

    // Streaming state moved into the body producer.
    struct St<R> {
        reader: R,
        ctr: Ctr,
        hasher: Arc<Mutex<Sha256>>,
    }
    let st = St {
        reader,
        ctr: Ctr::new(key, iv),
        hasher: hasher.clone(),
    };

    let body = futures_util::stream::unfold(st, |mut st| async move {
        let mut buf = vec![0u8; READ_CHUNK];
        match st.reader.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                st.ctr.apply(&mut buf);
                st.hasher.lock().unwrap().update(&buf);
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), st))
            }
            Err(e) => Some((Err(e), st)),
        }
    });

    crate::output::status(&format!("Uploading {} bytes...", size));
    net.put_stream(&url, size, body).await?;

    let digest = hasher.lock().unwrap().clone().finalize();
    let hash = hex::encode(crypto::ripemd160(&digest));

    let finish = net
        .finish_upload(bucket, &hex::encode(index), &hash, &slot.uuid)
        .await?;
    Ok(finish.id)
}

/// Multipart upload: continuous CTR stream sliced into 15MB parts, PUT concurrently.
async fn upload_multipart(
    net: &NetworkApi,
    bucket: &str,
    size: u64,
    path: &Path,
    key: &[u8; 32],
    iv: &[u8],
    index: &[u8],
) -> Result<String> {
    let num_parts = size.div_ceil(PART_SIZE as u64) as u32;
    let start = net.start_upload(bucket, size, num_parts).await?;
    let slot = start
        .uploads
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no upload slot returned"))?;
    let urls = slot.urls.ok_or_else(|| anyhow!("no upload urls returned"))?;
    let upload_id = slot
        .upload_id
        .ok_or_else(|| anyhow!("no UploadId returned"))?;

    let mut hasher = Sha256::new();
    let mut ctr = Ctr::new(key, iv);
    let mut file = tokio::fs::File::open(path).await?;

    let sem = Arc::new(tokio::sync::Semaphore::new(UPLOAD_CONCURRENCY));
    let mut handles = Vec::new();
    let mut part_buf: Vec<u8> = Vec::with_capacity(PART_SIZE);
    let mut part_number: u32 = 1;
    let mut read_buf = vec![0u8; READ_CHUNK];
    let mut uploaded: u64 = 0;

    loop {
        let n = file.read(&mut read_buf).await?;
        if n == 0 {
            break;
        }
        let mut chunk = read_buf[..n].to_vec();
        ctr.apply(&mut chunk);
        hasher.update(&chunk);
        part_buf.extend_from_slice(&chunk);

        while part_buf.len() >= PART_SIZE {
            let rest = part_buf.split_off(PART_SIZE);
            let body = std::mem::replace(&mut part_buf, rest);
            dispatch_part(net, &urls, &sem, &mut handles, part_number, body).await?;
            part_number += 1;
        }
    }
    if !part_buf.is_empty() {
        let body = std::mem::take(&mut part_buf);
        dispatch_part(net, &urls, &sem, &mut handles, part_number, body).await?;
    }

    let mut parts = Vec::with_capacity(handles.len());
    for h in handles {
        let p = h.await.map_err(|e| anyhow!("part task panicked: {e}"))??;
        uploaded += 1;
        parts.push(p);
    }
    parts.sort_by_key(|p| p.part_number);
    let _ = uploaded;

    let digest = hasher.finalize();
    let hash = hex::encode(crypto::ripemd160(&digest));

    let finish = net
        .finish_multipart_upload(bucket, &hex::encode(index), &hash, &slot.uuid, &upload_id, &parts)
        .await?;
    Ok(finish.id)
}

async fn dispatch_part(
    net: &NetworkApi,
    urls: &[String],
    sem: &Arc<tokio::sync::Semaphore>,
    handles: &mut Vec<tokio::task::JoinHandle<Result<PartRef>>>,
    part_number: u32,
    body: Vec<u8>,
) -> Result<()> {
    let url = urls
        .get((part_number - 1) as usize)
        .ok_or_else(|| anyhow!("missing presigned url for part {part_number}"))?
        .clone();
    let permit = sem.clone().acquire_owned().await.unwrap();
    let net = net.clone();
    handles.push(tokio::spawn(async move {
        let _permit = permit;
        let etag = net.put_part(&url, body).await?;
        crate::output::status(&format!("Uploaded part {part_number}"));
        Ok(PartRef { part_number, etag })
    }));
    Ok(())
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
    let mut written: u64 = 0;

    for shard in &shards {
        let resp = net.download_shard_stream(&shard.url).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let mut bytes = chunk?.to_vec();
            ctr.apply(&mut bytes);
            out.write_all(&bytes).await?;
            written += bytes.len() as u64;
            // Progress always to stderr so it never mixes into a piped download.
            if size > 0 && !crate::output::is_json() {
                let pct = (written as f64 / size as f64 * 100.0).min(100.0);
                eprint!("\rDownloading {pct:.0}%");
                use std::io::Write;
                let _ = std::io::stderr().flush();
            }
        }
    }
    out.flush().await?;
    if !crate::output::is_json() {
        eprint!("\r");
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

/// Create a folder, retrying transient failures; returns None if it already exists.
async fn create_folder_with_retry(
    api: &DriveApi,
    token: &str,
    name: &str,
    parent_uuid: &str,
) -> Result<Option<String>> {
    for attempt in 0..=FOLDER_CREATE_RETRIES {
        match api.create_folder(token, name, parent_uuid).await {
            Ok(v) => {
                let uuid = v["uuid"].as_str().unwrap_or_default().to_string();
                return Ok(Some(uuid));
            }
            Err(e) => {
                if e.to_string().to_lowercase().contains("already exists") {
                    return Ok(None);
                }
                if attempt < FOLDER_CREATE_RETRIES {
                    tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAYS_MS[attempt]))
                        .await;
                } else {
                    return Err(e);
                }
            }
        }
    }
    Ok(None)
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
) -> Result<()> {
    let meta = std::fs::metadata(&file.abs)?;
    let size = meta.len();
    let file_type = file
        .abs
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let mut file_id = String::new();
    if size > 0 {
        file_id = upload_file_to_network(net, bucket, mnemonic, &file.abs, size).await?;
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
pub async fn upload_folder(local_path: &str, destination: Option<&str>) -> Result<()> {
    let creds = Arc::new(auth::get_auth_details().await?);

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
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            match upload_one_file(&net, &api, &token, &bucket, &mnemonic, &file, &parent_uuid).await {
                Ok(()) => {
                    uploaded.fetch_add(file.size, Ordering::Relaxed);
                    crate::output::status(&format!("Uploaded {}", file.name));
                }
                Err(e) => {
                    crate::output::status(&format!("Failed to upload {}: {e}", file.name));
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }

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

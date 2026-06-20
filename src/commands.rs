//! upload-file and download-file. Fully streaming — never holds a whole file in RAM.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::auth;
use crate::config;
use crate::crypto::{self, Ctr};
use crate::network::{NetworkApi, PartRef};
use crate::{api::DriveApi, models::DriveFileData};

const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024; // 100MB
const PART_SIZE: usize = 15 * 1024 * 1024; // 15MB
const READ_CHUNK: usize = 1024 * 1024; // 1MB stream granularity
const UPLOAD_CONCURRENCY: usize = 10;

fn to_rfc3339(t: SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.to_rfc3339()
}

/// Upload a local file to Internxt Drive (streaming; single-part or multipart).
pub async fn upload_file(file_path: &str, destination: Option<&str>) -> Result<()> {
    let creds = auth::read_credentials()?;
    let user = &creds.user;

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

    let folder_uuid = match destination {
        Some(d) if !d.trim().is_empty() => d.to_string(),
        _ => user.root_folder_id.clone(),
    };

    let mut file_id = String::new();

    if size > 0 {
        let net = NetworkApi::new(&user.bridge_user, &user.user_id);

        let mut index = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut index);
        let iv = index[0..16].to_vec();
        let key = crypto::generate_file_key(&user.mnemonic, &user.bucket, &index)?;

        println!("Preparing network...");
        file_id = if size > MULTIPART_THRESHOLD {
            upload_multipart(&net, &user.bucket, size, path, &key, &iv, &index).await?
        } else {
            upload_single(&net, &user.bucket, size, path, &key, &iv, &index).await?
        };
    }

    let creation = to_rfc3339(meta.created().unwrap_or_else(|_| SystemTime::now()));
    let modification = to_rfc3339(meta.modified().unwrap_or_else(|_| SystemTime::now()));

    let api = DriveApi::new();
    let drive_file = api
        .create_file_entry(
            &creds.token,
            &stem,
            &file_type,
            size,
            &folder_uuid,
            &file_id,
            &user.bucket,
            &creation,
            &modification,
        )
        .await?;

    println!(
        "File uploaded successfully, view it at {}/file/{}",
        config::drive_web_url(),
        drive_file.uuid
    );
    Ok(())
}

/// Single presigned-URL upload, body streamed straight from disk through CTR.
async fn upload_single(
    net: &NetworkApi,
    bucket: &str,
    size: u64,
    path: &Path,
    key: &[u8; 32],
    iv: &[u8],
    index: &[u8],
) -> Result<String> {
    let start = net.start_upload(bucket, size, 1).await?;
    let slot = start
        .uploads
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("no upload slot returned"))?;
    let url = slot.url.ok_or_else(|| anyhow!("no upload url returned"))?;

    let hasher = Arc::new(Mutex::new(Sha256::new()));
    let file = tokio::fs::File::open(path).await?;

    // Streaming state moved into the body producer.
    struct St {
        file: tokio::fs::File,
        ctr: Ctr,
        hasher: Arc<Mutex<Sha256>>,
    }
    let st = St {
        file,
        ctr: Ctr::new(key, iv),
        hasher: hasher.clone(),
    };

    let body = futures_util::stream::unfold(st, |mut st| async move {
        let mut buf = vec![0u8; READ_CHUNK];
        match st.file.read(&mut buf).await {
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

    println!("Uploading {} bytes...", size);
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
        println!("Uploaded part {part_number}");
        Ok(PartRef { part_number, etag })
    }));
    Ok(())
}

/// Download + decrypt a file by uuid, streaming chunks to disk.
pub async fn download_file(uuid: &str, directory: Option<&str>, overwrite: bool) -> Result<()> {
    let creds = auth::read_credentials()?;
    let user = &creds.user;

    let api = DriveApi::new();
    println!("Getting file metadata...");
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

    let dir = directory.filter(|d| !d.trim().is_empty()).unwrap_or(".");
    let out_path = Path::new(dir).join(&filename);

    if out_path.exists() && !overwrite {
        return Err(anyhow!(
            "File already exists, use --overwrite to overwrite: {}",
            out_path.display()
        ));
    }

    let size = meta.size.0;
    if size == 0 {
        std::fs::write(&out_path, b"")?;
        println!("File downloaded successfully to {}", out_path.display());
        return Ok(());
    }

    let file_id = meta
        .file_id
        .clone()
        .ok_or_else(|| anyhow!("file has no network fileId"))?;
    let bucket = if meta.bucket.is_empty() {
        user.bucket.clone()
    } else {
        meta.bucket.clone()
    };

    println!("Preparing network...");
    let net = NetworkApi::new(&user.bridge_user, &user.user_id);
    let links = net.get_download_links(&bucket, &file_id).await?;
    if matches!(links.version, None | Some(1)) {
        return Err(anyhow!("File version 1 not supported"));
    }

    let index = hex::decode(&links.index)?;
    let iv = &index[0..16];
    let key = crypto::generate_file_key(&user.mnemonic, &bucket, &index)?;

    let mut shards = links.shards.clone();
    shards.sort_by_key(|s| s.index);

    let mut ctr = Ctr::new(&key, iv);
    let mut out = tokio::fs::File::create(&out_path).await?;
    let mut written: u64 = 0;

    for shard in &shards {
        let resp = net.download_shard_stream(&shard.url).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let mut bytes = chunk?.to_vec();
            ctr.apply(&mut bytes);
            out.write_all(&bytes).await?;
            written += bytes.len() as u64;
            if size > 0 {
                let pct = (written as f64 / size as f64 * 100.0).min(100.0);
                print!("\rDownloading {pct:.0}%");
                use std::io::Write;
                let _ = std::io::stdout().flush();
            }
        }
    }
    out.flush().await?;
    println!("\rFile downloaded successfully to {}", out_path.display());
    Ok(())
}

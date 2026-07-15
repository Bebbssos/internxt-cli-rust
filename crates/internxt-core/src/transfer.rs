//! Streaming network (bridge) transfer primitives — fully streaming, never holds
//! a whole file in RAM. These are the reusable core behind `upload-file`,
//! `upload-folder`, `download-file`, the WebDAV/FUSE backends and `sync`.
//!
//! Byte progress is reported through an optional [`ProgressSink`]; when `None`,
//! progress is silently discarded. No terminal output lives here.

use anyhow::{anyhow, Result};
use bytes::Bytes;
use futures_util::StreamExt;
use rand::RngExt;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::crypto::{self, Ctr};
use crate::network::NetworkApi;
use crate::progress::{noop_sink, ProgressSink};

// fs-only deps (the path-based / multipart / folder-create helpers).
#[cfg(feature = "fs")]
use crate::api::DriveApi;
#[cfg(feature = "fs")]
use crate::network::PartRef;
#[cfg(feature = "fs")]
use std::path::Path;

const READ_CHUNK: usize = 1024 * 1024; // 1MB stream granularity
#[cfg(feature = "fs")]
const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024; // 100MB
#[cfg(feature = "fs")]
const PART_SIZE: usize = 15 * 1024 * 1024; // 15MB
#[cfg(feature = "fs")]
const UPLOAD_CONCURRENCY: usize = 10;
#[cfg(feature = "fs")]
const FOLDER_CREATE_RETRIES: usize = 2;
#[cfg(feature = "fs")]
const RETRY_DELAYS_MS: [u64; 3] = [500, 1000, 2000];

/// Encrypt + upload a file's bytes to the network, returning the network file id.
/// Picks single-part or multipart based on size. Shared by upload-file / upload-folder.
/// `pb` = an optional progress sink to report byte progress into (e.g. a shared
/// bar for folder uploads). When `None`, progress is discarded.
///
/// Requires the `fs` feature (reads from a filesystem path). For a non-fs source
/// use [`upload_stream_to_network`].
#[cfg(feature = "fs")]
pub async fn upload_file_to_network(
    net: &NetworkApi,
    bucket: &str,
    mnemonic: &str,
    path: &Path,
    size: u64,
    pb: Option<Arc<dyn ProgressSink>>,
) -> Result<String> {
    let mut index = [0u8; 32];
    rand::rng().fill(&mut index);
    let iv = index[0..16].to_vec();
    let key = crypto::generate_file_key(mnemonic, bucket, &index)?;
    let pb = pb.unwrap_or_else(noop_sink);

    if size > MULTIPART_THRESHOLD {
        upload_multipart(net, bucket, size, path, &key, &iv, &index, &pb).await
    } else {
        let file = tokio::fs::File::open(path).await?;
        upload_single(net, bucket, size, file, &key, &iv, &index, &pb).await
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
    pb: Option<Arc<dyn ProgressSink>>,
) -> Result<String>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut index = [0u8; 32];
    rand::rng().fill(&mut index);
    let iv = index[0..16].to_vec();
    let key = crypto::generate_file_key(mnemonic, bucket, &index)?;
    let pb = pb.unwrap_or_else(noop_sink);
    upload_single(net, bucket, size, reader, &key, &iv, &index, &pb).await
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
    pb: &Arc<dyn ProgressSink>,
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
        pb: Arc<dyn ProgressSink>,
    }
    let st = St {
        reader,
        ctr: Ctr::new(key, iv),
        hasher: hasher.clone(),
        pb: pb.clone(),
    };

    let body = futures_util::stream::unfold(st, |mut st| async move {
        let mut buf = vec![0u8; READ_CHUNK];
        match st.reader.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                st.ctr.apply(&mut buf);
                st.hasher.lock().unwrap().update(&buf);
                st.pb.inc(n as u64);
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), st))
            }
            Err(e) => Some((Err(e), st)),
        }
    });

    net.put_stream(&url, size, body).await?;

    let digest = hasher.lock().unwrap().clone().finalize();
    let hash = hex::encode(crypto::ripemd160(&digest));

    let finish = net
        .finish_upload(bucket, &hex::encode(index), &hash, &slot.uuid)
        .await?;
    Ok(finish.id)
}

/// Multipart upload: continuous CTR stream sliced into 15MB parts, PUT concurrently.
#[cfg(feature = "fs")]
#[allow(clippy::too_many_arguments)]
async fn upload_multipart(
    net: &NetworkApi,
    bucket: &str,
    size: u64,
    path: &Path,
    key: &[u8; 32],
    iv: &[u8],
    index: &[u8],
    pb: &Arc<dyn ProgressSink>,
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
            dispatch_part(net, &urls, &sem, pb, &mut handles, part_number, body).await?;
            part_number += 1;
        }
    }
    if !part_buf.is_empty() {
        let body = std::mem::take(&mut part_buf);
        dispatch_part(net, &urls, &sem, pb, &mut handles, part_number, body).await?;
    }

    let mut parts = Vec::with_capacity(handles.len());
    for h in handles {
        let p = h.await.map_err(|e| anyhow!("part task panicked: {e}"))??;
        parts.push(p);
    }
    parts.sort_by_key(|p| p.part_number);

    let digest = hasher.finalize();
    let hash = hex::encode(crypto::ripemd160(&digest));

    let finish = net
        .finish_multipart_upload(bucket, &hex::encode(index), &hash, &slot.uuid, &upload_id, &parts)
        .await?;
    Ok(finish.id)
}

#[cfg(feature = "fs")]
async fn dispatch_part(
    net: &NetworkApi,
    urls: &[String],
    sem: &Arc<tokio::sync::Semaphore>,
    pb: &Arc<dyn ProgressSink>,
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
    let pb = pb.clone();
    let len = body.len() as u64;
    handles.push(tokio::spawn(async move {
        let _permit = permit;
        let etag = net.put_part(&url, body).await?;
        pb.inc(len);
        Ok(PartRef { part_number, etag })
    }));
    Ok(())
}

/// Stream a network file's decrypted bytes into an arbitrary writer. Reusable
/// core behind `download-file`, the WebDAV GET handler and the FUSE read path.
///
/// `range` optionally restricts output to a byte window `(start, len)`. With
/// per-shard sizes the CTR keystream is seeked to `start` and only the covering
/// shards are fetched (boundary shards byte-ranged over HTTP); otherwise it falls
/// back to decrypting from byte 0 and discarding the prefix. No progress / status
/// output — the caller owns that.
pub async fn download_file_to_writer<W>(
    net: &NetworkApi,
    mnemonic: &str,
    bucket: &str,
    file_id: &str,
    out: &mut W,
    range: Option<(u64, u64)>,
) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let links = net.get_download_links(bucket, file_id).await?;
    if matches!(links.version, None | Some(1)) {
        return Err(anyhow!("File version 1 not supported"));
    }

    let index = hex::decode(&links.index)?;
    let iv = &index[0..16];
    let key = crypto::generate_file_key(mnemonic, bucket, &index)?;

    let mut shards = links.shards.clone();
    shards.sort_by_key(|s| s.index);

    let mut ctr = Ctr::new(&key, iv);

    let (start, end) = match range {
        // Exclusive byte window `[start, end)`, clamped to the file size.
        Some((start, len)) => {
            let start = start.min(links.size);
            (start, start.saturating_add(len).min(links.size))
        }
        // Whole file.
        None => (0, links.size),
    };
    if end <= start {
        out.flush().await?;
        return Ok(());
    }

    // The whole file is one continuous CTR stream sliced into shards (ordered by
    // `index`). With per-shard sizes we can seek the keystream to `start`, skip
    // shards entirely below the window, and byte-range the boundary shards over
    // HTTP — so a partial read only fetches the covered bytes. A single-shard
    // file needs no per-shard size (the shard spans the whole file); a
    // multi-shard file whose sizes the API omitted falls back to decrypting
    // from byte 0 and discarding the prefix (correct, but re-fetches it).
    let sizes_known = shards.len() == 1 || shards.iter().all(|s| s.size > 0);

    if sizes_known {
        ctr.seek(start);
        let mut base: u64 = 0;
        for shard in &shards {
            let ssize = if shards.len() == 1 { links.size } else { shard.size };
            let shard_start = base;
            let shard_end = base + ssize; // exclusive
            base = shard_end;
            if shard_end <= start {
                continue; // entirely before the window
            }
            if shard_start >= end {
                break; // entirely after the window
            }
            let ov_start = start.max(shard_start);
            let ov_end = end.min(shard_end); // exclusive
            let local_start = ov_start - shard_start;
            let local_end_incl = ov_end - shard_start - 1; // inclusive, S3 semantics
            let whole = local_start == 0 && ov_end == shard_end;
            let resp = if whole {
                net.download_shard_stream(&shard.url).await?
            } else {
                net.download_shard_range_stream(&shard.url, local_start, local_end_incl)
                    .await?
            };
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let mut bytes = chunk?.to_vec();
                ctr.apply(&mut bytes);
                out.write_all(&bytes).await?;
            }
        }
    } else {
        // Fallback: multi-shard file with unknown sizes. Decrypt continuously
        // from the start and skip the prefix bytes before `start`.
        let mut to_skip = start;
        let mut remaining = end - start;
        'shards: for shard in &shards {
            let resp = net.download_shard_stream(&shard.url).await?;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let mut bytes = chunk?.to_vec();
                ctr.apply(&mut bytes);
                let mut slice: &[u8] = &bytes;
                if to_skip > 0 {
                    let drop = (to_skip as usize).min(slice.len());
                    slice = &slice[drop..];
                    to_skip -= drop as u64;
                }
                let take = (remaining as usize).min(slice.len());
                slice = &slice[..take];
                remaining -= take as u64;
                if !slice.is_empty() {
                    out.write_all(slice).await?;
                }
                if remaining == 0 {
                    break 'shards;
                }
            }
        }
    }
    out.flush().await?;
    Ok(())
}

/// Create a folder, retrying transient failures; returns None if it already exists.
///
/// Requires the `fs` feature (uses `tokio::time` for retry backoff).
#[cfg(feature = "fs")]
pub async fn create_folder_with_retry(
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

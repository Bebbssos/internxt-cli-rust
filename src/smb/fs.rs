//! Drive-backed `ShareBackend` / `Handle` for the vendored `smb-server` crate.
//!
//! The `smb-server` crate owns the SMB2/3 wire protocol, NTLM auth and signing;
//! we only implement its storage abstraction over Internxt Drive — the same
//! shape as `fuser::Filesystem` for the FUSE backend. Paths (not inodes) are the
//! key here, so there is no inode table: each op re-resolves the path through
//! the shared folder tree (`crate::serve::tree`), which the short-TTL
//! `FolderCache` keeps cheap.
//!
//! Reads reuse FUSE's sequential-streaming reader (forward-skip, backward-seek
//! restarts as a ranged download). Writes reuse FUSE's whole-file model: a temp
//! file backs the handle, existing content is materialized lazily, and the temp
//! file is uploaded in full on `close`, replacing the Drive entry.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use rand::RngExt;
use serde_json::json;
use smb_server::{
    BackendCapabilities, DirEntry, FileInfo, FileTimes, Handle, OpenIntent, OpenOptions, ShareBackend,
    SmbError, SmbPath, SmbResult,
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, DuplexStream};

use internxt_core::api::DriveApi;
use internxt_core::models::Credentials;
use crate::serve::cache::FolderCache;
use crate::serve::creds::SharedCreds;
use crate::serve::tree::{self, FileItem};

/// 1601→1970 gap in 100ns ticks (FILETIME epoch offset).
const FILETIME_OFFSET: u64 = 116_444_736_000_000_000;

/// Forward gaps up to this size are skipped by reading-and-discarding from the
/// live stream; past it, the stream restarts as a ranged download instead —
/// see the matching constant in `fuse/fs.rs` for why this matters for MP4
/// playback (moov-atom-at-end probing jumps).
const MAX_FORWARD_SKIP: u64 = 8 * 1024 * 1024;

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Per-op request trace (`[METHOD] path`). Verbose-only: printed just when
/// `--verbose` is set, so a busy share doesn't spam stderr by default.
pub(crate) fn log(msg: &str) {
    crate::serve::log::trace(msg);
}

/// Stable, non-zero 64-bit id derived from a Drive uuid, for `FileInfo.file_index`.
/// The kernel cifs client uses this as the server inode number; if every entry
/// reports 0 it decides the server has no usable inode numbers, aliases them all
/// onto one, and hands out `Stale file handle` on the mount root. FNV-1a over the
/// uuid gives a stable per-item value; coerce 0 away (empty/new uuid → 1).
fn stable_ino(uuid: &str) -> u64 {
    if uuid.is_empty() {
        // Unknown (e.g. a not-yet-uploaded file): 0 lets the dispatcher fall
        // back to the per-open FileId for single-file stat.
        return 0;
    }
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in uuid.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if h == 0 { 1 } else { h }
}

/// RFC3339 timestamp → Windows FILETIME (100ns ticks since 1601). Falls back to
/// the epoch offset (1970) on parse failure.
fn rfc3339_to_filetime(s: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| {
            let secs = dt.timestamp();
            if secs < 0 {
                0
            } else {
                FILETIME_OFFSET + (secs as u64) * 10_000_000 + (dt.timestamp_subsec_nanos() as u64 / 100)
            }
        })
        .unwrap_or(FILETIME_OFFSET)
}

/// Map an internal (anyhow) error to an SMB error. Network/API/IO failures all
/// surface as `STATUS_UNEXPECTED_IO_ERROR`.
fn to_smb(e: anyhow::Error) -> SmbError {
    SmbError::Io(std::io::Error::other(format!("{e:#}")))
}

/// Split a filename into (plainName, extension-without-dot). A leading-dot name
/// like `.env` is treated as having no extension. Mirrors the FUSE backend.
fn split_name(name: &str) -> (String, String) {
    let p = Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
    if stem.is_empty() {
        (name.to_string(), String::new())
    } else {
        (stem.to_string(), ext.to_string())
    }
}

fn temp_path(dir: Option<&Path>) -> PathBuf {
    let mut rnd = [0u8; 16];
    rand::rng().fill(&mut rnd);
    let base = dir.map(|d| d.to_path_buf()).unwrap_or_else(std::env::temp_dir);
    base.join(format!("internxt-smb-{}.tmp", hex::encode(rnd)))
}

/// Case-insensitive DOS-style wildcard match (`*` and `?`). `*`, `*.*` and the
/// empty string match everything (the dispatcher already collapses those to
/// `None`, but we stay robust). Iterative with backtracking.
fn glob_match(pattern: &str, name: &str) -> bool {
    if pattern.is_empty() || pattern == "*" || pattern == "*.*" {
        return true;
    }
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut star_n): (Option<usize>, usize) = (None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_n = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_n += 1;
            ni = star_n;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

// ---------------------------------------------------------------------------
// Shared backend state
// ---------------------------------------------------------------------------

/// Cloneable, shared server state for the SMB backend.
pub struct Inner {
    shared: Arc<SharedCreds>,
    cache: Arc<FolderCache>,
    root_folder: String,
    root_updated_at: String,
    delete_permanently: bool,
    read_only: bool,
    spool_dir: Option<PathBuf>,
    upload_sem: Option<Arc<tokio::sync::Semaphore>>,
    upload_limit: crate::upload_limit::UploadLimit,
}

impl Inner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shared: Arc<SharedCreds>,
        cache: Arc<FolderCache>,
        root_folder: String,
        root_updated_at: String,
        delete_permanently: bool,
        read_only: bool,
        spool_dir: Option<PathBuf>,
        upload_sem: Option<Arc<tokio::sync::Semaphore>>,
        upload_limit: crate::upload_limit::UploadLimit,
    ) -> Self {
        Inner {
            shared,
            cache,
            root_folder,
            root_updated_at,
            delete_permanently,
            read_only,
            spool_dir,
            upload_sem,
            upload_limit,
        }
    }

    fn creds(&self) -> Arc<Credentials> {
        self.shared.get()
    }

    async fn acquire_upload(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        match &self.upload_sem {
            Some(s) => s.clone().acquire_owned().await.ok(),
            None => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Read handle: sequential streaming reader (ported from the FUSE backend)
// ---------------------------------------------------------------------------

/// A sequential download stream positioned at `pos`, feeding a pipe.
struct ReadStream {
    reader: DuplexStream,
    pos: u64,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for ReadStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Read-only file body: lazily (re)starts a decrypt stream and serves reads
/// from it. Forward gaps up to [`MAX_FORWARD_SKIP`] skip in-stream; a backward
/// read, or a forward gap past the threshold, restarts as a ranged download
/// of only the covering shards.
struct ReadState {
    file_id: String,
    bucket: String,
    mnemonic: String,
    net_user: String,
    net_pass: String,
    size: u64,
    stream: tokio::sync::Mutex<Option<ReadStream>>,
}

impl ReadState {
    fn start_stream(&self, start: u64) -> ReadStream {
        let (mut writer, reader) = tokio::io::duplex(256 * 1024);
        let net = crate::net_client::network_api(&self.net_user, &self.net_pass);
        let mnemonic = self.mnemonic.clone();
        let bucket = self.bucket.clone();
        let file_id = self.file_id.clone();
        let size = self.size;
        let task = tokio::spawn(async move {
            let range = if start == 0 {
                None
            } else {
                Some((start, size.saturating_sub(start)))
            };
            if let Err(e) = internxt_core::transfer::download_file_to_writer(
                &net, &mnemonic, &bucket, &file_id, &mut writer, range,
            )
            .await
            {
                crate::serve::log::warn(&format!("[smb] read stream error: {e:#}"));
            }
            let _ = writer.shutdown().await;
        });
        ReadStream { reader, pos: start, task }
    }

    async fn read_at(&self, offset: u64, len: usize) -> SmbResult<Bytes> {
        if self.file_id.is_empty() || offset >= self.size {
            return Ok(Bytes::new());
        }
        let mut guard = self.stream.lock().await;
        let restart = match &*guard {
            Some(s) => offset < s.pos || offset - s.pos > MAX_FORWARD_SKIP,
            None => true,
        };
        if restart {
            *guard = Some(self.start_stream(offset));
        }
        let stream = guard.as_mut().unwrap();

        // Forward gap: discard bytes already downloaded to reach `offset`.
        if offset > stream.pos {
            let mut to_skip = offset - stream.pos;
            let mut scratch = [0u8; 64 * 1024];
            while to_skip > 0 {
                let want = (to_skip as usize).min(scratch.len());
                let n = stream
                    .reader
                    .read(&mut scratch[..want])
                    .await
                    .map_err(SmbError::Io)?;
                if n == 0 {
                    break;
                }
                to_skip -= n as u64;
                stream.pos += n as u64;
            }
        }

        let cap = (self.size - offset).min(len as u64) as usize;
        let mut buf = vec![0u8; cap];
        let mut filled = 0;
        while filled < cap {
            let n = stream.reader.read(&mut buf[filled..]).await.map_err(SmbError::Io)?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        stream.pos += filled as u64;
        Ok(Bytes::from(buf))
    }
}

// ---------------------------------------------------------------------------
// Write handle: temp-file-backed whole-file model (ported from the FUSE backend)
// ---------------------------------------------------------------------------

struct WriteState {
    inner: Arc<Inner>,
    temp_path: PathBuf,
    file: tokio::sync::Mutex<tokio::fs::File>,
    /// Whether existing Drive content has been pulled into the temp file yet.
    materialized: tokio::sync::Mutex<bool>,
    /// Whether the buffer differs from Drive (needs upload on close).
    dirty: AtomicBool,
    /// Guard against a double upload if `close` runs twice.
    finalized: AtomicBool,
    size: AtomicU64,
    // Upload target.
    parent_uuid: String,
    plain: String,
    ftype: String,
    bucket: String,
    mnemonic: String,
    net_user: String,
    net_pass: String,
    // Existing entry to replace (PUT /files/{uuid}); None for a brand-new file.
    existing_uuid: Mutex<Option<String>>,
    // Source for lazy materialization of existing content.
    base_file_id: Option<String>,
    base_bucket: String,
    base_size: u64,
}

impl WriteState {
    /// Pull the file's existing Drive content into the temp file before a
    /// partial write/read. No-op once done or when there's nothing to pull.
    async fn ensure_materialized(&self) -> SmbResult<()> {
        let mut done = self.materialized.lock().await;
        if *done {
            return Ok(());
        }
        if let Some(fid) = &self.base_file_id {
            if self.base_size > 0 {
                let net = crate::net_client::network_api(&self.net_user, &self.net_pass);
                let mut f = self.file.lock().await;
                f.seek(std::io::SeekFrom::Start(0)).await.map_err(SmbError::Io)?;
                internxt_core::transfer::download_file_to_writer(
                    &net,
                    &self.mnemonic,
                    &self.base_bucket,
                    fid,
                    &mut *f,
                    None,
                )
                .await
                .map_err(to_smb)?;
                f.flush().await.map_err(SmbError::Io)?;
                self.size.store(self.base_size, Ordering::SeqCst);
            }
        }
        *done = true;
        Ok(())
    }

    /// Upload the temp file whole and create/replace the Drive entry. No-op when
    /// nothing was written (and not a freshly-created file).
    async fn finalize(&self) -> SmbResult<()> {
        if self.finalized.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if !self.dirty.load(Ordering::SeqCst) {
            return Ok(());
        }
        {
            let mut f = self.file.lock().await;
            f.flush().await.map_err(SmbError::Io)?;
        }
        let size = self.size.load(Ordering::SeqCst);
        self.inner.upload_limit.check(size).map_err(to_smb)?;

        let creds = self.inner.creds();
        let token = &creds.token;
        let api = DriveApi::for_credentials(&creds);
        let net = crate::net_client::network_api(&self.net_user, &self.net_pass);

        let _permit = self.inner.acquire_upload().await;
        let file_id = if size == 0 {
            String::new()
        } else {
            internxt_core::transfer::upload_file_to_network(
                &net,
                &self.bucket,
                &self.mnemonic,
                &self.temp_path,
                size,
                None,
            )
            .await
            .map_err(to_smb)?
        };
        let now = now_rfc3339();
        // Replace an existing entry in place (keeps uuid/name/folder, swaps
        // fileId+size) — createFileEntry would 409 on the duplicate name.
        let old = self.existing_uuid.lock().unwrap().take();
        let file_uuid = match old {
            Some(old_uuid) => api
                .replace_file(token, &old_uuid, &file_id, size)
                .await
                .map_err(to_smb)?
                .uuid,
            None => api
                .create_file_entry(
                    token,
                    &self.plain,
                    &self.ftype,
                    size,
                    &self.parent_uuid,
                    &file_id,
                    &self.bucket,
                    &now,
                    &now,
                )
                .await
                .map_err(to_smb)?
                .uuid,
        };

        crate::serve::thumbnail::upload_thumbnail_best_effort(
            &net, &api, token, &self.bucket, &self.mnemonic, &file_uuid, &self.ftype,
            &self.temp_path, size, "smb",
        )
        .await;
        // New/updated file must show up immediately for this process's own
        // subsequent list_dir/open, same as folder mutations already do.
        self.inner.cache.invalidate(&self.parent_uuid);
        Ok(())
    }
}

impl Drop for WriteState {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

// ---------------------------------------------------------------------------
// Handles
// ---------------------------------------------------------------------------

enum FileBody {
    Read(ReadState),
    Write(Arc<WriteState>),
}

/// An open file handle (read-streaming or temp-file write).
pub struct FileHandle {
    name: String,
    updated_at: String,
    /// Drive uuid (empty for a not-yet-uploaded new file) → server inode number.
    uuid: String,
    body: FileBody,
}

/// An open directory handle.
pub struct DirHandle {
    inner: Arc<Inner>,
    uuid: String,
    name: String,
    updated_at: String,
}

fn file_info_file(name: String, size: u64, updated_at: &str, uuid: &str) -> FileInfo {
    let ft = rfc3339_to_filetime(updated_at);
    FileInfo {
        name,
        end_of_file: size,
        allocation_size: size,
        creation_time: ft,
        last_access_time: ft,
        last_write_time: ft,
        change_time: ft,
        is_directory: false,
        file_index: stable_ino(uuid),
    }
}

fn file_info_dir(name: String, updated_at: &str, uuid: &str) -> FileInfo {
    let ft = rfc3339_to_filetime(updated_at);
    FileInfo {
        name,
        end_of_file: 0,
        allocation_size: 0,
        creation_time: ft,
        last_access_time: ft,
        last_write_time: ft,
        change_time: ft,
        is_directory: true,
        file_index: stable_ino(uuid),
    }
}

#[async_trait]
impl Handle for FileHandle {
    async fn read(&self, offset: u64, len: u32) -> SmbResult<Bytes> {
        match &self.body {
            FileBody::Read(rs) => rs.read_at(offset, len as usize).await,
            FileBody::Write(ws) => {
                ws.ensure_materialized().await?;
                let mut f = ws.file.lock().await;
                f.seek(std::io::SeekFrom::Start(offset)).await.map_err(SmbError::Io)?;
                let mut buf = vec![0u8; len as usize];
                let mut filled = 0;
                while filled < buf.len() {
                    let n = f.read(&mut buf[filled..]).await.map_err(SmbError::Io)?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                buf.truncate(filled);
                Ok(Bytes::from(buf))
            }
        }
    }

    async fn write(&self, offset: u64, data: &[u8]) -> SmbResult<u32> {
        let ws = match &self.body {
            FileBody::Write(ws) => ws,
            FileBody::Read(_) => return Err(SmbError::AccessDenied),
        };
        // checked_add: a client-supplied offset near u64::MAX combined with any
        // len must not silently wrap to a small value and sail past the size
        // gate below (release builds wrap on overflow instead of panicking).
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| to_smb(anyhow::anyhow!("write offset {offset} + len {} overflows", data.len())))?;
        // Gate by the high-water mark this write would set (final size is only
        // known at close). Over-limit → access denied.
        ws.inner.upload_limit.check(end).map_err(to_smb)?;
        ws.ensure_materialized().await?;
        {
            let mut f = ws.file.lock().await;
            f.seek(std::io::SeekFrom::Start(offset)).await.map_err(SmbError::Io)?;
            f.write_all(data).await.map_err(SmbError::Io)?;
        }
        ws.size.fetch_max(end, Ordering::SeqCst);
        ws.dirty.store(true, Ordering::SeqCst);
        Ok(data.len() as u32)
    }

    async fn flush(&self) -> SmbResult<()> {
        // Uploads happen on close; flush just syncs the temp file.
        if let FileBody::Write(ws) = &self.body {
            let mut f = ws.file.lock().await;
            f.flush().await.map_err(SmbError::Io)?;
        }
        Ok(())
    }

    async fn stat(&self) -> SmbResult<FileInfo> {
        let size = match &self.body {
            FileBody::Read(rs) => rs.size,
            FileBody::Write(ws) => ws.size.load(Ordering::SeqCst),
        };
        Ok(file_info_file(self.name.clone(), size, &self.updated_at, &self.uuid))
    }

    async fn set_times(&self, _times: FileTimes) -> SmbResult<()> {
        // Drive does not support setting arbitrary timestamps; accept silently.
        Ok(())
    }

    async fn truncate(&self, len: u64) -> SmbResult<()> {
        let ws = match &self.body {
            FileBody::Write(ws) => ws,
            FileBody::Read(_) => return Err(SmbError::AccessDenied),
        };
        if len == 0 {
            // Truncate-to-empty: no need to pull the old content first.
            *ws.materialized.lock().await = true;
        } else {
            ws.ensure_materialized().await?;
        }
        {
            let f = ws.file.lock().await;
            f.set_len(len).await.map_err(SmbError::Io)?;
        }
        ws.size.store(len, Ordering::SeqCst);
        ws.dirty.store(true, Ordering::SeqCst);
        Ok(())
    }

    async fn list_dir(&self, _pattern: Option<&str>) -> SmbResult<Vec<DirEntry>> {
        Err(SmbError::NotADirectory)
    }

    async fn close(self: Box<Self>) -> SmbResult<()> {
        if let FileBody::Write(ws) = &self.body {
            ws.finalize().await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Handle for DirHandle {
    async fn read(&self, _offset: u64, _len: u32) -> SmbResult<Bytes> {
        Err(SmbError::IsDirectory)
    }

    async fn write(&self, _offset: u64, _data: &[u8]) -> SmbResult<u32> {
        Err(SmbError::IsDirectory)
    }

    async fn flush(&self) -> SmbResult<()> {
        Ok(())
    }

    async fn stat(&self) -> SmbResult<FileInfo> {
        Ok(file_info_dir(self.name.clone(), &self.updated_at, &self.uuid))
    }

    async fn set_times(&self, _times: FileTimes) -> SmbResult<()> {
        Ok(())
    }

    async fn truncate(&self, _len: u64) -> SmbResult<()> {
        Err(SmbError::IsDirectory)
    }

    async fn list_dir(&self, pattern: Option<&str>) -> SmbResult<Vec<DirEntry>> {
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        let (folders, files) = tokio::try_join!(
            tree::list_folders(&api, &creds.token, &self.uuid, &self.inner.cache),
            tree::list_files_cached(&api, &creds.token, &self.uuid, &self.inner.cache),
        )
        .map_err(to_smb)?;
        let matches = |name: &str| pattern.map(|p| glob_match(p, name)).unwrap_or(true);
        let mut out = Vec::with_capacity(folders.len() + files.len());
        for f in &folders {
            if matches(&f.plain_name) {
                out.push(DirEntry {
                    info: file_info_dir(f.plain_name.clone(), &f.updated_at, &f.uuid),
                });
            }
        }
        for f in &files {
            let name = f.display_name();
            if matches(&name) {
                out.push(DirEntry {
                    info: file_info_file(name, f.size, &f.updated_at, &f.uuid),
                });
            }
        }
        Ok(out)
    }

    async fn close(self: Box<Self>) -> SmbResult<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Backend
// ---------------------------------------------------------------------------

/// The Drive-backed SMB share backend.
pub struct DriveBackend {
    inner: Arc<Inner>,
}

impl DriveBackend {
    pub fn new(inner: Arc<Inner>) -> Self {
        DriveBackend { inner }
    }

    /// Build a read-only streaming handle for an existing file.
    fn open_read(&self, creds: &Credentials, f: FileItem) -> Box<dyn Handle> {
        let bucket = if f.bucket.is_empty() {
            creds.bucket().to_string()
        } else {
            f.bucket.clone()
        };
        let rs = ReadState {
            file_id: f.file_id.clone().unwrap_or_default(),
            bucket,
            mnemonic: creds.mnemonic().to_string(),
            net_user: creds.net_user().to_string(),
            net_pass: creds.net_pass().to_string(),
            size: f.size,
            stream: tokio::sync::Mutex::new(None),
        };
        Box::new(FileHandle {
            name: f.display_name(),
            updated_at: f.updated_at,
            uuid: f.uuid,
            body: FileBody::Read(rs),
        })
    }

    /// Build a temp-file-backed write handle. `existing` = the Drive file being
    /// replaced (None for a brand-new file); `truncate` skips materializing the
    /// old content (the handle starts empty).
    async fn open_write(
        &self,
        creds: &Credentials,
        parent_uuid: &str,
        name: &str,
        existing: Option<FileItem>,
        truncate: bool,
    ) -> SmbResult<Box<dyn Handle>> {
        let temp = temp_path(self.inner.spool_dir.as_deref());
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .await
            .map_err(SmbError::Io)?;
        let (plain, ftype) = split_name(name);
        let brand_new = existing.is_none();
        let (existing_uuid, base_file_id, base_bucket, base_size, updated_at) = match &existing {
            Some(f) => {
                let base_bucket = if f.bucket.is_empty() {
                    creds.bucket().to_string()
                } else {
                    f.bucket.clone()
                };
                (
                    Some(f.uuid.clone()),
                    f.file_id.clone(),
                    base_bucket,
                    f.size,
                    f.updated_at.clone(),
                )
            }
            None => (None, None, creds.bucket().to_string(), 0, now_rfc3339()),
        };
        // Existing file's uuid → stable inode; empty for a brand-new file.
        let handle_uuid = existing_uuid.clone().unwrap_or_default();
        let ws = WriteState {
            inner: self.inner.clone(),
            temp_path: temp,
            file: tokio::sync::Mutex::new(file),
            // Brand-new or truncating opens have nothing to pull.
            materialized: tokio::sync::Mutex::new(brand_new || truncate),
            // Force an (empty) entry for brand-new files; a replace/truncate is
            // only uploaded once actually written to, unless truncating.
            dirty: AtomicBool::new(brand_new || truncate),
            finalized: AtomicBool::new(false),
            size: AtomicU64::new(0),
            parent_uuid: parent_uuid.to_string(),
            plain,
            ftype,
            bucket: creds.bucket().to_string(),
            mnemonic: creds.mnemonic().to_string(),
            net_user: creds.net_user().to_string(),
            net_pass: creds.net_pass().to_string(),
            existing_uuid: Mutex::new(existing_uuid),
            base_file_id,
            base_bucket,
            base_size,
        };
        Ok(Box::new(FileHandle {
            name: name.to_string(),
            updated_at,
            uuid: handle_uuid,
            body: FileBody::Write(Arc::new(ws)),
        }))
    }

    fn dir_handle(&self, uuid: String, name: String, updated_at: String) -> Box<dyn Handle> {
        Box::new(DirHandle {
            inner: self.inner.clone(),
            uuid,
            name,
            updated_at,
        })
    }
}

#[async_trait]
impl ShareBackend for DriveBackend {
    async fn open(&self, path: &SmbPath, opts: OpenOptions) -> SmbResult<Box<dyn Handle>> {
        log(&format!("[OPEN] \\{} {:?}", path.display_backslash(), opts.intent));

        // Root is always the share-root directory.
        if path.is_root() {
            if opts.non_directory {
                return Err(SmbError::IsDirectory);
            }
            return Ok(self.dir_handle(
                self.inner.root_folder.clone(),
                String::new(),
                self.inner.root_updated_at.clone(),
            ));
        }

        let name = path.file_name().unwrap().to_string();
        let parent_path = path.parent().unwrap();
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        let parent = tree::resolve_folder(
            &api,
            &creds.token,
            &self.inner.root_folder,
            &self.inner.root_updated_at,
            parent_path.components(),
            &self.inner.cache,
        )
        .await
        .map_err(to_smb)?
        .ok_or(SmbError::PathNotFound)?;

        let found_folder = tree::find_folder(&api, &creds.token, &parent.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_smb)?;
        let found_file = tree::find_file(&api, &creds.token, &parent.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_smb)?;

        // Directory CREATE (FILE_DIRECTORY_FILE set).
        if opts.directory {
            if found_file.is_some() {
                return Err(SmbError::NotADirectory);
            }
            if let Some(f) = found_folder {
                if opts.intent == OpenIntent::Create {
                    return Err(SmbError::Exists);
                }
                return Ok(self.dir_handle(f.uuid, f.plain_name, f.updated_at));
            }
            // Missing: create it for create-class dispositions.
            if matches!(opts.intent, OpenIntent::Create | OpenIntent::OpenOrCreate) {
                if self.inner.read_only {
                    return Err(SmbError::AccessDenied);
                }
                let created = api
                    .create_folder(&creds.token, &name, &parent.uuid)
                    .await
                    .map_err(to_smb)?;
                self.inner.cache.invalidate(&parent.uuid);
                let uuid = created
                    .get("uuid")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                return Ok(self.dir_handle(uuid, name, now_rfc3339()));
            }
            return Err(SmbError::NotFound);
        }

        // No directory flag: an existing folder (when a file isn't present and
        // a plain file wasn't explicitly required) opens as a directory — e.g.
        // a client opening a folder just to stat/enumerate it.
        if found_file.is_none() {
            if let Some(f) = found_folder {
                if opts.non_directory {
                    return Err(SmbError::IsDirectory);
                }
                if opts.intent == OpenIntent::Create {
                    return Err(SmbError::Exists);
                }
                return Ok(self.dir_handle(f.uuid, f.plain_name, f.updated_at));
            }
        }

        // File dispositions.
        match opts.intent {
            OpenIntent::Open => {
                let f = found_file.ok_or(SmbError::NotFound)?;
                if opts.write && !self.inner.read_only {
                    self.open_write(&creds, &parent.uuid, &name, Some(f), false).await
                } else {
                    Ok(self.open_read(&creds, f))
                }
            }
            OpenIntent::Create => {
                if found_file.is_some() {
                    return Err(SmbError::Exists);
                }
                if self.inner.read_only {
                    return Err(SmbError::AccessDenied);
                }
                self.open_write(&creds, &parent.uuid, &name, None, false).await
            }
            OpenIntent::OpenOrCreate => match found_file {
                Some(f) => {
                    if opts.write && !self.inner.read_only {
                        self.open_write(&creds, &parent.uuid, &name, Some(f), false).await
                    } else {
                        Ok(self.open_read(&creds, f))
                    }
                }
                None => {
                    if self.inner.read_only {
                        return Err(SmbError::AccessDenied);
                    }
                    self.open_write(&creds, &parent.uuid, &name, None, false).await
                }
            },
            OpenIntent::Truncate => {
                let f = found_file.ok_or(SmbError::NotFound)?;
                if self.inner.read_only {
                    return Err(SmbError::AccessDenied);
                }
                self.open_write(&creds, &parent.uuid, &name, Some(f), true).await
            }
            OpenIntent::OverwriteOrCreate => {
                if self.inner.read_only {
                    return Err(SmbError::AccessDenied);
                }
                self.open_write(&creds, &parent.uuid, &name, found_file, true).await
            }
        }
    }

    async fn unlink(&self, path: &SmbPath) -> SmbResult<()> {
        log(&format!("[UNLINK] \\{}", path.display_backslash()));
        if path.is_root() {
            return Err(SmbError::AccessDenied);
        }
        if self.inner.read_only {
            return Err(SmbError::AccessDenied);
        }
        let name = path.file_name().unwrap().to_string();
        let parent_path = path.parent().unwrap();
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        let parent = tree::resolve_folder(
            &api,
            &creds.token,
            &self.inner.root_folder,
            &self.inner.root_updated_at,
            parent_path.components(),
            &self.inner.cache,
        )
        .await
        .map_err(to_smb)?
        .ok_or(SmbError::PathNotFound)?;

        if let Some(f) = tree::find_file(&api, &creds.token, &parent.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_smb)?
        {
            if self.inner.delete_permanently {
                api.delete_file(&creds.token, &f.uuid).await.map_err(to_smb)?;
            } else {
                api.trash_items(&creds.token, json!([{ "uuid": f.uuid, "type": "file" }]))
                    .await
                    .map_err(to_smb)?;
            }
            self.inner.cache.invalidate(&parent.uuid);
            return Ok(());
        }
        if let Some(f) = tree::find_folder(&api, &creds.token, &parent.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_smb)?
        {
            if self.inner.delete_permanently {
                api.delete_folder(&creds.token, &f.uuid).await.map_err(to_smb)?;
            } else {
                api.trash_items(&creds.token, json!([{ "uuid": f.uuid, "type": "folder" }]))
                    .await
                    .map_err(to_smb)?;
            }
            self.inner.cache.invalidate(&parent.uuid);
            self.inner.cache.invalidate(&f.uuid);
            return Ok(());
        }
        Err(SmbError::NotFound)
    }

    async fn rename(&self, from: &SmbPath, to: &SmbPath) -> SmbResult<()> {
        log(&format!(
            "[RENAME] \\{} -> \\{}",
            from.display_backslash(),
            to.display_backslash()
        ));
        if self.inner.read_only {
            return Err(SmbError::AccessDenied);
        }
        if from.is_root() || to.is_root() {
            return Err(SmbError::AccessDenied);
        }
        let src_name = from.file_name().unwrap().to_string();
        let dst_name = to.file_name().unwrap().to_string();
        let src_parent_path = from.parent().unwrap();
        let dst_parent_path = to.parent().unwrap();
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);

        let src_parent = tree::resolve_folder(
            &api,
            &creds.token,
            &self.inner.root_folder,
            &self.inner.root_updated_at,
            src_parent_path.components(),
            &self.inner.cache,
        )
        .await
        .map_err(to_smb)?
        .ok_or(SmbError::PathNotFound)?;
        let dst_parent = tree::resolve_folder(
            &api,
            &creds.token,
            &self.inner.root_folder,
            &self.inner.root_updated_at,
            dst_parent_path.components(),
            &self.inner.cache,
        )
        .await
        .map_err(to_smb)?
        .ok_or(SmbError::PathNotFound)?;

        // Resolve the source item (file first, then folder).
        let src_file = tree::find_file(&api, &creds.token, &src_parent.uuid, &src_name, &self.inner.cache)
            .await
            .map_err(to_smb)?;
        let (uuid, is_folder, cur_name) = if let Some(f) = src_file {
            let cur = f.display_name();
            (f.uuid, false, cur)
        } else {
            match tree::find_folder(&api, &creds.token, &src_parent.uuid, &src_name, &self.inner.cache)
                .await
                .map_err(to_smb)?
            {
                Some(f) => (f.uuid, true, f.plain_name),
                None => return Err(SmbError::NotFound),
            }
        };

        // Reject if the destination name already exists in the target folder.
        let dst_file_clash =
            tree::find_file(&api, &creds.token, &dst_parent.uuid, &dst_name, &self.inner.cache)
                .await
                .map_err(to_smb)?
                .is_some();
        let dst_folder_clash =
            tree::find_folder(&api, &creds.token, &dst_parent.uuid, &dst_name, &self.inner.cache)
                .await
                .map_err(to_smb)?
                .is_some();
        let clashes = dst_file_clash || dst_folder_clash;
        // A no-op rename to the same path is fine; anything else that clashes is not.
        if clashes && !(src_parent.uuid == dst_parent.uuid && cur_name == dst_name) {
            return Err(SmbError::Exists);
        }

        // Move to the new parent when it differs.
        if src_parent.uuid != dst_parent.uuid {
            if is_folder {
                api.move_folder(&creds.token, &uuid, &dst_parent.uuid).await.map_err(to_smb)?;
            } else {
                api.move_file(&creds.token, &uuid, &dst_parent.uuid).await.map_err(to_smb)?;
            }
        }
        // Rename when the final name differs.
        if cur_name != dst_name {
            if is_folder {
                api.rename_folder(&creds.token, &uuid, &dst_name).await.map_err(to_smb)?;
            } else {
                let (plain, ftype) = split_name(&dst_name);
                api.rename_file(&creds.token, &uuid, &plain, &ftype).await.map_err(to_smb)?;
            }
        }
        self.inner.cache.invalidate(&src_parent.uuid);
        self.inner.cache.invalidate(&dst_parent.uuid);
        Ok(())
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            is_read_only: self.inner.read_only,
            case_sensitive: false,
        }
    }
}

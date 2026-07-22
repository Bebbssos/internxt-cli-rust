//! Drive-backed `NFSFileSystem` for the `nfsserve` crate.
//!
//! `nfsserve` owns the NFSv3 wire protocol, mount and portmap; we only implement
//! its storage abstraction over Internxt Drive — the NFS analog of
//! `fuser::Filesystem`. Like FUSE, NFS identifies objects by a stable 64-bit id
//! (`fileid3`), so we keep a two-way inode table (`InodeTable`) interning Drive
//! items lazily on `lookup` / `readdir`. The export root is id 1 (id 0 is
//! reserved).
//!
//! Reads reuse the sequential-streaming reader (forward-skip, backward-seek →
//! ranged download), cached per file id so a run of sequential NFS reads shares
//! one decrypt stream.
//!
//! Writes are the awkward part: NFSv3 has no open/close for data, so there is no
//! `close` on which to finalize an upload. Each written file is buffered to a
//! temp file (existing content materialized lazily) and marked dirty; a
//! background sweeper (`sweep`) uploads the buffer once writes have gone idle,
//! replacing the Drive entry in place, and evicts the buffer once it has been
//! quiet longer still. A final `flush_all` runs on shutdown.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use nfsserve::nfs::{
    fattr3, fileid3, filename3, ftype3, nfspath3, nfsstat3, nfstime3, nfsstring, sattr3, set_size3,
    specdata3,
};
use nfsserve::vfs::{DirEntry, NFSFileSystem, ReadDirResult, VFSCapabilities};
use rand::RngExt;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, DuplexStream};

use internxt_core::api::DriveApi;
use internxt_core::models::Credentials;
use crate::serve::cache::FolderCache;
use crate::serve::creds::SharedCreds;
use crate::serve::tree::{self, FileItem, FolderItem};

/// Export root id (id 0 is reserved by NFS).
const ROOT_ID: fileid3 = 1;
/// Arbitrary constant filesystem id reported in every `fattr3`.
const FSID: u64 = 0x494e_5854; // "INXT"

/// A dirty write buffer is uploaded once no write has touched it for this long.
const FLUSH_IDLE: std::time::Duration = std::time::Duration::from_secs(2);
/// A clean (already-flushed) write buffer is dropped after this much quiet,
/// releasing its temp file; later reads fall back to the streaming reader.
const EVICT_IDLE: std::time::Duration = std::time::Duration::from_secs(30);

/// Delay before the *first* retry after a flush failure. Doubles per
/// additional consecutive failure (see [`retry_backoff`]), so a persistently
/// failing upload (quota exceeded, a file that's too large, a revoked
/// credential, anything non-transient) backs off instead of hammering the
/// Drive API every [`SWEEP_INTERVAL`](super::SWEEP_INTERVAL) tick forever.
const RETRY_BASE: std::time::Duration = std::time::Duration::from_secs(2);
/// Cap on the backoff delay: a long-failing buffer still gets retried at a
/// bounded cadence (never longer than this apart) — it must still eventually
/// succeed once whatever was wrong is fixed (e.g. the user frees up quota),
/// just without the unbounded retry storm.
const RETRY_MAX: std::time::Duration = std::time::Duration::from_secs(60);

/// Delay before retrying a buffer that has failed to flush `fail_count`
/// consecutive times in a row: `0` (no prior failure since the last success)
/// retries as soon as [`FLUSH_IDLE`] allows; otherwise
/// `RETRY_BASE * 2^(fail_count - 1)`, capped at [`RETRY_MAX`]. This only
/// paces *when* the next attempt happens — a dirty buffer is always retried
/// eventually, never given up on.
fn retry_backoff(fail_count: u32) -> std::time::Duration {
    if fail_count == 0 {
        return std::time::Duration::ZERO;
    }
    let shift = fail_count.saturating_sub(1).min(31);
    RETRY_BASE
        .checked_mul(1u32 << shift)
        .map(|d| d.min(RETRY_MAX))
        .unwrap_or(RETRY_MAX)
}

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Map an internal (anyhow) error to an NFS status. Network/API/IO failures all
/// surface as `NFS3ERR_IO`.
fn to_nfs(e: anyhow::Error) -> nfsstat3 {
    crate::serve::log::warn(&format!("[nfs] error: {e:#}"));
    nfsstat3::NFS3ERR_IO
}

fn fname_to_string(f: &filename3) -> String {
    String::from_utf8_lossy(&f.0).to_string()
}

fn string_to_fname(s: &str) -> filename3 {
    nfsstring(s.as_bytes().to_vec())
}

/// RFC3339 → `nfstime3`. Falls back to the epoch on parse failure.
fn rfc3339_to_nfstime(s: &str) -> nfstime3 {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| nfstime3 {
            seconds: dt.timestamp().clamp(0, u32::MAX as i64) as u32,
            nseconds: dt.timestamp_subsec_nanos(),
        })
        .unwrap_or(nfstime3 { seconds: 0, nseconds: 0 })
}

/// Split a filename into (plainName, extension-without-dot). Mirrors the other
/// backends: a leading-dot name like `.env` has no extension.
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
    base.join(format!("internxt-nfs-{}.tmp", hex::encode(rnd)))
}

#[derive(Clone, Copy, PartialEq)]
enum NodeKind {
    Dir,
    File,
}

/// Everything we remember about one interned inode.
#[derive(Clone)]
struct NodeData {
    uuid: String,
    parent: fileid3,
    parent_uuid: String,
    name: String,
    kind: NodeKind,
    size: u64,
    bucket: String,
    file_id: Option<String>,
    file_type: String,
    plain_name: String,
    updated_at: String,
}

#[derive(Default)]
struct InodeTable {
    map: HashMap<fileid3, NodeData>,
    by_parent_name: HashMap<(fileid3, String), fileid3>,
    next_id: fileid3,
}

// ---------------------------------------------------------------------------
// Read handle: sequential streaming reader (ported from the SMB/FUSE backends)
// ---------------------------------------------------------------------------

/// Forward gaps up to this size are skipped by reading-and-discarding from the
/// live stream; past it, the stream restarts as a ranged download instead —
/// see the matching constant in `fuse/fs.rs` for why this matters for MP4
/// playback (moov-atom-at-end probing jumps).
const MAX_FORWARD_SKIP: u64 = 8 * 1024 * 1024;

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

/// Read-only file body, cached per file id: lazily (re)starts a decrypt stream
/// and serves reads from it. Forward gaps up to [`MAX_FORWARD_SKIP`] skip
/// in-stream; a backward read, or a forward gap past the threshold, restarts
/// as a ranged download of only the covering shards.
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
                crate::serve::log::warn(&format!("[nfs] read stream error: {e:#}"));
            }
            let _ = writer.shutdown().await;
        });
        ReadStream { reader, pos: start, task }
    }

    async fn read_at(&self, offset: u64, len: usize) -> anyhow::Result<Vec<u8>> {
        if self.file_id.is_empty() || offset >= self.size {
            return Ok(Vec::new());
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

        if offset > stream.pos {
            let mut to_skip = offset - stream.pos;
            let mut scratch = [0u8; 64 * 1024];
            while to_skip > 0 {
                let want = (to_skip as usize).min(scratch.len());
                let n = stream.reader.read(&mut scratch[..want]).await?;
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
            let n = stream.reader.read(&mut buf[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        stream.pos += filled as u64;
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// Write buffer: temp-file whole-file model with idle upload (no NFS close)
// ---------------------------------------------------------------------------

struct WriteBuffer {
    fileid: fileid3,
    temp_path: PathBuf,
    file: tokio::sync::Mutex<tokio::fs::File>,
    materialized: tokio::sync::Mutex<bool>,
    /// One flush at a time per buffer.
    flush_lock: tokio::sync::Mutex<()>,
    dirty: AtomicBool,
    size: AtomicU64,
    /// Bumped every time `write_at`/`setattr` mutates the temp file, always
    /// under the `file` lock alongside the mutation it accompanies. A flush
    /// snapshots this before uploading and compares it after: if it moved, a
    /// write landed while the (path-based, out-of-band) upload was reading the
    /// file, so the just-uploaded bytes may be stale or torn and `dirty` must
    /// NOT be cleared — see `clear_dirty_if_unchanged`.
    write_gen: AtomicU64,
    last_write: Mutex<Instant>,
    /// Consecutive flush failures since the last success (or since creation).
    /// Reset to 0 on a successful flush; drives [`retry_backoff`] so a
    /// persistently-failing buffer is retried with growing gaps instead of
    /// every sweep tick — see `sweep`'s dirty branch.
    fail_count: AtomicU32,
    /// When the most recent flush *attempt* (successful or not) started.
    /// Compared against `retry_backoff(fail_count)` in `sweep` to decide
    /// whether enough time has passed to retry.
    last_attempt: Mutex<Instant>,
    /// Existing Drive entry to replace in place (uuid stays, fileId+size swap).
    /// `None` = a not-yet-created file: the first flush creates the entry (with
    /// content) and stores its uuid here, so later flushes replace. Deferring the
    /// create avoids a 0-byte `createFileEntry`, which free/legacy plans reject
    /// (HTTP 402 "You can not have empty files").
    existing_uuid: Mutex<Option<String>>,
    /// Upload target for the create path (when `existing_uuid` is `None`).
    parent_uuid: String,
    plain: String,
    ftype: String,
    bucket: String,
    mnemonic: String,
    net_user: String,
    net_pass: String,
    base_file_id: Option<String>,
    base_bucket: String,
    base_size: u64,
}

impl WriteBuffer {
    async fn ensure_materialized(&self) -> anyhow::Result<()> {
        let mut done = self.materialized.lock().await;
        if *done {
            return Ok(());
        }
        if let Some(fid) = &self.base_file_id {
            if self.base_size > 0 {
                let net = crate::net_client::network_api(&self.net_user, &self.net_pass);
                let mut f = self.file.lock().await;
                f.seek(std::io::SeekFrom::Start(0)).await?;
                internxt_core::transfer::download_file_to_writer(
                    &net,
                    &self.mnemonic,
                    &self.base_bucket,
                    fid,
                    &mut *f,
                    None,
                )
                .await?;
                f.flush().await?;
                self.size.store(self.base_size, Ordering::SeqCst);
            }
        }
        *done = true;
        Ok(())
    }

    async fn write_at(&self, offset: u64, data: &[u8]) -> anyhow::Result<()> {
        self.ensure_materialized().await?;
        {
            let mut f = self.file.lock().await;
            f.seek(std::io::SeekFrom::Start(offset)).await?;
            f.write_all(data).await?;
            // Bump size/gen while still holding the file lock so a concurrent
            // flush that takes this same lock (see `flush_writer`) observes a
            // consistent (bytes, size, gen) snapshot — never a gen that has
            // already moved past a size/byte update it hasn't seen yet.
            self.size.fetch_max(offset + data.len() as u64, Ordering::SeqCst);
            self.write_gen.fetch_add(1, Ordering::SeqCst);
        }
        self.dirty.store(true, Ordering::SeqCst);
        *self.last_write.lock().unwrap() = Instant::now();
        Ok(())
    }

    /// True if no write has landed since `gen_before` (a snapshot taken by
    /// `flush_writer` before it started the out-of-band upload read). Only in
    /// that case is it safe to clear `dirty` — otherwise a write happened
    /// while the upload was reading the file, the just-uploaded bytes may be
    /// stale/torn, and `dirty` must stay set so the next sweep re-flushes.
    fn clear_dirty_if_unchanged(&self, gen_before: u64) -> bool {
        if self.write_gen.load(Ordering::SeqCst) == gen_before {
            self.dirty.store(false, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    /// Record a failed flush attempt: bumps the consecutive-failure count and
    /// stamps the attempt time, so `sweep` backs off before retrying (see
    /// [`retry_backoff`]). Called on every `flush_writer` error path that
    /// leaves `dirty` set.
    fn record_flush_failure(&self) {
        self.fail_count.fetch_add(1, Ordering::SeqCst);
        *self.last_attempt.lock().unwrap() = Instant::now();
    }

    /// Record a successful flush: clears the failure count so a later failure
    /// (e.g. after the buffer is written to again) starts backing off from
    /// scratch rather than compounding on old failures.
    fn record_flush_success(&self) {
        self.fail_count.store(0, Ordering::SeqCst);
        *self.last_attempt.lock().unwrap() = Instant::now();
    }

    /// Record a failed flush attempt and log it, including the running
    /// failure count and the backoff before the next retry — so an operator
    /// watching the logs can see the retry pacing (e.g. "next retry in 32s")
    /// instead of a wall of identical one-second-apart lines.
    fn log_flush_failure(&self, msg: &str) {
        self.record_flush_failure();
        let fail_count = self.fail_count.load(Ordering::SeqCst);
        let backoff = retry_backoff(fail_count);
        crate::serve::log::warn(&format!(
            "{msg} (fileid={}, consecutive failures={fail_count}, next retry in {}s)",
            self.fileid,
            backoff.as_secs()
        ));
    }

    async fn read_at(&self, offset: u64, len: usize) -> anyhow::Result<Vec<u8>> {
        self.ensure_materialized().await?;
        let size = self.size.load(Ordering::SeqCst);
        if offset >= size {
            return Ok(Vec::new());
        }
        let mut f = self.file.lock().await;
        f.seek(std::io::SeekFrom::Start(offset)).await?;
        let cap = (size - offset).min(len as u64) as usize;
        let mut buf = vec![0u8; cap];
        let mut filled = 0;
        while filled < cap {
            let n = f.read(&mut buf[filled..]).await?;
            if n == 0 {
                break;
            }
            filled += n;
        }
        buf.truncate(filled);
        Ok(buf)
    }
}

impl Drop for WriteBuffer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

// ---------------------------------------------------------------------------
// Shared backend state
// ---------------------------------------------------------------------------

pub struct Inner {
    shared: Arc<SharedCreds>,
    cache: Arc<FolderCache>,
    delete_permanently: bool,
    read_only: bool,
    spool_dir: Option<PathBuf>,
    upload_sem: Option<Arc<tokio::sync::Semaphore>>,
    upload_limit: crate::upload_limit::UploadLimit,
    inodes: Mutex<InodeTable>,
    readers: Mutex<HashMap<fileid3, Arc<ReadState>>>,
    writers: Mutex<HashMap<fileid3, Arc<WriteBuffer>>>,
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
        let mut table = InodeTable {
            next_id: 2,
            ..Default::default()
        };
        table.map.insert(
            ROOT_ID,
            NodeData {
                uuid: root_folder.clone(),
                parent: ROOT_ID,
                parent_uuid: root_folder,
                name: String::new(),
                kind: NodeKind::Dir,
                size: 0,
                bucket: String::new(),
                file_id: None,
                file_type: String::new(),
                plain_name: String::new(),
                updated_at: root_updated_at,
            },
        );
        Inner {
            shared,
            cache,
            delete_permanently,
            read_only,
            spool_dir,
            upload_sem,
            upload_limit,
            inodes: Mutex::new(table),
            readers: Mutex::new(HashMap::new()),
            writers: Mutex::new(HashMap::new()),
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

    fn node(&self, id: fileid3) -> Option<NodeData> {
        self.inodes.lock().unwrap().map.get(&id).cloned()
    }

    fn writer(&self, id: fileid3) -> Option<Arc<WriteBuffer>> {
        self.writers.lock().unwrap().get(&id).cloned()
    }

    /// Intern (or refresh) a child under `parent`, returning its stable id.
    fn intern(&self, parent: fileid3, mut nd: NodeData) -> fileid3 {
        nd.parent = parent;
        let mut t = self.inodes.lock().unwrap();
        let key = (parent, nd.name.clone());
        if let Some(&id) = t.by_parent_name.get(&key) {
            if let Some(existing) = t.map.get_mut(&id) {
                existing.uuid = nd.uuid;
                existing.kind = nd.kind;
                existing.size = nd.size;
                existing.bucket = nd.bucket;
                existing.file_id = nd.file_id;
                existing.file_type = nd.file_type;
                existing.plain_name = nd.plain_name;
                existing.updated_at = nd.updated_at;
                existing.parent_uuid = nd.parent_uuid;
            }
            return id;
        }
        let id = t.next_id;
        t.next_id += 1;
        t.map.insert(id, nd);
        t.by_parent_name.insert(key, id);
        id
    }

    fn remove_node(&self, id: fileid3) {
        let mut t = self.inodes.lock().unwrap();
        if let Some(nd) = t.map.remove(&id) {
            t.by_parent_name.remove(&(nd.parent, nd.name));
        }
        drop(t);
        self.readers.lock().unwrap().remove(&id);
        self.writers.lock().unwrap().remove(&id);
    }

    fn set_node_size(&self, id: fileid3, size: u64) {
        if let Some(nd) = self.inodes.lock().unwrap().map.get_mut(&id) {
            nd.size = size;
        }
    }

    /// Set a node's Drive uuid (after its entry is created on first flush).
    fn set_node_uuid(&self, id: fileid3, uuid: &str) {
        if let Some(nd) = self.inodes.lock().unwrap().map.get_mut(&id) {
            nd.uuid = uuid.to_string();
        }
    }

    /// Update a file node after a flush (new fileId + size).
    fn update_file_node(&self, id: fileid3, file_id: String, size: u64, bucket: String) {
        if let Some(nd) = self.inodes.lock().unwrap().map.get_mut(&id) {
            nd.file_id = if file_id.is_empty() { None } else { Some(file_id) };
            nd.size = size;
            nd.bucket = bucket;
            nd.updated_at = now_rfc3339();
        }
    }

    fn node_from_folder(&self, parent: fileid3, parent_uuid: &str, f: &FolderItem) -> NodeData {
        NodeData {
            uuid: f.uuid.clone(),
            parent,
            parent_uuid: parent_uuid.to_string(),
            name: f.plain_name.clone(),
            kind: NodeKind::Dir,
            size: 0,
            bucket: String::new(),
            file_id: None,
            file_type: String::new(),
            plain_name: f.plain_name.clone(),
            updated_at: f.updated_at.clone(),
        }
    }

    fn node_from_file(&self, parent: fileid3, parent_uuid: &str, f: &FileItem) -> NodeData {
        NodeData {
            uuid: f.uuid.clone(),
            parent,
            parent_uuid: parent_uuid.to_string(),
            name: f.display_name(),
            kind: NodeKind::File,
            size: f.size,
            bucket: f.bucket.clone(),
            file_id: f.file_id.clone(),
            file_type: f.file_type.clone(),
            plain_name: f.plain_name.clone(),
            updated_at: f.updated_at.clone(),
        }
    }

    fn make_fattr(&self, id: fileid3, kind: NodeKind, size: u64, updated_at: &str) -> fattr3 {
        let t = rfc3339_to_nfstime(updated_at);
        let (ftype, mode, nlink) = match kind {
            NodeKind::Dir => (ftype3::NF3DIR, 0o777, 2),
            NodeKind::File => (ftype3::NF3REG, 0o666, 1),
        };
        fattr3 {
            ftype,
            mode,
            nlink,
            uid: 0,
            gid: 0,
            size,
            used: size,
            rdev: specdata3 { specdata1: 0, specdata2: 0 },
            fsid: FSID,
            fileid: id,
            atime: t,
            mtime: t,
            ctime: t,
        }
    }

    /// The effective size of a node (buffered size if a write buffer exists).
    fn effective_size(&self, id: fileid3, node: &NodeData) -> u64 {
        self.writer(id)
            .map(|wb| wb.size.load(Ordering::SeqCst))
            .unwrap_or(node.size)
    }

    /// Cached streaming reader for a file id (built from `node` on first use).
    fn reader_for(&self, id: fileid3, node: &NodeData) -> Arc<ReadState> {
        if let Some(rs) = self.readers.lock().unwrap().get(&id) {
            return rs.clone();
        }
        let creds = self.creds();
        let bucket = if node.bucket.is_empty() {
            creds.bucket().to_string()
        } else {
            node.bucket.clone()
        };
        let rs = Arc::new(ReadState {
            file_id: node.file_id.clone().unwrap_or_default(),
            bucket,
            mnemonic: creds.mnemonic().to_string(),
            net_user: creds.net_user().to_string(),
            net_pass: creds.net_pass().to_string(),
            size: node.size,
            stream: tokio::sync::Mutex::new(None),
        });
        self.readers
            .lock()
            .unwrap()
            .entry(id)
            .or_insert(rs)
            .clone()
    }

    /// Get (or lazily create) the write buffer for a file id.
    async fn writer_for(&self, id: fileid3, node: &NodeData) -> Result<Arc<WriteBuffer>, nfsstat3> {
        if let Some(wb) = self.writer(id) {
            return Ok(wb);
        }
        let creds = self.creds();
        let temp = temp_path(self.spool_dir.as_deref());
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .await
            .map_err(|_| nfsstat3::NFS3ERR_IO)?;
        let base_bucket = if node.bucket.is_empty() {
            creds.bucket().to_string()
        } else {
            node.bucket.clone()
        };
        let existing_uuid = if node.uuid.is_empty() {
            None
        } else {
            Some(node.uuid.clone())
        };
        let wb = Arc::new(WriteBuffer {
            fileid: id,
            temp_path: temp,
            file: tokio::sync::Mutex::new(file),
            materialized: tokio::sync::Mutex::new(false),
            flush_lock: tokio::sync::Mutex::new(()),
            dirty: AtomicBool::new(false),
            size: AtomicU64::new(node.size),
            write_gen: AtomicU64::new(0),
            last_write: Mutex::new(Instant::now()),
            fail_count: AtomicU32::new(0),
            last_attempt: Mutex::new(Instant::now()),
            existing_uuid: Mutex::new(existing_uuid),
            parent_uuid: node.parent_uuid.clone(),
            plain: node.plain_name.clone(),
            ftype: node.file_type.clone(),
            bucket: creds.bucket().to_string(),
            mnemonic: creds.mnemonic().to_string(),
            net_user: creds.net_user().to_string(),
            net_pass: creds.net_pass().to_string(),
            base_file_id: node.file_id.clone(),
            base_bucket,
            base_size: node.size,
        });
        Ok(self.writers.lock().unwrap().entry(id).or_insert(wb).clone())
    }

    /// Upload a dirty buffer and replace the Drive entry in place. Best-effort:
    /// on error the buffer stays dirty and is retried on the next sweep.
    async fn flush_writer(&self, wb: Arc<WriteBuffer>) {
        let _g = wb.flush_lock.lock().await;
        if !wb.dirty.load(Ordering::SeqCst) {
            return;
        }
        // Snapshot (gen, size) together, under the same lock `write_at` holds
        // while it mutates the file, so this pairing is consistent with the
        // bytes on disk at this instant. The upload below reads the temp file
        // by path through a separate handle (not this lock) — `gen_before` is
        // how we detect if a write raced that out-of-band read.
        let (gen_before, size) = {
            let mut f = wb.file.lock().await;
            if let Err(e) = f.flush().await {
                wb.log_flush_failure(&format!("[nfs] flush temp failed: {e}"));
                return;
            }
            (wb.write_gen.load(Ordering::SeqCst), wb.size.load(Ordering::SeqCst))
        };
        let existing = wb.existing_uuid.lock().unwrap().clone();
        // A not-yet-created file with no content: don't call createFileEntry with
        // a 0-byte body (free/legacy plans reject empty files with HTTP 402). Stop
        // retrying; if it's later written, `dirty` flips back on and it uploads.
        // Not a failure (nothing was attempted), so clear any stale backoff too.
        if size == 0 && existing.is_none() {
            wb.clear_dirty_if_unchanged(gen_before);
            wb.fail_count.store(0, Ordering::SeqCst);
            return;
        }
        if let Err(e) = self.upload_limit.check(size) {
            crate::serve::log::warn(&format!("[nfs] upload rejected: {e:#}"));
            // Too big to ever upload; drop the dirty flag so we stop retrying.
            // Not a transient failure, so clear the backoff state too — a
            // later write that shrinks the file back under the limit should
            // get an immediate first attempt, not an inherited backoff.
            wb.dirty.store(false, Ordering::SeqCst);
            wb.fail_count.store(0, Ordering::SeqCst);
            return;
        }
        let creds = self.creds();
        let token = &creds.token;
        let api = DriveApi::for_credentials(&creds);
        let net = crate::net_client::network_api(&wb.net_user, &wb.net_pass);

        let _permit = self.acquire_upload().await;
        let file_id = if size == 0 {
            String::new()
        } else {
            match internxt_core::transfer::upload_file_to_network(
                &net,
                &wb.bucket,
                &wb.mnemonic,
                &wb.temp_path,
                size,
                None,
            )
            .await
            {
                Ok(v) => v,
                Err(e) => {
                    wb.log_flush_failure(&format!("[nfs] upload failed: {e:#}"));
                    return;
                }
            }
        };
        let now = now_rfc3339();
        let file_uuid = match existing {
            // Replace an existing Drive entry in place (keeps uuid/name/folder).
            Some(uuid) => {
                if let Err(e) = api.replace_file(token, &uuid, &file_id, size).await {
                    wb.log_flush_failure(&format!("[nfs] replace_file failed: {e:#}"));
                    return;
                }
                // Cached listing would otherwise keep serving the old size.
                self.cache.invalidate(&wb.parent_uuid);
                uuid
            }
            // First flush of a new file: create the entry now (with content), then
            // remember its uuid so subsequent flushes replace instead of re-create.
            None => match api
                .create_file_entry(
                    token,
                    &wb.plain,
                    &wb.ftype,
                    size,
                    &wb.parent_uuid,
                    &file_id,
                    &wb.bucket,
                    &now,
                    &now,
                )
                .await
            {
                Ok(created) => {
                    *wb.existing_uuid.lock().unwrap() = Some(created.uuid.clone());
                    self.set_node_uuid(wb.fileid, &created.uuid);
                    self.cache.invalidate(&wb.parent_uuid);
                    created.uuid
                }
                Err(e) => {
                    wb.log_flush_failure(&format!("[nfs] createFileEntry failed: {e:#}"));
                    return;
                }
            },
        };

        crate::serve::thumbnail::upload_thumbnail_best_effort(
            &net, &api, token, &wb.bucket, &wb.mnemonic, &file_uuid, &wb.ftype, &wb.temp_path,
            size, "nfs",
        )
        .await;
        // Bookkeeping (uuid/file_id/size on the Drive-side node) is always kept
        // in sync with what was actually uploaded, even if a write raced the
        // upload — the entry really was created/replaced with these values.
        // But `dirty` is only cleared if nothing wrote to the buffer while the
        // upload was reading it; otherwise clearing it here would silently
        // drop the racing write (it set `dirty = true` but this unconditional
        // clear used to stomp it right back to `false`, and the buffer's
        // eventual eviction discarded the never-uploaded bytes for good).
        // Leaving `dirty = true` means the next sweep re-flushes with the
        // buffer's current (post-race) content.
        let cleared = wb.clear_dirty_if_unchanged(gen_before);
        // The upload itself succeeded (bytes landed on Drive) regardless of
        // whether a race left `dirty` set again — reset the failure streak so
        // a future failure starts backing off from scratch.
        wb.record_flush_success();
        self.update_file_node(wb.fileid, file_id, size, wb.bucket.clone());
        self.readers.lock().unwrap().remove(&wb.fileid);
        if cleared {
            crate::serve::log::trace(&format!(
                "[nfs] [UPLOAD] fileid={} ({size} bytes) done",
                wb.fileid
            ));
        } else {
            crate::serve::log::trace(&format!(
                "[nfs] [UPLOAD] fileid={} ({size} bytes) done, but written to \
                 again during upload; re-flushing on next sweep",
                wb.fileid
            ));
        }
    }

    /// One sweep: flush idle-dirty buffers, evict long-quiet clean ones.
    pub async fn sweep(&self) {
        let snapshot: Vec<(fileid3, Arc<WriteBuffer>)> = self
            .writers
            .lock()
            .unwrap()
            .iter()
            .map(|(id, wb)| (*id, wb.clone()))
            .collect();
        let now = Instant::now();
        for (id, wb) in snapshot {
            let idle = now.duration_since(*wb.last_write.lock().unwrap());
            if wb.dirty.load(Ordering::SeqCst) {
                if idle < FLUSH_IDLE {
                    continue;
                }
                // Beyond the idle threshold, a buffer with no failure history
                // is flushed on this tick as before. One that has been
                // failing repeatedly (quota exceeded, revoked credential,
                // ...) is only retried once its backoff has elapsed, so a
                // permanently-failing upload backs off instead of hitting the
                // Drive API (and the log) every SWEEP_INTERVAL tick forever —
                // it is still retried, just less often the longer it fails.
                let fail_count = wb.fail_count.load(Ordering::SeqCst);
                if fail_count > 0 {
                    let since_attempt = now.duration_since(*wb.last_attempt.lock().unwrap());
                    if since_attempt < retry_backoff(fail_count) {
                        continue;
                    }
                }
                self.flush_writer(wb).await;
            } else if idle >= EVICT_IDLE {
                self.writers.lock().unwrap().remove(&id);
            }
        }
    }

    /// Flush every dirty buffer (called on shutdown).
    pub async fn flush_all(&self) {
        let snapshot: Vec<Arc<WriteBuffer>> = self
            .writers
            .lock()
            .unwrap()
            .values()
            .filter(|wb| wb.dirty.load(Ordering::SeqCst))
            .cloned()
            .collect();
        for wb in snapshot {
            self.flush_writer(wb).await;
        }
    }
}

// ---------------------------------------------------------------------------
// NFSFileSystem
// ---------------------------------------------------------------------------

/// The Drive-backed NFSv3 filesystem handed to `nfsserve`.
pub struct DriveNfs {
    inner: Arc<Inner>,
}

impl DriveNfs {
    pub fn new(inner: Arc<Inner>) -> Self {
        DriveNfs { inner }
    }
}

#[async_trait]
impl NFSFileSystem for DriveNfs {
    fn capabilities(&self) -> VFSCapabilities {
        if self.inner.read_only {
            VFSCapabilities::ReadOnly
        } else {
            VFSCapabilities::ReadWrite
        }
    }

    fn root_dir(&self) -> fileid3 {
        ROOT_ID
    }

    async fn lookup(&self, dirid: fileid3, filename: &filename3) -> Result<fileid3, nfsstat3> {
        let name = fname_to_string(filename);
        if name == "." {
            return Ok(dirid);
        }
        let dir = self.inner.node(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
        if dir.kind != NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        if name == ".." {
            return Ok(dir.parent);
        }
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        // Files first (cached), then folders (cached).
        if let Some(f) = tree::find_file(&api, &creds.token, &dir.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_nfs)?
        {
            let nd = self.inner.node_from_file(dirid, &dir.uuid, &f);
            return Ok(self.inner.intern(dirid, nd));
        }
        if let Some(f) = tree::find_folder(&api, &creds.token, &dir.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_nfs)?
        {
            let nd = self.inner.node_from_folder(dirid, &dir.uuid, &f);
            return Ok(self.inner.intern(dirid, nd));
        }
        Err(nfsstat3::NFS3ERR_NOENT)
    }

    async fn getattr(&self, id: fileid3) -> Result<fattr3, nfsstat3> {
        let node = self.inner.node(id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let size = self.inner.effective_size(id, &node);
        Ok(self.inner.make_fattr(id, node.kind, size, &node.updated_at))
    }

    async fn setattr(&self, id: fileid3, setattr: sattr3) -> Result<fattr3, nfsstat3> {
        if self.inner.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let node = self.inner.node(id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        // Only a size change (truncate/extend) is meaningful for Drive; other
        // attributes (mode/owner/times) are accepted silently.
        if let set_size3::size(sz) = setattr.size {
            if node.kind != NodeKind::File {
                return Err(nfsstat3::NFS3ERR_ISDIR);
            }
            let wb = self.inner.writer_for(id, &node).await?;
            if sz == 0 {
                *wb.materialized.lock().await = true;
            } else {
                wb.ensure_materialized().await.map_err(to_nfs)?;
            }
            {
                let f = wb.file.lock().await;
                f.set_len(sz).await.map_err(|_| nfsstat3::NFS3ERR_IO)?;
                // size + write_gen move together under the file lock, same as
                // `write_at`, so a concurrent flush's snapshot stays consistent.
                wb.size.store(sz, Ordering::SeqCst);
                wb.write_gen.fetch_add(1, Ordering::SeqCst);
            }
            wb.dirty.store(true, Ordering::SeqCst);
            *wb.last_write.lock().unwrap() = Instant::now();
            self.inner.set_node_size(id, sz);
            self.inner.readers.lock().unwrap().remove(&id);
        }
        let size = self.inner.effective_size(id, &node);
        Ok(self.inner.make_fattr(id, node.kind, size, &node.updated_at))
    }

    async fn read(&self, id: fileid3, offset: u64, count: u32) -> Result<(Vec<u8>, bool), nfsstat3> {
        let node = self.inner.node(id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        if node.kind != NodeKind::File {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        // A live write buffer is the freshest content; read from it.
        if let Some(wb) = self.inner.writer(id) {
            let size = wb.size.load(Ordering::SeqCst);
            let data = wb.read_at(offset, count as usize).await.map_err(to_nfs)?;
            let eof = offset + data.len() as u64 >= size;
            return Ok((data, eof));
        }
        let rs = self.inner.reader_for(id, &node);
        let data = rs.read_at(offset, count as usize).await.map_err(to_nfs)?;
        let eof = offset + data.len() as u64 >= rs.size;
        Ok((data, eof))
    }

    async fn write(&self, id: fileid3, offset: u64, data: &[u8]) -> Result<fattr3, nfsstat3> {
        if self.inner.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let node = self.inner.node(id).ok_or(nfsstat3::NFS3ERR_STALE)?;
        if node.kind != NodeKind::File {
            return Err(nfsstat3::NFS3ERR_ISDIR);
        }
        // Gate by the high-water mark this write would set. checked_add: a
        // client-supplied offset near u64::MAX combined with any len must not
        // silently wrap to a small value and sail past the size gate (release
        // builds wrap on overflow instead of panicking).
        let end = offset.checked_add(data.len() as u64).ok_or(nfsstat3::NFS3ERR_FBIG)?;
        if self.inner.upload_limit.check(end).is_err() {
            return Err(nfsstat3::NFS3ERR_FBIG);
        }
        let wb = self.inner.writer_for(id, &node).await?;
        wb.write_at(offset, data).await.map_err(to_nfs)?;
        let size = wb.size.load(Ordering::SeqCst);
        self.inner.set_node_size(id, size);
        self.inner.readers.lock().unwrap().remove(&id);
        Ok(self.inner.make_fattr(id, NodeKind::File, size, &node.updated_at))
    }

    async fn create(
        &self,
        dirid: fileid3,
        filename: &filename3,
        _attr: sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        if self.inner.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = fname_to_string(filename);
        let dir = self.inner.node(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);

        // An existing file of that name is returned as-is (UNCHECKED create).
        if let Some(f) = tree::find_file(&api, &creds.token, &dir.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_nfs)?
        {
            let nd = self.inner.node_from_file(dirid, &dir.uuid, &f);
            let id = self.inner.intern(dirid, nd);
            return Ok((id, self.inner.make_fattr(id, NodeKind::File, f.size, &f.updated_at)));
        }

        // Intern a pending node only — the Drive entry is created on first flush,
        // with content, so we never POST a 0-byte file (free/legacy plans 402 on
        // empty files). A file created but never written just never persists.
        let (plain, ftype) = split_name(&name);
        let now = now_rfc3339();
        let nd = NodeData {
            uuid: String::new(),
            parent: dirid,
            parent_uuid: dir.uuid.clone(),
            name: name.clone(),
            kind: NodeKind::File,
            size: 0,
            bucket: creds.bucket().to_string(),
            file_id: None,
            file_type: ftype,
            plain_name: plain,
            updated_at: now.clone(),
        };
        let id = self.inner.intern(dirid, nd);
        Ok((id, self.inner.make_fattr(id, NodeKind::File, 0, &now)))
    }

    async fn create_exclusive(
        &self,
        dirid: fileid3,
        filename: &filename3,
    ) -> Result<fileid3, nfsstat3> {
        if self.inner.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = fname_to_string(filename);
        let dir = self.inner.node(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        if tree::find_file(&api, &creds.token, &dir.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_nfs)?
            .is_some()
        {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }
        // Pending node only; the Drive entry is created on first flush (see `create`).
        let (plain, ftype) = split_name(&name);
        let now = now_rfc3339();
        let nd = NodeData {
            uuid: String::new(),
            parent: dirid,
            parent_uuid: dir.uuid.clone(),
            name: name.clone(),
            kind: NodeKind::File,
            size: 0,
            bucket: creds.bucket().to_string(),
            file_id: None,
            file_type: ftype,
            plain_name: plain,
            updated_at: now,
        };
        Ok(self.inner.intern(dirid, nd))
    }

    async fn mkdir(
        &self,
        dirid: fileid3,
        dirname: &filename3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        if self.inner.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = fname_to_string(dirname);
        let dir = self.inner.node(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        let existing = tree::find_folder(&api, &creds.token, &dir.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_nfs)?;
        if existing.is_some() {
            return Err(nfsstat3::NFS3ERR_EXIST);
        }
        let created = api
            .create_folder(&creds.token, &name, &dir.uuid)
            .await
            .map_err(to_nfs)?;
        self.inner.cache.invalidate(&dir.uuid);
        let uuid = created
            .get("uuid")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let now = now_rfc3339();
        let nd = NodeData {
            uuid,
            parent: dirid,
            parent_uuid: dir.uuid.clone(),
            name: name.clone(),
            kind: NodeKind::Dir,
            size: 0,
            bucket: String::new(),
            file_id: None,
            file_type: String::new(),
            plain_name: name,
            updated_at: now.clone(),
        };
        let id = self.inner.intern(dirid, nd);
        Ok((id, self.inner.make_fattr(id, NodeKind::Dir, 0, &now)))
    }

    async fn remove(&self, dirid: fileid3, filename: &filename3) -> Result<(), nfsstat3> {
        if self.inner.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let name = fname_to_string(filename);
        let dir = self.inner.node(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);

        if let Some(f) = tree::find_file(&api, &creds.token, &dir.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_nfs)?
        {
            if self.inner.delete_permanently {
                api.delete_file(&creds.token, &f.uuid).await.map_err(to_nfs)?;
            } else {
                api.trash_items(&creds.token, json!([{ "uuid": f.uuid, "type": "file" }]))
                    .await
                    .map_err(to_nfs)?;
            }
            self.inner.cache.invalidate(&dir.uuid);
            self.remove_child(dirid, &name);
            return Ok(());
        }
        if let Some(f) = tree::find_folder(&api, &creds.token, &dir.uuid, &name, &self.inner.cache)
            .await
            .map_err(to_nfs)?
        {
            if self.inner.delete_permanently {
                api.delete_folder(&creds.token, &f.uuid).await.map_err(to_nfs)?;
            } else {
                api.trash_items(&creds.token, json!([{ "uuid": f.uuid, "type": "folder" }]))
                    .await
                    .map_err(to_nfs)?;
            }
            self.inner.cache.invalidate(&dir.uuid);
            self.inner.cache.invalidate(&f.uuid);
            self.remove_child(dirid, &name);
            return Ok(());
        }
        Err(nfsstat3::NFS3ERR_NOENT)
    }

    async fn rename(
        &self,
        from_dirid: fileid3,
        from_filename: &filename3,
        to_dirid: fileid3,
        to_filename: &filename3,
    ) -> Result<(), nfsstat3> {
        if self.inner.read_only {
            return Err(nfsstat3::NFS3ERR_ROFS);
        }
        let src_name = fname_to_string(from_filename);
        let dst_name = fname_to_string(to_filename);
        let src_dir = self.inner.node(from_dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let dst_dir = self.inner.node(to_dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);

        // Resolve the source (file first, then folder).
        let src_file = tree::find_file(&api, &creds.token, &src_dir.uuid, &src_name, &self.inner.cache)
            .await
            .map_err(to_nfs)?;
        let (uuid, is_folder, cur_name) = if let Some(f) = src_file {
            let cur = f.display_name();
            (f.uuid, false, cur)
        } else {
            match tree::find_folder(&api, &creds.token, &src_dir.uuid, &src_name, &self.inner.cache)
                .await
                .map_err(to_nfs)?
            {
                Some(f) => (f.uuid, true, f.plain_name),
                None => return Err(nfsstat3::NFS3ERR_NOENT),
            }
        };

        if src_dir.uuid != dst_dir.uuid {
            if is_folder {
                api.move_folder(&creds.token, &uuid, &dst_dir.uuid).await.map_err(to_nfs)?;
            } else {
                api.move_file(&creds.token, &uuid, &dst_dir.uuid).await.map_err(to_nfs)?;
            }
        }
        if cur_name != dst_name {
            if is_folder {
                api.rename_folder(&creds.token, &uuid, &dst_name).await.map_err(to_nfs)?;
            } else {
                let (plain, ftype) = split_name(&dst_name);
                api.rename_file(&creds.token, &uuid, &plain, &ftype).await.map_err(to_nfs)?;
            }
        }
        self.inner.cache.invalidate(&src_dir.uuid);
        self.inner.cache.invalidate(&dst_dir.uuid);
        self.remove_child(from_dirid, &src_name);
        Ok(())
    }

    async fn readdir(
        &self,
        dirid: fileid3,
        start_after: fileid3,
        max_entries: usize,
    ) -> Result<ReadDirResult, nfsstat3> {
        let dir = self.inner.node(dirid).ok_or(nfsstat3::NFS3ERR_STALE)?;
        if dir.kind != NodeKind::Dir {
            return Err(nfsstat3::NFS3ERR_NOTDIR);
        }
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        let (folders, files) = tokio::try_join!(
            tree::list_folders(&api, &creds.token, &dir.uuid, &self.inner.cache),
            tree::list_files_cached(&api, &creds.token, &dir.uuid, &self.inner.cache),
        )
        .map_err(to_nfs)?;

        // Intern every child to a stable id, then order deterministically by id
        // so `start_after` (a previously returned id) resumes correctly.
        let mut all: Vec<(fileid3, NodeKind, u64, String, String)> =
            Vec::with_capacity(folders.len() + files.len());
        for f in &folders {
            let nd = self.inner.node_from_folder(dirid, &dir.uuid, f);
            let id = self.inner.intern(dirid, nd);
            all.push((id, NodeKind::Dir, 0, f.plain_name.clone(), f.updated_at.clone()));
        }
        for f in &files {
            let nd = self.inner.node_from_file(dirid, &dir.uuid, f);
            let id = self.inner.intern(dirid, nd);
            all.push((id, NodeKind::File, f.size, f.display_name(), f.updated_at.clone()));
        }
        all.sort_by_key(|e| e.0);

        let mut entries = Vec::new();
        let mut end = true;
        for (id, kind, size, name, updated_at) in all.into_iter().filter(|e| e.0 > start_after) {
            if entries.len() >= max_entries {
                end = false;
                break;
            }
            let size = if kind == NodeKind::File {
                self.inner.writer(id).map(|wb| wb.size.load(Ordering::SeqCst)).unwrap_or(size)
            } else {
                0
            };
            entries.push(DirEntry {
                fileid: id,
                name: string_to_fname(&name),
                attr: self.inner.make_fattr(id, kind, size, &updated_at),
            });
        }
        Ok(ReadDirResult { entries, end })
    }

    async fn symlink(
        &self,
        _dirid: fileid3,
        _linkname: &filename3,
        _symlink: &nfspath3,
        _attr: &sattr3,
    ) -> Result<(fileid3, fattr3), nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }

    async fn readlink(&self, _id: fileid3) -> Result<nfspath3, nfsstat3> {
        Err(nfsstat3::NFS3ERR_NOTSUPP)
    }
}

impl DriveNfs {
    /// Drop a stale interned child (and its reader/writer) after a mutation.
    fn remove_child(&self, parent: fileid3, name: &str) {
        let id = self
            .inner
            .inodes
            .lock()
            .unwrap()
            .by_parent_name
            .get(&(parent, name.to_string()))
            .copied();
        if let Some(id) = id {
            self.inner.remove_node(id);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests: write/flush race on `WriteBuffer`
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `WriteBuffer` backed by a real temp file but with no Drive
    /// identity (`base_file_id: None`, empty creds), so `write_at` /
    /// `ensure_materialized` never touch the network — only the local file
    /// and the atomics under test.
    async fn test_write_buffer() -> Arc<WriteBuffer> {
        let temp = temp_path(None);
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .await
            .unwrap();
        Arc::new(WriteBuffer {
            fileid: 42,
            temp_path: temp,
            file: tokio::sync::Mutex::new(file),
            materialized: tokio::sync::Mutex::new(false),
            flush_lock: tokio::sync::Mutex::new(()),
            dirty: AtomicBool::new(false),
            size: AtomicU64::new(0),
            write_gen: AtomicU64::new(0),
            last_write: Mutex::new(Instant::now()),
            fail_count: AtomicU32::new(0),
            last_attempt: Mutex::new(Instant::now()),
            existing_uuid: Mutex::new(None),
            parent_uuid: String::new(),
            plain: "test".to_string(),
            ftype: String::new(),
            bucket: String::new(),
            mnemonic: String::new(),
            net_user: String::new(),
            net_pass: String::new(),
            base_file_id: None,
            base_bucket: String::new(),
            base_size: 0,
        })
    }

    /// Reproduces the core of the flush/write race: a write that lands
    /// between a flush's `(gen, size)` snapshot and the point it would
    /// otherwise unconditionally clear `dirty` must NOT be lost — the old
    /// code's unconditional `wb.dirty.store(false, ..)` would clobber the
    /// `true` the racing write just set, and the buffer's content would
    /// silently never reach Drive. `clear_dirty_if_unchanged` must refuse to
    /// clear `dirty` when a write raced it, and must clear it cleanly when
    /// none did.
    #[tokio::test]
    async fn flush_does_not_clobber_a_racing_write() {
        let wb = test_write_buffer().await;

        wb.write_at(0, b"hello").await.unwrap();
        assert!(wb.dirty.load(Ordering::SeqCst));

        // Mirrors `flush_writer`'s snapshot: taken before the (simulated,
        // out-of-band) upload starts reading the file.
        let gen_before = wb.write_gen.load(Ordering::SeqCst);

        // A write lands while the "upload" is in flight — e.g. a background
        // sweeper's flush is awaiting a network call when another NFS write
        // RPC comes in on the same file.
        wb.write_at(5, b" world").await.unwrap();
        assert_eq!(wb.size.load(Ordering::SeqCst), 11);

        // The flush finishes "uploading" (the stale, pre-race snapshot) and
        // tries to clear dirty using the gen it captured before the race.
        let cleared = wb.clear_dirty_if_unchanged(gen_before);

        assert!(!cleared, "must not report a clean clear when a write raced the flush");
        assert!(
            wb.dirty.load(Ordering::SeqCst),
            "a write that raced the flush must leave dirty=true so the next \
             sweep re-uploads it — losing this would silently drop the write"
        );

        // Next sweep's flush re-snapshots and, with no further writes, is
        // able to clear dirty normally.
        let gen_before_2 = wb.write_gen.load(Ordering::SeqCst);
        let cleared_2 = wb.clear_dirty_if_unchanged(gen_before_2);
        assert!(cleared_2);
        assert!(!wb.dirty.load(Ordering::SeqCst));
    }

    /// Same race, but with a real concurrent task and a timing gap standing
    /// in for the network-bound upload, instead of calling the write inline.
    #[tokio::test]
    async fn concurrent_write_during_simulated_upload_is_not_lost() {
        let wb = test_write_buffer().await;
        wb.write_at(0, b"hello").await.unwrap();

        let gen_before = wb.write_gen.load(Ordering::SeqCst);

        let wb2 = wb.clone();
        let writer_task = tokio::spawn(async move {
            // Stands in for a write RPC arriving mid-upload.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            wb2.write_at(5, b" world").await.unwrap();
        });

        // Stands in for the network-bound upload read.
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        writer_task.await.unwrap();

        let cleared = wb.clear_dirty_if_unchanged(gen_before);
        assert!(!cleared);
        assert!(wb.dirty.load(Ordering::SeqCst));
        assert_eq!(wb.size.load(Ordering::SeqCst), 11);
    }

    /// `retry_backoff` must grow with the failure count and cap at
    /// `RETRY_MAX`, never overflow/panic, and never be negative/decreasing.
    #[test]
    fn retry_backoff_grows_and_then_caps() {
        assert_eq!(retry_backoff(0), std::time::Duration::ZERO, "no failure yet ⇒ no backoff");
        assert_eq!(retry_backoff(1), RETRY_BASE);
        assert_eq!(retry_backoff(2), RETRY_BASE * 2);
        assert_eq!(retry_backoff(3), RETRY_BASE * 4);

        let mut prev = std::time::Duration::ZERO;
        for n in 1..10 {
            let d = retry_backoff(n);
            assert!(d <= RETRY_MAX, "backoff must never exceed the cap");
            assert!(d >= prev, "backoff must never shrink as failures accumulate");
            prev = d;
        }
        assert_eq!(retry_backoff(10), RETRY_MAX, "large counts settle at the cap");
        // A pathologically large failure count must not overflow or panic.
        assert_eq!(retry_backoff(u32::MAX), RETRY_MAX);
    }

    /// The core of this fix: a buffer that fails every flush attempt must
    /// never be given up on (it must stay dirty forever, so the data is
    /// never discarded), but the failure bookkeeping that `sweep` consults
    /// before calling `flush_writer` again must show a growing, capped
    /// backoff — not "retry unconditionally every tick" (the old bug, which
    /// hammered the Drive API and the log once a second forever on a
    /// permanently-failing upload).
    #[tokio::test]
    async fn permanently_failing_buffer_backs_off_but_never_gives_up() {
        let wb = test_write_buffer().await;
        wb.write_at(0, b"hello").await.unwrap();
        assert!(wb.dirty.load(Ordering::SeqCst));

        let mut last_backoff = std::time::Duration::ZERO;
        for expected_count in 1..=6u32 {
            // Mirrors what `flush_writer` does on each of its failure paths.
            wb.log_flush_failure("simulated permanent failure");
            assert_eq!(wb.fail_count.load(Ordering::SeqCst), expected_count);
            // Never given up on: still dirty, so the next sweep still retries.
            assert!(wb.dirty.load(Ordering::SeqCst), "a failing upload must never lose its data");

            let backoff = retry_backoff(wb.fail_count.load(Ordering::SeqCst));
            assert!(backoff >= last_backoff, "backoff must grow (or hold at the cap)");
            assert!(backoff <= RETRY_MAX);
            last_backoff = backoff;
        }

        // A later success (e.g. the user freed up quota) resets the streak so
        // a fresh failure afterwards backs off from scratch instead of
        // compounding on the old one.
        wb.record_flush_success();
        assert_eq!(wb.fail_count.load(Ordering::SeqCst), 0);
        assert_eq!(retry_backoff(wb.fail_count.load(Ordering::SeqCst)), std::time::Duration::ZERO);
    }
}

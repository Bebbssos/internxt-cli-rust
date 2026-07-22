//! The `fuser::Filesystem` implementation backing `internxt mount`.
//!
//! ## Bridging sync FUSE → async
//! `fuser` calls our methods on its own (synchronous) session thread. Anything
//! that touches the network is dispatched onto the tokio runtime via a stored
//! `Handle`: the method clones the shared state, `spawn`s a task that does the
//! async work, and the task calls `reply.*`. The `Reply*` objects are `Send`, so
//! the session thread never blocks and independent ops run concurrently.
//!
//! ## Inodes
//! Drive items are keyed by uuid; the kernel needs stable `u64` inodes, so we
//! keep a two-way table (`InodeTable`): `ino -> NodeData` and
//! `(parent_ino, name) -> ino`. The mount root is inode 1. Children are interned
//! lazily on `lookup` / `readdir`.
//!
//! ## Writes (whole-file model)
//! Internxt has no partial update. A file opened for writing is backed by a temp
//! file: existing content is materialized into it lazily (on first read/write,
//! skipped when the open is immediately truncated), writes patch the temp file,
//! and on the final `release` the temp file is uploaded in full and a new Drive
//! file entry replaces the old one.
//!
//! `release` is fire-and-forget from the kernel's point of view — it lets the
//! writer's `close()` return before our reply (and thus the upload it awaits)
//! completes — so a failed upload has nowhere to report to by the time it's
//! known. `NodeData::failed_upload` + `Inner::take_failed_upload` give a later
//! read-only `open()` a one-shot chance to surface it as `EIO` instead of the
//! failure being visible only in the server's own log.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use fuser::{
    BsdFileFlags, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, InitFlags, KernelConfig, LockOwner, OpenAccMode, OpenFlags, RenameFlags, ReplyAttr,
    ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus, ReplyEmpty, ReplyEntry, ReplyOpen,
    ReplyStatfs, ReplyWrite, Request, TimeOrNow, WriteFlags,
};
use rand::RngExt;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, DuplexStream};
use tokio::runtime::Handle as RtHandle;

use super::MountConfig;
use internxt_core::api::DriveApi;
use internxt_core::models::Credentials;
use crate::serve::cache::FolderCache;
use crate::serve::creds::SharedCreds;
use crate::serve::tree::{self, FileItem, FolderItem};

/// Root inode, per the FUSE protocol.
const ROOT_INO: u64 = 1;

/// Forward gaps up to this size are skipped by reading-and-discarding from the
/// live stream (cheap, no new request). Past it, discarding would pull more
/// data over the wire than a fresh ranged request costs in round-trip
/// overhead, so a big forward jump restarts the stream instead — same as a
/// backward seek. This matters for MP4s with the `moov` atom at the end of
/// the file (non-"faststart"): a player probing metadata jumps from byte 0
/// to near EOF, and without this threshold that jump silently downloads (and
/// discards) the entire file.
const MAX_FORWARD_SKIP: u64 = 8 * 1024 * 1024;

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Per-operation request trace (`[METHOD] path`). Verbose-only: printed just
/// when `--verbose` is set, so a busy mount doesn't spam stderr by default.
pub(crate) fn log(msg: &str) {
    crate::serve::log::trace(msg);
}

/// Parse an RFC3339 timestamp into a `SystemTime`, falling back to the epoch.
fn parse_time(s: &str) -> SystemTime {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp()).ok())
        .map(|secs| UNIX_EPOCH + Duration::from_secs(secs))
        .unwrap_or(UNIX_EPOCH)
}

/// Map any internal error to an errno for the kernel.
fn errno(_e: &anyhow::Error) -> Errno {
    Errno::EIO
}

#[derive(Clone, Copy, PartialEq)]
enum NodeKind {
    Dir,
    File,
}

/// Everything we remember about one inode.
#[derive(Clone)]
struct NodeData {
    /// Drive uuid. Empty for a freshly-`create`d file not yet uploaded.
    uuid: String,
    parent: u64,
    /// Parent folder's Drive uuid (destination for uploads / new entries).
    parent_uuid: String,
    name: String,
    kind: NodeKind,
    size: u64,
    bucket: String,
    file_id: Option<String>,
    file_type: String,
    plain_name: String,
    updated_at: String,
    /// Set when the most recent `finalize_write` for this inode (Drive
    /// upload + `replace_file`/`create_file_entry`, run from `release()`
    /// after the writer's `close()` has already returned success — see
    /// `pending_uploads` above) failed for any reason: quota, a transient
    /// network/server error, auth, a concurrent-create 409, anything. That
    /// failure is otherwise only ever visible in the server's own log
    /// (`release`'s `crate::serve::log::warn`); the writer already got a
    /// successful `close()`, and `getattr`/`ls` keep showing the size the
    /// (failed) write set locally via `set_node_size`. This can't make the
    /// original `close()` fail — that ship has sailed by the time
    /// `finalize_write` even runs — but a later read-only `open()` checks
    /// this (via `Inner::take_failed_upload`) and returns a one-shot `EIO`
    /// instead of silently serving content that may not match, or may never
    /// have reached, Drive at all. One-shot: cleared the moment it's
    /// surfaced, and also cleared by the next *successful* `finalize_write`
    /// (see `Inner::record_finalize_result`), so a file doesn't stay wedged
    /// after the underlying problem is fixed and the file is rewritten.
    /// Deliberately excluded from `intern`'s metadata refresh (a routine
    /// `lookup`/`readdir` must not clear it before `open()` gets a chance to
    /// surface it).
    failed_upload: Option<String>,
}

#[derive(Default)]
struct InodeTable {
    map: HashMap<u64, NodeData>,
    by_parent_name: HashMap<(u64, String), u64>,
    next_ino: u64,
}

/// One open file handle: either a streaming reader (read-only opens), a
/// temp-file-backed writer (write opens and `create`), or a directory-listing
/// snapshot (opendir).
enum Handle {
    Read(ReadHandle),
    Write(Arc<WriteHandle>),
    /// (child inode, name, attrs) snapshot for one opendir/readdir(plus)/releasedir session.
    Dir(Vec<(u64, String, NodeData)>),
}

/// A sequential download stream positioned at `pos` in the file, feeding a pipe.
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

/// Read-only handle: lazily (re)starts a sequential decrypt stream and serves
/// reads from it. Forward gaps up to [`MAX_FORWARD_SKIP`] are skipped in-stream;
/// a backward seek, or a forward gap past the threshold, restarts as a ranged
/// download.
struct ReadHandle {
    file_id: String,
    bucket: String,
    mnemonic: String,
    net_user: String,
    net_pass: String,
    size: u64,
    stream: tokio::sync::Mutex<Option<ReadStream>>,
}

impl ReadHandle {
    /// Spawn a producer that decrypts `[start, size)` into a duplex pipe.
    fn start_stream(&self, rt: &RtHandle, start: u64) -> ReadStream {
        let (mut writer, reader) = tokio::io::duplex(256 * 1024);
        let net = crate::net_client::network_api(&self.net_user, &self.net_pass);
        let mnemonic = self.mnemonic.clone();
        let bucket = self.bucket.clone();
        let file_id = self.file_id.clone();
        let size = self.size;
        let task = rt.spawn(async move {
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
                crate::serve::log::warn(&format!("[mount] read stream error: {e:#}"));
            }
            let _ = writer.shutdown().await;
        });
        ReadStream { reader, pos: start, task }
    }

    async fn read_at(&self, rt: &RtHandle, offset: u64, size: usize) -> Result<Vec<u8>> {
        let mut guard = self.stream.lock().await;
        // (Re)start when there is no stream, the offset is behind the cursor,
        // or the forward gap is too big to cheaply skip over in-stream.
        let restart = match &*guard {
            Some(s) => offset < s.pos || offset - s.pos > MAX_FORWARD_SKIP,
            None => true,
        };
        if restart {
            *guard = Some(self.start_stream(rt, offset));
        }
        let stream = guard.as_mut().unwrap();

        // Forward gap: discard bytes already downloaded to reach `offset`.
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

        let mut buf = vec![0u8; size];
        let mut filled = 0;
        while filled < size {
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

/// Write handle: a temp file that is uploaded whole on release.
struct WriteHandle {
    ino: u64,
    temp_path: PathBuf,
    file: tokio::sync::Mutex<tokio::fs::File>,
    /// Whether existing Drive content has been pulled into the temp file yet.
    materialized: tokio::sync::Mutex<bool>,
    /// Whether the buffer differs from what's on Drive (needs upload on release).
    dirty: AtomicBool,
    size: AtomicU64,
    // Upload target.
    parent_uuid: String,
    plain: String,
    ftype: String,
    bucket: String,
    mnemonic: String,
    net_user: String,
    net_pass: String,
    // Existing entry to replace (trash/delete) after the new one is created.
    existing_uuid: Mutex<Option<String>>,
    // Source for lazy materialization of existing content.
    base_file_id: Option<String>,
    base_bucket: String,
    base_size: u64,
}

impl WriteHandle {
    /// Ensure the temp file holds the file's existing Drive content before a
    /// partial write/read. Cheap no-op once done or when there's nothing to pull.
    async fn ensure_materialized(&self) -> Result<()> {
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
}

/// Shared, cloneable server state.
pub struct Inner {
    shared: Arc<SharedCreds>,
    cache: Arc<FolderCache>,
    config: MountConfig,
    uid: u32,
    gid: u32,
    inodes: Mutex<InodeTable>,
    handles: Mutex<HashMap<u64, Arc<Handle>>>,
    next_fh: AtomicU64,
    upload_sem: Option<Arc<tokio::sync::Semaphore>>,
    upload_limit: crate::upload_limit::UploadLimit,
    /// Inodes with a `finalize_write` currently in flight. FUSE `release` is
    /// fire-and-forget from the caller's point of view — the kernel lets the
    /// client's `close()` return before our reply (and thus the upload it
    /// awaits) completes. A read-open racing in during that window would
    /// otherwise build a `ReadHandle` from a stale/empty `file_id` and fail
    /// with a raw server error ("File id is malformed"). `open()` waits on
    /// the per-inode lock here before proceeding, instead of racing ahead.
    pending_uploads: Mutex<HashMap<u64, Arc<tokio::sync::Mutex<()>>>>,
}

impl Inner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        shared: Arc<SharedCreds>,
        cache: Arc<FolderCache>,
        root_uuid: String,
        root_updated_at: String,
        upload_sem: Option<Arc<tokio::sync::Semaphore>>,
        upload_limit: crate::upload_limit::UploadLimit,
        config: MountConfig,
    ) -> Self {
        let mut table = InodeTable {
            next_ino: 2,
            ..Default::default()
        };
        table.map.insert(
            ROOT_INO,
            NodeData {
                uuid: root_uuid.clone(),
                parent: ROOT_INO,
                parent_uuid: root_uuid.clone(),
                name: String::new(),
                kind: NodeKind::Dir,
                size: 0,
                bucket: String::new(),
                file_id: None,
                file_type: String::new(),
                plain_name: String::new(),
                updated_at: root_updated_at,
                failed_upload: None,
            },
        );
        Inner {
            shared,
            cache,
            config,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            inodes: Mutex::new(table),
            handles: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            upload_sem,
            upload_limit,
            pending_uploads: Mutex::new(HashMap::new()),
        }
    }

    fn creds(&self) -> Arc<Credentials> {
        self.shared.get()
    }

    fn ttl(&self) -> Duration {
        Duration::from_secs(self.config.cache_ttl)
    }

    fn node(&self, ino: u64) -> Option<NodeData> {
        self.inodes.lock().unwrap().map.get(&ino).cloned()
    }

    /// Reconstruct an inode's path by walking parent links, for logging.
    fn path_of(&self, ino: u64) -> String {
        if ino == ROOT_INO {
            return "/".to_string();
        }
        let t = self.inodes.lock().unwrap();
        let mut parts = Vec::new();
        let mut cur = ino;
        while cur != ROOT_INO {
            match t.map.get(&cur) {
                Some(nd) => {
                    parts.push(nd.name.clone());
                    cur = nd.parent;
                }
                None => {
                    parts.push(format!("<{cur}>"));
                    break;
                }
            }
        }
        parts.reverse();
        format!("/{}", parts.join("/"))
    }

    /// Path of a child `name` under `parent_ino` (child not yet interned).
    fn child_path(&self, parent_ino: u64, name: &str) -> String {
        let parent = self.path_of(parent_ino);
        if parent == "/" {
            format!("/{name}")
        } else {
            format!("{parent}/{name}")
        }
    }

    fn set_node_size(&self, ino: u64, size: u64) {
        if let Some(nd) = self.inodes.lock().unwrap().map.get_mut(&ino) {
            nd.size = size;
        }
    }

    /// Intern (or refresh) a child under `parent_ino`, returning its inode.
    fn intern(&self, parent_ino: u64, mut nd: NodeData) -> u64 {
        nd.parent = parent_ino;
        let mut t = self.inodes.lock().unwrap();
        let key = (parent_ino, nd.name.clone());
        if let Some(&ino) = t.by_parent_name.get(&key) {
            if let Some(existing) = t.map.get_mut(&ino) {
                // Refresh mutable metadata; keep the stable inode.
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
            return ino;
        }
        let ino = t.next_ino;
        t.next_ino += 1;
        t.map.insert(ino, nd);
        t.by_parent_name.insert(key, ino);
        ino
    }

    /// Drop an inode and its parent-name index entry.
    fn remove_node(&self, ino: u64) {
        let mut t = self.inodes.lock().unwrap();
        if let Some(nd) = t.map.remove(&ino) {
            t.by_parent_name.remove(&(nd.parent, nd.name));
        }
    }

    fn node_from_folder(&self, parent_ino: u64, parent_uuid: &str, f: &FolderItem) -> NodeData {
        NodeData {
            uuid: f.uuid.clone(),
            parent: parent_ino,
            parent_uuid: parent_uuid.to_string(),
            name: f.plain_name.clone(),
            kind: NodeKind::Dir,
            size: 0,
            bucket: String::new(),
            file_id: None,
            file_type: String::new(),
            plain_name: f.plain_name.clone(),
            updated_at: f.updated_at.clone(),
            failed_upload: None,
        }
    }

    fn node_from_file(&self, parent_ino: u64, parent_uuid: &str, f: &FileItem) -> NodeData {
        NodeData {
            uuid: f.uuid.clone(),
            parent: parent_ino,
            parent_uuid: parent_uuid.to_string(),
            name: f.display_name(),
            kind: NodeKind::File,
            size: f.size,
            bucket: f.bucket.clone(),
            file_id: f.file_id.clone(),
            file_type: f.file_type.clone(),
            plain_name: f.plain_name.clone(),
            updated_at: f.updated_at.clone(),
            failed_upload: None,
        }
    }

    /// Build the full entry list for a directory (`.`, `..`, subfolders,
    /// files), interning each child to a stable inode. Called once per
    /// `opendir` so the whole enumeration — which the kernel may split across
    /// several `readdir` calls for a large directory — sees one consistent
    /// snapshot instead of re-fetching (and possibly re-ordering, since file
    /// listings are never cached) on every call.
    async fn build_dir_entries(&self, ino: u64, node: &NodeData) -> Result<Vec<(u64, String, NodeData)>> {
        let creds = self.creds();
        let api = DriveApi::for_credentials(&creds);
        // Independent calls: run concurrently rather than paying two sequential
        // network round trips for one directory listing.
        let (folders, files) = tokio::try_join!(
            tree::list_folders(&api, &creds.token, &node.uuid, &self.cache),
            tree::list_files(&api, &creds.token, &node.uuid),
        )?;

        let mut entries: Vec<(u64, String, NodeData)> =
            Vec::with_capacity(files.len() + folders.len() + 2);
        entries.push((ino, ".".to_string(), node.clone()));
        let parent_nd = self.node(node.parent).unwrap_or_else(|| node.clone());
        entries.push((node.parent, "..".to_string(), parent_nd));
        for f in &folders {
            let nd = self.node_from_folder(ino, &node.uuid, f);
            let child = self.intern(ino, nd.clone());
            entries.push((child, f.plain_name.clone(), nd));
        }
        for f in &files {
            let nd = self.node_from_file(ino, &node.uuid, f);
            let child = self.intern(ino, nd.clone());
            entries.push((child, f.display_name(), nd));
        }
        Ok(entries)
    }

    fn attr(&self, ino: u64, nd: &NodeData) -> FileAttr {
        let time = parse_time(&nd.updated_at);
        let (kind, perm, nlink) = match nd.kind {
            NodeKind::Dir => (FileType::Directory, 0o755, 2),
            NodeKind::File => (FileType::RegularFile, 0o644, 1),
        };
        FileAttr {
            ino: INodeNo(ino),
            size: nd.size,
            blocks: nd.size.div_ceil(512),
            atime: time,
            mtime: time,
            ctime: time,
            crtime: time,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn new_fh(&self, handle: Handle) -> u64 {
        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        self.handles.lock().unwrap().insert(fh, Arc::new(handle));
        fh
    }

    fn get_handle(&self, fh: u64) -> Option<Arc<Handle>> {
        self.handles.lock().unwrap().get(&fh).cloned()
    }

    /// Any open write handle for `ino`. Used by setattr to apply an O_TRUNC
    /// truncate that the kernel delivers as a fh-less setattr (FATTR_FH unset)
    /// *after* the write handle was opened — without this the handle keeps its
    /// stale base_size and a following write re-materializes the old content.
    fn find_write_handle(&self, ino: u64) -> Option<Arc<WriteHandle>> {
        self.handles
            .lock()
            .unwrap()
            .values()
            .find_map(|h| match h.as_ref() {
                Handle::Write(wh) if wh.ino == ino => Some(wh.clone()),
                _ => None,
            })
    }

    fn take_handle(&self, fh: u64) -> Option<Arc<Handle>> {
        self.handles.lock().unwrap().remove(&fh)
    }

    /// Mark `ino` as having a `finalize_write` in flight, returning the lock
    /// to hold for its duration. Reuses an existing entry if one is already
    /// registered (e.g. overlapping writes to the same inode), so waiters
    /// queue behind whichever upload is currently running.
    fn begin_pending_upload(&self, ino: u64) -> Arc<tokio::sync::Mutex<()>> {
        self.pending_uploads
            .lock()
            .unwrap()
            .entry(ino)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Clear the pending-upload marker for `ino`, but only if it's still the
    /// same lock we registered (a later writer may have already replaced it).
    fn end_pending_upload(&self, ino: u64, lock: &Arc<tokio::sync::Mutex<()>>) {
        let mut m = self.pending_uploads.lock().unwrap();
        if m.get(&ino).is_some_and(|cur| Arc::ptr_eq(cur, lock)) {
            m.remove(&ino);
        }
    }

    /// The in-flight upload lock for `ino`, if any. `open()` waits on this
    /// before building a read handle so it doesn't race a finalize in progress.
    fn pending_upload(&self, ino: u64) -> Option<Arc<tokio::sync::Mutex<()>>> {
        self.pending_uploads.lock().unwrap().get(&ino).cloned()
    }

    /// Record the outcome of a `finalize_write` for `ino` on its `NodeData`:
    /// `Err` sets `failed_upload` (message included, for whatever future
    /// diagnostic use), `Ok` clears it. Split out from `finalize_write` so
    /// this bookkeeping is unit-testable without a network round trip.
    fn record_finalize_result(&self, ino: u64, result: &Result<()>) {
        if let Some(nd) = self.inodes.lock().unwrap().map.get_mut(&ino) {
            nd.failed_upload = result.as_ref().err().map(|e| format!("{e:#}"));
        }
    }

    /// Take (clear) and report whether `ino` has a recorded upload failure.
    /// One-shot by design: `open()`'s read-only path calls this once per
    /// open, so the failure is surfaced exactly once and doesn't wedge the
    /// file shut forever (see `NodeData::failed_upload`).
    fn take_failed_upload(&self, ino: u64) -> bool {
        self.inodes
            .lock()
            .unwrap()
            .map
            .get_mut(&ino)
            .is_some_and(|nd| nd.failed_upload.take().is_some())
    }

    async fn acquire_upload(&self) -> Result<Option<tokio::sync::OwnedSemaphorePermit>> {
        match &self.upload_sem {
            Some(s) => Ok(Some(s.clone().acquire_owned().await?)),
            None => Ok(None),
        }
    }

    /// Finalize a write handle: upload the temp file whole and swap the Drive
    /// entry. No-op when nothing was written. Records the success/failure on
    /// the node either way (see `record_finalize_result`); the caller
    /// (`release`) still logs the error itself, unchanged.
    async fn finalize_write(&self, wh: Arc<WriteHandle>) -> Result<()> {
        if !wh.dirty.load(Ordering::SeqCst) {
            return Ok(());
        }
        let result = self.finalize_write_inner(&wh).await;
        self.record_finalize_result(wh.ino, &result);
        result
    }

    async fn finalize_write_inner(&self, wh: &WriteHandle) -> Result<()> {
        {
            let mut f = wh.file.lock().await;
            f.flush().await?;
        }
        let size = wh.size.load(Ordering::SeqCst);
        let creds = self.creds();
        let token = &creds.token;
        let api = DriveApi::for_credentials(&creds);
        let net = crate::net_client::network_api(&wh.net_user, &wh.net_pass);

        let _permit = self.acquire_upload().await?;
        let file_id = if size == 0 {
            String::new()
        } else {
            internxt_core::transfer::upload_file_to_network(&net, &wh.bucket, &wh.mnemonic, &wh.temp_path, size, None)
                .await?
        };
        let now = now_rfc3339();
        // If this handle wraps an existing Drive file, replace its content in
        // place (PUT /files/{uuid}) — keeps the same uuid/name/folder and swaps
        // fileId+size. createFileEntry would 409 ("File already exists") on the
        // duplicate name, so replace instead of the old create-then-trash dance.
        let old = wh.existing_uuid.lock().unwrap().take();
        let result_uuid = match old {
            Some(old_uuid) => api.replace_file(token, &old_uuid, &file_id, size).await?.uuid,
            None => {
                api.create_file_entry(
                    token,
                    &wh.plain,
                    &wh.ftype,
                    size,
                    &wh.parent_uuid,
                    &file_id,
                    &wh.bucket,
                    &now,
                    &now,
                )
                .await?
                .uuid
            }
        };

        crate::serve::thumbnail::upload_thumbnail_best_effort(
            &net, &api, token, &wh.bucket, &wh.mnemonic, &result_uuid, &wh.ftype, &wh.temp_path,
            size, "fuse",
        )
        .await;

        let mut t = self.inodes.lock().unwrap();
        if let Some(nd) = t.map.get_mut(&wh.ino) {
            nd.uuid = result_uuid;
            nd.file_id = if file_id.is_empty() { None } else { Some(file_id) };
            nd.size = size;
            nd.updated_at = now;
            nd.bucket = wh.bucket.clone();
        }
        Ok(())
    }
}

/// Split a filename into (plainName, extension-without-dot). A leading-dot name
/// like `.env` is treated as having no extension.
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
    base.join(format!("internxt-mount-{}.tmp", hex::encode(rnd)))
}

/// The filesystem object handed to `fuser`. Cheap to clone-share via `Inner`.
pub struct InxtFs {
    inner: Arc<Inner>,
    rt: RtHandle,
}

impl InxtFs {
    pub fn new(inner: Arc<Inner>, rt: RtHandle) -> Self {
        InxtFs { inner, rt }
    }
}

impl Filesystem for InxtFs {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        // Always request readdirplus (not just the adaptive AUTO variant): a
        // plain (non-color) `ls` doesn't stat every entry, but anything that
        // does — color-aware `ls`, `ls -l`, a file manager — would otherwise
        // trigger one `lookup` per entry, and lookup's file-listing fetch is
        // never cached. Folding attrs into readdir itself avoids that.
        let _ = config.add_capabilities(InitFlags::FUSE_DO_READDIRPLUS);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let inner = self.inner.clone();
        let name = name.to_string_lossy().into_owned();
        self.rt.spawn(async move {
            let parent = parent.0;
            log(&format!("[LOOKUP] {}", inner.child_path(parent, &name)));
            let Some(pnode) = inner.node(parent) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let creds = inner.creds();
            let api = DriveApi::for_credentials(&creds);
            // Files first, then folders (folder listing is cached).
            match tree::list_files(&api, &creds.token, &pnode.uuid).await {
                Ok(files) => {
                    if let Some(f) = files.iter().find(|f| f.display_name() == name) {
                        let nd = inner.node_from_file(parent, &pnode.uuid, f);
                        let ino = inner.intern(parent, nd.clone());
                        reply.entry(&inner.ttl(), &inner.attr(ino, &nd), Generation(0));
                        return;
                    }
                }
                Err(e) => {
                    reply.error(errno(&e));
                    return;
                }
            }
            match tree::find_folder(&api, &creds.token, &pnode.uuid, &name, &inner.cache).await {
                Ok(Some(f)) => {
                    let nd = inner.node_from_folder(parent, &pnode.uuid, &f);
                    let ino = inner.intern(parent, nd.clone());
                    reply.entry(&inner.ttl(), &inner.attr(ino, &nd), Generation(0));
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    reply.error(errno(&e));
                    return;
                }
            }
            reply.error(Errno::ENOENT);
        });
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        log(&format!("[GETATTR] {}", self.inner.path_of(ino.0)));
        match self.inner.node(ino.0) {
            Some(nd) => reply.attr(&self.inner.ttl(), &self.inner.attr(ino.0, &nd)),
            None => reply.error(Errno::ENOENT),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        _req: &Request,
        ino: INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            log(&format!("[SETATTR] {} size={size:?}", inner.path_of(ino.0)));
            // Truncate against an open write handle. Prefer the handle the
            // kernel passed; fall back to any open write handle for this inode,
            // because an O_TRUNC open delivers the truncate as a fh-less setattr
            // (kernel strips O_TRUNC from open and issues a separate size=0
            // setattr with FATTR_FH unset). Missing that would leave the handle
            // holding the old base_size, so the next write re-downloads the old
            // content and the save appends new-then-old.
            if let Some(sz) = size {
                let wh = fh
                    .and_then(|fh| match inner.get_handle(fh.0).as_deref() {
                        Some(Handle::Write(wh)) => Some(wh.clone()),
                        _ => None,
                    })
                    .or_else(|| inner.find_write_handle(ino.0));
                if let Some(wh) = wh {
                    let res: Result<()> = async {
                        if sz == 0 {
                            *wh.materialized.lock().await = true;
                        } else {
                            wh.ensure_materialized().await?;
                        }
                        {
                            let f = wh.file.lock().await;
                            f.set_len(sz).await?;
                        }
                        wh.size.store(sz, Ordering::SeqCst);
                        wh.dirty.store(true, Ordering::SeqCst);
                        Ok(())
                    }
                    .await;
                    if let Err(e) = res {
                        reply.error(errno(&e));
                        return;
                    }
                    inner.set_node_size(ino.0, sz);
                } else {
                    // No open handle: reflect the size in our metadata only.
                    inner.set_node_size(ino.0, sz);
                }
            }
            match inner.node(ino.0) {
                Some(nd) => reply.attr(&inner.ttl(), &inner.attr(ino.0, &nd)),
                None => reply.error(Errno::ENOENT),
            }
        });
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            let ino = ino.0;
            log(&format!("[OPENDIR] {}", inner.path_of(ino)));
            let Some(node) = inner.node(ino) else {
                reply.error(Errno::ENOENT);
                return;
            };
            if node.kind != NodeKind::Dir {
                reply.error(Errno::ENOTDIR);
                return;
            }
            // Snapshot the listing once for the lifetime of this opendir, so a
            // large directory that the kernel reads via several `readdir` calls
            // sees one consistent view instead of re-fetching (and possibly
            // re-ordering, since file listings are never cached) every call.
            match inner.build_dir_entries(ino, &node).await {
                Ok(entries) => {
                    let fh = inner.new_fh(Handle::Dir(entries));
                    reply.opened(FileHandle(fh), FopenFlags::empty());
                }
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            log(&format!("[READDIR] {} offset={offset}", inner.path_of(ino.0)));
            // The snapshot taken at `opendir` is the source of truth for this
            // whole enumeration; only fall back to a fresh live fetch if the
            // handle is somehow gone (e.g. a client calling readdir without a
            // matching opendir).
            let entries = match inner.get_handle(fh.0).as_deref() {
                Some(Handle::Dir(entries)) => entries.clone(),
                _ => {
                    let Some(node) = inner.node(ino.0) else {
                        reply.error(Errno::ENOENT);
                        return;
                    };
                    match inner.build_dir_entries(ino.0, &node).await {
                        Ok(v) => v,
                        Err(e) => {
                            reply.error(errno(&e));
                            return;
                        }
                    }
                }
            };

            for (i, (child, name, nd)) in entries.into_iter().enumerate().skip(offset as usize) {
                let kind = match nd.kind {
                    NodeKind::Dir => FileType::Directory,
                    NodeKind::File => FileType::RegularFile,
                };
                // `offset` is the index of the *next* entry to return.
                if reply.add(INodeNo(child), (i + 1) as u64, kind, &name) {
                    break;
                }
            }
            reply.ok();
        });
    }

    /// Same as `readdir`, but each entry carries its full attrs so the kernel
    /// doesn't need a separate `lookup` per entry to stat it — which is what a
    /// color-aware `ls` (or anything doing `stat` on every name) would
    /// otherwise trigger, turning one directory listing into 1 + N live round
    /// trips (file listings are never cached, so each of those N `lookup`s was
    /// re-fetching the same data `readdir` just fetched).
    fn readdirplus(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            log(&format!("[READDIRPLUS] {} offset={offset}", inner.path_of(ino.0)));
            let entries = match inner.get_handle(fh.0).as_deref() {
                Some(Handle::Dir(entries)) => entries.clone(),
                _ => {
                    let Some(node) = inner.node(ino.0) else {
                        reply.error(Errno::ENOENT);
                        return;
                    };
                    match inner.build_dir_entries(ino.0, &node).await {
                        Ok(v) => v,
                        Err(e) => {
                            reply.error(errno(&e));
                            return;
                        }
                    }
                }
            };

            let ttl = inner.ttl();
            for (i, (child, name, nd)) in entries.into_iter().enumerate().skip(offset as usize) {
                let attr = inner.attr(child, &nd);
                // `offset` is the index of the *next* entry to return.
                if reply.add(INodeNo(child), (i + 1) as u64, &name, &ttl, &attr, Generation(0)) {
                    break;
                }
            }
            reply.ok();
        });
    }

    fn releasedir(&self, _req: &Request, ino: INodeNo, fh: FileHandle, _flags: OpenFlags, reply: ReplyEmpty) {
        log(&format!("[RELEASEDIR] {}", self.inner.path_of(ino.0)));
        self.inner.take_handle(fh.0);
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            let mode = match flags.acc_mode() {
                OpenAccMode::O_RDONLY => "ro",
                OpenAccMode::O_WRONLY => "wo",
                OpenAccMode::O_RDWR => "rw",
            };
            log(&format!("[OPEN] {} ({mode})", inner.path_of(ino.0)));
            let Some(nd) = inner.node(ino.0) else {
                reply.error(Errno::ENOENT);
                return;
            };
            if nd.kind != NodeKind::File {
                reply.error(Errno::EISDIR);
                return;
            }
            let write = !matches!(flags.acc_mode(), OpenAccMode::O_RDONLY);
            if !write {
                // A write to this same inode may have just `release()`d — FUSE
                // release doesn't block the writer's `close()`, so its
                // `finalize_write` upload can still be running here. Wait for
                // it instead of racing ahead with a stale/empty file_id (see
                // `pending_uploads` doc comment on `Inner`).
                if let Some(lock) = inner.pending_upload(ino.0) {
                    drop(lock.lock().await);
                }
                let nd = match inner.node(ino.0) {
                    Some(nd) => nd,
                    None => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                };
                // The write that just finished (if any) may have failed to
                // ever reach Drive — quota, a transient network/server
                // error, a concurrent-create 409, anything (see
                // `NodeData::failed_upload`). The writer's `close()` already
                // returned success and can't be un-told; surface it here
                // instead, once, so this read-only open doesn't silently
                // serve content that may not match (or may not exist on)
                // Drive at all. The flag clears as it's taken, so this is a
                // one-time speed bump, not a permanent lockout: a retry (or
                // a fresh, successful write) behaves normally again.
                if inner.take_failed_upload(ino.0) {
                    reply.error(Errno::EIO);
                    return;
                }
                let creds = inner.creds();
                let bucket = if nd.bucket.is_empty() {
                    creds.bucket().to_string()
                } else {
                    nd.bucket.clone()
                };
                let rh = ReadHandle {
                    file_id: nd.file_id.clone().unwrap_or_default(),
                    bucket,
                    mnemonic: creds.mnemonic().to_string(),
                    net_user: creds.net_user().to_string(),
                    net_pass: creds.net_pass().to_string(),
                    size: nd.size,
                    stream: tokio::sync::Mutex::new(None),
                };
                let fh = inner.new_fh(Handle::Read(rh));
                reply.opened(FileHandle(fh), FopenFlags::empty());
                return;
            }

            if inner.config.read_only {
                reply.error(Errno::EROFS);
                return;
            }
            match make_write_handle(&inner, ino.0, &nd).await {
                Ok(wh) => {
                    let fh = inner.new_fh(Handle::Write(Arc::new(wh)));
                    reply.opened(FileHandle(fh), FopenFlags::empty());
                }
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let inner = self.inner.clone();
        let rt = self.rt.clone();
        self.rt.spawn(async move {
            log(&format!(
                "[READ] {} off={offset} size={size}",
                inner.path_of(ino.0)
            ));
            let Some(handle) = inner.get_handle(fh.0) else {
                reply.error(Errno::EBADF);
                return;
            };
            match handle.as_ref() {
                Handle::Read(rh) => match rh.read_at(&rt, offset, size as usize).await {
                    Ok(buf) => reply.data(&buf),
                    Err(e) => reply.error(errno(&e)),
                },
                Handle::Write(wh) => {
                    let res: Result<Vec<u8>> = async {
                        wh.ensure_materialized().await?;
                        let mut f = wh.file.lock().await;
                        f.seek(std::io::SeekFrom::Start(offset)).await?;
                        let mut buf = vec![0u8; size as usize];
                        let mut filled = 0;
                        while filled < buf.len() {
                            let n = f.read(&mut buf[filled..]).await?;
                            if n == 0 {
                                break;
                            }
                            filled += n;
                        }
                        buf.truncate(filled);
                        Ok(buf)
                    }
                    .await;
                    match res {
                        Ok(buf) => reply.data(&buf),
                        Err(e) => reply.error(errno(&e)),
                    }
                }
                Handle::Dir(_) => reply.error(Errno::EISDIR),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        let inner = self.inner.clone();
        let data = data.to_vec();
        self.rt.spawn(async move {
            log(&format!(
                "[WRITE] {} off={offset} len={}",
                inner.path_of(ino.0),
                data.len()
            ));
            let Some(handle) = inner.get_handle(fh.0) else {
                reply.error(Errno::EBADF);
                return;
            };
            let Handle::Write(wh) = handle.as_ref() else {
                reply.error(Errno::EBADF);
                return;
            };
            let len = data.len();
            // Reject writes that would push the file past the upload cap. The
            // final size is only known at release, so gate each write by the
            // high-water mark it would set (EFBIG = "file too large").
            // checked_add: a client-supplied offset near u64::MAX combined with
            // any len must not silently wrap to a small value and sail past the
            // size gate (release builds wrap on overflow instead of panicking).
            let Some(end) = offset.checked_add(len as u64) else {
                reply.error(Errno::EFBIG);
                return;
            };
            if inner.upload_limit.check(end).is_err() {
                reply.error(Errno::EFBIG);
                return;
            }
            let res: Result<()> = async {
                wh.ensure_materialized().await?;
                let mut f = wh.file.lock().await;
                f.seek(std::io::SeekFrom::Start(offset)).await?;
                f.write_all(&data).await?;
                Ok(())
            }
            .await;
            if let Err(e) = res {
                reply.error(errno(&e));
                return;
            }
            wh.size.fetch_max(end, Ordering::SeqCst);
            wh.dirty.store(true, Ordering::SeqCst);
            inner.set_node_size(ino.0, wh.size.load(Ordering::SeqCst));
            reply.written(len as u32);
        });
    }

    fn flush(&self, _req: &Request, _ino: INodeNo, _fh: FileHandle, _lo: LockOwner, reply: ReplyEmpty) {
        // Uploads happen on release; flush is a no-op (may be called many times).
        reply.ok();
    }

    fn fsync(&self, _req: &Request, ino: INodeNo, _fh: FileHandle, _datasync: bool, reply: ReplyEmpty) {
        log(&format!("[FSYNC] {}", self.inner.path_of(ino.0)));
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let inner = self.inner.clone();
        self.rt.spawn(async move {
            let path = inner.path_of(ino.0);
            log(&format!("[RELEASE] {path}"));
            if let Some(handle) = inner.take_handle(fh.0) {
                if let Handle::Write(wh) = handle.as_ref() {
                    let dirty = wh.dirty.load(Ordering::SeqCst);
                    let size = wh.size.load(Ordering::SeqCst);
                    if dirty {
                        log(&format!("[UPLOAD] {path} ({size} bytes)"));
                    }
                    // Register (and hold) the pending-upload lock for the
                    // duration of finalize_write, so a read-open racing in on
                    // this inode (possible because the kernel lets this
                    // release's caller's close() return before we finish
                    // here) blocks on it instead of reading a stale file_id.
                    let pending = dirty.then(|| inner.begin_pending_upload(ino.0));
                    let _guard = match &pending {
                        Some(lock) => Some(lock.lock().await),
                        None => None,
                    };
                    let result = inner.finalize_write(wh.clone()).await;
                    drop(_guard);
                    if let Some(lock) = &pending {
                        inner.end_pending_upload(ino.0, lock);
                    }
                    match result {
                        Ok(_) if dirty => log(&format!("[UPLOAD] {path} done")),
                        Ok(_) => {}
                        Err(e) => {
                            crate::serve::log::warn(&format!("[ERROR] upload {path} failed: {e:#}"))
                        }
                    }
                }
            }
            reply.ok();
        });
    }

    fn create(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        let inner = self.inner.clone();
        let name = name.to_string_lossy().into_owned();
        self.rt.spawn(async move {
            log(&format!("[CREATE] {}", inner.child_path(parent.0, &name)));
            if inner.config.read_only {
                reply.error(Errno::EROFS);
                return;
            }
            let Some(pnode) = inner.node(parent.0) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let creds = inner.creds();
            let (plain, ftype) = split_name(&name);
            let temp = temp_path(inner.config.spool_dir.as_deref());
            let file = match tokio::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temp)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    reply.error(errno(&anyhow!(e)));
                    return;
                }
            };
            let wh = WriteHandle {
                ino: 0, // patched below after interning
                temp_path: temp,
                file: tokio::sync::Mutex::new(file),
                materialized: tokio::sync::Mutex::new(true), // brand-new: nothing to pull
                dirty: AtomicBool::new(true),                // ensure an (empty) entry is made
                size: AtomicU64::new(0),
                parent_uuid: pnode.uuid.clone(),
                plain: plain.clone(),
                ftype: ftype.clone(),
                bucket: creds.bucket().to_string(),
                mnemonic: creds.mnemonic().to_string(),
                net_user: creds.net_user().to_string(),
                net_pass: creds.net_pass().to_string(),
                existing_uuid: Mutex::new(None),
                base_file_id: None,
                base_bucket: creds.bucket().to_string(),
                base_size: 0,
            };
            // Intern a pending node so getattr/lookup work before release.
            let nd = NodeData {
                uuid: String::new(),
                parent: parent.0,
                parent_uuid: pnode.uuid.clone(),
                name: name.clone(),
                kind: NodeKind::File,
                size: 0,
                bucket: creds.bucket().to_string(),
                file_id: None,
                file_type: ftype,
                plain_name: plain,
                updated_at: now_rfc3339(),
                failed_upload: None,
            };
            let ino = inner.intern(parent.0, nd.clone());
            let mut wh = wh;
            wh.ino = ino;
            let fh = inner.new_fh(Handle::Write(Arc::new(wh)));
            reply.created(
                &inner.ttl(),
                &inner.attr(ino, &nd),
                Generation(0),
                FileHandle(fh),
                FopenFlags::empty(),
            );
        });
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let inner = self.inner.clone();
        let name = name.to_string_lossy().into_owned();
        self.rt.spawn(async move {
            log(&format!("[MKDIR] {}", inner.child_path(parent.0, &name)));
            if inner.config.read_only {
                reply.error(Errno::EROFS);
                return;
            }
            let Some(pnode) = inner.node(parent.0) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let creds = inner.creds();
            let api = DriveApi::for_credentials(&creds);
            // Reject if a folder of that name already exists.
            match tree::find_folder(&api, &creds.token, &pnode.uuid, &name, &inner.cache).await {
                Ok(Some(_)) => {
                    reply.error(Errno::EEXIST);
                    return;
                }
                Ok(None) => {}
                Err(e) => {
                    reply.error(errno(&e));
                    return;
                }
            }
            let created = match api.create_folder(&creds.token, &name, &pnode.uuid).await {
                Ok(v) => v,
                Err(e) => {
                    reply.error(errno(&e));
                    return;
                }
            };
            inner.cache.invalidate(&pnode.uuid);
            let uuid = created
                .get("uuid")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string();
            let nd = NodeData {
                uuid,
                parent: parent.0,
                parent_uuid: pnode.uuid.clone(),
                name: name.clone(),
                kind: NodeKind::Dir,
                size: 0,
                bucket: String::new(),
                file_id: None,
                file_type: String::new(),
                plain_name: name,
                updated_at: now_rfc3339(),
                failed_upload: None,
            };
            let ino = inner.intern(parent.0, nd.clone());
            reply.entry(&inner.ttl(), &inner.attr(ino, &nd), Generation(0));
        });
    }

    fn unlink(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let inner = self.inner.clone();
        let name = name.to_string_lossy().into_owned();
        self.rt.spawn(async move {
            log(&format!("[UNLINK] {}", inner.child_path(parent.0, &name)));
            if inner.config.read_only {
                reply.error(Errno::EROFS);
                return;
            }
            let Some(pnode) = inner.node(parent.0) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let creds = inner.creds();
            let api = DriveApi::for_credentials(&creds);
            let files = match tree::list_files(&api, &creds.token, &pnode.uuid).await {
                Ok(v) => v,
                Err(e) => {
                    reply.error(errno(&e));
                    return;
                }
            };
            let Some(f) = files.into_iter().find(|f| f.display_name() == name) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let res = if inner.config.delete_permanently {
                api.delete_file(&creds.token, &f.uuid).await
            } else {
                api.trash_items(&creds.token, json!([{ "uuid": f.uuid, "type": "file" }]))
                    .await
            };
            match res {
                Ok(_) => {
                    // Look up the inode in its own statement so the mutex guard
                    // is dropped before `remove_node` re-locks it (non-reentrant).
                    let ino = inner
                        .inodes
                        .lock()
                        .unwrap()
                        .by_parent_name
                        .get(&(parent.0, name.clone()))
                        .copied();
                    if let Some(ino) = ino {
                        inner.remove_node(ino);
                    }
                    reply.ok();
                }
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn rmdir(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let inner = self.inner.clone();
        let name = name.to_string_lossy().into_owned();
        self.rt.spawn(async move {
            log(&format!("[RMDIR] {}", inner.child_path(parent.0, &name)));
            if inner.config.read_only {
                reply.error(Errno::EROFS);
                return;
            }
            let Some(pnode) = inner.node(parent.0) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let creds = inner.creds();
            let api = DriveApi::for_credentials(&creds);
            let Some(f) = (match tree::find_folder(&api, &creds.token, &pnode.uuid, &name, &inner.cache).await {
                Ok(v) => v,
                Err(e) => {
                    reply.error(errno(&e));
                    return;
                }
            }) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let res = if inner.config.delete_permanently {
                api.delete_folder(&creds.token, &f.uuid).await
            } else {
                api.trash_items(&creds.token, json!([{ "uuid": f.uuid, "type": "folder" }]))
                    .await
            };
            match res {
                Ok(_) => {
                    inner.cache.invalidate(&pnode.uuid);
                    inner.cache.invalidate(&f.uuid);
                    // Own statement so the guard drops before `remove_node` re-locks.
                    let ino = inner
                        .inodes
                        .lock()
                        .unwrap()
                        .by_parent_name
                        .get(&(parent.0, name.clone()))
                        .copied();
                    if let Some(ino) = ino {
                        inner.remove_node(ino);
                    }
                    reply.ok();
                }
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn rename(
        &self,
        _req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        _flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let inner = self.inner.clone();
        let name = name.to_string_lossy().into_owned();
        let newname = newname.to_string_lossy().into_owned();
        self.rt.spawn(async move {
            log(&format!(
                "[RENAME] {} -> {}",
                inner.child_path(parent.0, &name),
                inner.child_path(newparent.0, &newname)
            ));
            if inner.config.read_only {
                reply.error(Errno::EROFS);
                return;
            }
            let (Some(pnode), Some(np)) = (inner.node(parent.0), inner.node(newparent.0)) else {
                reply.error(Errno::ENOENT);
                return;
            };
            let creds = inner.creds();
            let api = DriveApi::for_credentials(&creds);

            // Resolve the source item (file first, then folder).
            let src_file = match tree::list_files(&api, &creds.token, &pnode.uuid).await {
                Ok(v) => v.into_iter().find(|f| f.display_name() == name),
                Err(e) => {
                    reply.error(errno(&e));
                    return;
                }
            };
            let (uuid, is_folder, cur_name) = if let Some(f) = src_file {
                let cur = f.display_name();
                (f.uuid, false, cur)
            } else {
                match tree::find_folder(&api, &creds.token, &pnode.uuid, &name, &inner.cache).await {
                    Ok(Some(f)) => (f.uuid, true, f.plain_name),
                    Ok(None) => {
                        reply.error(Errno::ENOENT);
                        return;
                    }
                    Err(e) => {
                        reply.error(errno(&e));
                        return;
                    }
                }
            };

            let res: Result<()> = async {
                // Move to the new parent when it differs.
                if parent.0 != newparent.0 {
                    if is_folder {
                        api.move_folder(&creds.token, &uuid, &np.uuid).await?;
                    } else {
                        api.move_file(&creds.token, &uuid, &np.uuid).await?;
                    }
                }
                // Rename when the final name differs.
                if cur_name != newname {
                    if is_folder {
                        api.rename_folder(&creds.token, &uuid, &newname).await?;
                    } else {
                        let (plain, ftype) = split_name(&newname);
                        api.rename_file(&creds.token, &uuid, &plain, &ftype).await?;
                    }
                }
                Ok(())
            }
            .await;

            match res {
                Ok(_) => {
                    // Drop stale inode mappings; the kernel will re-lookup.
                    inner.cache.invalidate(&pnode.uuid);
                    inner.cache.invalidate(&np.uuid);
                    let stale = inner
                        .inodes
                        .lock()
                        .unwrap()
                        .by_parent_name
                        .get(&(parent.0, name.clone()))
                        .copied();
                    if let Some(ino) = stale {
                        inner.remove_node(ino);
                    }
                    reply.ok();
                }
                Err(e) => reply.error(errno(&e)),
            }
        });
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // Large, mostly-free volume (Internxt reports quota elsewhere).
        let blocks: u64 = 1 << 40;
        reply.statfs(blocks, blocks, blocks, 0, 0, 4096, 255, 4096);
    }
}

/// Build a write handle for an existing file: an empty temp file plus the base
/// info needed to materialize current content lazily and to replace the entry.
async fn make_write_handle(inner: &Inner, ino: u64, nd: &NodeData) -> Result<WriteHandle> {
    let creds = inner.creds();
    let temp = temp_path(inner.config.spool_dir.as_deref());
    let file = tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&temp)
        .await?;
    let base_bucket = if nd.bucket.is_empty() {
        creds.bucket().to_string()
    } else {
        nd.bucket.clone()
    };
    let existing = if nd.uuid.is_empty() {
        None
    } else {
        Some(nd.uuid.clone())
    };
    Ok(WriteHandle {
        ino,
        temp_path: temp,
        file: tokio::sync::Mutex::new(file),
        materialized: tokio::sync::Mutex::new(false),
        dirty: AtomicBool::new(false),
        size: AtomicU64::new(nd.size),
        parent_uuid: nd.parent_uuid.clone(),
        plain: nd.plain_name.clone(),
        ftype: nd.file_type.clone(),
        bucket: creds.bucket().to_string(),
        mnemonic: creds.mnemonic().to_string(),
        net_user: creds.net_user().to_string(),
        net_pass: creds.net_pass().to_string(),
        existing_uuid: Mutex::new(existing),
        base_file_id: nd.file_id.clone(),
        base_bucket,
        base_size: nd.size,
    })
}

impl Drop for WriteHandle {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use internxt_core::models::{Credentials, UserInfo};
    use std::time::Instant;

    /// A minimal `Inner` — dummy credentials, no cache TTL, unlimited uploads —
    /// good enough to exercise the `pending_uploads` bookkeeping without
    /// touching the network (nothing here awaits anything network-bound).
    async fn test_inner() -> Arc<Inner> {
        let creds = Credentials {
            token: String::new(),
            user: UserInfo {
                email: String::new(),
                mnemonic: String::new(),
                bucket: String::new(),
                bridge_user: String::new(),
                user_id: String::new(),
                root_folder_id: String::new(),
                ecc_private_key: None,
                kyber_private_key: None,
            },
            workspace: None,
        };
        // `no_upload_limit: true` short-circuits before any network call.
        let upload_limit = crate::upload_limit::resolve(
            &crate::upload_limit::UploadLimitArgs {
                no_upload_limit: true,
                max_upload_size: None,
            },
            &creds,
        )
        .await
        .unwrap();
        Arc::new(Inner::new(
            Arc::new(SharedCreds::new(creds)),
            Arc::new(FolderCache::new(0)),
            String::new(),
            now_rfc3339(),
            None,
            upload_limit,
            MountConfig {
                mountpoint: PathBuf::new(),
                cache_ttl: 0,
                delete_permanently: false,
                spool_dir: None,
                read_only: false,
                allow_other: false,
            },
        ))
    }

    /// Regression test for the FUSE read-after-write race: `release()` fires
    /// `finalize_write` in a spawned task, but the kernel lets the writer's
    /// `close()` return before that task (and thus our reply) finishes — so a
    /// second `open()` for read can land while the upload is still in flight.
    /// Before this fix, `open()` built a `ReadHandle` straight from `nd.file_id`
    /// (still `None`/empty mid-upload), which failed reads with a raw server
    /// error ("File id is malformed"). This mirrors `release()`'s locking
    /// (register + hold the per-inode lock for the "upload") against what
    /// `open()`'s read path now does (wait on it before proceeding), with a
    /// sleep standing in for the network-bound upload — same style as
    /// `nfs::fs::tests::concurrent_write_during_simulated_upload_is_not_lost`.
    #[tokio::test]
    async fn read_open_waits_for_in_flight_finalize_before_racing_ahead() {
        let inner = test_inner().await;
        let ino = 42u64;

        let lock = inner.begin_pending_upload(ino);
        let lock_bg = lock.clone();
        let inner_bg = inner.clone();
        let finalize_task = tokio::spawn(async move {
            let _guard = lock_bg.lock().await;
            // Stands in for the network-bound upload `finalize_write` awaits.
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            drop(_guard);
            inner_bg.end_pending_upload(ino, &lock_bg);
        });

        // Let the "release" task grab the lock first, mirroring the real
        // race where finalize_write is already running by the time a second
        // open() lands.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;

        // What `open()`'s read path now does before building a `ReadHandle`.
        let start = Instant::now();
        if let Some(l) = inner.pending_upload(ino) {
            drop(l.lock().await);
        }
        let waited = start.elapsed();

        finalize_task.await.unwrap();

        assert!(
            waited >= std::time::Duration::from_millis(30),
            "a read-open racing an in-flight finalize_write must block until \
             it completes, not race ahead and read a stale/empty file_id"
        );
        assert!(
            inner.pending_upload(ino).is_none(),
            "the pending-upload marker must be cleared once finalize_write completes"
        );
    }

    /// `end_pending_upload` only clears the map entry it matches (`Arc::ptr_eq`),
    /// so a late/stale call from one finalize can't clobber a second write's
    /// still-active registration for the same inode (e.g. rapid overwrite).
    #[tokio::test]
    async fn end_pending_upload_does_not_clobber_a_newer_registration() {
        let inner = test_inner().await;
        let ino = 7u64;

        let first = inner.begin_pending_upload(ino);
        inner.end_pending_upload(ino, &first);
        assert!(inner.pending_upload(ino).is_none());

        let second = inner.begin_pending_upload(ino);
        // Stale call using the first (already-cleared) registration's Arc.
        inner.end_pending_upload(ino, &first);
        assert!(
            inner.pending_upload(ino).is_some(),
            "a stale end_pending_upload call must not remove a newer registration"
        );

        inner.end_pending_upload(ino, &second);
        assert!(inner.pending_upload(ino).is_none());
    }

    /// A minimal file `NodeData`, for tests exercising `failed_upload`
    /// bookkeeping without going through a real `create()`/`finalize_write`
    /// (which need a temp file and network).
    fn file_node(parent: u64, name: &str) -> NodeData {
        NodeData {
            uuid: "uuid-1".to_string(),
            parent,
            parent_uuid: String::new(),
            name: name.to_string(),
            kind: NodeKind::File,
            size: 0,
            bucket: String::new(),
            file_id: Some("file-1".to_string()),
            file_type: String::new(),
            plain_name: name.to_string(),
            updated_at: now_rfc3339(),
            failed_upload: None,
        }
    }

    /// Regression test for the silent-write-failure bug: `release()`'s
    /// `finalize_write` runs after the writer's `close()` has already
    /// returned success (see the module doc comment and `pending_uploads`),
    /// so a failure — quota, a transient network/server error, a
    /// concurrent-create 409, anything — was previously visible only in the
    /// server's own log. `record_finalize_result` (called from
    /// `finalize_write` around every real upload attempt) must record that
    /// failure on the node, `take_failed_upload` must surface it exactly
    /// once (mirroring what `open()`'s read-only path does), and a
    /// subsequent successful `finalize_write` must clear it so a fixed/
    /// rewritten file doesn't stay poisoned.
    #[tokio::test]
    async fn record_finalize_result_marks_failure_and_clears_on_next_success() {
        let inner = test_inner().await;
        let ino = inner.intern(ROOT_INO, file_node(ROOT_INO, "quota_test.bin"));

        let err: Result<()> = Err(anyhow!(
            "startUpload failed: HTTP 420 Bad Request \"Max space used\""
        ));
        inner.record_finalize_result(ino, &err);
        assert!(
            inner.node(ino).unwrap().failed_upload.is_some(),
            "a failed finalize_write must be recorded on the node"
        );

        // What open()'s read-only path does before serving/refusing a read.
        assert!(
            inner.take_failed_upload(ino),
            "the failure must be surfaced on the next read-only open()"
        );
        assert!(
            !inner.take_failed_upload(ino),
            "the surfaced failure is one-shot: a second open() must not see it again"
        );

        // Re-mark it, then simulate the user fixing the problem and
        // rewriting the file successfully.
        inner.record_finalize_result(ino, &err);
        assert!(inner.node(ino).unwrap().failed_upload.is_some());
        inner.record_finalize_result(ino, &Ok(()));
        assert!(
            inner.node(ino).unwrap().failed_upload.is_none(),
            "a subsequent successful finalize_write must clear the failure flag"
        );
    }

    /// `intern()` is called on every routine `lookup`/`readdir` to refresh a
    /// child's metadata from a live Drive listing — which has no notion of
    /// `failed_upload` and always supplies `None` for it. If that refresh
    /// clobbered the flag, practically any `ls` racing in between a failed
    /// upload and the next `open()` would erase the signal before it could
    /// ever be surfaced. It must not.
    #[tokio::test]
    async fn failed_upload_flag_survives_a_directory_listing_refresh() {
        let inner = test_inner().await;
        let name = "quota_test.bin";
        let ino = inner.intern(ROOT_INO, file_node(ROOT_INO, name));

        inner.record_finalize_result(ino, &Err(anyhow!("boom")));
        assert!(inner.node(ino).unwrap().failed_upload.is_some());

        let refreshed = inner.intern(ROOT_INO, file_node(ROOT_INO, name));
        assert_eq!(
            refreshed, ino,
            "the same (parent, name) must resolve to the same inode"
        );
        assert!(
            inner.node(ino).unwrap().failed_upload.is_some(),
            "intern()'s metadata refresh must not clear a pending failed_upload flag"
        );
    }
}

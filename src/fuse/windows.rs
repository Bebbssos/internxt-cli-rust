//! Windows half of the FUSE backend: mounts via WinFSP (through the MIT-licensed
//! `winfsp_wrs` wrapper — not SnowflakePowered's `winfsp`/`winfsp-sys`, which
//! are GPL-3.0 and wrong for this project). See README § FUSE/WinFSP mount
//! support for build/runtime requirements.
//!
//! Path-based (no inode table): WinFSP hands every callback a full path from
//! the mount root, so lookups go through the same shared Drive-tree walk
//! (`crate::serve::tree`) the WebDAV/SMB/SFTP backends already use, rather than
//! FUSE's parent-inode model — closest in shape to `sftp::fs`, not `unix::fs`.
//!
//! Unlike `fuser`, `winfsp_wrs`'s trait methods are synchronous — WinFSP's own
//! worker-thread pool calls them directly and expects a direct return, with no
//! deferred-reply mechanism. Every method here blocks on the shared tokio
//! runtime (`rt.block_on`) instead of spawning and replying later. One
//! consequence: unlike FUSE's `release`, `cleanup` here can't be fire-and-forget
//! — it blocks until the upload finishes, so the read-after-write race that
//! `unix::fs`'s `pending_uploads` works around can't happen on this backend.
//!
//! Whole-file model, same as every other backend: a write buffers to a temp
//! file and is uploaded whole when the last handle's `cleanup` fires.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use rand::RngExt;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, DuplexStream};
use tokio::runtime::Handle as RtHandle;
use winfsp_wrs::{
    init as winfsp_init, u16cstr, u16str, CleanupFlags, CreateFileInfo, CreateOptions, DirInfo,
    FileAccessRights, FileAttributes, FileContextKind, FileInfo, FileSystem, FileSystemInterface,
    OperationGuardStrategy, Params, PSecurityDescriptor, SecurityDescriptor, U16CStr, U16CString,
    VolumeInfo, WriteMode, NTSTATUS, STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY,
    STATUS_FILE_IS_A_DIRECTORY, STATUS_IO_DEVICE_ERROR, STATUS_MEDIA_WRITE_PROTECTED,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
};

use crate::fuse::MountConfig;
use internxt_core::api::DriveApi;
use internxt_core::models::Credentials;
use crate::serve::cache::FolderCache;
use crate::serve::creds::SharedCreds;
use crate::serve::recent_window::RecentWindow;
use crate::serve::tree::{self, FileItem};

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

pub(crate) fn log(msg: &str) {
    crate::serve::log::trace(msg);
}

/// RFC3339 -> Windows FILETIME (100ns intervals since 1601-01-01). Falls back
/// to "now" on parse failure so a bad timestamp never surfaces as a 0/invalid
/// file time to Explorer.
fn filetime(s: &str) -> u64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| winfsp_wrs::filetime_from_utc(dt.with_timezone(&chrono::Utc)))
        .unwrap_or_else(|_| winfsp_wrs::filetime_now())
}

/// Split a filename into (plainName, extension-without-dot). Mirrors the other
/// backends (`unix::fs::split_name`, `sftp::fs::split_name`).
fn split_name(name: &str) -> (String, String) {
    let p = std::path::Path::new(name);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
    if stem.is_empty() {
        (name.to_string(), String::new())
    } else {
        (stem.to_string(), ext.to_string())
    }
}

fn temp_path(dir: Option<&std::path::Path>) -> PathBuf {
    let mut rnd = [0u8; 16];
    rand::rng().fill(&mut rnd);
    let base = dir.map(|d| d.to_path_buf()).unwrap_or_else(std::env::temp_dir);
    base.join(format!("internxt-mount-{}.tmp", hex::encode(rnd)))
}

/// Split a WinFSP path (`\`-separated, absolute from the mount root) into
/// components. WinFSP always hands us normalized paths (no `.`/`..`).
fn path_components(p: &U16CStr) -> Vec<String> {
    p.to_string_lossy()
        .split('\\')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn is_drive_letter(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':'
}

/// A directory-listing entry: the plain Rust name kept alongside the packed
/// `DirInfo` (whose own `file_name` buffer isn't practical to read back out —
/// `DirInfo` has no accessor for it) so `read_directory`'s marker-based
/// pagination has something to sort/compare against.
fn dir_info(name: &str, updated_at: &str) -> (String, DirInfo) {
    let mut fi = FileInfo::default();
    let t = filetime(updated_at);
    fi.set_file_attributes(FileAttributes::DIRECTORY)
        .set_file_size(0)
        .set_allocation_size(0)
        .set_time(t)
        .set_hard_links(0);
    (name.to_string(), DirInfo::from_str(fi, name))
}

fn file_dir_info(name: &str, size: u64, updated_at: &str) -> (String, DirInfo) {
    let mut fi = FileInfo::default();
    let t = filetime(updated_at);
    fi.set_file_attributes(FileAttributes::NORMAL)
        .set_file_size(size)
        .set_allocation_size(size)
        .set_time(t)
        .set_hard_links(0);
    (name.to_string(), DirInfo::from_str(fi, name))
}

fn dir_file_info(updated_at: &str) -> FileInfo {
    let mut fi = FileInfo::default();
    let t = filetime(updated_at);
    fi.set_file_attributes(FileAttributes::DIRECTORY)
        .set_file_size(0)
        .set_allocation_size(0)
        .set_time(t)
        .set_hard_links(0);
    fi
}

fn plain_file_info(size: u64, updated_at: &str) -> FileInfo {
    let mut fi = FileInfo::default();
    let t = filetime(updated_at);
    fi.set_file_attributes(FileAttributes::NORMAL)
        .set_file_size(size)
        .set_allocation_size(size)
        .set_time(t)
        .set_hard_links(0);
    fi
}

/// Map an internal error to an NTSTATUS. Same catch-all posture as the other
/// backends (`unix::fs::errno`, `sftp::fs::to_status`) — no fine-grained
/// mapping, just a generic I/O failure logged for diagnosis.
fn err_status(e: &anyhow::Error) -> NTSTATUS {
    crate::serve::log::warn(&format!("[mount] error: {e:#}"));
    STATUS_IO_DEVICE_ERROR
}

// ---------------------------------------------------------------------------
// Read handle: sequential streaming reader (ported from unix::fs / sftp::fs)
// ---------------------------------------------------------------------------

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

struct ReadState {
    file_id: String,
    bucket: String,
    mnemonic: String,
    net_user: String,
    net_pass: String,
    size: u64,
    stream: tokio::sync::Mutex<Option<ReadStream>>,
    recent: tokio::sync::Mutex<RecentWindow>,
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
                crate::serve::log::warn(&format!("[mount] read stream error: {e:#}"));
            }
            let _ = writer.shutdown().await;
        });
        ReadStream { reader, pos: start, task }
    }

    async fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if self.file_id.is_empty() || offset >= self.size {
            return Ok(Vec::new());
        }
        if let Some(buf) = self.recent.lock().await.read_full(offset, len) {
            return Ok(buf);
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
                self.recent.lock().await.push(&scratch[..n], stream.pos);
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
        self.recent.lock().await.push(&buf, offset);
        stream.pos += filled as u64;
        Ok(buf)
    }
}

// ---------------------------------------------------------------------------
// Write handle: temp-file-backed whole-file model (ported from unix::fs / sftp::fs)
// ---------------------------------------------------------------------------

struct WriteState {
    temp_path: PathBuf,
    file: tokio::sync::Mutex<tokio::fs::File>,
    materialized: tokio::sync::Mutex<bool>,
    dirty: AtomicBool,
    finalized: AtomicBool,
    size: AtomicU64,
    bucket: String,
    mnemonic: String,
    net_user: String,
    net_pass: String,
    base_file_id: Option<String>,
    base_bucket: String,
    base_size: u64,
}

impl WriteState {
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
                    &net, &self.mnemonic, &self.base_bucket, fid, &mut *f, None,
                )
                .await?;
                f.flush().await?;
                self.size.store(self.base_size, Ordering::SeqCst);
            }
        }
        *done = true;
        Ok(())
    }

    /// Reset to empty (overwrite/truncate-on-open): skip materializing old
    /// content, start dirty so an empty entry still gets uploaded on cleanup.
    async fn reset_to_empty(&self) -> Result<()> {
        *self.materialized.lock().await = true;
        {
            let f = self.file.lock().await;
            f.set_len(0).await?;
        }
        self.size.store(0, Ordering::SeqCst);
        self.dirty.store(true, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for WriteState {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

// ---------------------------------------------------------------------------
// File context: what WinFSP hands back to us on every call after open/create
// ---------------------------------------------------------------------------

/// Mutable bookkeeping on an open context: the Drive identity of the item,
/// refreshed after a rename or a successful upload so later calls on the same
/// handle (`get_file_info`, a second `cleanup`) see the current uuid/parent.
struct CtxState {
    /// Drive uuid; empty for a not-yet-uploaded pending file.
    uuid: String,
    parent_uuid: String,
    plain_name: String,
    file_type: String,
    size: u64,
    updated_at: String,
}

enum Body {
    Dir(Vec<(String, DirInfo)>),
    Read(ReadState),
    Write(WriteState),
}

struct WinCtx {
    is_dir: bool,
    state: Mutex<CtxState>,
    body: Body,
}

impl WinCtx {
    fn file_info(&self) -> FileInfo {
        let st = self.state.lock().unwrap();
        if self.is_dir {
            dir_file_info(&st.updated_at)
        } else {
            let size = match &self.body {
                Body::Write(ws) => ws.size.load(Ordering::SeqCst),
                _ => st.size,
            };
            plain_file_info(size, &st.updated_at)
        }
    }
}

// ---------------------------------------------------------------------------
// Shared backend state
// ---------------------------------------------------------------------------

struct Inner {
    creds: Arc<SharedCreds>,
    cache: Arc<FolderCache>,
    config: MountConfig,
    root_folder: String,
    root_updated_at: String,
    upload_sem: Option<Arc<tokio::sync::Semaphore>>,
    upload_limit: crate::upload_limit::UploadLimit,
    /// Static full-access descriptor handed back for every `get_security`
    /// call — Drive has no per-user ACL concept, same posture as `unix::fs`
    /// reporting fixed 0o755/0o644 owned by the mounting user.
    security: SecurityDescriptor,
}

impl Inner {
    fn creds(&self) -> Arc<Credentials> {
        self.creds.get()
    }

    async fn acquire_upload(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        match &self.upload_sem {
            Some(s) => s.clone().acquire_owned().await.ok(),
            None => None,
        }
    }

    /// Resolve the folder at `components` from the mount root.
    async fn resolve_dir(&self, components: &[String]) -> Result<tree::FolderItem, NTSTATUS> {
        let creds = self.creds();
        let api = DriveApi::for_credentials(&creds);
        tree::resolve_folder(
            &api,
            &creds.token,
            &self.root_folder,
            &self.root_updated_at,
            components,
            &self.cache,
        )
        .await
        .map_err(|e| err_status(&e))?
        .ok_or(STATUS_OBJECT_NAME_NOT_FOUND)
    }

    /// Build a directory context: fetch + sort the listing once, same shape as
    /// `unix::fs::build_dir_entries`/`sftp::fs::opendir`.
    async fn open_dir(&self, uuid: &str, updated_at: &str) -> Result<Arc<WinCtx>, NTSTATUS> {
        let creds = self.creds();
        let api = DriveApi::for_credentials(&creds);
        let (folders, files) = tokio::try_join!(
            tree::list_folders(&api, &creds.token, uuid, &self.cache),
            tree::list_files_cached(&api, &creds.token, uuid, &self.cache),
        )
        .map_err(|e| err_status(&e))?;

        let mut entries: Vec<(String, DirInfo)> = Vec::with_capacity(folders.len() + files.len() + 2);
        entries.push(dir_info(".", updated_at));
        entries.push(dir_info("..", updated_at));
        for f in &folders {
            entries.push(dir_info(&f.plain_name, &f.updated_at));
        }
        for f in &files {
            entries.push(file_dir_info(&f.display_name(), f.size, &f.updated_at));
        }
        // WinFSP's marker-based pagination needs entries in ascending name
        // order (see `read_directory`); "." and ".." stay pinned first.
        entries[2..].sort_by(|a, b| a.0.cmp(&b.0));

        Ok(Arc::new(WinCtx {
            is_dir: true,
            state: Mutex::new(CtxState {
                uuid: uuid.to_string(),
                parent_uuid: String::new(),
                plain_name: String::new(),
                file_type: String::new(),
                size: 0,
                updated_at: updated_at.to_string(),
            }),
            body: Body::Dir(entries),
        }))
    }

    fn open_read(&self, creds: &Credentials, f: &FileItem) -> Body {
        let bucket = if f.bucket.is_empty() {
            creds.bucket().to_string()
        } else {
            f.bucket.clone()
        };
        Body::Read(ReadState {
            file_id: f.file_id.clone().unwrap_or_default(),
            bucket,
            mnemonic: creds.mnemonic().to_string(),
            net_user: creds.net_user().to_string(),
            net_pass: creds.net_pass().to_string(),
            size: f.size,
            stream: tokio::sync::Mutex::new(None),
            recent: tokio::sync::Mutex::new(RecentWindow::new(self.config.recent_window)),
        })
    }

    /// Build a temp-file-backed write body. `existing = None` for a brand-new
    /// file (starts empty, dirty); `Some` for an existing file opened for
    /// write (materializes lazily).
    async fn open_write(
        &self,
        creds: &Credentials,
        existing: Option<&FileItem>,
    ) -> Result<Body, NTSTATUS> {
        let temp = temp_path(self.config.spool_dir.as_deref());
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .await
            .map_err(|_| STATUS_IO_DEVICE_ERROR)?;
        let brand_new = existing.is_none();
        let (base_file_id, base_bucket, base_size) = match existing {
            Some(f) => {
                let base_bucket = if f.bucket.is_empty() {
                    creds.bucket().to_string()
                } else {
                    f.bucket.clone()
                };
                (f.file_id.clone(), base_bucket, f.size)
            }
            None => (None, creds.bucket().to_string(), 0),
        };
        Ok(Body::Write(WriteState {
            temp_path: temp,
            file: tokio::sync::Mutex::new(file),
            materialized: tokio::sync::Mutex::new(brand_new),
            dirty: AtomicBool::new(brand_new),
            finalized: AtomicBool::new(false),
            size: AtomicU64::new(if brand_new { 0 } else { existing.unwrap().size }),
            bucket: creds.bucket().to_string(),
            mnemonic: creds.mnemonic().to_string(),
            net_user: creds.net_user().to_string(),
            net_pass: creds.net_pass().to_string(),
            base_file_id,
            base_bucket,
            base_size,
        }))
    }

    /// Finalize a write context: upload the temp file whole and replace/create
    /// the Drive entry. No-op when nothing was written. Updates `ctx.state` so
    /// a subsequent `get_file_info`/`cleanup` on the same handle sees the
    /// result. Unlike `unix::fs::finalize_write`, this runs to completion
    /// before `cleanup` returns — WinFSP callbacks are synchronous, so there's
    /// no fire-and-forget window for a racing open to land in.
    async fn finalize(&self, ctx: &WinCtx, ws: &WriteState) -> Result<()> {
        if ws.finalized.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if !ws.dirty.load(Ordering::SeqCst) {
            return Ok(());
        }
        {
            let mut f = ws.file.lock().await;
            f.flush().await?;
        }
        let size = ws.size.load(Ordering::SeqCst);
        self.upload_limit.check(size)?;

        let creds = self.creds();
        let token = &creds.token;
        let api = DriveApi::for_credentials(&creds);
        let net = crate::net_client::network_api(&ws.net_user, &ws.net_pass);

        let _permit = self.acquire_upload().await;
        let file_id = if size == 0 {
            String::new()
        } else {
            internxt_core::transfer::upload_file_to_network(
                &net, &ws.bucket, &ws.mnemonic, &ws.temp_path, size, None,
            )
            .await?
        };
        let now = now_rfc3339();
        let (parent_uuid, plain, ftype, old_uuid) = {
            let st = ctx.state.lock().unwrap();
            (
                st.parent_uuid.clone(),
                st.plain_name.clone(),
                st.file_type.clone(),
                (!st.uuid.is_empty()).then(|| st.uuid.clone()),
            )
        };
        let result_uuid = match old_uuid {
            Some(uuid) => api.replace_file(token, &uuid, &file_id, size).await?.uuid,
            None => {
                api.create_file_entry(
                    token, &plain, &ftype, size, &parent_uuid, &file_id, &ws.bucket, &now, &now,
                )
                .await?
                .uuid
            }
        };

        crate::serve::thumbnail::upload_thumbnail_best_effort(
            &net, &api, token, &ws.bucket, &ws.mnemonic, &result_uuid, &ftype, &ws.temp_path,
            size, "mount",
        )
        .await;

        {
            let mut st = ctx.state.lock().unwrap();
            st.uuid = result_uuid;
            st.size = size;
            st.updated_at = now;
        }
        self.cache.invalidate(&parent_uuid);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The `FileSystemInterface` implementation
// ---------------------------------------------------------------------------

struct InxtWinFs {
    inner: Arc<Inner>,
    rt: RtHandle,
}

impl InxtWinFs {
    /// Resolve `file_name` to (parent components, name, parent uuid) — the
    /// shape every mutating op (create/open/rename/delete) needs. Errors with
    /// `STATUS_OBJECT_NAME_NOT_FOUND` if the parent doesn't exist.
    async fn resolve_parent(&self, comps: &[String]) -> Result<(tree::FolderItem, String), NTSTATUS> {
        let (parent_comps, name) = comps.split_at(comps.len() - 1);
        let parent = self.inner.resolve_dir(parent_comps).await?;
        Ok((parent, name[0].clone()))
    }
}

impl FileSystemInterface for InxtWinFs {
    type FileContext = Arc<WinCtx>;

    const GET_VOLUME_INFO_DEFINED: bool = true;
    const CREATE_EX_DEFINED: bool = true;
    const OPEN_DEFINED: bool = true;
    const OVERWRITE_DEFINED: bool = true;
    const CLEANUP_DEFINED: bool = true;
    const CLOSE_DEFINED: bool = true;
    const READ_DEFINED: bool = true;
    const WRITE_DEFINED: bool = true;
    const FLUSH_DEFINED: bool = true;
    const GET_FILE_INFO_DEFINED: bool = true;
    const SET_BASIC_INFO_DEFINED: bool = true;
    const SET_FILE_SIZE_DEFINED: bool = true;
    const CAN_DELETE_DEFINED: bool = true;
    const RENAME_DEFINED: bool = true;
    const GET_SECURITY_DEFINED: bool = true;
    const READ_DIRECTORY_DEFINED: bool = true;

    fn get_volume_info(&self) -> Result<VolumeInfo, NTSTATUS> {
        // Large, mostly-free volume (Internxt reports quota elsewhere) — same
        // posture as `unix::fs::statfs`.
        VolumeInfo::new(1u64 << 50, 1u64 << 50, u16str!("Internxt Drive"))
            .map_err(|_| STATUS_IO_DEVICE_ERROR)
    }

    fn create_ex(
        &self,
        file_name: &U16CStr,
        create_file_info: CreateFileInfo,
        _security_descriptor: SecurityDescriptor,
        _buffer: &[u8],
        _extra_buffer_is_reparse_point: bool,
    ) -> Result<(Self::FileContext, FileInfo), NTSTATUS> {
        let comps = path_components(file_name);
        log(&format!("[CREATE] \\{}", comps.join("\\")));
        if self.inner.config.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        if comps.is_empty() {
            return Err(STATUS_OBJECT_NAME_COLLISION);
        }
        self.rt.block_on(async {
            let (parent, name) = self.resolve_parent(&comps).await?;
            let creds = self.inner.creds();
            let api = DriveApi::for_credentials(&creds);

            if create_file_info.create_options.is(CreateOptions::FILE_DIRECTORY_FILE) {
                if tree::find_folder(&api, &creds.token, &parent.uuid, &name, &self.inner.cache)
                    .await
                    .map_err(|e| err_status(&e))?
                    .is_some()
                {
                    return Err(STATUS_OBJECT_NAME_COLLISION);
                }
                let created = api
                    .create_folder(&creds.token, &name, &parent.uuid)
                    .await
                    .map_err(|e| err_status(&e))?;
                self.inner.cache.invalidate(&parent.uuid);
                let uuid = created
                    .get("uuid")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string();
                let updated_at = now_rfc3339();
                let ctx = Arc::new(WinCtx {
                    is_dir: true,
                    state: Mutex::new(CtxState {
                        uuid,
                        parent_uuid: parent.uuid.clone(),
                        plain_name: name,
                        file_type: String::new(),
                        size: 0,
                        updated_at: updated_at.clone(),
                    }),
                    body: Body::Dir(vec![dir_info(".", &updated_at), dir_info("..", &updated_at)]),
                });
                let fi = ctx.file_info();
                Ok((ctx, fi))
            } else {
                let (plain, ftype) = split_name(&name);
                let body = self.inner.open_write(&creds, None).await?;
                let updated_at = now_rfc3339();
                let ctx = Arc::new(WinCtx {
                    is_dir: false,
                    state: Mutex::new(CtxState {
                        uuid: String::new(),
                        parent_uuid: parent.uuid.clone(),
                        plain_name: plain,
                        file_type: ftype,
                        size: 0,
                        updated_at,
                    }),
                    body,
                });
                let fi = ctx.file_info();
                Ok((ctx, fi))
            }
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: CreateOptions,
        granted_access: FileAccessRights,
    ) -> Result<(Self::FileContext, FileInfo), NTSTATUS> {
        let comps = path_components(file_name);
        log(&format!("[OPEN] \\{}", comps.join("\\")));
        self.rt.block_on(async {
            if comps.is_empty() {
                let ctx = self
                    .inner
                    .open_dir(&self.inner.root_folder, &self.inner.root_updated_at)
                    .await?;
                let fi = ctx.file_info();
                return Ok((ctx, fi));
            }
            let (parent, name) = self.resolve_parent(&comps).await?;
            let creds = self.inner.creds();
            let api = DriveApi::for_credentials(&creds);

            if let Some(f) =
                tree::find_folder(&api, &creds.token, &parent.uuid, &name, &self.inner.cache)
                    .await
                    .map_err(|e| err_status(&e))?
            {
                let ctx = self.inner.open_dir(&f.uuid, &f.updated_at).await?;
                {
                    let mut st = ctx.state.lock().unwrap();
                    st.parent_uuid = parent.uuid.clone();
                    st.plain_name = f.plain_name.clone();
                }
                let fi = ctx.file_info();
                return Ok((ctx, fi));
            }
            let Some(f) = tree::find_file(&api, &creds.token, &parent.uuid, &name, &self.inner.cache)
                .await
                .map_err(|e| err_status(&e))?
            else {
                return Err(STATUS_OBJECT_NAME_NOT_FOUND);
            };
            let want_write = granted_access.is(FileAccessRights::FILE_WRITE_DATA);
            if want_write && self.inner.config.read_only {
                return Err(STATUS_MEDIA_WRITE_PROTECTED);
            }
            let body = if want_write {
                self.inner.open_write(&creds, Some(&f)).await?
            } else {
                self.inner.open_read(&creds, &f)
            };
            let ctx = Arc::new(WinCtx {
                is_dir: false,
                state: Mutex::new(CtxState {
                    uuid: f.uuid,
                    parent_uuid: parent.uuid.clone(),
                    plain_name: f.plain_name,
                    file_type: f.file_type,
                    size: f.size,
                    updated_at: f.updated_at,
                }),
                body,
            });
            let fi = ctx.file_info();
            Ok((ctx, fi))
        })
    }

    fn overwrite(
        &self,
        file_context: Self::FileContext,
        _file_attributes: FileAttributes,
        _replace_file_attributes: bool,
        _allocation_size: u64,
    ) -> Result<FileInfo, NTSTATUS> {
        if self.inner.config.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        let Body::Write(ws) = &file_context.body else {
            return Err(STATUS_FILE_IS_A_DIRECTORY);
        };
        self.rt
            .block_on(ws.reset_to_empty())
            .map_err(|_| STATUS_IO_DEVICE_ERROR)?;
        Ok(file_context.file_info())
    }

    fn cleanup(&self, file_context: Self::FileContext, _file_name: Option<&U16CStr>, flags: CleanupFlags) {
        self.rt.block_on(async {
            if flags.is(CleanupFlags::DELETE) {
                let (uuid, parent_uuid) = {
                    let st = file_context.state.lock().unwrap();
                    (st.uuid.clone(), st.parent_uuid.clone())
                };
                if uuid.is_empty() {
                    return;
                }
                let creds = self.inner.creds();
                let api = DriveApi::for_credentials(&creds);
                let item_type = if file_context.is_dir { "folder" } else { "file" };
                let res = if self.inner.config.delete_permanently {
                    if file_context.is_dir {
                        api.delete_folder(&creds.token, &uuid).await
                    } else {
                        api.delete_file(&creds.token, &uuid).await
                    }
                } else {
                    api.trash_items(
                        &creds.token,
                        serde_json::json!([{ "uuid": uuid, "type": item_type }]),
                    )
                    .await
                };
                match res {
                    Ok(_) => {
                        self.inner.cache.invalidate(&parent_uuid);
                        if file_context.is_dir {
                            self.inner.cache.invalidate(&uuid);
                        }
                    }
                    Err(e) => crate::serve::log::warn(&format!("[mount] delete failed: {e:#}")),
                }
                return;
            }
            if let Body::Write(ws) = &file_context.body {
                if let Err(e) = self.inner.finalize(&file_context, ws).await {
                    crate::serve::log::warn(&format!("[mount] upload failed: {e:#}"));
                }
            }
        });
    }

    fn close(&self, _file_context: Self::FileContext) {
        // Dropping `file_context` (the last strong ref) here is what actually
        // runs `WriteState`'s `Drop` (temp-file cleanup) — nothing else to do.
    }

    fn read(&self, file_context: Self::FileContext, buffer: &mut [u8], offset: u64) -> Result<usize, NTSTATUS> {
        self.rt.block_on(async {
            match &file_context.body {
                Body::Read(rs) => {
                    let data = rs.read_at(offset, buffer.len()).await.map_err(|e| err_status(&e))?;
                    buffer[..data.len()].copy_from_slice(&data);
                    Ok(data.len())
                }
                Body::Write(ws) => {
                    ws.ensure_materialized().await.map_err(|e| err_status(&e))?;
                    let mut f = ws.file.lock().await;
                    f.seek(std::io::SeekFrom::Start(offset)).await.map_err(|_| STATUS_IO_DEVICE_ERROR)?;
                    let mut filled = 0;
                    while filled < buffer.len() {
                        let n = f.read(&mut buffer[filled..]).await.map_err(|_| STATUS_IO_DEVICE_ERROR)?;
                        if n == 0 {
                            break;
                        }
                        filled += n;
                    }
                    Ok(filled)
                }
                Body::Dir(_) => Err(STATUS_FILE_IS_A_DIRECTORY),
            }
        })
    }

    fn write(&self, file_context: Self::FileContext, buffer: &[u8], mode: WriteMode) -> Result<(usize, FileInfo), NTSTATUS> {
        let Body::Write(ws) = &file_context.body else {
            return Err(STATUS_ACCESS_DENIED);
        };
        self.rt.block_on(async {
            ws.ensure_materialized().await.map_err(|_| STATUS_IO_DEVICE_ERROR)?;
            let offset = match mode {
                WriteMode::Normal { offset } | WriteMode::ConstrainedIO { offset } => offset,
                WriteMode::WriteToEOF => ws.size.load(Ordering::SeqCst),
            };
            let Some(end) = offset.checked_add(buffer.len() as u64) else {
                return Err(STATUS_IO_DEVICE_ERROR);
            };
            if matches!(mode, WriteMode::ConstrainedIO { .. }) && end > ws.size.load(Ordering::SeqCst) {
                // Must not extend the file — trim to what fits.
                let cap = ws.size.load(Ordering::SeqCst).saturating_sub(offset) as usize;
                let buffer = &buffer[..cap.min(buffer.len())];
                if buffer.is_empty() {
                    return Ok((0, file_context.file_info()));
                }
            }
            if self.inner.upload_limit.check(end).is_err() {
                return Err(STATUS_IO_DEVICE_ERROR);
            }
            {
                let mut f = ws.file.lock().await;
                f.seek(std::io::SeekFrom::Start(offset)).await.map_err(|_| STATUS_IO_DEVICE_ERROR)?;
                f.write_all(buffer).await.map_err(|_| STATUS_IO_DEVICE_ERROR)?;
            }
            ws.size.fetch_max(end, Ordering::SeqCst);
            ws.dirty.store(true, Ordering::SeqCst);
            Ok((buffer.len(), file_context.file_info()))
        })
    }

    fn flush(&self, file_context: Self::FileContext) -> Result<FileInfo, NTSTATUS> {
        // Uploads happen on cleanup; flush is a no-op (may be called many times) —
        // same posture as `unix::fs::flush`.
        Ok(file_context.file_info())
    }

    fn get_file_info(&self, file_context: Self::FileContext) -> Result<FileInfo, NTSTATUS> {
        Ok(file_context.file_info())
    }

    fn set_basic_info(
        &self,
        file_context: Self::FileContext,
        _file_attributes: FileAttributes,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _change_time: u64,
    ) -> Result<FileInfo, NTSTATUS> {
        // Drive has no arbitrary attribute/timestamp storage; accept silently
        // (same posture as `unix::fs::setattr`'s ignored uid/gid/mode/time).
        Ok(file_context.file_info())
    }

    fn set_file_size(
        &self,
        file_context: Self::FileContext,
        new_size: u64,
        _set_allocation_size: bool,
    ) -> Result<FileInfo, NTSTATUS> {
        if self.inner.config.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        let Body::Write(ws) = &file_context.body else {
            return Err(STATUS_ACCESS_DENIED);
        };
        self.rt.block_on(async {
            if new_size == 0 {
                *ws.materialized.lock().await = true;
            } else {
                ws.ensure_materialized().await.map_err(|_| STATUS_IO_DEVICE_ERROR)?;
            }
            {
                let f = ws.file.lock().await;
                f.set_len(new_size).await.map_err(|_| STATUS_IO_DEVICE_ERROR)?;
            }
            ws.size.store(new_size, Ordering::SeqCst);
            ws.dirty.store(true, Ordering::SeqCst);
            Ok(file_context.file_info())
        })
    }

    fn can_delete(&self, file_context: Self::FileContext, _file_name: &U16CStr) -> Result<(), NTSTATUS> {
        if self.inner.config.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        if file_context.is_dir {
            if let Body::Dir(entries) = &file_context.body {
                // Only "." and ".." — an empty directory.
                if entries.len() > 2 {
                    return Err(STATUS_DIRECTORY_NOT_EMPTY);
                }
            }
        }
        Ok(())
    }

    fn rename(
        &self,
        file_context: Self::FileContext,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> Result<(), NTSTATUS> {
        if self.inner.config.read_only {
            return Err(STATUS_MEDIA_WRITE_PROTECTED);
        }
        let src = path_components(file_name);
        let dst = path_components(new_file_name);
        if src.is_empty() || dst.is_empty() {
            return Err(STATUS_ACCESS_DENIED);
        }
        log(&format!("[RENAME] \\{} -> \\{}", src.join("\\"), dst.join("\\")));
        self.rt.block_on(async {
            let (_, dst_name) = dst.split_at(dst.len() - 1);
            let dst_name = dst_name[0].clone();
            let dst_parent_comps = &dst[..dst.len() - 1];
            let dst_parent = self.inner.resolve_dir(dst_parent_comps).await?;
            let creds = self.inner.creds();
            let api = DriveApi::for_credentials(&creds);

            if !replace_if_exists {
                let clash_file =
                    tree::find_file(&api, &creds.token, &dst_parent.uuid, &dst_name, &self.inner.cache)
                        .await
                        .map_err(|e| err_status(&e))?
                        .is_some();
                let clash_dir =
                    tree::find_folder(&api, &creds.token, &dst_parent.uuid, &dst_name, &self.inner.cache)
                        .await
                        .map_err(|e| err_status(&e))?
                        .is_some();
                if clash_file || clash_dir {
                    return Err(STATUS_OBJECT_NAME_COLLISION);
                }
            }

            let (uuid, old_parent_uuid) = {
                let st = file_context.state.lock().unwrap();
                (st.uuid.clone(), st.parent_uuid.clone())
            };
            if uuid.is_empty() {
                // Not-yet-uploaded pending file: nothing on Drive to rename yet,
                // just relabel the local context for the eventual upload.
                let mut st = file_context.state.lock().unwrap();
                let (plain, ftype) = split_name(&dst_name);
                st.parent_uuid = dst_parent.uuid;
                st.plain_name = plain;
                st.file_type = ftype;
                return Ok(());
            }

            if old_parent_uuid != dst_parent.uuid {
                if file_context.is_dir {
                    api.move_folder(&creds.token, &uuid, &dst_parent.uuid).await.map_err(|e| err_status(&e))?;
                } else {
                    api.move_file(&creds.token, &uuid, &dst_parent.uuid).await.map_err(|e| err_status(&e))?;
                }
            }
            let cur_name = {
                let st = file_context.state.lock().unwrap();
                if file_context.is_dir {
                    st.plain_name.clone()
                } else if st.file_type.is_empty() {
                    st.plain_name.clone()
                } else {
                    format!("{}.{}", st.plain_name, st.file_type)
                }
            };
            if cur_name != dst_name {
                if file_context.is_dir {
                    api.rename_folder(&creds.token, &uuid, &dst_name).await.map_err(|e| err_status(&e))?;
                } else {
                    let (plain, ftype) = split_name(&dst_name);
                    api.rename_file(&creds.token, &uuid, &plain, &ftype).await.map_err(|e| err_status(&e))?;
                }
            }
            self.inner.cache.invalidate(&old_parent_uuid);
            self.inner.cache.invalidate(&dst_parent.uuid);
            {
                let mut st = file_context.state.lock().unwrap();
                let (plain, ftype) = split_name(&dst_name);
                st.parent_uuid = dst_parent.uuid;
                st.plain_name = plain;
                st.file_type = ftype;
            }
            Ok(())
        })
    }

    fn get_security(&self, _file_context: Self::FileContext) -> Result<PSecurityDescriptor, NTSTATUS> {
        Ok(self.inner.security.as_ptr())
    }

    fn read_directory(
        &self,
        file_context: Self::FileContext,
        marker: Option<&U16CStr>,
        mut add_dir_info: impl FnMut(DirInfo) -> bool,
    ) -> Result<(), NTSTATUS> {
        let Body::Dir(entries) = &file_context.body else {
            return Err(STATUS_NOT_A_DIRECTORY);
        };
        let marker = marker.map(|m| m.to_string_lossy());
        for (name, entry) in entries {
            if let Some(m) = &marker {
                if name.as_str() <= m.as_str() {
                    continue;
                }
            }
            if !add_dir_info(*entry) {
                break;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn serve(
    shared: Arc<crate::serve::run::Shared>,
    config: MountConfig,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> Result<()> {
    winfsp_init().map_err(|e| {
        anyhow::anyhow!(
            "{e} — WinFsp doesn't appear to be installed. See README § FUSE/WinFSP \
             mount support, or install it from https://winfsp.dev/."
        )
    })?;
    if let Some(dir) = &config.spool_dir {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("spool directory {} is not usable: {e}", dir.display()))?;
    }
    let mountpoint_str = config.mountpoint.to_string_lossy().to_string();
    if !is_drive_letter(&mountpoint_str) && !config.mountpoint.is_dir() {
        return Err(anyhow::anyhow!(
            "mountpoint {mountpoint_str} does not exist or is not a directory (and isn't a drive letter like X:)"
        ));
    }

    let read_only = config.read_only;
    let sddl = u16cstr!("O:BAG:BAD:P(A;;FA;;;WD)");
    let security = SecurityDescriptor::from_wstr(sddl)
        .map_err(|e| anyhow::anyhow!("failed to build the mount's security descriptor: {e}"))?;

    let inner = Arc::new(Inner {
        creds: shared.creds.clone(),
        cache: shared.cache.clone(),
        root_folder: shared.root_folder.clone(),
        root_updated_at: shared.root_updated_at.clone(),
        upload_sem: shared.upload_sem.clone(),
        upload_limit: shared.upload_limit,
        config,
        security,
    });
    let fs_impl = InxtWinFs { inner: inner.clone(), rt: tokio::runtime::Handle::current() };

    let mut params = Params::default();
    params
        .volume_params
        .set_sector_size(4096)
        .set_sectors_per_allocation_unit(1)
        .set_case_sensitive_search(false)
        .set_case_preserved_names(true)
        .set_unicode_on_disk(true)
        .set_persistent_acls(false)
        .set_read_only_volume(read_only)
        .set_file_info_timeout((inner.config.cache_ttl * 1000).min(u32::MAX as u64) as u32)
        .set_volume_info_timeout((inner.config.cache_ttl * 1000).min(u32::MAX as u64) as u32)
        .set_dir_info_timeout((inner.config.cache_ttl * 1000).min(u32::MAX as u64) as u32);
    let _ = params.volume_params.set_file_system_name(u16cstr!("INXT"));
    params.guard_strategy = OperationGuardStrategy::Coarse;

    let mountpoint = U16CString::from_str(&mountpoint_str)
        .map_err(|e| anyhow::anyhow!("mountpoint {mountpoint_str} is not representable: {e}"))?;

    let host = FileSystem::start(params, Some(&mountpoint), fs_impl).map_err(|status| {
        anyhow::anyhow!(
            "failed to mount at {mountpoint_str}: WinFSP status {status:#x} (no WinFsp \
             driver found, or the mountpoint is in use — see README § FUSE/WinFSP mount support)"
        )
    })?;

    crate::output::status(&format!(
        "Internxt Drive mounted at {mountpoint_str}{}",
        if read_only { " (read-only)" } else { "" }
    ));

    shutdown.await;
    crate::output::status(&format!("\nUnmounting {mountpoint_str}."));
    tokio::task::spawn_blocking(move || host.stop()).await?;
    Ok(())
}

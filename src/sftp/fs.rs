//! Drive-backed SSH + SFTP handlers for `russh` / `russh-sftp`.
//!
//! `russh` owns the SSH transport, key exchange and auth; `russh-sftp` owns the
//! SFTP subsystem wire protocol. We implement `russh::server::Handler` (accept a
//! session, hand the `sftp` subsystem to the SFTP loop) and
//! `russh_sftp::server::Handler` over Internxt Drive — the SFTP analog of
//! `fuser::Filesystem`.
//!
//! Path-based (no inode table): every op re-resolves the path through the shared
//! folder tree (`crate::serve::tree`), which the short-TTL `FolderCache` keeps
//! cheap. SFTP has explicit open/close, so writes reuse the SMB/FUSE whole-file
//! temp model (materialize existing lazily, upload whole on `close`, replace the
//! Drive entry) and reads reuse the sequential-streaming reader (forward-skip,
//! backward-seek → ranged download).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result as AnyResult;
use rand::RngExt;
use russh::keys::PublicKey;
use russh::server::{Auth, ChannelOpenHandle, Handler as SshHandler, Msg, Server, Session};
use russh::{Channel, ChannelId};
use russh_sftp::protocol::{
    Attrs, Data, File, FileAttributes, Handle as SftpHandle, Name, OpenFlags, Status, StatusCode,
    Version,
};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, DuplexStream};

use internxt_core::api::DriveApi;
use internxt_core::models::Credentials;
use crate::serve::cache::FolderCache;
use crate::serve::creds::SharedCreds;
use crate::serve::tree::{self, FileItem, FolderItem};

/// Full `st_mode` for a regular file (type bits | rw-r--r--).
const MODE_FILE: u32 = 0o100_644;
/// Full `st_mode` for a directory (type bits | rwxr-xr-x).
const MODE_DIR: u32 = 0o40_755;

pub(crate) fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Per-op request trace. Verbose-only: printed just when `--verbose` is set.
pub(crate) fn log(msg: &str) {
    crate::serve::log::trace(msg);
}

/// Map an internal (anyhow) error to an SFTP status. Network/API/IO failures all
/// surface as a generic failure.
fn to_status(e: anyhow::Error) -> StatusCode {
    crate::serve::log::warn(&format!("[sftp] error: {e:#}"));
    StatusCode::Failure
}

/// RFC3339 → unix seconds (u32), clamped. Falls back to 0 on parse failure.
fn rfc3339_to_unix(s: &str) -> u32 {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp().clamp(0, u32::MAX as i64) as u32)
        .unwrap_or(0)
}

/// Split a filename into (plainName, extension-without-dot). A leading-dot name
/// like `.env` is treated as having no extension. Mirrors the other backends.
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
    base.join(format!("internxt-sftp-{}.tmp", hex::encode(rnd)))
}

/// Normalize an SFTP path into folder-name components, resolving `.`/`..` and
/// dropping empty segments. The SFTP namespace is absolute; a client's relative
/// `.` resolves to the share root (empty components).
fn norm_components(path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s.to_string()),
        }
    }
    out
}

fn attrs_file(size: u64, updated_at: &str) -> FileAttributes {
    let t = rfc3339_to_unix(updated_at);
    FileAttributes {
        size: Some(size),
        uid: Some(0),
        user: None,
        gid: Some(0),
        group: None,
        permissions: Some(MODE_FILE),
        atime: Some(t),
        mtime: Some(t),
    }
}

fn attrs_dir(updated_at: &str) -> FileAttributes {
    let t = rfc3339_to_unix(updated_at);
    FileAttributes {
        size: Some(0),
        uid: Some(0),
        user: None,
        gid: Some(0),
        group: None,
        permissions: Some(MODE_DIR),
        atime: Some(t),
        mtime: Some(t),
    }
}

// ---------------------------------------------------------------------------
// Shared backend state
// ---------------------------------------------------------------------------

/// Cloneable, shared server state for the SFTP backend.
pub struct SftpInner {
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

impl SftpInner {
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
        SftpInner {
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

    /// Resolve the folder at `components` from the share root.
    async fn resolve_dir(&self, components: &[String]) -> Result<FolderItem, StatusCode> {
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
        .map_err(to_status)?
        .ok_or(StatusCode::NoSuchFile)
    }
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

/// Read-only file body: lazily (re)starts a decrypt stream and serves reads from
/// it. Forward gaps up to [`MAX_FORWARD_SKIP`] skip in-stream; a backward
/// read, or a forward gap past the threshold, restarts as a ranged download of
/// only the covering shards.
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
                crate::serve::log::warn(&format!("[sftp] read stream error: {e:#}"));
            }
            let _ = writer.shutdown().await;
        });
        ReadStream { reader, pos: start, task }
    }

    async fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, StatusCode> {
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
                    .map_err(|_| StatusCode::Failure)?;
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
            let n = stream
                .reader
                .read(&mut buf[filled..])
                .await
                .map_err(|_| StatusCode::Failure)?;
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
// Write handle: temp-file-backed whole-file model (ported from the SMB backend)
// ---------------------------------------------------------------------------

struct WriteState {
    inner: Arc<SftpInner>,
    temp_path: PathBuf,
    file: tokio::sync::Mutex<tokio::fs::File>,
    materialized: tokio::sync::Mutex<bool>,
    dirty: AtomicBool,
    finalized: AtomicBool,
    size: AtomicU64,
    parent_uuid: String,
    plain: String,
    ftype: String,
    bucket: String,
    mnemonic: String,
    net_user: String,
    net_pass: String,
    existing_uuid: Mutex<Option<String>>,
    base_file_id: Option<String>,
    base_bucket: String,
    base_size: u64,
}

impl WriteState {
    async fn ensure_materialized(&self) -> Result<(), StatusCode> {
        let mut done = self.materialized.lock().await;
        if *done {
            return Ok(());
        }
        if let Some(fid) = &self.base_file_id {
            if self.base_size > 0 {
                let net = crate::net_client::network_api(&self.net_user, &self.net_pass);
                let mut f = self.file.lock().await;
                f.seek(std::io::SeekFrom::Start(0))
                    .await
                    .map_err(|_| StatusCode::Failure)?;
                internxt_core::transfer::download_file_to_writer(
                    &net,
                    &self.mnemonic,
                    &self.base_bucket,
                    fid,
                    &mut *f,
                    None,
                )
                .await
                .map_err(to_status)?;
                f.flush().await.map_err(|_| StatusCode::Failure)?;
                self.size.store(self.base_size, Ordering::SeqCst);
            }
        }
        *done = true;
        Ok(())
    }

    async fn finalize(&self) -> Result<(), StatusCode> {
        if self.finalized.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if !self.dirty.load(Ordering::SeqCst) {
            return Ok(());
        }
        {
            let mut f = self.file.lock().await;
            f.flush().await.map_err(|_| StatusCode::Failure)?;
        }
        let size = self.size.load(Ordering::SeqCst);
        self.inner.upload_limit.check(size).map_err(to_status)?;

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
            .map_err(to_status)?
        };
        let now = now_rfc3339();
        let old = self.existing_uuid.lock().unwrap().take();
        let file_uuid = match old {
            Some(old_uuid) => api
                .replace_file(token, &old_uuid, &file_id, size)
                .await
                .map_err(to_status)?
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
                .map_err(to_status)?
                .uuid,
        };

        crate::serve::thumbnail::upload_thumbnail_best_effort(
            &net, &api, token, &self.bucket, &self.mnemonic, &file_uuid, &self.ftype,
            &self.temp_path, size, "sftp",
        )
        .await;
        Ok(())
    }
}

impl Drop for WriteState {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.temp_path);
    }
}

// ---------------------------------------------------------------------------
// Open handles (per SFTP session)
// ---------------------------------------------------------------------------

/// A cached directory listing served across repeated READDIR calls; SFTP expects
/// the full listing followed by an `Eof`.
struct DirState {
    files: Vec<File>,
    served: bool,
}

enum Handle {
    Read { size: u64, updated_at: String, state: ReadState },
    Write(Arc<WriteState>),
    Dir(DirState),
}

// ---------------------------------------------------------------------------
// SSH server + session
// ---------------------------------------------------------------------------

/// `russh::server::Server` factory: one `SshSession` per TCP connection.
pub struct SshServer {
    inner: Arc<SftpInner>,
    username: String,
    password: Option<String>,
}

impl SshServer {
    pub fn new(inner: Arc<SftpInner>, username: String, password: Option<String>) -> Self {
        SshServer { inner, username, password }
    }
}

impl Server for SshServer {
    type Handler = SshSession;
    fn new_client(&mut self, _peer: Option<SocketAddr>) -> SshSession {
        SshSession {
            inner: self.inner.clone(),
            username: self.username.clone(),
            password: self.password.clone(),
            clients: HashMap::new(),
        }
    }
}

/// Per-connection SSH handler. Authenticates the client and, on an `sftp`
/// subsystem request, hands the channel stream to the SFTP loop.
pub struct SshSession {
    inner: Arc<SftpInner>,
    username: String,
    password: Option<String>,
    clients: HashMap<ChannelId, Channel<Msg>>,
}

impl SshHandler for SshSession {
    type Error = anyhow::Error;

    async fn auth_password(&mut self, user: &str, password: &str) -> AnyResult<Auth> {
        let user_ok = user == self.username;
        let pass_ok = match &self.password {
            Some(pw) => password == pw,
            None => true,
        };
        if user_ok && pass_ok {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            })
        }
    }

    async fn auth_publickey(&mut self, _user: &str, _key: &PublicKey) -> AnyResult<Auth> {
        // Force password auth (public keys aren't provisioned for this share).
        Ok(Auth::Reject {
            proceed_with_methods: None,
            partial_success: false,
        })
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> AnyResult<()> {
        reply.accept().await;
        self.clients.insert(channel.id(), channel);
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, session: &mut Session) -> AnyResult<()> {
        session.close(channel)?;
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> AnyResult<()> {
        if name == "sftp" {
            if let Some(channel) = self.clients.remove(&channel_id) {
                let sftp = SftpSession::new(self.inner.clone());
                session.channel_success(channel_id)?;
                russh_sftp::server::run(channel.into_stream(), sftp).await;
            } else {
                session.channel_failure(channel_id)?;
            }
        } else {
            session.channel_failure(channel_id)?;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// SFTP session (one per subsystem request; driven by a single task loop)
// ---------------------------------------------------------------------------

/// Per-SFTP-subsystem handler. Owns the open-handle table for this session; the
/// `russh-sftp` loop drives it sequentially so no locking is needed on the map.
pub struct SftpSession {
    inner: Arc<SftpInner>,
    handles: HashMap<String, Handle>,
    next_handle: u64,
    version: Option<u32>,
}

impl SftpSession {
    fn new(inner: Arc<SftpInner>) -> Self {
        SftpSession {
            inner,
            handles: HashMap::new(),
            next_handle: 0,
            version: None,
        }
    }

    fn new_handle_id(&mut self) -> String {
        let id = self.next_handle;
        self.next_handle += 1;
        format!("h{id}")
    }

    fn ro(&self) -> Result<(), StatusCode> {
        if self.inner.read_only {
            Err(StatusCode::PermissionDenied)
        } else {
            Ok(())
        }
    }

    /// Stat a path → attributes (folder first, then file).
    async fn stat_path(&self, path: &str) -> Result<FileAttributes, StatusCode> {
        let comps = norm_components(path);
        if comps.is_empty() {
            return Ok(attrs_dir(&self.inner.root_updated_at));
        }
        let (parent_comps, name) = comps.split_at(comps.len() - 1);
        let name = &name[0];
        let parent = self.inner.resolve_dir(parent_comps).await?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        if let Some(f) = tree::find_folder(&api, &creds.token, &parent.uuid, name, &self.inner.cache)
            .await
            .map_err(to_status)?
        {
            return Ok(attrs_dir(&f.updated_at));
        }
        let files = tree::list_files(&api, &creds.token, &parent.uuid)
            .await
            .map_err(to_status)?;
        if let Some(f) = files.into_iter().find(|f| &f.display_name() == name) {
            return Ok(attrs_file(f.size, &f.updated_at));
        }
        Err(StatusCode::NoSuchFile)
    }

    /// Build a read handle for an existing file.
    fn open_read(&self, creds: &Credentials, f: FileItem) -> Handle {
        let bucket = if f.bucket.is_empty() {
            creds.bucket().to_string()
        } else {
            f.bucket.clone()
        };
        Handle::Read {
            size: f.size,
            updated_at: f.updated_at.clone(),
            state: ReadState {
                file_id: f.file_id.clone().unwrap_or_default(),
                bucket,
                mnemonic: creds.mnemonic().to_string(),
                net_user: creds.net_user().to_string(),
                net_pass: creds.net_pass().to_string(),
                size: f.size,
                stream: tokio::sync::Mutex::new(None),
            },
        }
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
    ) -> Result<Handle, StatusCode> {
        let temp = temp_path(self.inner.spool_dir.as_deref());
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temp)
            .await
            .map_err(|_| StatusCode::Failure)?;
        let (plain, ftype) = split_name(name);
        let brand_new = existing.is_none();
        let (existing_uuid, base_file_id, base_bucket, base_size) = match &existing {
            Some(f) => {
                let base_bucket = if f.bucket.is_empty() {
                    creds.bucket().to_string()
                } else {
                    f.bucket.clone()
                };
                (Some(f.uuid.clone()), f.file_id.clone(), base_bucket, f.size)
            }
            None => (None, None, creds.bucket().to_string(), 0),
        };
        let ws = WriteState {
            inner: self.inner.clone(),
            temp_path: temp,
            file: tokio::sync::Mutex::new(file),
            materialized: tokio::sync::Mutex::new(brand_new || truncate),
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
        Ok(Handle::Write(Arc::new(ws)))
    }
}

impl russh_sftp::server::Handler for SftpSession {
    type Error = StatusCode;

    fn unimplemented(&self) -> StatusCode {
        StatusCode::OpUnsupported
    }

    async fn init(
        &mut self,
        version: u32,
        _extensions: HashMap<String, String>,
    ) -> Result<Version, StatusCode> {
        if self.version.is_some() {
            return Err(StatusCode::ConnectionLost);
        }
        self.version = Some(version);
        Ok(Version::new())
    }

    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, StatusCode> {
        let comps = norm_components(&path);
        let canonical = format!("/{}", comps.join("/"));
        Ok(Name {
            id,
            files: vec![File::dummy(canonical)],
        })
    }

    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        log(&format!("[STAT] {path}"));
        let attrs = self.stat_path(&path).await?;
        Ok(Attrs { id, attrs })
    }

    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, StatusCode> {
        log(&format!("[LSTAT] {path}"));
        let attrs = self.stat_path(&path).await?;
        Ok(Attrs { id, attrs })
    }

    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, StatusCode> {
        let attrs = match self.handles.get(&handle) {
            Some(Handle::Read { size, updated_at, .. }) => attrs_file(*size, updated_at),
            Some(Handle::Write(ws)) => {
                let size = ws.size.load(Ordering::SeqCst);
                attrs_file(size, &now_rfc3339())
            }
            Some(Handle::Dir(_)) => attrs_dir(&now_rfc3339()),
            None => return Err(StatusCode::Failure),
        };
        Ok(Attrs { id, attrs })
    }

    async fn setstat(
        &mut self,
        id: u32,
        _path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, StatusCode> {
        // Drive has no arbitrary attribute setting; accept silently so tools that
        // chmod/utimes after upload don't fail the transfer.
        Ok(ok_status(id))
    }

    async fn fsetstat(
        &mut self,
        id: u32,
        _handle: String,
        _attrs: FileAttributes,
    ) -> Result<Status, StatusCode> {
        Ok(ok_status(id))
    }

    async fn opendir(&mut self, id: u32, path: String) -> Result<SftpHandle, StatusCode> {
        log(&format!("[OPENDIR] {path}"));
        let comps = norm_components(&path);
        let dir = self.inner.resolve_dir(&comps).await?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        let (folders, files) = tokio::try_join!(
            tree::list_folders(&api, &creds.token, &dir.uuid, &self.inner.cache),
            tree::list_files(&api, &creds.token, &dir.uuid),
        )
        .map_err(to_status)?;
        let mut list: Vec<File> = Vec::with_capacity(folders.len() + files.len() + 2);
        list.push(File::new(".", attrs_dir(&dir.updated_at)));
        list.push(File::new("..", attrs_dir(&dir.updated_at)));
        for f in &folders {
            list.push(File::new(f.plain_name.clone(), attrs_dir(&f.updated_at)));
        }
        for f in &files {
            list.push(File::new(f.display_name(), attrs_file(f.size, &f.updated_at)));
        }
        let handle = self.new_handle_id();
        self.handles.insert(
            handle.clone(),
            Handle::Dir(DirState { files: list, served: false }),
        );
        Ok(SftpHandle { id, handle })
    }

    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, StatusCode> {
        match self.handles.get_mut(&handle) {
            Some(Handle::Dir(dir)) => {
                if dir.served {
                    return Err(StatusCode::Eof);
                }
                dir.served = true;
                Ok(Name {
                    id,
                    files: dir.files.clone(),
                })
            }
            _ => Err(StatusCode::Failure),
        }
    }

    async fn open(
        &mut self,
        id: u32,
        filename: String,
        pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<SftpHandle, StatusCode> {
        log(&format!("[OPEN] {filename} {pflags:?}"));
        let comps = norm_components(&filename);
        if comps.is_empty() {
            return Err(StatusCode::Failure);
        }
        let (parent_comps, name) = comps.split_at(comps.len() - 1);
        let name = name[0].clone();
        let parent = self.inner.resolve_dir(parent_comps).await?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        let files = tree::list_files(&api, &creds.token, &parent.uuid)
            .await
            .map_err(to_status)?;
        let existing = files.into_iter().find(|f| f.display_name() == name);

        let want_write = pflags.contains(OpenFlags::WRITE)
            || pflags.contains(OpenFlags::CREATE)
            || pflags.contains(OpenFlags::TRUNCATE);

        let handle = if want_write {
            self.ro()?;
            if pflags.contains(OpenFlags::EXCLUDE) && existing.is_some() {
                return Err(StatusCode::Failure);
            }
            if existing.is_none() && !pflags.contains(OpenFlags::CREATE) {
                return Err(StatusCode::NoSuchFile);
            }
            // A write open truncates unless it explicitly keeps content (READ set
            // without TRUNCATE — e.g. an in-place editor). Default SFTP put uses
            // WRITE|CREATE|TRUNC, so this is the common path.
            let truncate = existing.is_none()
                || pflags.contains(OpenFlags::TRUNCATE)
                || !pflags.contains(OpenFlags::READ);
            self.open_write(&creds, &parent.uuid, &name, existing, truncate)
                .await?
        } else {
            let f = existing.ok_or(StatusCode::NoSuchFile)?;
            self.open_read(&creds, f)
        };

        let hid = self.new_handle_id();
        self.handles.insert(hid.clone(), handle);
        Ok(SftpHandle { id, handle: hid })
    }

    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, StatusCode> {
        match self.handles.get(&handle) {
            Some(Handle::Read { state, .. }) => {
                let data = state.read_at(offset, len as usize).await?;
                if data.is_empty() {
                    Err(StatusCode::Eof)
                } else {
                    Ok(Data { id, data })
                }
            }
            Some(Handle::Write(ws)) => {
                ws.ensure_materialized().await?;
                let mut f = ws.file.lock().await;
                f.seek(std::io::SeekFrom::Start(offset))
                    .await
                    .map_err(|_| StatusCode::Failure)?;
                let mut buf = vec![0u8; len as usize];
                let mut filled = 0;
                while filled < buf.len() {
                    let n = f.read(&mut buf[filled..]).await.map_err(|_| StatusCode::Failure)?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                buf.truncate(filled);
                if buf.is_empty() {
                    Err(StatusCode::Eof)
                } else {
                    Ok(Data { id, data: buf })
                }
            }
            _ => Err(StatusCode::Failure),
        }
    }

    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, StatusCode> {
        let ws = match self.handles.get(&handle) {
            Some(Handle::Write(ws)) => ws.clone(),
            _ => return Err(StatusCode::PermissionDenied),
        };
        let end = offset + data.len() as u64;
        ws.inner.upload_limit.check(end).map_err(to_status)?;
        ws.ensure_materialized().await?;
        {
            let mut f = ws.file.lock().await;
            f.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|_| StatusCode::Failure)?;
            f.write_all(&data).await.map_err(|_| StatusCode::Failure)?;
        }
        ws.size.fetch_max(end, Ordering::SeqCst);
        ws.dirty.store(true, Ordering::SeqCst);
        Ok(ok_status(id))
    }

    async fn close(&mut self, id: u32, handle: String) -> Result<Status, StatusCode> {
        if let Some(Handle::Write(ws)) = self.handles.remove(&handle) {
            ws.finalize().await?;
        }
        Ok(ok_status(id))
    }

    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, StatusCode> {
        log(&format!("[MKDIR] {path}"));
        self.ro()?;
        let comps = norm_components(&path);
        if comps.is_empty() {
            return Err(StatusCode::Failure);
        }
        let (parent_comps, name) = comps.split_at(comps.len() - 1);
        let name = &name[0];
        let parent = self.inner.resolve_dir(parent_comps).await?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        let existing = tree::find_folder(&api, &creds.token, &parent.uuid, name, &self.inner.cache)
            .await
            .map_err(to_status)?;
        if existing.is_some() {
            return Err(StatusCode::Failure);
        }
        api.create_folder(&creds.token, name, &parent.uuid)
            .await
            .map_err(to_status)?;
        self.inner.cache.invalidate(&parent.uuid);
        Ok(ok_status(id))
    }

    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, StatusCode> {
        log(&format!("[RMDIR] {path}"));
        self.ro()?;
        let comps = norm_components(&path);
        if comps.is_empty() {
            return Err(StatusCode::PermissionDenied);
        }
        let (parent_comps, name) = comps.split_at(comps.len() - 1);
        let name = &name[0];
        let parent = self.inner.resolve_dir(parent_comps).await?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        let f = tree::find_folder(&api, &creds.token, &parent.uuid, name, &self.inner.cache)
            .await
            .map_err(to_status)?
            .ok_or(StatusCode::NoSuchFile)?;
        if self.inner.delete_permanently {
            api.delete_folder(&creds.token, &f.uuid).await.map_err(to_status)?;
        } else {
            api.trash_items(&creds.token, json!([{ "uuid": f.uuid, "type": "folder" }]))
                .await
                .map_err(to_status)?;
        }
        self.inner.cache.invalidate(&parent.uuid);
        self.inner.cache.invalidate(&f.uuid);
        Ok(ok_status(id))
    }

    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, StatusCode> {
        log(&format!("[REMOVE] {filename}"));
        self.ro()?;
        let comps = norm_components(&filename);
        if comps.is_empty() {
            return Err(StatusCode::Failure);
        }
        let (parent_comps, name) = comps.split_at(comps.len() - 1);
        let name = &name[0];
        let parent = self.inner.resolve_dir(parent_comps).await?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);
        let files = tree::list_files(&api, &creds.token, &parent.uuid)
            .await
            .map_err(to_status)?;
        let f = files
            .into_iter()
            .find(|f| &f.display_name() == name)
            .ok_or(StatusCode::NoSuchFile)?;
        if self.inner.delete_permanently {
            api.delete_file(&creds.token, &f.uuid).await.map_err(to_status)?;
        } else {
            api.trash_items(&creds.token, json!([{ "uuid": f.uuid, "type": "file" }]))
                .await
                .map_err(to_status)?;
        }
        Ok(ok_status(id))
    }

    async fn rename(
        &mut self,
        id: u32,
        oldpath: String,
        newpath: String,
    ) -> Result<Status, StatusCode> {
        log(&format!("[RENAME] {oldpath} -> {newpath}"));
        self.ro()?;
        let src = norm_components(&oldpath);
        let dst = norm_components(&newpath);
        if src.is_empty() || dst.is_empty() {
            return Err(StatusCode::Failure);
        }
        let (src_parent_c, src_name) = src.split_at(src.len() - 1);
        let (dst_parent_c, dst_name) = dst.split_at(dst.len() - 1);
        let src_name = src_name[0].clone();
        let dst_name = dst_name[0].clone();
        let src_parent = self.inner.resolve_dir(src_parent_c).await?;
        let dst_parent = self.inner.resolve_dir(dst_parent_c).await?;
        let creds = self.inner.creds();
        let api = DriveApi::for_credentials(&creds);

        // Resolve the source (file first, then folder).
        let files = tree::list_files(&api, &creds.token, &src_parent.uuid)
            .await
            .map_err(to_status)?;
        let (uuid, is_folder, cur_name) =
            if let Some(f) = files.into_iter().find(|f| f.display_name() == src_name) {
                let cur = f.display_name();
                (f.uuid, false, cur)
            } else {
                match tree::find_folder(&api, &creds.token, &src_parent.uuid, &src_name, &self.inner.cache)
                    .await
                    .map_err(to_status)?
                {
                    Some(f) => (f.uuid, true, f.plain_name),
                    None => return Err(StatusCode::NoSuchFile),
                }
            };

        // Reject if the destination name already exists.
        let dst_files = tree::list_files(&api, &creds.token, &dst_parent.uuid)
            .await
            .map_err(to_status)?;
        let dst_folder_clash =
            tree::find_folder(&api, &creds.token, &dst_parent.uuid, &dst_name, &self.inner.cache)
                .await
                .map_err(to_status)?
                .is_some();
        let clashes = dst_files.iter().any(|f| f.display_name() == dst_name) || dst_folder_clash;
        if clashes && !(src_parent.uuid == dst_parent.uuid && cur_name == dst_name) {
            return Err(StatusCode::Failure);
        }

        if src_parent.uuid != dst_parent.uuid {
            if is_folder {
                api.move_folder(&creds.token, &uuid, &dst_parent.uuid)
                    .await
                    .map_err(to_status)?;
            } else {
                api.move_file(&creds.token, &uuid, &dst_parent.uuid)
                    .await
                    .map_err(to_status)?;
            }
        }
        if cur_name != dst_name {
            if is_folder {
                api.rename_folder(&creds.token, &uuid, &dst_name)
                    .await
                    .map_err(to_status)?;
            } else {
                let (plain, ftype) = split_name(&dst_name);
                api.rename_file(&creds.token, &uuid, &plain, &ftype)
                    .await
                    .map_err(to_status)?;
            }
        }
        self.inner.cache.invalidate(&src_parent.uuid);
        self.inner.cache.invalidate(&dst_parent.uuid);
        Ok(ok_status(id))
    }
}

/// A success `SSH_FXP_STATUS`.
fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "en-US".to_string(),
    }
}

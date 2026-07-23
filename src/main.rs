mod accounts;
mod auth;
mod commands;
mod drive_ops;
#[cfg(feature = "fuse")]
mod fuse;
mod net_client;
#[cfg(feature = "nfs")]
mod nfs;
mod output;
mod paths;
#[cfg(any(feature = "webdav", feature = "fuse", feature = "smb", feature = "nfs", feature = "sftp"))]
mod serve;
#[cfg(feature = "sftp")]
mod sftp;
#[cfg(feature = "self-update")]
mod self_update_cmd;
#[cfg(feature = "smb")]
mod smb;
#[cfg(feature = "sso")]
mod sso;
mod sync;
mod thumbnail_ops;
mod upload_limit;
#[cfg(feature = "webdav")]
mod webdav;
mod workspaces;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use std::io::Write;

#[derive(Parser)]
#[command(name = "ixr", version, about = "Internxt CLI (Rust port)")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
    /// Output the result as a single JSON object (suppresses progress output).
    #[arg(long, global = true, default_value_t = false)]
    json: bool,
    /// Prevent the CLI from prompting for input; error out instead.
    #[arg(
        short = 'x',
        long = "non-interactive",
        global = true,
        env = "IXR_NONINTERACTIVE",
        default_value_t = false,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    non_interactive: bool,
    /// Disable the idle-read timeout on network transfers (uploads/downloads).
    /// Use this if a slow `--stdin` producer or `--stdout` consumer trips a
    /// false timeout on an otherwise-healthy transfer over a slow link.
    /// Connect timeout stays on regardless: a hung connection attempt is
    /// unrelated to transfer speed and should still fail fast.
    #[arg(
        long,
        global = true,
        env = "IXR_NO_TIMEOUTS",
        default_value_t = false,
        value_parser = clap::builder::BoolishValueParser::new()
    )]
    no_timeout: bool,
}

#[derive(Subcommand)]
// The `Serve` variant carries many protocol-specific flags (more with the
// non-default nfs/sftp features). This enum is parsed once at startup, so the
// size disparity between variants is irrelevant.
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Log in to your Internxt account.
    ///
    /// Alias for `login-sso` when built with the `sso` feature (default, matching
    /// the official CLI's SSO-only `login`), otherwise an alias for `login-legacy`.
    /// Use `login-sso` or `login-legacy` directly to force a specific flow.
    Login {
        /// SSO: address the browser uses to reach this machine (default 127.0.0.1).
        #[arg(long, env = "IXR_LOGIN_SERVER_HOST")]
        host: Option<String>,
        /// SSO: port for the local callback server (default: a random free port).
        #[arg(long, env = "IXR_LOGIN_SERVER_PORT")]
        port: Option<u16>,
        #[arg(short, long)]
        email: Option<String>,
        #[arg(short, long)]
        password: Option<String>,
        /// The two-factor auth code (TOTP).
        #[arg(short = 'w', long)]
        twofactor: Option<String>,
        /// The TOTP secret token, used to generate a code. Takes priority over --twofactor.
        #[arg(short = 't', long)]
        twofactortoken: Option<String>,
        /// If already logged in as a different account, add this one alongside it
        /// (and switch to it) instead of prompting.
        #[arg(long, conflicts_with = "replace", default_value_t = false)]
        add: bool,
        /// If already logged in as a different account, log it out and replace it
        /// with this one instead of prompting.
        #[arg(long, conflicts_with = "add", default_value_t = false)]
        replace: bool,
    },
    /// Log in with email and password (legacy flow).
    LoginLegacy {
        #[arg(short, long)]
        email: Option<String>,
        #[arg(short, long)]
        password: Option<String>,
        /// The two-factor auth code (TOTP).
        #[arg(short = 'w', long)]
        twofactor: Option<String>,
        /// The TOTP secret token, used to generate a code. Takes priority over --twofactor.
        #[arg(short = 't', long)]
        twofactortoken: Option<String>,
        /// If already logged in as a different account, add this one alongside it
        /// (and switch to it) instead of prompting.
        #[arg(long, conflicts_with = "replace", default_value_t = false)]
        add: bool,
        /// If already logged in as a different account, log it out and replace it
        /// with this one instead of prompting.
        #[arg(long, conflicts_with = "add", default_value_t = false)]
        replace: bool,
    },
    /// Log in via the web-based SSO flow (requires the `sso` feature).
    LoginSso {
        /// Address the browser uses to reach this machine (default 127.0.0.1).
        #[arg(long, env = "IXR_LOGIN_SERVER_HOST")]
        host: Option<String>,
        /// Port for the local callback server (default: a random free port).
        #[arg(long, env = "IXR_LOGIN_SERVER_PORT")]
        port: Option<u16>,
        /// If already logged in as a different account, add this one alongside it
        /// (and switch to it) instead of prompting.
        #[arg(long, conflicts_with = "replace", default_value_t = false)]
        add: bool,
        /// If already logged in as a different account, log it out and replace it
        /// with this one instead of prompting.
        #[arg(long, conflicts_with = "add", default_value_t = false)]
        replace: bool,
    },
    /// Upload a file to Internxt Drive.
    #[command(hide = true)]
    UploadFile(UploadFileArgs),
    /// Upload a folder (recursively) to Internxt Drive.
    #[command(hide = true)]
    UploadFolder(UploadFolderArgs),
    /// Download a file from Internxt Drive by uuid or path.
    #[command(hide = true)]
    DownloadFile(DownloadFileArgs),
    /// Download a folder (recursively) from Internxt Drive.
    #[command(hide = true)]
    DownloadFolder(DownloadFolderArgs),
    /// Log out the current user from the Internxt CLI.
    Logout {
        /// Log out of every stored account instead of just the resolved one.
        #[arg(long, default_value_t = false)]
        all: bool,
    },
    /// Display the current user logged into the Internxt CLI.
    Whoami,
    /// Manage logged-in accounts (see `accounts list` / `accounts switch`).
    ///
    /// New — no official equivalent (the official CLI supports one account at a
    /// time).
    #[command(subcommand)]
    Accounts(AccountsCmd),
    /// Show account usage: plan, used space (drive/backups), space limit and
    /// per-file upload limit.
    #[command(alias = "account", alias = "account-info")]
    Usage,
    /// List the contents of a folder.
    List {
        /// The folder id to list. Leave empty for the root folder.
        #[arg(short = 'i', long)]
        id: Option<String>,
        /// The folder path to list (e.g. `/a/b`), alternative to --id.
        #[arg(short, long)]
        path: Option<String>,
        /// Display additional information (modified date, size).
        #[arg(short, long, default_value_t = false)]
        extended: bool,
    },
    /// Create a folder in your Internxt Drive.
    #[command(hide = true)]
    CreateFolder(CreateFolderArgs),
    /// Move a file into a destination folder.
    #[command(hide = true)]
    MoveFile(MoveFileArgs),
    /// Move a folder into a destination folder.
    #[command(hide = true)]
    MoveFolder(MoveFolderArgs),
    /// Rename a file.
    #[command(hide = true)]
    RenameFile(RenameFileArgs),
    /// Rename a folder.
    #[command(hide = true)]
    RenameFolder(RenameFolderArgs),
    /// Move a file to the trash.
    #[command(hide = true)]
    TrashFile(TrashFileArgs),
    /// Move a folder to the trash.
    #[command(hide = true)]
    TrashFolder(TrashFolderArgs),
    /// List the contents of the trash.
    #[command(hide = true)]
    TrashList(TrashListArgs),
    /// Restore a trashed file into a destination folder.
    #[command(hide = true)]
    TrashRestoreFile(TrashRestoreFileArgs),
    /// Restore a trashed folder into a destination folder.
    #[command(hide = true)]
    TrashRestoreFolder(TrashRestoreFolderArgs),
    /// Empty the trash permanently. This action cannot be undone.
    #[command(hide = true)]
    TrashClear(TrashClearArgs),
    /// Permanently delete a file. This action cannot be undone.
    #[command(hide = true)]
    DeletePermanentlyFile(DeletePermanentlyFileArgs),
    /// Permanently delete a folder. This action cannot be undone.
    #[command(hide = true)]
    DeletePermanentlyFolder(DeletePermanentlyFolderArgs),
    /// Move a file to the trash (or permanently delete with --permanent).
    #[command(hide = true)]
    DeleteFile(DeleteFileArgs),
    /// Move a folder to the trash (or permanently delete with --permanent).
    #[command(hide = true)]
    DeleteFolder(DeleteFolderArgs),
    /// List the workspaces you belong to.
    #[command(hide = true)]
    WorkspacesList(WorkspacesListArgs),
    /// Set the active workspace for subsequent commands.
    #[command(hide = true)]
    WorkspacesUse(WorkspacesUseArgs),
    /// Unset the active workspace (operate within your personal drive space).
    #[command(hide = true)]
    WorkspacesUnset,
    /// One-way sync: make a remote Drive folder match a local folder (push).
    #[command(hide = true)]
    SyncUp {
        /// The local directory to sync from.
        #[arg(short, long)]
        local: String,
        /// The remote folder uuid to sync into. Leave empty for the root folder.
        #[arg(short, long)]
        remote: Option<String>,
        /// The remote folder path to sync into (e.g. `/a/b`), alternative to --remote.
        #[arg(long)]
        remote_path: Option<String>,
        /// Delete remote files not present locally. Optional value: `trash`
        /// (default) or `permanent`.
        #[arg(long, num_args = 0..=1, default_missing_value = "default")]
        delete: Option<String>,
        /// Print the planned actions without transferring anything.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Skip empty (0-byte) files instead of uploading them. Internxt
        /// rejects empty files on free/legacy plans (HTTP 402); use this to
        /// avoid those failures instead of seeing them reported.
        #[arg(long, default_value_t = false)]
        exclude_empty_files: bool,
        #[command(flatten)]
        limit: upload_limit::UploadLimitArgs,
    },
    /// Serve your Internxt Drive over one or more protocols (runs until Ctrl-C).
    ///
    /// Protocols are given as a comma-separated list, e.g. `serve webdav` or
    /// `serve webdav,fuse`. All selected backends share one credential holder,
    /// one folder cache and one global upload limit. Shared knobs are bare flags
    /// (`--cache-ttl`, `--folder-uuid`, `--delete-permanently`, `--spool`,
    /// `--spool-dir`, `--max-concurrent-uploads`, `--read-only`); protocol
    /// specific knobs are prefixed (`--webdav-port`, `--fuse-mountpoint`, …).
    #[cfg(any(feature = "webdav", feature = "fuse", feature = "smb", feature = "nfs", feature = "sftp"))]
    Serve {
        /// Comma-separated protocols to serve (known: webdav, fuse, smb, nfs, sftp).
        #[arg(value_name = "PROTOCOLS")]
        protocols: String,

        // ---- shared ----
        /// Drive folder uuid to expose as the root of every backend. Omit for
        /// the account / workspace root.
        #[arg(short = 'i', long)]
        folder_uuid: Option<String>,
        /// Cache folder listings for this many seconds (also the FUSE kernel
        /// attr/entry TTL). Shared by all backends. Matches rclone's own
        /// `--dir-cache-time` default (300s/5min): a short TTL can expire
        /// mid-traversal on a deep path or a folder with hundreds of
        /// entries (each level/page is a network round trip), forcing a
        /// redundant re-fetch of ancestors that were only just resolved.
        #[arg(long, default_value_t = 300)]
        cache_ttl: u64,
        /// Disable caching (same as --cache-ttl 0).
        #[arg(long, default_value_t = false)]
        no_cache: bool,
        /// Bytes of trailing-stream retention on the read path, shared by
        /// FUSE/SMB/NFS/SFTP (WebDAV's GET is one-shot per request and doesn't
        /// use this). Lets a small backward/forward re-read (e.g. a media
        /// player re-visiting a container-index box during MP4/MKV parsing)
        /// be served from memory instead of restarting the download stream
        /// (a fresh network round trip). 0 disables it entirely.
        #[arg(long, default_value_t = crate::serve::recent_window::DEFAULT_RECENT_WINDOW)]
        recent_window: u64,
        /// Delete files permanently instead of moving them to trash.
        #[arg(short = 'd', long, default_value_t = false)]
        delete_permanently: bool,
        /// Spool each upload body to a temp file before uploading (WebDAV PUT;
        /// FUSE writes always spool). More robust for concurrent/slow clients.
        #[arg(long, default_value_t = false)]
        spool: bool,
        /// Directory for spool temp files (default: system temp dir).
        #[arg(long)]
        spool_dir: Option<String>,
        /// Max uploads transferring at once, across all backends (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        max_concurrent_uploads: usize,
        /// Serve read-only: reject all writes/mutations on every backend.
        #[arg(long, default_value_t = false)]
        read_only: bool,
        /// Verbose: log every per-operation request across all backends. Without
        /// it, only errors/warnings are printed.
        #[arg(short = 'v', long, default_value_t = false)]
        verbose: bool,
        #[command(flatten)]
        limit: upload_limit::UploadLimitArgs,

        // ---- webdav ----
        /// WebDAV: host to bind (and advertise). 0.0.0.0 accepts LAN clients.
        #[cfg(feature = "webdav")]
        #[arg(long, default_value = "127.0.0.1")]
        webdav_host: String,
        /// WebDAV: port to listen on.
        #[cfg(feature = "webdav")]
        #[arg(long, default_value_t = 3005)]
        webdav_port: u16,
        /// WebDAV: serve over HTTPS instead of plain HTTP.
        #[cfg(feature = "webdav")]
        #[arg(long, default_value_t = false)]
        webdav_https: bool,
        /// WebDAV: TLS certificate (PEM). With --webdav-https and no cert, a
        /// self-signed cert is generated.
        #[cfg(feature = "webdav")]
        #[arg(long, requires = "webdav_https")]
        webdav_cert: Option<String>,
        /// WebDAV: TLS private key (PEM). Required alongside --webdav-cert.
        #[cfg(feature = "webdav")]
        #[arg(long, requires = "webdav_cert")]
        webdav_key: Option<String>,
        /// WebDAV: server request timeout in minutes (0 = none).
        #[cfg(feature = "webdav")]
        #[arg(long, default_value_t = 60)]
        webdav_timeout: u64,
        /// WebDAV: auto-create missing parent folders on PUT / MKCOL.
        #[cfg(feature = "webdav")]
        #[arg(long, default_value_t = false)]
        webdav_create_full_path: bool,
        /// WebDAV: require HTTP Basic auth (needs --webdav-username/--webdav-password).
        #[cfg(feature = "webdav")]
        #[arg(long, default_value_t = false)]
        webdav_custom_auth: bool,
        /// WebDAV: username for --webdav-custom-auth.
        #[cfg(feature = "webdav")]
        #[arg(long)]
        webdav_username: Option<String>,
        /// WebDAV: password for --webdav-custom-auth.
        #[cfg(feature = "webdav")]
        #[arg(long)]
        webdav_password: Option<String>,

        // ---- fuse ----
        /// FUSE: mount target, required when `fuse` is served (a directory on
        /// Unix; a drive letter like `X:` or a directory on Windows).
        #[cfg(feature = "fuse")]
        #[arg(long, value_name = "MOUNTPOINT")]
        fuse_mountpoint: Option<String>,
        /// FUSE: allow other users (and root) to access the mount (needs
        /// user_allow_other in /etc/fuse.conf on Linux). Unix only — no
        /// WinFSP equivalent.
        #[cfg(all(unix, feature = "fuse"))]
        #[arg(long, default_value_t = false)]
        fuse_allow_other: bool,

        // ---- smb ----
        /// SMB: host to bind (and advertise). 0.0.0.0 accepts LAN clients.
        #[cfg(feature = "smb")]
        #[arg(long, default_value = "127.0.0.1")]
        smb_host: String,
        /// SMB: TCP port. The well-known SMB port 445 needs root/admin; the
        /// default is an unprivileged port.
        #[cfg(feature = "smb")]
        #[arg(long, default_value_t = 4445)]
        smb_port: u16,
        /// SMB: exported share name (`\\host\<share>`).
        #[cfg(feature = "smb")]
        #[arg(long, default_value = "internxt")]
        smb_share: String,
        /// SMB: require authentication as this user (with --smb-password).
        #[cfg(feature = "smb")]
        #[arg(long, default_value = "internxt")]
        smb_username: String,
        /// SMB: password required from clients. Omit for an anonymous (guest)
        /// share — most SMB clients (Windows especially) refuse anonymous, so a
        /// password is recommended.
        #[cfg(feature = "smb")]
        #[arg(long)]
        smb_password: Option<String>,

        // ---- nfs ----
        /// NFS: host to bind (and advertise). 0.0.0.0 accepts LAN clients.
        #[cfg(feature = "nfs")]
        #[arg(long, default_value = "127.0.0.1")]
        nfs_host: String,
        /// NFS: TCP port. The well-known NFS port 2049 needs root/admin; the
        /// default is an unprivileged port (mount with `-o port=...,mountport=...`).
        #[cfg(feature = "nfs")]
        #[arg(long, default_value_t = 12049)]
        nfs_port: u16,

        // ---- sftp ----
        /// SFTP: host to bind (and advertise). 0.0.0.0 accepts LAN clients.
        #[cfg(feature = "sftp")]
        #[arg(long, default_value = "127.0.0.1")]
        sftp_host: String,
        /// SFTP: TCP port. The well-known SSH port 22 needs root/admin; the
        /// default is an unprivileged port (connect with `sftp -P <port> ...`).
        #[cfg(feature = "sftp")]
        #[arg(long, default_value_t = 2022)]
        sftp_port: u16,
        /// SFTP: username required from clients.
        #[cfg(feature = "sftp")]
        #[arg(long, default_value = "internxt")]
        sftp_username: String,
        /// SFTP: password required from clients. Omit to accept any password
        /// (the username is still required). A password is recommended.
        #[cfg(feature = "sftp")]
        #[arg(long)]
        sftp_password: Option<String>,
        /// SFTP: path to an SSH host private key (OpenSSH/PEM). Omit to generate
        /// an ephemeral key on each start (clients will see a changed host key).
        #[cfg(feature = "sftp")]
        #[arg(long, value_name = "PATH")]
        sftp_host_key: Option<String>,
    },
    /// Mount your Internxt Drive as a local filesystem via FUSE/WinFSP (runs
    /// until Ctrl-C).
    ///
    /// Requires a driver at runtime: fuse3 on Linux, macFUSE on macOS,
    /// fusefs-libs3 on FreeBSD, WinFsp on Windows (see README § FUSE/WinFSP
    /// mount support). Full read-write: writes buffer to a temp file and
    /// upload in full when the file is closed.
    #[cfg(feature = "fuse")]
    Mount {
        /// Mount target: must already exist. A directory on Unix; a drive
        /// letter like `X:` or a directory on Windows.
        #[arg(value_name = "MOUNTPOINT")]
        mountpoint: String,
        /// Drive folder uuid to expose as the mount root. Omit for the drive root.
        #[arg(short = 'i', long)]
        folder_uuid: Option<String>,
        /// Cache folder listings + kernel attributes for this many seconds.
        /// Matches rclone's own `--dir-cache-time` default (300s/5min): a
        /// short TTL can expire mid-traversal on a deep path or a folder
        /// with hundreds of entries (each level/page is a network round
        /// trip), forcing a redundant re-fetch of ancestors that were only
        /// just resolved.
        #[arg(long, default_value_t = 300)]
        cache_ttl: u64,
        /// Disable caching (same as --cache-ttl 0; always live, slower).
        #[arg(long, default_value_t = false)]
        no_cache: bool,
        /// Bytes of trailing-stream retention on the read path. Lets a small
        /// backward/forward re-read (e.g. a media player re-visiting a
        /// container-index box during MP4/MKV parsing) be served from memory
        /// instead of restarting the download stream (a fresh network round
        /// trip). 0 disables it entirely.
        #[arg(long, default_value_t = crate::serve::recent_window::DEFAULT_RECENT_WINDOW)]
        recent_window: u64,
        /// Delete files permanently instead of moving them to trash.
        #[arg(short = 'd', long, default_value_t = false)]
        delete_permanently: bool,
        /// Directory for per-write temp buffers (default: system temp dir).
        #[arg(long)]
        spool_dir: Option<String>,
        /// Max file uploads transferring at once (0 = unlimited).
        #[arg(long, default_value_t = 0)]
        max_concurrent_uploads: usize,
        /// Mount read-only (reject all writes/mutations).
        #[arg(long, default_value_t = false)]
        read_only: bool,
        /// Allow other users (and root) to access the mount (needs
        /// user_allow_other in /etc/fuse.conf on Linux). Unix only — no
        /// WinFSP equivalent.
        #[cfg(unix)]
        #[arg(long, default_value_t = false)]
        allow_other: bool,
        /// Verbose: log every per-operation request. Without it, only
        /// errors/warnings are printed.
        #[arg(short = 'v', long, default_value_t = false)]
        verbose: bool,
        #[command(flatten)]
        limit: upload_limit::UploadLimitArgs,
    },
    /// One-way sync: make a local folder match a remote Drive folder (pull).
    #[command(hide = true)]
    SyncDown {
        /// The local directory to sync into.
        #[arg(short, long)]
        local: String,
        /// The remote folder uuid to sync from. Leave empty for the root folder.
        #[arg(short, long)]
        remote: Option<String>,
        /// The remote folder path to sync from (e.g. `/a/b`), alternative to --remote.
        #[arg(long)]
        remote_path: Option<String>,
        /// Delete local files not present remotely. Optional value: `remove`
        /// (default; OS trash not yet supported).
        #[arg(long, num_args = 0..=1, default_missing_value = "default")]
        delete: Option<String>,
        /// Print the planned actions without transferring anything.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Print the uuid of the Drive file/folder at a given path.
    #[command(alias = "get-id")]
    IdFromPath {
        /// The Drive path (e.g. `/a/b/file.txt` or `/a/b`).
        #[arg(short, long)]
        path: String,
    },
    /// Print the full Drive path of a file/folder given its uuid.
    #[command(alias = "get-path")]
    PathFromId {
        /// The uuid of the file or folder.
        #[arg(short, long)]
        id: String,
    },
    /// Manage a file's thumbnail: generate, upload a custom one, or download it.
    #[command(subcommand, alias = "thumbnails")]
    Thumbnail(ThumbnailCmd),

    // ---- space-form topic groups ----
    //
    // These are the primary, documented command surface (`upload file`, `trash
    // folder`, `workspaces use`, ...). The flat hyphenated variants above (e.g.
    // `UploadFile`, `TrashFolder`) are kept only as hidden compatibility
    // aliases — they still parse (matches the official oclif-based CLI, which
    // accepts both `upload-file` and `upload file`), but are hidden from
    // `--help` to keep the top-level command list short. Use `help-all` to see
    // every hidden alias. `login-legacy`/`login-sso`/`id-from-path`/
    // `path-from-id` have no natural resource group and stay flat-only.
    /// Upload a file or folder (see `upload file` / `upload folder`).
    #[command(subcommand)]
    Upload(UploadCmd),
    /// Download a file or folder (see `download file` / `download folder`).
    #[command(subcommand)]
    Download(DownloadCmd),
    /// Create a folder (see `create folder`).
    #[command(subcommand)]
    Create(CreateCmd),
    /// Move a file or folder (see `move file` / `move folder`).
    #[command(subcommand)]
    Move(MoveCmd),
    /// Rename a file or folder (see `rename file` / `rename folder`).
    #[command(subcommand)]
    Rename(RenameCmd),
    /// Manage the trash (see `trash file|folder|list|clear|restore`).
    #[command(subcommand)]
    Trash(TrashCmd),
    /// Delete a file or folder (see `delete file|folder`, `delete permanently file|folder`).
    #[command(subcommand)]
    Delete(DeleteCmd),
    /// Manage workspaces (see `workspaces list|use|unset`).
    #[command(subcommand)]
    Workspaces(WorkspacesCmd),
    /// One-way sync (see `sync up` / `sync down`).
    #[command(subcommand)]
    Sync(SyncCmd),
    /// Show every command, including hidden compatibility aliases for the
    /// flat/hyphenated names (e.g. `upload-file`).
    HelpAll,
    /// Update this binary to the latest GitHub release in place.
    ///
    /// Only meaningful for the standalone binary distribution. If `ixr` was
    /// installed via a package manager (AUR, `cargo install`, Docker), use
    /// that instead — this will fight it for ownership of the file.
    #[cfg(feature = "self-update")]
    Update {
        /// Only check whether a newer release exists; don't install it.
        #[arg(long, default_value_t = false)]
        check: bool,
        /// Install without prompting for confirmation.
        #[arg(short = 'y', long, default_value_t = false)]
        yes: bool,
        /// Consider pre-releases too. By default only stable releases are
        /// considered.
        #[arg(long, default_value_t = false)]
        pre_release: bool,
    },
}

#[derive(clap::Args)]
struct UploadFileArgs {
    /// Path to the file. Omit when using --stdin.
    #[arg(short, long)]
    file: Option<String>,
    /// Destination folder uuid. Leave empty for root.
    #[arg(short = 'i', long)]
    destination: Option<String>,
    /// Destination folder path (e.g. `/a/b`), alternative to --destination.
    #[arg(long)]
    dest_path: Option<String>,
    /// Read the file body from stdin instead of --file. Requires --name.
    #[arg(long, default_value_t = false)]
    stdin: bool,
    /// Drive filename (with extension) for the uploaded file. Required with --stdin;
    /// with --file, overrides the name/extension taken from the source path.
    #[arg(short, long)]
    name: Option<String>,
    /// Exact byte length of stdin. If given, streams directly; otherwise stdin is
    /// spooled to a temp file to learn its size.
    #[arg(short = 's', long)]
    size: Option<u64>,
    #[command(flatten)]
    limit: upload_limit::UploadLimitArgs,
}

#[derive(clap::Args)]
struct UploadFolderArgs {
    /// The path to the folder on your system.
    #[arg(short, long)]
    folder: Option<String>,
    /// Destination folder id. Leave empty for the root folder.
    #[arg(short = 'i', long)]
    destination: Option<String>,
    /// Destination folder path (e.g. `/a/b`), alternative to --destination.
    #[arg(long)]
    dest_path: Option<String>,
    /// Skip empty (0-byte) files instead of uploading them. Internxt rejects
    /// empty files on free/legacy plans (HTTP 402); use this to avoid those
    /// failures instead of seeing them reported.
    #[arg(long, default_value_t = false)]
    exclude_empty_files: bool,
    #[command(flatten)]
    limit: upload_limit::UploadLimitArgs,
}

#[derive(Subcommand)]
enum UploadCmd {
    /// Upload a file to Internxt Drive.
    File(UploadFileArgs),
    /// Upload a folder (recursively) to Internxt Drive.
    Folder(UploadFolderArgs),
}

#[derive(clap::Args)]
struct DownloadFileArgs {
    /// The uuid of the file to download.
    #[arg(short, long)]
    id: Option<String>,
    /// The Drive path of the file (e.g. `/a/b/file.txt`), alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
    #[arg(short, long)]
    directory: Option<String>,
    #[arg(short, long, default_value_t = false)]
    overwrite: bool,
    /// Write the decrypted bytes to stdout instead of a file (status goes to stderr).
    #[arg(long, default_value_t = false)]
    stdout: bool,
    /// Write directly to the destination path instead of a temp file + rename.
    /// Restores the old behavior: faster/simpler, but an interrupted download
    /// (network drop, Ctrl-C) can leave a truncated file at the destination path.
    #[arg(long, default_value_t = false)]
    legacy_write: bool,
}

#[derive(clap::Args)]
struct DownloadFolderArgs {
    /// The uuid of the folder to download.
    #[arg(short, long)]
    id: Option<String>,
    /// The Drive path of the folder (e.g. `/a/b`), alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
    /// Local directory to download into (a subfolder named after the Drive
    /// folder is created inside it). Defaults to the current directory.
    #[arg(short, long)]
    directory: Option<String>,
    /// Overwrite/merge into an already-existing destination folder.
    #[arg(short, long, default_value_t = false)]
    overwrite: bool,
}

#[derive(Subcommand)]
enum DownloadCmd {
    /// Download a file from Internxt Drive by uuid or path.
    File(DownloadFileArgs),
    /// Download a folder (recursively) from Internxt Drive.
    Folder(DownloadFolderArgs),
}

#[derive(clap::Args)]
struct CreateFolderArgs {
    /// The name for the new folder.
    #[arg(short, long)]
    name: Option<String>,
    /// Parent folder id. Leave empty for the root folder.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// Parent folder path (e.g. `/a/b`), alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
}

#[derive(Subcommand)]
enum CreateCmd {
    /// Create a folder in your Internxt Drive.
    Folder(CreateFolderArgs),
}

#[derive(clap::Args)]
struct MoveFileArgs {
    /// The id of the file to move.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// The Drive path of the file to move, alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
    /// Destination folder id. Leave empty for the root folder.
    #[arg(short, long)]
    destination: Option<String>,
    /// Destination folder path, alternative to --destination.
    #[arg(long)]
    dest_path: Option<String>,
}

#[derive(clap::Args)]
struct MoveFolderArgs {
    /// The id of the folder to move.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// The Drive path of the folder to move, alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
    /// Destination folder id. Leave empty for the root folder.
    #[arg(short, long)]
    destination: Option<String>,
    /// Destination folder path, alternative to --destination.
    #[arg(long)]
    dest_path: Option<String>,
}

#[derive(Subcommand)]
enum MoveCmd {
    /// Move a file into a destination folder.
    File(MoveFileArgs),
    /// Move a folder into a destination folder.
    Folder(MoveFolderArgs),
}

#[derive(clap::Args)]
struct RenameFileArgs {
    /// The id of the file to rename.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// The Drive path of the file to rename, alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
    /// The new name for the file.
    #[arg(short, long)]
    name: Option<String>,
}

#[derive(clap::Args)]
struct RenameFolderArgs {
    /// The id of the folder to rename.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// The Drive path of the folder to rename, alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
    /// The new name for the folder.
    #[arg(short, long)]
    name: Option<String>,
}

#[derive(Subcommand)]
enum RenameCmd {
    /// Rename a file.
    File(RenameFileArgs),
    /// Rename a folder.
    Folder(RenameFolderArgs),
}

#[derive(clap::Args)]
struct TrashFileArgs {
    /// The id of the file to trash.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// The Drive path of the file to trash, alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
}

#[derive(clap::Args)]
struct TrashFolderArgs {
    /// The id of the folder to trash.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// The Drive path of the folder to trash, alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
}

#[derive(clap::Args)]
struct TrashListArgs {
    /// Display additional information (modified date, size).
    #[arg(short, long, default_value_t = false)]
    extended: bool,
}

#[derive(clap::Args)]
struct TrashRestoreFileArgs {
    /// The id of the file to restore.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// Destination folder id. Leave empty for the root folder.
    #[arg(short, long)]
    destination: Option<String>,
    /// Destination folder path, alternative to --destination.
    #[arg(long)]
    dest_path: Option<String>,
}

#[derive(clap::Args)]
struct TrashRestoreFolderArgs {
    /// The id of the folder to restore.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// Destination folder id. Leave empty for the root folder.
    #[arg(short, long)]
    destination: Option<String>,
    /// Destination folder path, alternative to --destination.
    #[arg(long)]
    dest_path: Option<String>,
}

#[derive(clap::Args)]
struct TrashClearArgs {
    /// Empty the trash without confirmation.
    #[arg(short, long, default_value_t = false)]
    force: bool,
}

#[derive(Subcommand)]
enum TrashRestoreCmd {
    /// Restore a trashed file into a destination folder.
    File(TrashRestoreFileArgs),
    /// Restore a trashed folder into a destination folder.
    Folder(TrashRestoreFolderArgs),
}

#[derive(Subcommand)]
enum TrashCmd {
    /// Move a file to the trash.
    File(TrashFileArgs),
    /// Move a folder to the trash.
    Folder(TrashFolderArgs),
    /// List the contents of the trash.
    List(TrashListArgs),
    /// Restore a trashed file or folder into a destination folder.
    #[command(subcommand)]
    Restore(TrashRestoreCmd),
    /// Empty the trash permanently. This action cannot be undone.
    Clear(TrashClearArgs),
}

#[derive(clap::Args)]
struct DeletePermanentlyFileArgs {
    /// The id of the file to permanently delete.
    #[arg(short = 'i', long)]
    id: Option<String>,
}

#[derive(clap::Args)]
struct DeletePermanentlyFolderArgs {
    /// The id of the folder to permanently delete.
    #[arg(short = 'i', long)]
    id: Option<String>,
}

#[derive(Subcommand)]
enum DeletePermanentlyCmd {
    /// Permanently delete a file. This action cannot be undone.
    File(DeletePermanentlyFileArgs),
    /// Permanently delete a folder. This action cannot be undone.
    Folder(DeletePermanentlyFolderArgs),
}

#[derive(clap::Args)]
struct DeleteFileArgs {
    /// The id of the file to delete.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// The Drive path of the file to delete, alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
    /// Delete permanently instead of moving to the trash. This action cannot be undone.
    #[arg(long, default_value_t = false)]
    permanent: bool,
}

#[derive(clap::Args)]
struct DeleteFolderArgs {
    /// The id of the folder to delete.
    #[arg(short = 'i', long)]
    id: Option<String>,
    /// The Drive path of the folder to delete, alternative to --id.
    #[arg(short, long)]
    path: Option<String>,
    /// Delete permanently instead of moving to the trash. This action cannot be undone.
    #[arg(long, default_value_t = false)]
    permanent: bool,
}

#[derive(Subcommand)]
enum DeleteCmd {
    /// Move a file to the trash (or permanently delete with --permanent).
    File(DeleteFileArgs),
    /// Move a folder to the trash (or permanently delete with --permanent).
    Folder(DeleteFolderArgs),
    /// Permanently delete a file or folder (cannot be undone).
    #[command(subcommand)]
    Permanently(DeletePermanentlyCmd),
}

#[derive(clap::Args)]
struct WorkspacesListArgs {
    /// Display additional information (owner, address, created at).
    #[arg(short, long, default_value_t = false)]
    extended: bool,
}

#[derive(clap::Args)]
struct WorkspacesUseArgs {
    /// The workspace id to activate. Use `workspaces-list` to view ids.
    #[arg(short = 'i', long, conflicts_with = "personal")]
    id: Option<String>,
    /// Switch back to your personal drive space (unset the active workspace).
    #[arg(short, long, default_value_t = false)]
    personal: bool,
}

#[derive(Subcommand)]
enum AccountsCmd {
    /// List every Internxt account currently logged in on this machine.
    List,
    /// Switch the active account for subsequent commands.
    Switch {
        /// Email of the account to switch to. Omit to pick interactively.
        #[arg(short, long)]
        email: Option<String>,
    },
}

#[derive(Subcommand)]
enum WorkspacesCmd {
    /// List the workspaces you belong to.
    List(WorkspacesListArgs),
    /// Set the active workspace for subsequent commands.
    Use(WorkspacesUseArgs),
    /// Unset the active workspace (operate within your personal drive space).
    Unset,
}

#[derive(Subcommand)]
enum SyncCmd {
    /// One-way sync: make a remote Drive folder match a local folder (push).
    Up {
        /// The local directory to sync from.
        #[arg(short, long)]
        local: String,
        /// The remote folder uuid to sync into. Leave empty for the root folder.
        #[arg(short, long)]
        remote: Option<String>,
        /// The remote folder path to sync into (e.g. `/a/b`), alternative to --remote.
        #[arg(long)]
        remote_path: Option<String>,
        /// Delete remote files not present locally. Optional value: `trash`
        /// (default) or `permanent`.
        #[arg(long, num_args = 0..=1, default_missing_value = "default")]
        delete: Option<String>,
        /// Print the planned actions without transferring anything.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
        /// Skip empty (0-byte) files instead of uploading them. Internxt
        /// rejects empty files on free/legacy plans (HTTP 402); use this to
        /// avoid those failures instead of seeing them reported.
        #[arg(long, default_value_t = false)]
        exclude_empty_files: bool,
        #[command(flatten)]
        limit: upload_limit::UploadLimitArgs,
    },
    /// One-way sync: make a local folder match a remote Drive folder (pull).
    Down {
        /// The local directory to sync into.
        #[arg(short, long)]
        local: String,
        /// The remote folder uuid to sync from. Leave empty for the root folder.
        #[arg(short, long)]
        remote: Option<String>,
        /// The remote folder path to sync from (e.g. `/a/b`), alternative to --remote.
        #[arg(long)]
        remote_path: Option<String>,
        /// Delete local files not present remotely. Optional value: `remove`
        /// (default; OS trash not yet supported).
        #[arg(long, num_args = 0..=1, default_missing_value = "default")]
        delete: Option<String>,
        /// Print the planned actions without transferring anything.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum ThumbnailCmd {
    /// Generate a thumbnail for a Drive file from its own image content.
    Generate {
        /// The uuid of the file.
        #[arg(short, long)]
        id: Option<String>,
        /// The Drive path of the file, alternative to --id.
        #[arg(short, long)]
        path: Option<String>,
    },
    /// Upload a custom thumbnail image for a Drive file.
    Upload {
        /// The uuid of the file.
        #[arg(short, long)]
        id: Option<String>,
        /// The Drive path of the file, alternative to --id.
        #[arg(short, long)]
        path: Option<String>,
        /// Local image to use as the thumbnail.
        #[arg(short, long)]
        file: String,
        /// Upload the image as-is instead of resizing it to a 300x300 PNG.
        #[arg(long, default_value_t = false)]
        raw: bool,
    },
    /// Download a Drive file's current thumbnail.
    Download {
        /// The uuid of the file.
        #[arg(short, long)]
        id: Option<String>,
        /// The Drive path of the file, alternative to --id.
        #[arg(short, long)]
        path: Option<String>,
        /// Directory to write the thumbnail into (default: current dir).
        #[arg(short, long)]
        directory: Option<String>,
        /// Overwrite an existing local file.
        #[arg(short, long, default_value_t = false)]
        overwrite: bool,
        /// Which thumbnail to fetch when a file has several (0-based, default 0).
        #[arg(long)]
        index: Option<usize>,
    },
    /// Display a Drive file's thumbnail inline in the terminal (Kitty/iTerm2, with
    /// a Unicode half-block fallback).
    #[cfg(feature = "termimage")]
    #[command(alias = "show")]
    Display {
        /// The uuid of the file.
        #[arg(short, long)]
        id: Option<String>,
        /// The Drive path of the file, alternative to --id.
        #[arg(short, long)]
        path: Option<String>,
        /// Which thumbnail to show when a file has several (0-based, default 0).
        #[arg(long)]
        index: Option<usize>,
        /// Max render width in terminal cells.
        #[arg(short, long)]
        width: Option<u32>,
        /// Max render height in terminal cells.
        #[arg(short = 'H', long)]
        height: Option<u32>,
    },
}

fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

/// Resolve a required flag: use the given value, else prompt interactively
/// (og's `CLIUtils.getValueFromFlag` fallback), else error in non-interactive mode.
fn required_or_prompt(value: Option<String>, flag: &str, prompt_msg: &str) -> Result<String> {
    match value {
        Some(v) => Ok(v),
        None if output::is_non_interactive() => {
            Err(anyhow::anyhow!("No value provided for required flag: {flag}"))
        }
        None => prompt(prompt_msg),
    }
}

/// Prints every command in the tree, including ones hidden from normal
/// `--help` output (the flat/hyphenated compatibility aliases).
fn print_help_all() {
    let cmd = Cli::command();
    println!("All commands, including hidden compatibility aliases ([hidden]):\n");
    print_subcommands(&cmd, 0);
}

fn print_subcommands(cmd: &clap::Command, depth: usize) {
    let indent = "  ".repeat(depth);
    for sub in cmd.get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        let hidden = if sub.is_hide_set() { " [hidden]" } else { "" };
        let aliases: Vec<&str> = sub.get_all_aliases().collect();
        let alias_str = if aliases.is_empty() {
            String::new()
        } else {
            format!(" (alias: {})", aliases.join(", "))
        };
        let about = sub.get_about().map(|s| s.to_string()).unwrap_or_default();
        println!("{indent}{}{}{} - {}", sub.get_name(), alias_str, hidden, about);
        print_subcommands(sub, depth + 1);
    }
}

async fn run_legacy_login(
    email: Option<String>,
    password: Option<String>,
    twofactor: Option<String>,
    twofactortoken: Option<String>,
    add: bool,
    replace: bool,
) -> Result<()> {
    let email = match email {
        Some(e) => e,
        None if output::is_non_interactive() => {
            return Err(anyhow::anyhow!("No value provided for required flag: email"))
        }
        None => prompt("What is your email? ")?,
    };
    let password = match password {
        Some(p) => p,
        None if output::is_non_interactive() => {
            return Err(anyhow::anyhow!(
                "No value provided for required flag: password"
            ))
        }
        None => rpassword::prompt_password("What is your password? ")?,
    };
    let creds = auth::login(
        &email,
        &password,
        twofactor.as_deref(),
        twofactortoken.as_deref(),
    )
    .await?;
    auth::save_login_credentials(&creds, add, replace)?;
    let message = format!("Successfully logged in to: {}", creds.user.email);
    output::emit(
        &format!("✓ {message}"),
        serde_json::json!({ "success": true, "message": message, "login": creds }),
    );
    Ok(())
}

#[cfg(feature = "sso")]
async fn run_sso_login(host: Option<String>, port: Option<u16>, add: bool, replace: bool) -> Result<()> {
    let creds = sso::login(host.as_deref(), port).await?;
    auth::save_login_credentials(&creds, add, replace)?;
    let message = format!("Successfully logged in to: {}", creds.user.email);
    output::emit(
        &format!("✓ {message}"),
        serde_json::json!({ "success": true, "message": message, "login": creds }),
    );
    Ok(())
}

// ---- shared command bodies, called from both the flat and space-form dispatch ----

async fn do_upload_file(args: UploadFileArgs) -> Result<()> {
    commands::upload_file(
        args.file.as_deref(),
        args.destination.as_deref(),
        args.dest_path.as_deref(),
        args.stdin,
        args.name.as_deref(),
        args.size,
        &args.limit,
    )
    .await
}

async fn do_upload_folder(args: UploadFolderArgs) -> Result<()> {
    let folder = required_or_prompt(
        args.folder,
        "folder",
        "What is the path to the folder on your computer? ",
    )?;
    commands::upload_folder(
        &folder,
        args.destination.as_deref(),
        args.dest_path.as_deref(),
        args.exclude_empty_files,
        &args.limit,
    )
    .await
}

async fn do_download_file(args: DownloadFileArgs) -> Result<()> {
    commands::download_file(
        args.id.as_deref(),
        args.path.as_deref(),
        args.directory.as_deref(),
        args.overwrite,
        args.stdout,
        args.legacy_write,
    )
    .await
}

async fn do_download_folder(args: DownloadFolderArgs) -> Result<()> {
    sync::download_folder(
        args.id.as_deref(),
        args.path.as_deref(),
        args.directory.as_deref(),
        args.overwrite,
    )
    .await
}

async fn do_create_folder(args: CreateFolderArgs) -> Result<()> {
    let name = required_or_prompt(
        args.name,
        "name",
        "What would you like to name the new folder? ",
    )?;
    drive_ops::create_folder(&name, args.id.as_deref(), args.path.as_deref()).await
}

async fn do_move_file(args: MoveFileArgs) -> Result<()> {
    drive_ops::move_file(
        args.id.as_deref(),
        args.path.as_deref(),
        args.destination.as_deref(),
        args.dest_path.as_deref(),
    )
    .await
}

async fn do_move_folder(args: MoveFolderArgs) -> Result<()> {
    drive_ops::move_folder(
        args.id.as_deref(),
        args.path.as_deref(),
        args.destination.as_deref(),
        args.dest_path.as_deref(),
    )
    .await
}

async fn do_rename_file(args: RenameFileArgs) -> Result<()> {
    let name = required_or_prompt(args.name, "name", "What is the new name of the file? ")?;
    drive_ops::rename_file(args.id.as_deref(), args.path.as_deref(), &name).await
}

async fn do_rename_folder(args: RenameFolderArgs) -> Result<()> {
    let name = required_or_prompt(args.name, "name", "What is the new name of the folder? ")?;
    drive_ops::rename_folder(args.id.as_deref(), args.path.as_deref(), &name).await
}

async fn do_delete_file(args: DeleteFileArgs) -> Result<()> {
    drive_ops::delete_file(args.id.as_deref(), args.path.as_deref(), args.permanent).await
}

async fn do_delete_folder(args: DeleteFolderArgs) -> Result<()> {
    drive_ops::delete_folder(args.id.as_deref(), args.path.as_deref(), args.permanent).await
}

async fn do_trash_file(args: TrashFileArgs) -> Result<()> {
    drive_ops::trash_file(args.id.as_deref(), args.path.as_deref()).await
}

async fn do_trash_folder(args: TrashFolderArgs) -> Result<()> {
    drive_ops::trash_folder(args.id.as_deref(), args.path.as_deref()).await
}

async fn do_trash_list(args: TrashListArgs) -> Result<()> {
    drive_ops::trash_list(args.extended).await
}

async fn do_trash_restore_file(args: TrashRestoreFileArgs) -> Result<()> {
    let id = required_or_prompt(args.id, "id", "What is the file id you want to restore? ")?;
    drive_ops::trash_restore_file(&id, args.destination.as_deref(), args.dest_path.as_deref()).await
}

async fn do_trash_restore_folder(args: TrashRestoreFolderArgs) -> Result<()> {
    let id = required_or_prompt(args.id, "id", "What is the folder id you want to restore? ")?;
    drive_ops::trash_restore_folder(&id, args.destination.as_deref(), args.dest_path.as_deref()).await
}

async fn do_trash_clear(args: TrashClearArgs) -> Result<()> {
    drive_ops::trash_clear(args.force).await
}

async fn do_delete_permanently_file(args: DeletePermanentlyFileArgs) -> Result<()> {
    let id = required_or_prompt(
        args.id,
        "id",
        "What is the file id you want to permanently delete? (This action cannot be undone) ",
    )?;
    drive_ops::delete_permanently_file(&id).await
}

async fn do_delete_permanently_folder(args: DeletePermanentlyFolderArgs) -> Result<()> {
    let id = required_or_prompt(
        args.id,
        "id",
        "What is the folder id you want to permanently delete? (This action cannot be undone) ",
    )?;
    drive_ops::delete_permanently_folder(&id).await
}

async fn do_workspaces_list(args: WorkspacesListArgs) -> Result<()> {
    workspaces::list(args.extended).await
}

async fn do_workspaces_use(args: WorkspacesUseArgs) -> Result<()> {
    workspaces::use_workspace(args.id.as_deref(), args.personal).await
}

#[tokio::main]
async fn main() {
    // Load .env (if present) before parsing args/env, so IXR_* vars can live
    // there instead of the real environment. Never overrides vars already set
    // in the environment.
    #[cfg(feature = "dotenv")]
    let _ = dotenvy::dotenv();

    // Identify as this front-end, not the official node CLI. Env
    // (IXR_INTERNXT_CLIENT / IXR_INTERNXT_VERSION) overrides for ad-hoc use —
    // env policy is the front-end's concern, so core stays env-free.
    internxt_core::config::set_client_identity(
        std::env::var("IXR_INTERNXT_CLIENT").unwrap_or_else(|_| "internxt-cli-rust".to_string()),
        std::env::var("IXR_INTERNXT_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string()),
    );
    let cli = Cli::parse();
    output::set_json(cli.json);
    output::set_non_interactive(cli.non_interactive);
    output::set_no_timeout(cli.no_timeout);
    if let Err(e) = run(cli).await {
        if output::is_json() {
            output::emit_error(&format!("{e:#}"));
        } else {
            eprintln!("✕ Error: {e:#}");
        }
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Login {
            host,
            port,
            email,
            password,
            twofactor,
            twofactortoken,
            add,
            replace,
        } => {
            #[cfg(feature = "sso")]
            {
                let _ = (email, password, twofactor, twofactortoken);
                run_sso_login(host, port, add, replace).await?;
            }
            #[cfg(not(feature = "sso"))]
            {
                let _ = (host, port);
                run_legacy_login(email, password, twofactor, twofactortoken, add, replace).await?;
            }
        }
        Commands::LoginLegacy {
            email,
            password,
            twofactor,
            twofactortoken,
            add,
            replace,
        } => {
            run_legacy_login(email, password, twofactor, twofactortoken, add, replace).await?;
        }
        Commands::LoginSso { host, port, add, replace } => {
            #[cfg(feature = "sso")]
            run_sso_login(host, port, add, replace).await?;
            #[cfg(not(feature = "sso"))]
            {
                let _ = (host, port, add, replace);
                return Err(anyhow::anyhow!(
                    "SSO login is not available: this binary was built without the `sso` feature"
                ));
            }
        }
        Commands::UploadFile(args) => do_upload_file(args).await?,
        Commands::UploadFolder(args) => do_upload_folder(args).await?,
        Commands::DownloadFile(args) => do_download_file(args).await?,
        Commands::DownloadFolder(args) => do_download_folder(args).await?,
        Commands::Logout { all } => drive_ops::logout(all).await?,
        Commands::Whoami => drive_ops::whoami().await?,
        Commands::Accounts(cmd) => match cmd {
            AccountsCmd::List => accounts::list().await?,
            AccountsCmd::Switch { email } => accounts::switch(email).await?,
        },
        Commands::Usage => drive_ops::usage().await?,
        Commands::List { id, path, extended } => {
            drive_ops::list(id.as_deref(), path.as_deref(), extended).await?
        }
        Commands::CreateFolder(args) => do_create_folder(args).await?,
        Commands::MoveFile(args) => do_move_file(args).await?,
        Commands::MoveFolder(args) => do_move_folder(args).await?,
        Commands::RenameFile(args) => do_rename_file(args).await?,
        Commands::RenameFolder(args) => do_rename_folder(args).await?,
        Commands::TrashFile(args) => do_trash_file(args).await?,
        Commands::TrashFolder(args) => do_trash_folder(args).await?,
        Commands::TrashList(args) => do_trash_list(args).await?,
        Commands::TrashRestoreFile(args) => do_trash_restore_file(args).await?,
        Commands::TrashRestoreFolder(args) => do_trash_restore_folder(args).await?,
        Commands::TrashClear(args) => do_trash_clear(args).await?,
        Commands::DeletePermanentlyFile(args) => do_delete_permanently_file(args).await?,
        Commands::DeletePermanentlyFolder(args) => do_delete_permanently_folder(args).await?,
        Commands::DeleteFile(args) => do_delete_file(args).await?,
        Commands::DeleteFolder(args) => do_delete_folder(args).await?,
        Commands::WorkspacesList(args) => do_workspaces_list(args).await?,
        Commands::WorkspacesUse(args) => do_workspaces_use(args).await?,
        Commands::WorkspacesUnset => workspaces::unset().await?,
        Commands::SyncUp {
            local,
            remote,
            remote_path,
            delete,
            dry_run,
            exclude_empty_files,
            limit,
        } => {
            sync::sync_up(
                &local,
                remote.as_deref(),
                remote_path.as_deref(),
                delete.as_deref(),
                dry_run,
                exclude_empty_files,
                &limit,
            )
            .await?
        }
        Commands::SyncDown {
            local,
            remote,
            remote_path,
            delete,
            dry_run,
        } => {
            sync::sync_down(
                &local,
                remote.as_deref(),
                remote_path.as_deref(),
                delete.as_deref(),
                dry_run,
            )
            .await?
        }
        Commands::IdFromPath { path } => paths::cmd_id_from_path(&path).await?,
        Commands::PathFromId { id } => paths::cmd_path_from_id(&id).await?,
        Commands::Thumbnail(cmd) => match cmd {
            ThumbnailCmd::Generate { id, path } => {
                thumbnail_ops::generate(id.as_deref(), path.as_deref()).await?
            }
            ThumbnailCmd::Upload {
                id,
                path,
                file,
                raw,
            } => thumbnail_ops::upload(id.as_deref(), path.as_deref(), &file, raw).await?,
            ThumbnailCmd::Download {
                id,
                path,
                directory,
                overwrite,
                index,
            } => {
                thumbnail_ops::download(id.as_deref(), path.as_deref(), directory.as_deref(), overwrite, index)
                    .await?
            }
            #[cfg(feature = "termimage")]
            ThumbnailCmd::Display {
                id,
                path,
                index,
                width,
                height,
            } => {
                thumbnail_ops::display(id.as_deref(), path.as_deref(), index, width, height).await?
            }
        },
        #[cfg(any(feature = "webdav", feature = "fuse", feature = "smb"))]
        Commands::Serve {
            protocols,
            folder_uuid,
            cache_ttl,
            no_cache,
            recent_window,
            delete_permanently,
            spool,
            spool_dir,
            max_concurrent_uploads,
            read_only,
            verbose,
            limit,
            #[cfg(feature = "webdav")]
            webdav_host,
            #[cfg(feature = "webdav")]
            webdav_port,
            #[cfg(feature = "webdav")]
            webdav_https,
            #[cfg(feature = "webdav")]
            webdav_cert,
            #[cfg(feature = "webdav")]
            webdav_key,
            #[cfg(feature = "webdav")]
            webdav_timeout,
            #[cfg(feature = "webdav")]
            webdav_create_full_path,
            #[cfg(feature = "webdav")]
            webdav_custom_auth,
            #[cfg(feature = "webdav")]
            webdav_username,
            #[cfg(feature = "webdav")]
            webdav_password,
            #[cfg(feature = "fuse")]
            fuse_mountpoint,
            #[cfg(all(unix, feature = "fuse"))]
            fuse_allow_other,
            #[cfg(feature = "smb")]
            smb_host,
            #[cfg(feature = "smb")]
            smb_port,
            #[cfg(feature = "smb")]
            smb_share,
            #[cfg(feature = "smb")]
            smb_username,
            #[cfg(feature = "smb")]
            smb_password,
            #[cfg(feature = "nfs")]
            nfs_host,
            #[cfg(feature = "nfs")]
            nfs_port,
            #[cfg(feature = "sftp")]
            sftp_host,
            #[cfg(feature = "sftp")]
            sftp_port,
            #[cfg(feature = "sftp")]
            sftp_username,
            #[cfg(feature = "sftp")]
            sftp_password,
            #[cfg(feature = "sftp")]
            sftp_host_key,
        } => {
            let protocols = serve::run::parse_protocols(&protocols)?;
            serve::log::set_verbose(verbose);
            let cache_ttl = if no_cache { 0 } else { cache_ttl };
            let spool_dir = spool_dir.map(std::path::PathBuf::from);
            // `spool` only affects the WebDAV backend (FUSE writes always spool);
            // acknowledge it on builds without WebDAV so it isn't flagged unused.
            #[cfg(not(feature = "webdav"))]
            let _ = spool;

            #[cfg(feature = "webdav")]
            let webdav = if protocols.contains(&serve::run::Protocol::Webdav) {
                let custom = if webdav_custom_auth {
                    match (webdav_username, webdav_password) {
                        (Some(u), Some(p)) => Some((u, p)),
                        _ => {
                            return Err(anyhow::anyhow!(
                                "--webdav-custom-auth requires --webdav-username and --webdav-password"
                            ))
                        }
                    }
                } else {
                    None
                };
                Some(webdav::WebdavConfig {
                    host: webdav_host,
                    port: webdav_port,
                    protocol: if webdav_https {
                        webdav::Protocol::Https
                    } else {
                        webdav::Protocol::Http
                    },
                    timeout_minutes: webdav_timeout,
                    create_full_path: webdav_create_full_path,
                    custom_auth: custom,
                    delete_permanently,
                    read_only,
                    spool,
                    spool_dir: spool_dir.clone(),
                    cert: webdav_cert.map(std::path::PathBuf::from),
                    key: webdav_key.map(std::path::PathBuf::from),
                })
            } else {
                None
            };

            #[cfg(feature = "fuse")]
            let fuse = if protocols.contains(&serve::run::Protocol::Fuse) {
                let mountpoint = fuse_mountpoint.ok_or_else(|| {
                    anyhow::anyhow!("serving `fuse` requires --fuse-mountpoint <MOUNTPOINT>")
                })?;
                #[cfg(unix)]
                let allow_other = fuse_allow_other;
                #[cfg(not(unix))]
                let allow_other = false;
                Some(fuse::MountConfig {
                    mountpoint: std::path::PathBuf::from(mountpoint),
                    cache_ttl,
                    delete_permanently,
                    spool_dir: spool_dir.clone(),
                    read_only,
                    allow_other,
                    recent_window,
                })
            } else {
                None
            };

            #[cfg(feature = "smb")]
            let smb = if protocols.contains(&serve::run::Protocol::Smb) {
                Some(smb::SmbConfig {
                    host: smb_host,
                    port: smb_port,
                    share_name: smb_share,
                    username: smb_username,
                    password: smb_password,
                    delete_permanently,
                    read_only,
                    spool_dir: spool_dir.clone(),
                    max_transfer_size: 1024 * 1024,
                    recent_window,
                })
            } else {
                None
            };

            #[cfg(feature = "nfs")]
            let nfs = if protocols.contains(&serve::run::Protocol::Nfs) {
                Some(nfs::NfsConfig {
                    host: nfs_host,
                    port: nfs_port,
                    delete_permanently,
                    read_only,
                    spool_dir: spool_dir.clone(),
                    recent_window,
                })
            } else {
                None
            };

            #[cfg(feature = "sftp")]
            let sftp = if protocols.contains(&serve::run::Protocol::Sftp) {
                Some(sftp::SftpConfig {
                    host: sftp_host,
                    port: sftp_port,
                    username: sftp_username,
                    password: sftp_password,
                    host_key: sftp_host_key.map(std::path::PathBuf::from),
                    delete_permanently,
                    read_only,
                    spool_dir: spool_dir.clone(),
                    recent_window,
                })
            } else {
                None
            };

            let config = serve::run::ServeConfig {
                protocols,
                folder_uuid,
                cache_ttl,
                max_concurrent_uploads,
                upload_limit: limit,
                #[cfg(feature = "webdav")]
                webdav,
                #[cfg(feature = "fuse")]
                fuse,
                #[cfg(feature = "smb")]
                smb,
                #[cfg(feature = "nfs")]
                nfs,
                #[cfg(feature = "sftp")]
                sftp,
            };
            serve::run::run(config).await?;
        }
        #[cfg(feature = "fuse")]
        Commands::Mount {
            mountpoint,
            folder_uuid,
            cache_ttl,
            no_cache,
            recent_window,
            delete_permanently,
            spool_dir,
            max_concurrent_uploads,
            read_only,
            #[cfg(unix)]
            allow_other,
            verbose,
            limit,
        } => {
            serve::log::set_verbose(verbose);
            let cache_ttl = if no_cache { 0 } else { cache_ttl };
            #[cfg(not(unix))]
            let allow_other = false;
            let mount = fuse::MountConfig {
                mountpoint: std::path::PathBuf::from(mountpoint),
                cache_ttl,
                delete_permanently,
                spool_dir: spool_dir.map(std::path::PathBuf::from),
                read_only,
                allow_other,
                recent_window,
            };
            let config = serve::run::ServeConfig {
                protocols: vec![serve::run::Protocol::Fuse],
                folder_uuid,
                cache_ttl,
                max_concurrent_uploads,
                upload_limit: limit,
                #[cfg(feature = "webdav")]
                webdav: None,
                fuse: Some(mount),
                #[cfg(feature = "smb")]
                smb: None,
                #[cfg(feature = "nfs")]
                nfs: None,
                #[cfg(feature = "sftp")]
                sftp: None,
            };
            serve::run::run(config).await?;
        }

        // ---- space-form dispatch: delegates to the exact same bodies as the
        // flat commands above ----
        Commands::Upload(cmd) => match cmd {
            UploadCmd::File(args) => do_upload_file(args).await?,
            UploadCmd::Folder(args) => do_upload_folder(args).await?,
        },
        Commands::Download(cmd) => match cmd {
            DownloadCmd::File(args) => do_download_file(args).await?,
            DownloadCmd::Folder(args) => do_download_folder(args).await?,
        },
        Commands::Create(cmd) => match cmd {
            CreateCmd::Folder(args) => do_create_folder(args).await?,
        },
        Commands::Move(cmd) => match cmd {
            MoveCmd::File(args) => do_move_file(args).await?,
            MoveCmd::Folder(args) => do_move_folder(args).await?,
        },
        Commands::Rename(cmd) => match cmd {
            RenameCmd::File(args) => do_rename_file(args).await?,
            RenameCmd::Folder(args) => do_rename_folder(args).await?,
        },
        Commands::Trash(cmd) => match cmd {
            TrashCmd::File(args) => do_trash_file(args).await?,
            TrashCmd::Folder(args) => do_trash_folder(args).await?,
            TrashCmd::List(args) => do_trash_list(args).await?,
            TrashCmd::Clear(args) => do_trash_clear(args).await?,
            TrashCmd::Restore(restore) => match restore {
                TrashRestoreCmd::File(args) => do_trash_restore_file(args).await?,
                TrashRestoreCmd::Folder(args) => do_trash_restore_folder(args).await?,
            },
        },
        Commands::Delete(cmd) => match cmd {
            DeleteCmd::File(args) => do_delete_file(args).await?,
            DeleteCmd::Folder(args) => do_delete_folder(args).await?,
            DeleteCmd::Permanently(perm) => match perm {
                DeletePermanentlyCmd::File(args) => do_delete_permanently_file(args).await?,
                DeletePermanentlyCmd::Folder(args) => do_delete_permanently_folder(args).await?,
            },
        },
        Commands::Workspaces(cmd) => match cmd {
            WorkspacesCmd::List(args) => do_workspaces_list(args).await?,
            WorkspacesCmd::Use(args) => do_workspaces_use(args).await?,
            WorkspacesCmd::Unset => workspaces::unset().await?,
        },
        Commands::Sync(cmd) => match cmd {
            SyncCmd::Up {
                local,
                remote,
                remote_path,
                delete,
                dry_run,
                exclude_empty_files,
                limit,
            } => {
                sync::sync_up(
                    &local,
                    remote.as_deref(),
                    remote_path.as_deref(),
                    delete.as_deref(),
                    dry_run,
                    exclude_empty_files,
                    &limit,
                )
                .await?
            }
            SyncCmd::Down {
                local,
                remote,
                remote_path,
                delete,
                dry_run,
            } => {
                sync::sync_down(&local, remote.as_deref(), remote_path.as_deref(), delete.as_deref(), dry_run)
                    .await?
            }
        },
        Commands::HelpAll => print_help_all(),
        #[cfg(feature = "self-update")]
        Commands::Update { check, yes, pre_release } => self_update_cmd::run(check, yes, pre_release).await?,
    }
    Ok(())
}

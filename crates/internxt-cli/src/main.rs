mod auth;
mod commands;
mod drive_ops;
#[cfg(all(unix, feature = "fuse"))]
mod fuse;
#[cfg(feature = "nfs")]
mod nfs;
mod output;
mod paths;
#[cfg(any(feature = "webdav", feature = "fuse", feature = "smb", feature = "nfs", feature = "sftp"))]
mod serve;
#[cfg(feature = "sftp")]
mod sftp;
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
use clap::{Parser, Subcommand};
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
        env = "INXT_NONINTERACTIVE",
        default_value_t = false
    )]
    non_interactive: bool,
}

#[derive(Subcommand)]
// The `Serve` variant carries many protocol-specific flags (more with the
// non-default nfs/sftp features). This enum is parsed once at startup, so the
// size disparity between variants is irrelevant.
#[allow(clippy::large_enum_variant)]
enum Commands {
    /// Log in to your Internxt account.
    ///
    /// Uses the web-based SSO flow when built with the `sso` feature (default),
    /// otherwise falls back to the legacy email/password flow. Use `login-sso`
    /// or `login-legacy` to force a specific flow.
    Login {
        /// SSO: address the browser uses to reach this machine (default 127.0.0.1).
        #[arg(long, env = "INXT_LOGIN_SERVER_HOST")]
        host: Option<String>,
        /// SSO: port for the local callback server (default: a random free port).
        #[arg(long, env = "INXT_LOGIN_SERVER_PORT")]
        port: Option<u16>,
        #[arg(short, long, env = "INXT_USER")]
        email: Option<String>,
        #[arg(short, long, env = "INXT_PASSWORD")]
        password: Option<String>,
        /// The two-factor auth code (TOTP).
        #[arg(short = 'w', long, env = "INXT_TWOFACTORCODE")]
        twofactor: Option<String>,
        /// The TOTP secret token, used to generate a code. Takes priority over --twofactor.
        #[arg(short = 't', long, env = "INXT_OTPTOKEN")]
        twofactortoken: Option<String>,
    },
    /// Log in with email and password (legacy flow).
    #[command(alias = "login:legacy")]
    LoginLegacy {
        #[arg(short, long, env = "INXT_USER")]
        email: Option<String>,
        #[arg(short, long, env = "INXT_PASSWORD")]
        password: Option<String>,
        /// The two-factor auth code (TOTP).
        #[arg(short = 'w', long, env = "INXT_TWOFACTORCODE")]
        twofactor: Option<String>,
        /// The TOTP secret token, used to generate a code. Takes priority over --twofactor.
        #[arg(short = 't', long, env = "INXT_OTPTOKEN")]
        twofactortoken: Option<String>,
    },
    /// Log in via the web-based SSO flow (requires the `sso` feature).
    #[command(alias = "login:sso")]
    LoginSso {
        /// Address the browser uses to reach this machine (default 127.0.0.1).
        #[arg(long, env = "INXT_LOGIN_SERVER_HOST")]
        host: Option<String>,
        /// Port for the local callback server (default: a random free port).
        #[arg(long, env = "INXT_LOGIN_SERVER_PORT")]
        port: Option<u16>,
    },
    /// Upload a file to Internxt Drive.
    #[command(alias = "upload:file")]
    UploadFile {
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
    },
    /// Upload a folder (recursively) to Internxt Drive.
    #[command(alias = "upload:folder")]
    UploadFolder {
        /// The path to the folder on your system.
        #[arg(short, long)]
        folder: Option<String>,
        /// Destination folder id. Leave empty for the root folder.
        #[arg(short = 'i', long)]
        destination: Option<String>,
        /// Destination folder path (e.g. `/a/b`), alternative to --destination.
        #[arg(long)]
        dest_path: Option<String>,
        #[command(flatten)]
        limit: upload_limit::UploadLimitArgs,
    },
    /// Download a file from Internxt Drive by uuid or path.
    #[command(alias = "download:file")]
    DownloadFile {
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
    },
    /// Log out the current user from the Internxt CLI.
    Logout,
    /// Display the current user logged into the Internxt CLI.
    Whoami,
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
    #[command(alias = "create:folder")]
    CreateFolder {
        /// The name for the new folder.
        #[arg(short, long)]
        name: Option<String>,
        /// Parent folder id. Leave empty for the root folder.
        #[arg(short = 'i', long)]
        id: Option<String>,
        /// Parent folder path (e.g. `/a/b`), alternative to --id.
        #[arg(short, long)]
        path: Option<String>,
    },
    /// Move a file into a destination folder.
    #[command(alias = "move:file")]
    MoveFile {
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
    },
    /// Move a folder into a destination folder.
    #[command(alias = "move:folder")]
    MoveFolder {
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
    },
    /// Rename a file.
    #[command(alias = "rename:file")]
    RenameFile {
        /// The id of the file to rename.
        #[arg(short = 'i', long)]
        id: Option<String>,
        /// The Drive path of the file to rename, alternative to --id.
        #[arg(short, long)]
        path: Option<String>,
        /// The new name for the file.
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Rename a folder.
    #[command(alias = "rename:folder")]
    RenameFolder {
        /// The id of the folder to rename.
        #[arg(short = 'i', long)]
        id: Option<String>,
        /// The Drive path of the folder to rename, alternative to --id.
        #[arg(short, long)]
        path: Option<String>,
        /// The new name for the folder.
        #[arg(short, long)]
        name: Option<String>,
    },
    /// Move a file to the trash.
    #[command(alias = "trash:file")]
    TrashFile {
        /// The id of the file to trash.
        #[arg(short = 'i', long)]
        id: Option<String>,
        /// The Drive path of the file to trash, alternative to --id.
        #[arg(short, long)]
        path: Option<String>,
    },
    /// Move a folder to the trash.
    #[command(alias = "trash:folder")]
    TrashFolder {
        /// The id of the folder to trash.
        #[arg(short = 'i', long)]
        id: Option<String>,
        /// The Drive path of the folder to trash, alternative to --id.
        #[arg(short, long)]
        path: Option<String>,
    },
    /// List the contents of the trash.
    #[command(alias = "trash:list")]
    TrashList {
        /// Display additional information (modified date, size).
        #[arg(short, long, default_value_t = false)]
        extended: bool,
    },
    /// Restore a trashed file into a destination folder.
    #[command(alias = "trash:restore:file")]
    TrashRestoreFile {
        /// The id of the file to restore.
        #[arg(short = 'i', long)]
        id: Option<String>,
        /// Destination folder id. Leave empty for the root folder.
        #[arg(short, long)]
        destination: Option<String>,
        /// Destination folder path, alternative to --destination.
        #[arg(long)]
        dest_path: Option<String>,
    },
    /// Restore a trashed folder into a destination folder.
    #[command(alias = "trash:restore:folder")]
    TrashRestoreFolder {
        /// The id of the folder to restore.
        #[arg(short = 'i', long)]
        id: Option<String>,
        /// Destination folder id. Leave empty for the root folder.
        #[arg(short, long)]
        destination: Option<String>,
        /// Destination folder path, alternative to --destination.
        #[arg(long)]
        dest_path: Option<String>,
    },
    /// Empty the trash permanently. This action cannot be undone.
    #[command(alias = "trash:clear")]
    TrashClear {
        /// Empty the trash without confirmation.
        #[arg(short, long, default_value_t = false)]
        force: bool,
    },
    /// Permanently delete a file. This action cannot be undone.
    #[command(alias = "delete:permanently:file")]
    DeletePermanentlyFile {
        /// The id of the file to permanently delete.
        #[arg(short = 'i', long)]
        id: Option<String>,
    },
    /// Permanently delete a folder. This action cannot be undone.
    #[command(alias = "delete:permanently:folder")]
    DeletePermanentlyFolder {
        /// The id of the folder to permanently delete.
        #[arg(short = 'i', long)]
        id: Option<String>,
    },
    /// List the workspaces you belong to.
    #[command(alias = "workspaces:list")]
    WorkspacesList {
        /// Display additional information (owner, address, created at).
        #[arg(short, long, default_value_t = false)]
        extended: bool,
    },
    /// Set the active workspace for subsequent commands.
    #[command(alias = "workspaces:use")]
    WorkspacesUse {
        /// The workspace id to activate. Use `workspaces-list` to view ids.
        #[arg(short = 'i', long, conflicts_with = "personal")]
        id: Option<String>,
        /// Switch back to your personal drive space (unset the active workspace).
        #[arg(short, long, default_value_t = false)]
        personal: bool,
    },
    /// Unset the active workspace (operate within your personal drive space).
    #[command(alias = "workspaces:unset")]
    WorkspacesUnset,
    /// One-way sync: make a remote Drive folder match a local folder (push).
    #[command(alias = "sync:up")]
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
    #[cfg(any(feature = "webdav", all(unix, feature = "fuse"), feature = "smb", feature = "nfs", feature = "sftp"))]
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
        /// attr/entry TTL). Shared by all backends.
        #[arg(long, default_value_t = 5)]
        cache_ttl: u64,
        /// Disable caching (same as --cache-ttl 0).
        #[arg(long, default_value_t = false)]
        no_cache: bool,
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
        /// FUSE: local directory to mount onto (required when `fuse` is served).
        #[cfg(all(unix, feature = "fuse"))]
        #[arg(long, value_name = "MOUNTPOINT")]
        fuse_mountpoint: Option<String>,
        /// FUSE: allow other users (and root) to access the mount (needs
        /// user_allow_other in /etc/fuse.conf on Linux).
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
    /// Mount your Internxt Drive as a local filesystem via FUSE (runs until Ctrl-C).
    ///
    /// Unix only (Linux / macOS / FreeBSD). Requires a FUSE driver at runtime
    /// (fuse3 on Linux, macFUSE on macOS). Full read-write: writes buffer to a
    /// temp file and upload in full when the file is closed.
    #[cfg(all(unix, feature = "fuse"))]
    Mount {
        /// Local directory to mount onto (must already exist).
        #[arg(value_name = "MOUNTPOINT")]
        mountpoint: String,
        /// Drive folder uuid to expose as the mount root. Omit for the drive root.
        #[arg(short = 'i', long)]
        folder_uuid: Option<String>,
        /// Cache folder listings + kernel attributes for this many seconds.
        #[arg(long, default_value_t = 5)]
        cache_ttl: u64,
        /// Disable caching (same as --cache-ttl 0; always live, slower).
        #[arg(long, default_value_t = false)]
        no_cache: bool,
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
        /// user_allow_other in /etc/fuse.conf on Linux).
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
    #[command(alias = "sync:down")]
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
    #[command(alias = "get-id", alias = "id:from:path")]
    IdFromPath {
        /// The Drive path (e.g. `/a/b/file.txt` or `/a/b`).
        #[arg(short, long)]
        path: String,
    },
    /// Print the full Drive path of a file/folder given its uuid.
    #[command(alias = "get-path", alias = "path:from:id")]
    PathFromId {
        /// The uuid of the file or folder.
        #[arg(short, long)]
        id: String,
    },
    /// Manage a file's thumbnail: generate, upload a custom one, or download it.
    #[command(subcommand, alias = "thumbnails")]
    Thumbnail(ThumbnailCmd),
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

async fn run_legacy_login(
    email: Option<String>,
    password: Option<String>,
    twofactor: Option<String>,
    twofactortoken: Option<String>,
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
    auth::save_credentials(&creds)?;
    let message = format!("Successfully logged in to: {}", creds.user.email);
    output::emit(
        &format!("✓ {message}"),
        serde_json::json!({ "success": true, "message": message, "login": creds }),
    );
    Ok(())
}

#[cfg(feature = "sso")]
async fn run_sso_login(host: Option<String>, port: Option<u16>) -> Result<()> {
    let creds = sso::login(host.as_deref(), port).await?;
    auth::save_credentials(&creds)?;
    let message = format!("Successfully logged in to: {}", creds.user.email);
    output::emit(
        &format!("✓ {message}"),
        serde_json::json!({ "success": true, "message": message, "login": creds }),
    );
    Ok(())
}

#[tokio::main]
async fn main() {
    // Identify as this front-end, not the official node CLI. Env
    // (INTERNXT_CLIENT / INTERNXT_VERSION) overrides for ad-hoc use — env policy
    // is the front-end's concern, so core stays env-free.
    internxt_core::config::set_client_identity(
        std::env::var("INTERNXT_CLIENT").unwrap_or_else(|_| "internxt-cli-rust".to_string()),
        std::env::var("INTERNXT_VERSION").unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string()),
    );
    let cli = Cli::parse();
    output::set_json(cli.json);
    output::set_non_interactive(cli.non_interactive);
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
        } => {
            #[cfg(feature = "sso")]
            {
                let _ = (email, password, twofactor, twofactortoken);
                run_sso_login(host, port).await?;
            }
            #[cfg(not(feature = "sso"))]
            {
                let _ = (host, port);
                run_legacy_login(email, password, twofactor, twofactortoken).await?;
            }
        }
        Commands::LoginLegacy {
            email,
            password,
            twofactor,
            twofactortoken,
        } => {
            run_legacy_login(email, password, twofactor, twofactortoken).await?;
        }
        Commands::LoginSso { host, port } => {
            #[cfg(feature = "sso")]
            run_sso_login(host, port).await?;
            #[cfg(not(feature = "sso"))]
            {
                let _ = (host, port);
                return Err(anyhow::anyhow!(
                    "SSO login is not available: this binary was built without the `sso` feature"
                ));
            }
        }
        Commands::UploadFile {
            file,
            destination,
            dest_path,
            stdin,
            name,
            size,
            limit,
        } => {
            commands::upload_file(
                file.as_deref(),
                destination.as_deref(),
                dest_path.as_deref(),
                stdin,
                name.as_deref(),
                size,
                &limit,
            )
            .await?;
        }
        Commands::UploadFolder {
            folder,
            destination,
            dest_path,
            limit,
        } => {
            let folder = required_or_prompt(
                folder,
                "folder",
                "What is the path to the folder on your computer? ",
            )?;
            commands::upload_folder(&folder, destination.as_deref(), dest_path.as_deref(), &limit)
                .await?;
        }
        Commands::DownloadFile {
            id,
            path,
            directory,
            overwrite,
            stdout,
        } => {
            commands::download_file(
                id.as_deref(),
                path.as_deref(),
                directory.as_deref(),
                overwrite,
                stdout,
            )
            .await?;
        }
        Commands::Logout => drive_ops::logout().await?,
        Commands::Whoami => drive_ops::whoami().await?,
        Commands::Usage => drive_ops::usage().await?,
        Commands::List { id, path, extended } => {
            drive_ops::list(id.as_deref(), path.as_deref(), extended).await?
        }
        Commands::CreateFolder { name, id, path } => {
            let name = required_or_prompt(
                name,
                "name",
                "What would you like to name the new folder? ",
            )?;
            drive_ops::create_folder(&name, id.as_deref(), path.as_deref()).await?
        }
        Commands::MoveFile {
            id,
            path,
            destination,
            dest_path,
        } => {
            drive_ops::move_file(
                id.as_deref(),
                path.as_deref(),
                destination.as_deref(),
                dest_path.as_deref(),
            )
            .await?
        }
        Commands::MoveFolder {
            id,
            path,
            destination,
            dest_path,
        } => {
            drive_ops::move_folder(
                id.as_deref(),
                path.as_deref(),
                destination.as_deref(),
                dest_path.as_deref(),
            )
            .await?
        }
        Commands::RenameFile { id, path, name } => {
            let name =
                required_or_prompt(name, "name", "What is the new name of the file? ")?;
            drive_ops::rename_file(id.as_deref(), path.as_deref(), &name).await?
        }
        Commands::RenameFolder { id, path, name } => {
            let name =
                required_or_prompt(name, "name", "What is the new name of the folder? ")?;
            drive_ops::rename_folder(id.as_deref(), path.as_deref(), &name).await?
        }
        Commands::TrashFile { id, path } => {
            drive_ops::trash_file(id.as_deref(), path.as_deref()).await?
        }
        Commands::TrashFolder { id, path } => {
            drive_ops::trash_folder(id.as_deref(), path.as_deref()).await?
        }
        Commands::TrashList { extended } => drive_ops::trash_list(extended).await?,
        Commands::TrashRestoreFile {
            id,
            destination,
            dest_path,
        } => {
            let id =
                required_or_prompt(id, "id", "What is the file id you want to restore? ")?;
            drive_ops::trash_restore_file(&id, destination.as_deref(), dest_path.as_deref()).await?
        }
        Commands::TrashRestoreFolder {
            id,
            destination,
            dest_path,
        } => {
            let id =
                required_or_prompt(id, "id", "What is the folder id you want to restore? ")?;
            drive_ops::trash_restore_folder(&id, destination.as_deref(), dest_path.as_deref())
                .await?
        }
        Commands::TrashClear { force } => drive_ops::trash_clear(force).await?,
        Commands::DeletePermanentlyFile { id } => {
            let id = required_or_prompt(
                id,
                "id",
                "What is the file id you want to permanently delete? (This action cannot be undone) ",
            )?;
            drive_ops::delete_permanently_file(&id).await?
        }
        Commands::DeletePermanentlyFolder { id } => {
            let id = required_or_prompt(
                id,
                "id",
                "What is the folder id you want to permanently delete? (This action cannot be undone) ",
            )?;
            drive_ops::delete_permanently_folder(&id).await?
        }
        Commands::WorkspacesList { extended } => workspaces::list(extended).await?,
        Commands::WorkspacesUse { id, personal } => {
            workspaces::use_workspace(id.as_deref(), personal).await?
        }
        Commands::WorkspacesUnset => workspaces::unset().await?,
        Commands::SyncUp {
            local,
            remote,
            remote_path,
            delete,
            dry_run,
            limit,
        } => {
            sync::sync_up(
                &local,
                remote.as_deref(),
                remote_path.as_deref(),
                delete.as_deref(),
                dry_run,
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
        #[cfg(any(feature = "webdav", all(unix, feature = "fuse"), feature = "smb"))]
        Commands::Serve {
            protocols,
            folder_uuid,
            cache_ttl,
            no_cache,
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
            #[cfg(all(unix, feature = "fuse"))]
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

            #[cfg(all(unix, feature = "fuse"))]
            let fuse = if protocols.contains(&serve::run::Protocol::Fuse) {
                let mountpoint = fuse_mountpoint.ok_or_else(|| {
                    anyhow::anyhow!("serving `fuse` requires --fuse-mountpoint <MOUNTPOINT>")
                })?;
                Some(fuse::MountConfig {
                    mountpoint: std::path::PathBuf::from(mountpoint),
                    cache_ttl,
                    delete_permanently,
                    spool_dir: spool_dir.clone(),
                    read_only,
                    allow_other: fuse_allow_other,
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
                #[cfg(all(unix, feature = "fuse"))]
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
        #[cfg(all(unix, feature = "fuse"))]
        Commands::Mount {
            mountpoint,
            folder_uuid,
            cache_ttl,
            no_cache,
            delete_permanently,
            spool_dir,
            max_concurrent_uploads,
            read_only,
            allow_other,
            verbose,
            limit,
        } => {
            serve::log::set_verbose(verbose);
            let cache_ttl = if no_cache { 0 } else { cache_ttl };
            let mount = fuse::MountConfig {
                mountpoint: std::path::PathBuf::from(mountpoint),
                cache_ttl,
                delete_permanently,
                spool_dir: spool_dir.map(std::path::PathBuf::from),
                read_only,
                allow_other,
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
    }
    Ok(())
}

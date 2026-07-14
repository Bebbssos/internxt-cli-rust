mod api;
mod auth;
mod commands;
mod config;
mod crypto;
mod drive_ops;
mod models;
mod network;
mod output;
#[cfg(feature = "sso")]
mod sso;
mod sync;
#[cfg(feature = "webdav")]
mod webdav;
mod workspaces;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::Write;

#[derive(Parser)]
#[command(name = "internxt", version, about = "Internxt CLI (Rust port)")]
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
        /// Read the file body from stdin instead of --file. Requires --name.
        #[arg(long, default_value_t = false)]
        stdin: bool,
        /// Drive filename (with extension) for the uploaded file. Required with --stdin.
        #[arg(short, long)]
        name: Option<String>,
        /// Exact byte length of stdin. If given, streams directly; otherwise stdin is
        /// spooled to a temp file to learn its size.
        #[arg(short = 's', long)]
        size: Option<u64>,
    },
    /// Upload a folder (recursively) to Internxt Drive.
    #[command(alias = "upload:folder")]
    UploadFolder {
        /// The path to the folder on your system.
        #[arg(short, long)]
        folder: String,
        /// Destination folder id. Leave empty for the root folder.
        #[arg(short = 'i', long)]
        destination: Option<String>,
    },
    /// Download a file from Internxt Drive by uuid.
    #[command(alias = "download:file")]
    DownloadFile {
        #[arg(short, long)]
        id: String,
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
    /// List the contents of a folder.
    List {
        /// The folder id to list. Leave empty for the root folder.
        #[arg(short = 'i', long)]
        id: Option<String>,
        /// Display additional information (modified date, size).
        #[arg(short, long, default_value_t = false)]
        extended: bool,
    },
    /// Create a folder in your Internxt Drive.
    #[command(alias = "create:folder")]
    CreateFolder {
        /// The name for the new folder.
        #[arg(short, long)]
        name: String,
        /// Parent folder id. Leave empty for the root folder.
        #[arg(short = 'i', long)]
        id: Option<String>,
    },
    /// Move a file into a destination folder.
    #[command(alias = "move:file")]
    MoveFile {
        /// The id of the file to move.
        #[arg(short = 'i', long)]
        id: String,
        /// Destination folder id. Leave empty for the root folder.
        #[arg(short, long)]
        destination: Option<String>,
    },
    /// Move a folder into a destination folder.
    #[command(alias = "move:folder")]
    MoveFolder {
        /// The id of the folder to move.
        #[arg(short = 'i', long)]
        id: String,
        /// Destination folder id. Leave empty for the root folder.
        #[arg(short, long)]
        destination: Option<String>,
    },
    /// Rename a file.
    #[command(alias = "rename:file")]
    RenameFile {
        /// The id of the file to rename.
        #[arg(short = 'i', long)]
        id: String,
        /// The new name for the file.
        #[arg(short, long)]
        name: String,
    },
    /// Rename a folder.
    #[command(alias = "rename:folder")]
    RenameFolder {
        /// The id of the folder to rename.
        #[arg(short = 'i', long)]
        id: String,
        /// The new name for the folder.
        #[arg(short, long)]
        name: String,
    },
    /// Move a file to the trash.
    #[command(alias = "trash:file")]
    TrashFile {
        /// The id of the file to trash.
        #[arg(short = 'i', long)]
        id: String,
    },
    /// Move a folder to the trash.
    #[command(alias = "trash:folder")]
    TrashFolder {
        /// The id of the folder to trash.
        #[arg(short = 'i', long)]
        id: String,
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
        id: String,
        /// Destination folder id. Leave empty for the root folder.
        #[arg(short, long)]
        destination: Option<String>,
    },
    /// Restore a trashed folder into a destination folder.
    #[command(alias = "trash:restore:folder")]
    TrashRestoreFolder {
        /// The id of the folder to restore.
        #[arg(short = 'i', long)]
        id: String,
        /// Destination folder id. Leave empty for the root folder.
        #[arg(short, long)]
        destination: Option<String>,
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
        id: String,
    },
    /// Permanently delete a folder. This action cannot be undone.
    #[command(alias = "delete:permanently:folder")]
    DeletePermanentlyFolder {
        /// The id of the folder to permanently delete.
        #[arg(short = 'i', long)]
        id: String,
    },
    /// List the workspaces you belong to.
    #[command(alias = "workspaces:list")]
    WorkspacesList {
        /// Display additional information (owner, address).
        #[arg(short, long, default_value_t = false)]
        extended: bool,
    },
    /// Set the active workspace for subsequent commands.
    #[command(alias = "workspaces:use")]
    WorkspacesUse {
        /// The workspace id to activate. Use `workspaces-list` to view ids.
        #[arg(short = 'i', long)]
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
        /// Delete remote files not present locally. Optional value: `trash`
        /// (default) or `permanent`.
        #[arg(long, num_args = 0..=1, default_missing_value = "default")]
        delete: Option<String>,
        /// Print the planned actions without transferring anything.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
    /// Serve your Internxt Drive over WebDAV (runs until Ctrl-C).
    ///
    /// Options are passed inline (no separate config command). Plain HTTP by
    /// default; use `--https` for HTTPS (self-signed unless `--cert`/`--key`
    /// are given; requires the `webdav-tls` feature).
    #[cfg(feature = "webdav")]
    Webdav {
        /// Host to bind (and advertise). Use 0.0.0.0 to accept LAN clients.
        #[arg(short = 'l', long, default_value = "127.0.0.1")]
        host: String,
        /// Port to listen on.
        #[arg(short, long, default_value_t = 3005)]
        port: u16,
        /// Serve over HTTPS instead of plain HTTP.
        #[arg(short = 's', long, default_value_t = false)]
        https: bool,
        /// TLS certificate (PEM). With --https and no --cert, a self-signed cert is generated.
        #[arg(long, requires = "https")]
        cert: Option<String>,
        /// TLS private key (PEM). Required alongside --cert.
        #[arg(long, requires = "cert")]
        key: Option<String>,
        /// Server request timeout in minutes (0 = none). Reserved for long transfers.
        #[arg(short, long, default_value_t = 60)]
        timeout: u64,
        /// Auto-create missing parent folders on PUT / MKCOL.
        #[arg(short = 'c', long, default_value_t = false)]
        create_full_path: bool,
        /// Require HTTP Basic auth from clients (needs --username and --password).
        #[arg(short = 'a', long, default_value_t = false)]
        custom_auth: bool,
        /// Username for --custom-auth.
        #[arg(short = 'u', long)]
        username: Option<String>,
        /// Password for --custom-auth.
        #[arg(short = 'w', long)]
        password: Option<String>,
        /// Delete files permanently instead of moving them to trash.
        #[arg(short = 'd', long, default_value_t = false)]
        delete_permanently: bool,
        /// Spool each PUT body to a temp file before uploading, instead of
        /// streaming the live client body straight to storage. More robust for
        /// concurrent/slow clients (e.g. WinSCP) that otherwise trip storage
        /// socket timeouts; costs temp disk + a little latency.
        #[arg(long, default_value_t = false)]
        spool: bool,
        /// Directory for --spool temp files (default: system temp dir).
        #[arg(long, requires = "spool")]
        spool_dir: Option<String>,
        /// Max PUT uploads transferring at once (0 = unlimited). Set 1 to fully
        /// serialize uploads — helps clients (e.g. WinSCP) that fan out many
        /// parallel PUTs and trip storage timeouts / connection aborts.
        #[arg(long, default_value_t = 0)]
        max_concurrent_uploads: usize,
        /// Cache folder listings for this many seconds to speed path resolution
        /// under bursts of requests. 0 disables the cache.
        #[arg(long, default_value_t = 5)]
        cache_ttl: u64,
        /// Disable the folder-listing cache (same as --cache-ttl 0).
        #[arg(long, default_value_t = false)]
        no_cache: bool,
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
        /// Delete local files not present remotely. Optional value: `remove`
        /// (default; OS trash not yet supported).
        #[arg(long, num_args = 0..=1, default_missing_value = "default")]
        delete: Option<String>,
        /// Print the planned actions without transferring anything.
        #[arg(long, default_value_t = false)]
        dry_run: bool,
    },
}

fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
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
    output::emit(
        &format!("✓ Successfully logged in to: {}", creds.user.email),
        serde_json::json!({ "success": true, "login": { "email": creds.user.email } }),
    );
    Ok(())
}

#[cfg(feature = "sso")]
async fn run_sso_login(host: Option<String>, port: Option<u16>) -> Result<()> {
    let creds = sso::login(host.as_deref(), port).await?;
    auth::save_credentials(&creds)?;
    output::emit(
        &format!("✓ Successfully logged in to: {}", creds.user.email),
        serde_json::json!({ "success": true, "login": { "email": creds.user.email } }),
    );
    Ok(())
}

#[tokio::main]
async fn main() {
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
            stdin,
            name,
            size,
        } => {
            commands::upload_file(
                file.as_deref(),
                destination.as_deref(),
                stdin,
                name.as_deref(),
                size,
            )
            .await?;
        }
        Commands::UploadFolder {
            folder,
            destination,
        } => {
            commands::upload_folder(&folder, destination.as_deref()).await?;
        }
        Commands::DownloadFile {
            id,
            directory,
            overwrite,
            stdout,
        } => {
            commands::download_file(&id, directory.as_deref(), overwrite, stdout).await?;
        }
        Commands::Logout => drive_ops::logout().await?,
        Commands::Whoami => drive_ops::whoami().await?,
        Commands::List { id, extended } => drive_ops::list(id.as_deref(), extended).await?,
        Commands::CreateFolder { name, id } => {
            drive_ops::create_folder(&name, id.as_deref()).await?
        }
        Commands::MoveFile { id, destination } => {
            drive_ops::move_file(&id, destination.as_deref()).await?
        }
        Commands::MoveFolder { id, destination } => {
            drive_ops::move_folder(&id, destination.as_deref()).await?
        }
        Commands::RenameFile { id, name } => drive_ops::rename_file(&id, &name).await?,
        Commands::RenameFolder { id, name } => drive_ops::rename_folder(&id, &name).await?,
        Commands::TrashFile { id } => drive_ops::trash_file(&id).await?,
        Commands::TrashFolder { id } => drive_ops::trash_folder(&id).await?,
        Commands::TrashList { extended } => drive_ops::trash_list(extended).await?,
        Commands::TrashRestoreFile { id, destination } => {
            drive_ops::trash_restore_file(&id, destination.as_deref()).await?
        }
        Commands::TrashRestoreFolder { id, destination } => {
            drive_ops::trash_restore_folder(&id, destination.as_deref()).await?
        }
        Commands::TrashClear { force } => drive_ops::trash_clear(force).await?,
        Commands::DeletePermanentlyFile { id } => {
            drive_ops::delete_permanently_file(&id).await?
        }
        Commands::DeletePermanentlyFolder { id } => {
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
            delete,
            dry_run,
        } => sync::sync_up(&local, remote.as_deref(), delete.as_deref(), dry_run).await?,
        Commands::SyncDown {
            local,
            remote,
            delete,
            dry_run,
        } => sync::sync_down(&local, remote.as_deref(), delete.as_deref(), dry_run).await?,
        #[cfg(feature = "webdav")]
        Commands::Webdav {
            host,
            port,
            https,
            cert,
            key,
            timeout,
            create_full_path,
            custom_auth,
            username,
            password,
            delete_permanently,
            spool,
            spool_dir,
            max_concurrent_uploads,
            cache_ttl,
            no_cache,
        } => {
            let custom = if custom_auth {
                match (username, password) {
                    (Some(u), Some(p)) => Some((u, p)),
                    _ => {
                        return Err(anyhow::anyhow!(
                            "--custom-auth requires --username and --password"
                        ))
                    }
                }
            } else {
                None
            };
            let config = webdav::WebdavConfig {
                host,
                port,
                protocol: if https {
                    webdav::Protocol::Https
                } else {
                    webdav::Protocol::Http
                },
                timeout_minutes: timeout,
                create_full_path,
                custom_auth: custom,
                delete_permanently,
                spool,
                spool_dir: spool_dir.map(std::path::PathBuf::from),
                max_concurrent_uploads,
                cache_ttl: if no_cache { 0 } else { cache_ttl },
                cert: cert.map(std::path::PathBuf::from),
                key: key.map(std::path::PathBuf::from),
            };
            webdav::run(config).await?;
        }
    }
    Ok(())
}

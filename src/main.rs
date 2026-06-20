mod api;
mod auth;
mod commands;
mod config;
mod crypto;
mod drive_ops;
mod models;
mod network;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::Write;

#[derive(Parser)]
#[command(name = "internxt", version, about = "Internxt CLI (Rust port)")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Log in with email and password (legacy flow).
    Login {
        #[arg(short, long)]
        email: Option<String>,
        #[arg(short, long)]
        password: Option<String>,
        #[arg(short = 'w', long)]
        twofactor: Option<String>,
    },
    /// Upload a file to Internxt Drive.
    #[command(alias = "upload:file")]
    UploadFile {
        #[arg(short, long)]
        file: String,
        /// Destination folder uuid. Leave empty for root.
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
}

fn prompt(msg: &str) -> Result<String> {
    print!("{msg}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("✕ Error: {e:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Login {
            email,
            password,
            twofactor,
        } => {
            let email = match email {
                Some(e) => e,
                None => prompt("What is your email? ")?,
            };
            let password = match password {
                Some(p) => p,
                None => rpassword::prompt_password("What is your password? ")?,
            };
            let creds = auth::login(&email, &password, twofactor.as_deref()).await?;
            auth::save_credentials(&creds)?;
            println!("✓ Successfully logged in to: {}", creds.user.email);
        }
        Commands::UploadFile { file, destination } => {
            commands::upload_file(&file, destination.as_deref()).await?;
        }
        Commands::DownloadFile {
            id,
            directory,
            overwrite,
        } => {
            commands::download_file(&id, directory.as_deref(), overwrite).await?;
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
    }
    Ok(())
}

mod api;
mod auth;
mod commands;
mod config;
mod crypto;
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
    }
    Ok(())
}

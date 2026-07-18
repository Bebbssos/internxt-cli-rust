//! CLI credential persistence + thin wrappers over [`internxt_core::auth`].
//!
//! Core deliberately does no filesystem I/O and leaves the terminal concerns
//! (2FA prompt, refresh warnings) to the front-end. This module owns *where and
//! how* credentials are stored (`~/.ixr/credentials`, CryptoJS-AES via core's
//! crypto) and injects the terminal callbacks, so the rest of the CLI can keep
//! calling `auth::*` unchanged.

use anyhow::{anyhow, Result};
use internxt_core::crypto;
use internxt_core::models::Credentials;
use std::path::PathBuf;

/// The CLI's data directory (`~/.ixr`). Separate from the official CLI's
/// `~/.internxt-cli` — the two don't share credentials.
pub fn data_dir() -> PathBuf {
    dirs::home_dir().expect("no home dir").join(".ixr")
}

/// The credentials file (`~/.ixr/credentials`).
pub fn credentials_file() -> PathBuf {
    data_dir().join("credentials")
}

/// Save credentials encrypted (CryptoJS AES with APP_CRYPTO_SECRET), same
/// crypto as the official CLI.
pub fn save_credentials(creds: &Credentials) -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let plain = serde_json::to_string(creds)?;
    let encrypted = crypto::encrypt_text(&plain);
    let path = credentials_file();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, encrypted)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the stored credentials (plain, non-refreshing).
pub fn read_credentials() -> Result<Credentials> {
    let path = credentials_file();
    let encrypted = std::fs::read_to_string(&path)
        .map_err(|_| anyhow!("Not logged in. Run `internxt login` first."))?;
    let plain = crypto::decrypt_text(&encrypted)?;
    Ok(serde_json::from_str(&plain)?)
}

/// Refreshing credential accessor: read from disk, let core validate/refresh the
/// token, persist back when it changed, and route core's best-effort warnings to
/// the human status stream. Use at the start of any command that talks to the API.
pub async fn get_auth_details() -> Result<Credentials> {
    let creds = read_credentials()?;
    let (creds, changed) = internxt_core::auth::refresh_credentials(creds, |m| {
        crate::output::status(&format!("warning: {m}"))
    })
    .await?;
    if changed {
        save_credentials(&creds)?;
    }
    Ok(creds)
}

/// Legacy email/password login. Prompts for the 2FA code on the terminal when the
/// account requires one and none was supplied (errors in non-interactive mode).
pub async fn login(
    email: &str,
    password: &str,
    tfa: Option<&str>,
    tfa_token: Option<&str>,
) -> Result<Credentials> {
    internxt_core::auth::login(email, password, tfa, tfa_token, prompt_2fa).await
}

fn prompt_2fa() -> Result<String> {
    use std::io::Write;
    if crate::output::is_non_interactive() {
        return Err(anyhow!("No value provided for required flag: twofactor"));
    }
    print!("What is your two-factor code? ");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim().to_string())
}

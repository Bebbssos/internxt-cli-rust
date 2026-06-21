//! Login flow + credential persistence. Mirrors og/cli auth.service + config.service.

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::api::DriveApi;
use crate::config;
use crate::crypto;
use crate::models::{Credentials, UserInfo};

/// Performs legacy email/password login and returns credentials.
///
/// `tfa` is a ready-to-use TOTP code; `tfa_token` is a TOTP *secret* from which
/// a code is generated and which takes priority over `tfa` (mirrors the node
/// CLI `--twofactortoken` flag).
pub async fn login(
    email: &str,
    password: &str,
    tfa: Option<&str>,
    tfa_token: Option<&str>,
) -> Result<Credentials> {
    let email = email.to_lowercase();
    let api = DriveApi::new();

    // 1. security details -> encrypted salt + whether 2FA is required
    let (encrypted_salt, tfa_enabled) = api.security_details(&email).await?;

    // 2. decrypt salt, hash password, re-encrypt hash
    let salt = crypto::decrypt_text(&encrypted_salt)?;
    let (_, hash) = crypto::pass_to_hash(password, Some(&salt))?;
    let encrypted_password_hash = crypto::encrypt_text(&hash);

    // 2b. obtain 2FA code if the account requires it. A TOTP secret token takes
    // priority over a literal code; otherwise prompt (unless non-interactive).
    let tfa_owned: Option<String> = if !tfa_enabled {
        None
    } else if let Some(token) = tfa_token.filter(|t| !t.trim().is_empty()) {
        Some(crypto::totp_now(token.trim())?)
    } else if let Some(code) = tfa.filter(|t| !t.trim().is_empty()) {
        Some(code.to_string())
    } else if crate::output::is_non_interactive() {
        return Err(anyhow!("No value provided for required flag: twofactor"));
    } else {
        use std::io::Write;
        print!("What is your two-factor code? ");
        std::io::stdout().flush().ok();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        Some(s.trim().to_string())
    };

    // 3. login access (without keys)
    let data = api
        .login_access(&email, &encrypted_password_hash, tfa_owned.as_deref())
        .await?;

    let token = data["newToken"]
        .as_str()
        .ok_or_else(|| anyhow!("no newToken in login response: {data}"))?
        .to_string();
    let user = &data["user"];

    let enc_mnemonic = field(user, "mnemonic")?;
    let mnemonic = crypto::decrypt_text_with_key(&enc_mnemonic, password)?;

    if !crypto::validate_mnemonic(&mnemonic) {
        return Err(anyhow!("decrypted mnemonic is invalid"));
    }

    let creds = Credentials {
        token,
        user: UserInfo {
            email: field(user, "email").unwrap_or(email),
            mnemonic,
            bucket: field(user, "bucket")?,
            bridge_user: field(user, "bridgeUser")?,
            user_id: field(user, "userId")?,
            root_folder_id: field(user, "rootFolderId")?,
        },
    };
    Ok(creds)
}

fn field(user: &Value, key: &str) -> Result<String> {
    user[key]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing user.{key}"))
}

/// Save credentials encrypted (CryptoJS AES with APP_CRYPTO_SECRET), like the node CLI.
pub fn save_credentials(creds: &Credentials) -> Result<()> {
    let dir = config::data_dir();
    std::fs::create_dir_all(&dir)?;
    let plain = serde_json::to_string(creds)?;
    let encrypted = crypto::encrypt_text(&plain);
    let path = config::credentials_file();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, encrypted)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn read_credentials() -> Result<Credentials> {
    let path = config::credentials_file();
    let encrypted = std::fs::read_to_string(&path)
        .map_err(|_| anyhow!("Not logged in. Run `internxt login` first."))?;
    let plain = crypto::decrypt_text(&encrypted)?;
    Ok(serde_json::from_str(&plain)?)
}

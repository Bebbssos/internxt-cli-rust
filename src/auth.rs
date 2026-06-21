//! Login flow + credential persistence. Mirrors og/cli auth.service + config.service.

use anyhow::{anyhow, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
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

    // Decrypt the ecc/kyber private keys when the response carries them. These
    // are only needed to decrypt workspace mnemonics, so failure is non-fatal.
    let (ecc_private_key, kyber_private_key) = decrypt_user_keys(user, password);

    let creds = Credentials {
        token,
        user: UserInfo {
            email: field(user, "email").unwrap_or(email),
            mnemonic,
            bucket: field(user, "bucket")?,
            bridge_user: field(user, "bridgeUser")?,
            user_id: field(user, "userId")?,
            root_folder_id: field(user, "rootFolderId")?,
            ecc_private_key,
            kyber_private_key,
        },
        workspace: None,
    };
    Ok(creds)
}

/// Decrypts the user's ecc + kyber private keys from the login response and
/// returns them base64-encoded as stored (mirrors node `doLogin`). Returns
/// `None` for a key when it is absent or cannot be decrypted.
fn decrypt_user_keys(user: &Value, password: &str) -> (Option<String>, Option<String>) {
    let decrypt = |encrypted: &str| -> Option<String> {
        let plain = crypto::decrypt_private_key(encrypted, password).ok()?;
        if plain.is_empty() {
            return None;
        }
        Some(B64.encode(plain.as_bytes()))
    };
    // Prefer the structured `keys.ecc/kyber`; fall back to the legacy `privateKey`.
    let ecc_enc = user["keys"]["ecc"]["privateKey"]
        .as_str()
        .or_else(|| user["privateKey"].as_str())
        .filter(|s| !s.is_empty());
    let kyber_enc = user["keys"]["kyber"]["privateKey"]
        .as_str()
        .filter(|s| !s.is_empty());
    (ecc_enc.and_then(decrypt), kyber_enc.and_then(decrypt))
}

/// Parses the `exp` claim (unix seconds) from a JWT without verifying the
/// signature. Mirrors node ValidationService.validateJwtAndCheckExpiration.
fn jwt_expiration(token: &str) -> Option<i64> {
    use base64::Engine;
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    let v: Value = serde_json::from_slice(&payload).ok()?;
    v["exp"].as_i64()
}

const TWO_DAYS_SECS: i64 = 2 * 24 * 60 * 60;

/// Reads stored credentials and, mirroring node's `getAuthDetails`, validates
/// the token and refreshes it when it is within two days of expiry. Use this
/// at the start of any command that talks to the API so the stored session
/// stays valid; `read_credentials` is the plain (non-refreshing) accessor.
pub async fn get_auth_details() -> Result<Credentials> {
    let mut creds = read_credentials()?;

    let exp = jwt_expiration(&creds.token)
        .ok_or_else(|| anyhow!("Stored credentials are invalid. Run `internxt login` again."))?;
    if !crypto::validate_mnemonic(&creds.user.mnemonic) {
        return Err(anyhow!(
            "Stored credentials are invalid. Run `internxt login` again."
        ));
    }

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let remaining = exp - now;

    if remaining <= 0 {
        return Err(anyhow!(
            "Your session has expired. Run `internxt login` again."
        ));
    }

    let mut changed = false;
    if remaining <= TWO_DAYS_SECS {
        match DriveApi::new().refresh_user_token(&creds.token).await {
            Ok(new_token) => {
                creds.token = new_token;
                changed = true;
            }
            // Refresh is best-effort: the current token is still valid (not yet
            // expired), so keep using it rather than failing the command.
            Err(e) => {
                crate::output::status(&format!("warning: token refresh failed: {e}"));
            }
        }
    }

    // Refresh the workspace credentials when their token is near/at expiry, by
    // re-fetching them (the network creds + tokenHeader rotate; the mnemonic and
    // root folder are stable). Best-effort, like the user-token refresh.
    if let Some(ws) = &creds.workspace {
        let ws_remaining = jwt_expiration(&ws.token).map(|e| e - now).unwrap_or(0);
        if ws_remaining <= TWO_DAYS_SECS {
            match refresh_workspace_credentials(&creds.token, ws.id.clone()).await {
                Ok(Some((token, bucket, user, pass))) => {
                    if let Some(w) = creds.workspace.as_mut() {
                        w.token = token;
                        w.bucket = bucket;
                        w.network_user = user;
                        w.network_pass = pass;
                    }
                    changed = true;
                }
                Ok(None) => {}
                Err(e) => {
                    crate::output::status(&format!("warning: workspace refresh failed: {e}"));
                }
            }
        }
    }

    if changed {
        save_credentials(&creds)?;
    }

    Ok(creds)
}

/// Re-fetches a workspace's network credentials + token header. Returns
/// `(tokenHeader, bucket, networkUser, networkPass)`.
async fn refresh_workspace_credentials(
    token: &str,
    workspace_id: String,
) -> Result<Option<(String, String, String, String)>> {
    let v = DriveApi::new()
        .get_workspace_credentials(token, &workspace_id)
        .await?;
    let token_header = match v["tokenHeader"].as_str() {
        Some(t) => t.to_string(),
        None => return Ok(None),
    };
    let bucket = v["bucket"].as_str().unwrap_or_default().to_string();
    let user = v["credentials"]["networkUser"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let pass = v["credentials"]["networkPass"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    Ok(Some((token_header, bucket, user, pass)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jwt_expiration_parses_exp() {
        // header.payload.signature where payload = {"exp":1700000000}
        let token = "eyJhbGciOiJIUzI1NiJ9.eyJleHAiOjE3MDAwMDAwMDB9.sig";
        assert_eq!(jwt_expiration(token), Some(1_700_000_000));
    }

    #[test]
    fn jwt_expiration_rejects_malformed() {
        assert_eq!(jwt_expiration("not-a-jwt"), None);
        assert_eq!(jwt_expiration("a.b"), None);
        // valid structure but payload has no exp
        assert_eq!(jwt_expiration("h.e30.s"), None);
    }
}

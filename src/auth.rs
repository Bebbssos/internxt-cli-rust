//! CLI credential persistence + thin wrappers over [`internxt_core::auth`].
//!
//! Core deliberately does no filesystem I/O and leaves the terminal concerns
//! (2FA prompt, refresh warnings) to the front-end. This module owns *where and
//! how* credentials are stored (`~/.ixr/credentials`, CryptoJS-AES via core's
//! crypto) and injects the terminal callbacks, so the rest of the CLI can keep
//! calling `auth::*` unchanged.
//!
//! **Multi-account**: the file holds every logged-in account plus a pointer to
//! the active one ([`AccountsFile`]), not a single bare [`Credentials`]. `IXR_USER`
//! (see [`env_account_override`]) lets any command target a specific account for
//! that invocation only, without changing which one is active.

use anyhow::{anyhow, Result};
use internxt_core::crypto;
use internxt_core::models::Credentials;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The CLI's data directory (`~/.ixr`). Separate from the official CLI's
/// `~/.internxt-cli` — the two don't share credentials.
pub fn data_dir() -> PathBuf {
    dirs::home_dir().expect("no home dir").join(".ixr")
}

/// The credentials file (`~/.ixr/credentials`). Holds every logged-in account.
pub fn credentials_file() -> PathBuf {
    data_dir().join("credentials")
}

/// On-disk container: every logged-in account, keyed by email, plus which one
/// is active. Encrypted as a whole with the same CryptoJS-AES scheme a bare
/// `Credentials` used to be stored with.
#[derive(Serialize, Deserialize, Default)]
struct AccountsFile {
    active: Option<String>,
    accounts: BTreeMap<String, Credentials>,
}

fn read_accounts_file() -> Result<AccountsFile> {
    let path = credentials_file();
    let encrypted = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Ok(AccountsFile::default()),
    };
    let plain = crypto::decrypt_text(&encrypted)?;
    Ok(serde_json::from_str(&plain)?)
}

fn write_accounts_file(file: &AccountsFile) -> Result<()> {
    if file.accounts.is_empty() {
        let path = credentials_file();
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        return Ok(());
    }
    let dir = data_dir();
    std::fs::create_dir_all(&dir)?;
    let plain = serde_json::to_string(file)?;
    let encrypted = crypto::encrypt_text(&plain);
    let path = credentials_file();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, encrypted)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Every logged-in account's email, sorted.
pub fn list_accounts() -> Result<Vec<String>> {
    Ok(read_accounts_file()?.accounts.into_keys().collect())
}

/// The active account's email. Falls back to "the only stored account" when
/// the pointer is unset but exactly one account exists.
pub fn active_account_email() -> Result<Option<String>> {
    let file = read_accounts_file()?;
    if let Some(active) = file.active.filter(|e| file.accounts.contains_key(e)) {
        return Ok(Some(active));
    }
    if file.accounts.len() == 1 {
        return Ok(file.accounts.into_keys().next());
    }
    Ok(None)
}

/// Set the active account. Errors if it isn't a stored account.
pub fn set_active_account(email: &str) -> Result<()> {
    let mut file = read_accounts_file()?;
    if !file.accounts.contains_key(email) {
        return Err(anyhow!("Account {email} is not logged in."));
    }
    file.active = Some(email.to_string());
    write_accounts_file(&file)
}

/// Save credentials and make that account the active one. Used by `login` and
/// by workspace-context mutation (`workspaces use`/`unset`), which persist the
/// current session unchanged.
pub fn save_credentials(creds: &Credentials) -> Result<()> {
    let mut file = read_accounts_file()?;
    file.accounts.insert(creds.user.email.clone(), creds.clone());
    file.active = Some(creds.user.email.clone());
    write_accounts_file(&file)
}

/// Save credentials without touching which account is active. Used for a
/// background token refresh and for the `IXR_USER`-scoped auto-login path, so
/// acting on a non-active account never silently steals "active" from
/// whatever the user is actually working in.
pub(crate) fn save_account_credentials_only(creds: &Credentials) -> Result<()> {
    let mut file = read_accounts_file()?;
    file.accounts.insert(creds.user.email.clone(), creds.clone());
    write_accounts_file(&file)
}

/// Remove a stored account. Clears the active pointer if it pointed there.
pub fn remove_account(email: &str) -> Result<()> {
    let mut file = read_accounts_file()?;
    file.accounts.remove(email);
    if file.active.as_deref() == Some(email) {
        file.active = None;
    }
    write_accounts_file(&file)
}

/// Every stored account's credentials (used by `logout --all` to invalidate
/// each session server-side before clearing them all).
pub fn all_credentials() -> Result<Vec<Credentials>> {
    Ok(read_accounts_file()?.accounts.into_values().collect())
}

/// Remove every stored account.
pub fn clear_all_accounts() -> Result<()> {
    let path = credentials_file();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Decide how to persist a fresh login when a *different* account may already
/// be active: same account (re-auth/refresh) just overwrites; a different
/// active account needs an explicit `add` or `replace` (flags, or an
/// interactive prompt when neither is given).
pub fn save_login_credentials(creds: &Credentials, add: bool, replace: bool) -> Result<()> {
    let new_email = &creds.user.email;
    let active = active_account_email()?;
    match active {
        Some(active) if active.eq_ignore_ascii_case(new_email) => save_credentials(creds),
        Some(active) => {
            let do_replace = if replace {
                true
            } else if add {
                false
            } else if crate::output::is_non_interactive() {
                return Err(anyhow!(
                    "Already logged in as {active}. Use --add to add {new_email} as another account, or --replace to log out {active} and replace it."
                ));
            } else {
                prompt_add_or_replace(&active, new_email)?
            };
            if do_replace {
                remove_account(&active)?;
            }
            save_credentials(creds)
        }
        None => save_credentials(creds),
    }
}

/// Interactive add-vs-replace picker (errors in json / non-interactive mode,
/// handled by the caller before this is reached).
fn prompt_add_or_replace(active_email: &str, new_email: &str) -> Result<bool> {
    use std::io::Write;
    println!("Already logged in as {active_email}.");
    print!("[A]dd {new_email} as another account, or [R]eplace {active_email}? (a/r) ");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(matches!(s.trim().to_lowercase().as_str(), "r" | "replace"))
}

/// Target account for this invocation: `IXR_USER` overrides the active
/// account for this command only (never persisted).
fn env_account_override() -> Option<String> {
    std::env::var("IXR_USER")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read the resolved account's stored credentials (plain, non-refreshing).
/// Used by `whoami`/`logout`, which handle "not logged in" specially.
pub fn read_credentials() -> Result<Credentials> {
    let file = read_accounts_file()?;
    let email = env_account_override()
        .or(active_account_email()?)
        .ok_or_else(|| anyhow!("Not logged in. Run `internxt login` first."))?;
    file.accounts
        .get(&email)
        .cloned()
        .ok_or_else(|| anyhow!("Account {email} is not logged in."))
}

/// `IXR_NO_PERSIST` (used together with `IXR_USER`): don't persist anything for
/// this invocation — not the `IXR_PASSWORD` auto-login result, not a refreshed
/// token. The account exists only in memory for this one command.
fn env_no_persist() -> bool {
    std::env::var("IXR_NO_PERSIST").is_ok()
}

/// `IXR_WORKSPACE_ID`: mirrors og's docker-entrypoint `INXT_WORKSPACE_ID`, but
/// scoped to this invocation only instead of persisting via `workspaces use`.
/// Never written to disk.
fn env_workspace_override() -> Option<String> {
    std::env::var("IXR_WORKSPACE_ID")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Refreshing credential accessor: resolve the target account (`IXR_USER`, else
/// the active one), auto-login it via `IXR_PASSWORD`/`IXR_TWOFACTORCODE`/
/// `IXR_OTPTOKEN` if it's an `IXR_USER` override with no stored session yet, let
/// core validate/refresh the token, persist back when it changed, and route
/// core's best-effort warnings to the human status stream. Use at the start of
/// any command that talks to the API.
///
/// With `IXR_USER` + `IXR_NO_PERSIST` both set, nothing this call learns or
/// refreshes is written to disk — the account lives only for this invocation.
///
/// If `IXR_WORKSPACE_ID` is set and differs from the persisted workspace (if
/// any), it overrides `creds.workspace` for this call only — never persisted,
/// never changes what `workspaces use` left active.
pub async fn get_auth_details() -> Result<Credentials> {
    let env_override = env_account_override();
    let no_persist = env_override.is_some() && env_no_persist();
    let email = match env_override.clone().or(active_account_email()?) {
        Some(e) => e,
        None => return Err(anyhow!("Not logged in. Run `internxt login` first.")),
    };

    let creds = match read_accounts_file()?.accounts.get(&email).cloned() {
        Some(c) => c,
        None if env_override.is_some() => {
            let password = std::env::var("IXR_PASSWORD")
                .ok()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "Account {email} (from IXR_USER) is not logged in. Set IXR_PASSWORD to auto-login, or run `internxt login` first."
                    )
                })?;
            let tfa = std::env::var("IXR_TWOFACTORCODE").ok().filter(|s| !s.is_empty());
            let tfa_token = std::env::var("IXR_OTPTOKEN").ok().filter(|s| !s.is_empty());
            let creds = login(&email, &password, tfa.as_deref(), tfa_token.as_deref()).await?;
            if !no_persist {
                save_account_credentials_only(&creds)?;
            }
            creds
        }
        None => return Err(anyhow!("Account {email} is not logged in.")),
    };

    let (mut creds, changed) = internxt_core::auth::refresh_credentials(creds, |m| {
        crate::output::status(&format!("warning: {m}"))
    })
    .await?;
    if changed && !no_persist {
        save_account_credentials_only(&creds)?;
    }

    if let Some(workspace_id) = env_workspace_override() {
        if creds.workspace.as_ref().map(|w| w.id.as_str()) != Some(workspace_id.as_str()) {
            let resp = internxt_core::api::DriveApi::new()
                .get_workspaces(&creds.token)
                .await?;
            let workspaces = crate::workspaces::available_workspaces(&resp);
            creds.workspace =
                Some(crate::workspaces::build_context(&creds, &workspaces, &workspace_id).await?);
        }
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

//! Multi-account management: `accounts list` / `accounts switch`. No official
//! CLI equivalent (it only supports one account at a time). Mirrors
//! `workspaces.rs`'s style for listing/picking.

use anyhow::{anyhow, Result};
use serde_json::json;
use std::io::Write;

use crate::auth;
use crate::output;

pub async fn list() -> Result<()> {
    let accounts = auth::list_accounts()?;
    let active = auth::active_account_email()?;

    if !output::is_json() {
        if accounts.is_empty() {
            output::status("No accounts logged in. Run `ixr login`.");
        } else {
            for email in &accounts {
                let marker = if Some(email) == active.as_ref() { "*" } else { " " };
                println!("{marker} {email}");
            }
        }
    }
    output::emit(
        "",
        json!({ "success": true, "accounts": accounts, "active": active }),
    );
    Ok(())
}

pub async fn switch(email: Option<String>) -> Result<()> {
    let accounts = auth::list_accounts()?;
    if accounts.is_empty() {
        return Err(anyhow!("No accounts logged in. Run `ixr login` first."));
    }

    let target = match email {
        Some(e) if !e.trim().is_empty() => e.trim().to_string(),
        _ => select_account(&accounts)?,
    };
    auth::set_active_account(&target)?;

    output::emit(
        &format!("✓ Switched to {target}."),
        json!({ "success": true, "active": target }),
    );
    Ok(())
}

/// Interactive account picker (errors in json / non-interactive mode).
fn select_account(accounts: &[String]) -> Result<String> {
    if output::is_json() || output::is_non_interactive() {
        return Err(anyhow!(
            "No value provided for required flag: email (use `accounts list` to view accounts)."
        ));
    }
    println!("Logged-in accounts:");
    for (i, email) in accounts.iter().enumerate() {
        println!("  [{}] {email}", i + 1);
    }
    print!("Which account do you want to use? (number) ");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let idx: usize = s.trim().parse().map_err(|_| anyhow!("Invalid selection."))?;
    accounts
        .get(idx.wrapping_sub(1))
        .cloned()
        .ok_or_else(|| anyhow!("Selection out of range."))
}

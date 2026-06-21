//! Workspace commands: list, use, unset. Mirrors og/cli workspaces-* commands
//! and workspace.service. Decrypts each workspace mnemonic with the user's
//! ecc/kyber private keys (see [`crate::crypto::decrypt_workspace_key`]).

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::api::DriveApi;
use crate::auth;
use crate::crypto;
use crate::drive_ops::{human_file_size, print_table};
use crate::models::{Credentials, WorkspaceContext};
use crate::output;

/// `availableWorkspaces` entries from GET /workspaces/.
fn available_workspaces(resp: &Value) -> Vec<Value> {
    resp["availableWorkspaces"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn used_space(ws: &Value) -> f64 {
    let drive = ws["workspaceUser"]["driveUsage"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let backups = ws["workspaceUser"]["backupsUsage"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    drive + backups
}

fn space_limit(ws: &Value) -> f64 {
    ws["workspaceUser"]["spaceLimit"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

pub async fn list(extended: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let resp = DriveApi::new().get_workspaces(&creds.token).await?;
    let workspaces = available_workspaces(&resp);

    if !output::is_json() {
        let mut rows = Vec::new();
        for ws in &workspaces {
            let name = ws["workspace"]["name"].as_str().unwrap_or("").to_string();
            let id = ws["workspace"]["id"].as_str().unwrap_or("").to_string();
            let used = human_file_size(used_space(ws));
            let avail = human_file_size(space_limit(ws));
            let mut row = vec![name, id, used, avail];
            if extended {
                row.push(ws["workspace"]["ownerId"].as_str().unwrap_or("").to_string());
                row.push(ws["workspace"]["address"].as_str().unwrap_or("").to_string());
            }
            rows.push(row);
        }
        let mut headers = vec!["Name", "Workspace ID", "Used space", "Available space"];
        if extended {
            headers.push("Owner ID");
            headers.push("Address");
        }
        if rows.is_empty() {
            output::status("No workspaces found.");
        } else {
            print_table(&headers, &rows);
        }
    }
    output::emit("", json!({ "success": true, "list": { "workspaces": workspaces } }));
    Ok(())
}

/// Build the active-workspace context for `workspace_id`: fetch credentials and
/// decrypt the workspace mnemonic with the user's keys.
async fn build_context(
    creds: &Credentials,
    workspaces: &[Value],
    workspace_id: &str,
) -> Result<WorkspaceContext> {
    let selected = workspaces
        .iter()
        .find(|w| w["workspace"]["id"].as_str() == Some(workspace_id))
        .ok_or_else(|| anyhow!("Workspace {workspace_id} not found."))?;

    let ecc = creds.user.ecc_private_key.as_deref().ok_or_else(|| {
        anyhow!("Your stored credentials have no private keys; run `internxt login` again to enable workspaces.")
    })?;
    let encrypted_mnemonic = selected["workspaceUser"]["key"]
        .as_str()
        .ok_or_else(|| anyhow!("workspace has no encryption key"))?;
    let mnemonic = crypto::decrypt_workspace_key(
        encrypted_mnemonic,
        ecc,
        creds.user.kyber_private_key.as_deref(),
    )
    .map_err(|e| anyhow!("Failed to decrypt workspace mnemonic: {e}"))?;
    if !crypto::validate_mnemonic(&mnemonic) {
        return Err(anyhow!("Decrypted workspace mnemonic is invalid."));
    }

    let cred = DriveApi::new()
        .get_workspace_credentials(&creds.token, workspace_id)
        .await?;

    Ok(WorkspaceContext {
        id: workspace_id.to_string(),
        name: selected["workspace"]["name"].as_str().unwrap_or("").to_string(),
        token: cred["tokenHeader"]
            .as_str()
            .ok_or_else(|| anyhow!("no tokenHeader in workspace credentials"))?
            .to_string(),
        bucket: cred["bucket"].as_str().unwrap_or("").to_string(),
        network_user: cred["credentials"]["networkUser"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        network_pass: cred["credentials"]["networkPass"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        mnemonic,
        root_folder_id: selected["workspaceUser"]["rootFolderId"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    })
}

pub async fn use_workspace(id: Option<&str>, personal: bool) -> Result<()> {
    if personal {
        return unset().await;
    }
    let mut creds = auth::get_auth_details().await?;
    let resp = DriveApi::new().get_workspaces(&creds.token).await?;
    let workspaces = available_workspaces(&resp);

    let workspace_id = match id {
        Some(i) if !i.trim().is_empty() => i.trim().to_string(),
        _ => select_workspace_id(&workspaces)?,
    };

    let context = build_context(&creds, &workspaces, &workspace_id).await?;
    let summary = json!({
        "id": context.id,
        "name": context.name,
        "bucket": context.bucket,
        "rootFolderId": context.root_folder_id,
    });
    creds.workspace = Some(context);
    auth::save_credentials(&creds)?;

    output::emit(
        &format!(
            "✓ Workspace {workspace_id} selected. All subsequent commands operate within this workspace until changed or unset."
        ),
        json!({ "success": true, "workspace": summary }),
    );
    Ok(())
}

/// Interactive workspace picker (errors in json / non-interactive mode).
fn select_workspace_id(workspaces: &[Value]) -> Result<String> {
    if output::is_json() || output::is_non_interactive() {
        return Err(anyhow!(
            "No value provided for required flag: id (use `workspaces list` to view ids)."
        ));
    }
    if workspaces.is_empty() {
        return Err(anyhow!("You have no workspaces."));
    }
    use std::io::Write;
    println!("Available workspaces:");
    for (i, ws) in workspaces.iter().enumerate() {
        let name = ws["workspace"]["name"].as_str().unwrap_or("");
        let id = ws["workspace"]["id"].as_str().unwrap_or("");
        println!("  [{}] {name} ({id})", i + 1);
    }
    print!("Which workspace do you want to use? (number) ");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let idx: usize = s
        .trim()
        .parse()
        .map_err(|_| anyhow!("Invalid selection."))?;
    let ws = workspaces
        .get(idx.wrapping_sub(1))
        .ok_or_else(|| anyhow!("Selection out of range."))?;
    ws["workspace"]["id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Invalid workspace."))
}

pub async fn unset() -> Result<()> {
    let mut creds = auth::get_auth_details().await?;
    creds.workspace = None;
    auth::save_credentials(&creds)?;
    output::emit(
        "✓ Personal drive space selected successfully.",
        json!({ "success": true, "message": "Personal drive space selected successfully." }),
    );
    Ok(())
}

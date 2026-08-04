//! Backups device management: list/rename/delete backup devices, and browse
//! or download what's backed up to one.
//!
//! New — no official CLI equivalent (backups are a desktop-app-only feature
//! there). The desktop app represents each backed-up device as a special
//! Drive folder ("device as folder", see og/drive-desktop-linux's
//! `backup.service.ts` / `BackupService.getDevices`): the actual continuous
//! watch-local-folders-and-upload daemon is out of scope for a CLI (no
//! background-service model here), but device management and browsing what's
//! already been backed up map cleanly onto commands.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::auth;
use crate::drive_ops::{self, format_date, human_file_size, print_table};
use crate::output;
use crate::paths;
use crate::sync;
use internxt_core::api::DriveApi;

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn bool_field(v: &Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

pub async fn devices_list(extended: bool, all: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let devices = paths::fetch_backup_devices(&api, &creds.token).await?;
    let shown: Vec<&Value> = devices.iter().filter(|d| all || !bool_field(d, "removed")).collect();

    if output::is_json() {
        let list: Vec<Value> = shown.into_iter().cloned().collect();
        output::emit("", json!({ "success": true, "list": { "devices": list } }));
        return Ok(());
    }

    if shown.is_empty() {
        output::status("No backup devices found.");
        return Ok(());
    }
    let mut rows = Vec::new();
    for d in &shown {
        let last_backup = str_field(d, "lastBackupAt");
        let mut row = vec![
            str_field(d, "plainName"),
            str_field(d, "uuid"),
            human_file_size(d.get("size").and_then(|s| s.as_f64()).unwrap_or(0.0)),
            if last_backup.is_empty() { "-".to_string() } else { format_date(&last_backup) },
        ];
        if extended {
            row.push(format_date(&str_field(d, "createdAt")));
            row.push(if bool_field(d, "removed") { "removed".to_string() } else { "active".to_string() });
        }
        rows.push(row);
    }
    let mut headers = vec!["Name", "Id", "Size", "Last backup"];
    if extended {
        headers.push("Created");
        headers.push("Status");
    }
    print_table(&headers, &rows);
    Ok(())
}

pub async fn devices_create(name: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let created = api.create_backup_device(&creds.token, name).await?;
    output::emit(
        &format!("✓ Backup device created: {name}"),
        json!({ "success": true, "device": created }),
    );
    Ok(())
}

pub async fn devices_rename(device: &str, name: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let devices = paths::fetch_backup_devices(&api, &creds.token).await?;
    let uuid = str_field(paths::resolve_backup_device(&devices, device)?, "uuid");

    let updated = api.rename_backup_device(&creds.token, &uuid, name).await?;
    output::emit(
        &format!("✓ Backup device renamed to: {name}"),
        json!({ "success": true, "device": updated }),
    );
    Ok(())
}

pub async fn devices_delete(device: &str, force: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let devices = paths::fetch_backup_devices(&api, &creds.token).await?;
    let target = paths::resolve_backup_device(&devices, device)?;
    let uuid = str_field(target, "uuid");
    let name = str_field(target, "plainName");

    if !force {
        if output::is_json() || output::is_non_interactive() {
            return Err(anyhow!(
                "The \"--force\" flag is required to delete a backup device in JSON / non-interactive mode."
            ));
        }
        use std::io::Write;
        print!(
            "Delete backup device '{name}' and everything backed up to it? Unlike Drive files/folders, backups have no trash to recover from — this cannot be undone. (y/N) "
        );
        std::io::stdout().flush().ok();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        if s.trim().to_lowercase().chars().next() != Some('y') {
            return Err(anyhow!("User confirmation is required to delete a backup device."));
        }
    }

    api.delete_backup_device(&creds.token, &uuid).await?;
    output::emit(
        &format!("✓ Backup device '{name}' deleted."),
        json!({ "success": true, "message": format!("Backup device '{name}' deleted.") }),
    );
    Ok(())
}

/// Resolve a device plus an optional subpath inside it (file or folder),
/// treating the device's Drive folder as the root for that path (same
/// `resolve_path` used for ordinary Drive paths, just rooted at the device
/// instead of the account/workspace root). `path: None` resolves to the
/// device root itself.
async fn resolve_device_subpath(
    api: &DriveApi,
    token: &str,
    device_uuid: &str,
    path: Option<&str>,
    expect: paths::Expect,
) -> Result<paths::Resolved> {
    match path {
        None => Ok(paths::Resolved { uuid: device_uuid.to_string(), is_folder: true }),
        Some(p) => paths::resolve_path(api, token, device_uuid, p, expect).await,
    }
}

/// List what's backed up to a device (or a subfolder inside it) — the
/// device's Drive folder is a normal folder, so this is a thin wrapper
/// around `drive_ops::list`.
pub async fn list(device: &str, path: Option<&str>, extended: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let devices = paths::fetch_backup_devices(&api, &creds.token).await?;
    let device_uuid = str_field(paths::resolve_backup_device(&devices, device)?, "uuid");
    let resolved = resolve_device_subpath(&api, &creds.token, &device_uuid, path, paths::Expect::Folder).await?;
    drive_ops::list(Some(&resolved.uuid), None, extended).await
}

/// Download everything backed up to a device (or a subfolder inside it) — a
/// thin wrapper around `sync::download_folder` against the resolved folder.
pub async fn download(device: &str, path: Option<&str>, directory: Option<&str>, overwrite: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let devices = paths::fetch_backup_devices(&api, &creds.token).await?;
    let device_uuid = str_field(paths::resolve_backup_device(&devices, device)?, "uuid");
    let resolved = resolve_device_subpath(&api, &creds.token, &device_uuid, path, paths::Expect::Folder).await?;
    sync::download_folder(Some(&resolved.uuid), None, directory, overwrite).await
}

/// Print the uuid (and JSON: id/isFolder/type) of a device or a file/folder
/// inside it — for scripting/chaining into other id-based commands (e.g.
/// `ixr mount /mnt/x --folder-uuid $(ixr backups get-id my-pc --path Documents)`).
pub async fn get_id(device: &str, path: Option<&str>) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let devices = paths::fetch_backup_devices(&api, &creds.token).await?;
    let device_uuid = str_field(paths::resolve_backup_device(&devices, device)?, "uuid");
    let resolved = resolve_device_subpath(&api, &creds.token, &device_uuid, path, paths::Expect::Any).await?;
    let kind = if resolved.is_folder { "folder" } else { "file" };
    output::emit(
        &resolved.uuid,
        json!({ "success": true, "uuid": resolved.uuid, "isFolder": resolved.is_folder, "type": kind }),
    );
    Ok(())
}

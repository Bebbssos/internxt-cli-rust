//! Lightweight drive commands: logout, whoami, list, folder/file ops, trash.
//! Mirrors og/cli command + drive service behaviour against DRIVE_NEW_API_URL.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Local};
use serde_json::{json, Value};

use crate::api::DriveApi;
use crate::auth;
use crate::config;

/// Resolve a folder uuid flag, falling back to the user's root folder when empty.
fn fallback_root(uuid: Option<&str>, root: &str) -> String {
    match uuid {
        Some(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => root.to_string(),
    }
}

/// Node FormatUtils.humanFileSize parity.
fn human_file_size(size: f64) -> String {
    const UNITS: [&str; 9] = ["B", "KB", "MB", "GB", "TB", "PB", "EB", "ZB", "YB"];
    let idx = if size <= 0.0 {
        0
    } else {
        ((size.ln() / 1024f64.ln()).floor() as usize).min(UNITS.len() - 1)
    };
    let val = size / 1024f64.powi(idx as i32);
    // toFixed(2) then Number() to drop trailing zeros.
    let s = format!("{val:.2}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{s} {}", UNITS[idx])
}

/// Node FormatUtils.formatDate parity: "D MMMM, YYYY [at] HH:mm" in local time.
fn format_date(iso: &str) -> String {
    match DateTime::parse_from_rfc3339(iso) {
        Ok(dt) => dt
            .with_timezone(&Local)
            .format("%-d %B, %Y at %H:%M")
            .to_string(),
        Err(_) => iso.to_string(),
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Print a simple aligned table.
fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            if cell.len() > widths[i] {
                widths[i] = cell.len();
            }
        }
    }
    let render = |cells: &[String]| {
        cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<width$}", c, width = widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
    };
    let header_cells: Vec<String> = headers.iter().map(|s| s.to_string()).collect();
    println!("{}", render(&header_cells));
    let sep: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    println!("{}", sep.join("  "));
    for row in rows {
        println!("{}", render(row));
    }
}

// ---- auth-adjacent ----

pub async fn logout() -> Result<()> {
    let creds = match auth::read_credentials() {
        Ok(c) => c,
        Err(_) => {
            println!("No user is currently logged in.");
            return Ok(());
        }
    };
    // Best effort: invalidate server-side, then always clear local credentials.
    let _ = DriveApi::new().logout(&creds.token).await;
    let path = config::credentials_file();
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    println!("✓ User logged out successfully.");
    Ok(())
}

pub async fn whoami() -> Result<()> {
    match auth::read_credentials() {
        Ok(creds) => {
            println!("✓ You are logged in as: {}.", creds.user.email);
            Ok(())
        }
        Err(_) => {
            println!("You are not logged in.");
            Ok(())
        }
    }
}

// ---- listing ----

pub async fn list(folder_id: Option<&str>, extended: bool) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    let folder_uuid = fallback_root(folder_id, &creds.user.root_folder_id);

    let folders = collect_all(&api, &creds.token, &folder_uuid, true).await?;
    let files = collect_all(&api, &creds.token, &folder_uuid, false).await?;

    render_items(&folders, &files, extended);
    Ok(())
}

/// Fetch all EXISTS subfolders/subfiles of a folder, following pagination.
async fn collect_all(
    api: &DriveApi,
    token: &str,
    folder_uuid: &str,
    folders: bool,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut offset: u32 = 0;
    loop {
        let page = if folders {
            api.get_folder_subfolders(token, folder_uuid, offset).await?
        } else {
            api.get_folder_subfiles(token, folder_uuid, offset).await?
        };
        let key = if folders { "folders" } else { "files" };
        let arr = page.get(key).and_then(|v| v.as_array()).cloned().unwrap_or_default();
        let got = arr.len() as u32;
        for item in arr {
            if str_field(&item, "status") == "EXISTS" {
                out.push(item);
            }
        }
        if got < 50 {
            break;
        }
        offset += got;
    }
    Ok(out)
}

/// Render a list/trash-list table for the given folders + files.
fn render_items(folders: &[Value], files: &[Value], extended: bool) {
    let mut rows: Vec<Vec<String>> = Vec::new();
    for f in folders {
        let mut row = vec![
            "folder".to_string(),
            str_field(f, "plainName"),
            str_field(f, "uuid"),
        ];
        if extended {
            row.push(format_date(&str_field(f, "updatedAt")));
            row.push("-".to_string());
        }
        rows.push(row);
    }
    for f in files {
        let plain = str_field(f, "plainName");
        let ftype = str_field(f, "type");
        let name = if ftype.is_empty() {
            plain
        } else {
            format!("{plain}.{ftype}")
        };
        let mut row = vec!["file".to_string(), name, str_field(f, "uuid")];
        if extended {
            let size = f
                .get("size")
                .map(|s| match s {
                    Value::Number(n) => n.as_f64().unwrap_or(0.0),
                    Value::String(s) => s.parse().unwrap_or(0.0),
                    _ => 0.0,
                })
                .unwrap_or(0.0);
            row.push(format_date(&str_field(f, "updatedAt")));
            row.push(human_file_size(size));
        }
        rows.push(row);
    }

    let headers: Vec<&str> = if extended {
        vec!["Type", "Name", "Id", "Modified", "Size"]
    } else {
        vec!["Type", "Name", "Id"]
    };
    print_table(&headers, &rows);
}

// ---- folder/file mutations ----

pub async fn create_folder(name: &str, parent_id: Option<&str>) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    let parent = fallback_root(parent_id, &creds.user.root_folder_id);

    println!("Creating folder...");
    let folder = api.create_folder(&creds.token, name, &parent).await?;
    let uuid = str_field(&folder, "uuid");
    let plain = str_field(&folder, "plainName");
    println!(
        "✓ Folder {} created successfully, view it at {}/folder/{}",
        plain,
        config::drive_web_url(),
        uuid
    );
    Ok(())
}

pub async fn move_file(file_id: &str, destination: Option<&str>) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    let dest = fallback_root(destination, &creds.user.root_folder_id);
    api.move_file(&creds.token, file_id, &dest).await?;
    println!("✓ File moved successfully to: {dest}");
    Ok(())
}

pub async fn move_folder(folder_id: &str, destination: Option<&str>) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    let dest = fallback_root(destination, &creds.user.root_folder_id);
    api.move_folder(&creds.token, folder_id, &dest).await?;
    println!("✓ Folder moved successfully to: {dest}");
    Ok(())
}

pub async fn rename_file(file_id: &str, new_name: &str) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    // Split into name + extension like node's path.parse.
    let p = std::path::Path::new(new_name);
    let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or(new_name);
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
    api.rename_file(&creds.token, file_id, name, ext).await?;
    let shown = if ext.is_empty() {
        name.to_string()
    } else {
        format!("{name}.{ext}")
    };
    println!("✓ File renamed successfully with: {shown}");
    Ok(())
}

pub async fn rename_folder(folder_id: &str, new_name: &str) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    api.rename_folder(&creds.token, folder_id, new_name).await?;
    println!("✓ Folder renamed successfully with: {new_name}");
    Ok(())
}

// ---- trash ----

pub async fn trash_file(file_id: &str) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    api.trash_items(&creds.token, json!([{ "uuid": file_id, "type": "file" }]))
        .await?;
    println!("✓ File trashed successfully.");
    Ok(())
}

pub async fn trash_folder(folder_id: &str) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    api.trash_items(&creds.token, json!([{ "uuid": folder_id, "type": "folder" }]))
        .await?;
    println!("✓ Folder trashed successfully.");
    Ok(())
}

pub async fn trash_list(extended: bool) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();

    let folders = collect_trash(&api, &creds.token, "folders").await?;
    let files = collect_trash(&api, &creds.token, "files").await?;
    render_items(&folders, &files, extended);
    Ok(())
}

async fn collect_trash(api: &DriveApi, token: &str, kind: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut offset: u32 = 0;
    loop {
        let page = api.trash_paginated(token, kind, offset).await?;
        let arr = page
            .get("result")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let got = arr.len() as u32;
        out.extend(arr);
        if got == 0 {
            break;
        }
        offset += got;
    }
    Ok(out)
}

pub async fn trash_restore_file(file_id: &str, destination: Option<&str>) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    let dest = fallback_root(destination, &creds.user.root_folder_id);
    api.move_file(&creds.token, file_id, &dest).await?;
    println!("✓ File restored successfully to: {dest}");
    Ok(())
}

pub async fn trash_restore_folder(folder_id: &str, destination: Option<&str>) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    let dest = fallback_root(destination, &creds.user.root_folder_id);
    api.move_folder(&creds.token, folder_id, &dest).await?;
    println!("✓ Folder restored successfully to: {dest}");
    Ok(())
}

pub async fn trash_clear(force: bool) -> Result<()> {
    let creds = auth::read_credentials()?;
    if !force {
        use std::io::Write;
        print!("Empty trash? All items in the Drive Trash will be permanently deleted. This action cannot be undone. (y/N) ");
        std::io::stdout().flush().ok();
        let mut s = String::new();
        std::io::stdin().read_line(&mut s)?;
        let c = s.trim().to_lowercase();
        if c.chars().next() != Some('y') {
            return Err(anyhow!(
                "User confirmation is required to empty the trash permanently."
            ));
        }
    }
    DriveApi::new().clear_trash(&creds.token).await?;
    println!("✓ Trash emptied successfully.");
    Ok(())
}

pub async fn delete_permanently_file(file_id: &str) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    // Confirm the file exists first (node fetches metadata, errors if missing).
    api.get_file_meta(&creds.token, file_id)
        .await
        .map_err(|_| anyhow!("File not found"))?;
    api.delete_file(&creds.token, file_id).await?;
    println!("✓ File permanently deleted successfully");
    Ok(())
}

pub async fn delete_permanently_folder(folder_id: &str) -> Result<()> {
    let creds = auth::read_credentials()?;
    let api = DriveApi::new();
    api.get_folder_meta(&creds.token, folder_id)
        .await
        .map_err(|_| anyhow!("Folder not found"))?;
    api.delete_folder(&creds.token, folder_id).await?;
    println!("✓ Folder permanently deleted successfully");
    Ok(())
}

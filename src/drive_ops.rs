//! Lightweight drive commands: logout, whoami, list, folder/file ops, trash.
//! Mirrors og/cli command + drive service behaviour against DRIVE_NEW_API_URL.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Local};
use serde_json::{json, Value};

use internxt_core::api::DriveApi;
use crate::auth;
use internxt_core::config;
use crate::output;
use crate::paths::{self, Expect};

/// Resolve a folder uuid flag, falling back to the user's root folder when empty.
fn fallback_root(uuid: Option<&str>, root: &str) -> String {
    match uuid {
        Some(u) if !u.trim().is_empty() => u.trim().to_string(),
        _ => root.to_string(),
    }
}

/// Node FormatUtils.humanFileSize parity.
pub(crate) fn human_file_size(size: f64) -> String {
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
pub(crate) fn format_date(iso: &str) -> String {
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

/// First non-empty of `v[primary]` / `v[fallback]` as a string. Mirrors node's
/// `folder.creationTime ?? folder.createdAt` / `modificationTime ?? updatedAt`.
fn str_field_or(v: &Value, primary: &str, fallback: &str) -> String {
    let p = str_field(v, primary);
    if !p.is_empty() {
        p
    } else {
        str_field(v, fallback)
    }
}

/// Print a simple aligned table.
pub(crate) fn print_table(headers: &[&str], rows: &[Vec<String>]) {
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

pub async fn logout(all: bool) -> Result<()> {
    if all {
        let creds_list = auth::all_credentials()?;
        if creds_list.is_empty() {
            // Not logged in anywhere: a real failure, not just an empty result —
            // return Err so main()'s generic error path sets a non-zero exit code
            // (it also happens to produce the same JSON shape as our old custom
            // `output::emit`, via `output::emit_error`).
            return Err(anyhow!("No user is currently logged in."));
        }
        // Best effort: invalidate every session server-side, then clear all local accounts.
        for creds in &creds_list {
            let _ = DriveApi::for_credentials(creds).logout(&creds.token).await;
        }
        let emails: Vec<&str> = creds_list.iter().map(|c| c.user.email.as_str()).collect();
        auth::clear_all_accounts()?;
        output::emit(
            "✓ Logged out of all accounts.",
            json!({ "success": true, "message": "Logged out of all accounts.", "accounts": emails }),
        );
        return Ok(());
    }
    let creds = match auth::read_credentials() {
        Ok(c) => c,
        Err(_) => return Err(anyhow!("No user is currently logged in.")),
    };
    // Best effort: invalidate server-side, then always clear the local account.
    let _ = DriveApi::for_credentials(&creds).logout(&creds.token).await;
    auth::remove_account(&creds.user.email)?;
    output::emit(
        "✓ User logged out successfully.",
        json!({ "success": true, "message": "User logged out successfully." }),
    );
    Ok(())
}

pub async fn whoami() -> Result<()> {
    let creds = match auth::read_credentials() {
        Ok(c) => c,
        // Not logged in is a real failure here (whoami has nothing to report):
        // return Err so main()'s generic error path sets a non-zero exit code.
        Err(_) => return Err(anyhow!("You are not logged in.")),
    };
    let email = creds.user.email.clone();
    match internxt_core::auth::refresh_credentials(creds, |m| {
        output::status(&format!("warning: {m}"))
    })
    .await
    {
        Ok((creds, changed)) => {
            if changed {
                let _ = auth::save_account_credentials_only(&creds);
            }
            let message = format!("You are logged in as: {}.", creds.user.email);
            output::emit(
                &format!("✓ {message}"),
                json!({ "success": true, "message": message, "login": creds }),
            );
            Ok(())
        }
        Err(_) => {
            // Session is expired or otherwise invalid: clear it, matching og's
            // whoami behaviour of logging the user out on a dead session, then
            // report failure (non-zero exit) via the same generic error path.
            let _ = auth::remove_account(&email);
            Err(anyhow!(
                "Your session has expired. You have been logged out. Please log in again."
            ))
        }
    }
}

/// og `UsageService.INFINITE_LIMIT` — space limits at/above this mean "unlimited".
const INFINITE_LIMIT: u64 = 99 * 1024 * 1024 * 1024 * 1024;

/// Derive a plan name from the payments tier `label` and subscription `type`,
/// as `Tier (Type)` — e.g. `Pro (Subscription)`. When both agree (`free`/`free`)
/// or one is absent, show just the single value: a genuine free account reads
/// `Free`, and a legacy lifetime account reads `Free (Lifetime)` — the tier
/// endpoint mislabels legacy plans `free`, but the `(Lifetime)` from
/// `/subscriptions` still conveys it's not a free plan. `None` = both unknown.
fn resolve_plan(label: Option<String>, subscription: Option<String>) -> Option<String> {
    let label = label.filter(|l| !l.trim().is_empty());
    let sub = subscription.filter(|s| !s.trim().is_empty());
    match (label, sub) {
        (Some(l), Some(t)) if l.eq_ignore_ascii_case(&t) => Some(cap_first(&l)),
        (Some(l), Some(t)) => Some(format!("{} ({})", cap_first(&l), cap_first(&t))),
        (Some(l), None) => Some(cap_first(&l)),
        (None, Some(t)) => Some(cap_first(&t)),
        (None, None) => None,
    }
}

/// Uppercase the first ASCII char, leave the rest untouched (preserves labels
/// like "Pro 10TB"; the API's subscription types are already lowercase).
fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

/// Account overview: plan, used space (drive/backups), space limit, per-file
/// upload cap. Not an og command — assembled from the same endpoints og's
/// UsageService uses, plus a best-effort payments tier lookup for the plan name.
pub async fn usage() -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let token = &creds.token;

    // Fan out: usage + limit + per-file cap are required; plan lookups (tier +
    // subscription) are best-effort on the separate payments API.
    let (usage, space_limit, upload_limit, tier, subscription) = tokio::join!(
        api.space_usage(token),
        api.space_limit(token),
        api.get_file_limits(token),
        api.user_tier(token),
        api.user_subscription(token),
    );
    let usage = usage?;
    let space_limit = space_limit?;
    let upload_limit = upload_limit?;
    let tier_label = tier.ok().flatten();
    let sub_type = subscription.ok().flatten();
    let plan = resolve_plan(tier_label.clone(), sub_type.clone());

    let space_infinite = space_limit >= INFINITE_LIMIT;
    let used_percent = if space_limit > 0 && !space_infinite {
        Some(usage.total as f64 / space_limit as f64 * 100.0)
    } else {
        None
    };
    let limit_str = if space_infinite {
        "Unlimited".to_string()
    } else {
        human_file_size(space_limit as f64)
    };
    let upload_str = match upload_limit {
        Some(b) => human_file_size(b as f64),
        None => "Unlimited".to_string(),
    };
    let plan_str = plan.clone().unwrap_or_else(|| "unknown".to_string());

    let used_line = match used_percent {
        Some(p) => format!(
            "{} / {} ({:.1}%)",
            human_file_size(usage.total as f64),
            limit_str,
            p
        ),
        None => format!("{} / {}", human_file_size(usage.total as f64), limit_str),
    };
    output::emit(
        &format!(
            "Plan:               {plan_str}\n\
             Used:               {used_line}\n  \
               Drive:            {}\n  \
               Backups:          {}\n\
             Space limit:        {limit_str}\n\
             Upload file limit:  {upload_str}",
            human_file_size(usage.drive as f64),
            human_file_size(usage.backups as f64),
        ),
        json!({
            "success": true,
            "usage": {
                "plan": plan,
                "planLabel": tier_label,
                "subscriptionType": sub_type,
                "used": usage.total,
                "drive": usage.drive,
                "backups": usage.backups,
                "spaceLimit": space_limit,
                "spaceLimitInfinite": space_infinite,
                "usedPercent": used_percent,
                "uploadFileLimit": upload_limit,
            }
        }),
    );
    Ok(())
}

// ---- listing ----

pub async fn list(id: Option<&str>, path: Option<&str>, extended: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let resolved =
        paths::resolve_opt(&api, &creds.token, creds.root_folder(), id, path, Expect::Folder).await?;
    let folder_uuid = fallback_root(resolved.as_deref(), creds.root_folder());

    // `paths::resolve_opt`'s `Expect::Folder` check only runs when resolving from
    // `--path` (via `resolve_path`); its `--id` branch passes the uuid through
    // unchecked (by design — see its doc comment). The folder-content listing
    // endpoints below don't 404 for a file's uuid, they just return an empty page,
    // so without this check `list --id <a-file-uuid>` would report a fake "success,
    // empty folder" instead of erroring. Only needed when the uuid actually came
    // from `--id`: `resolve_opt` already rejects id+path together, and the root
    // fallback is trusted, so id being non-empty here means the id-branch ran.
    if id.map(|s| !s.trim().is_empty()).unwrap_or(false)
        && api.get_folder_meta(&creds.token, &folder_uuid).await.is_err()
    {
        return Err(
            if api.get_file_meta_value(&creds.token, &folder_uuid).await.is_ok() {
                anyhow!("'{folder_uuid}' is a file, not a folder")
            } else {
                anyhow!("No such folder with id: {folder_uuid}")
            },
        );
    }

    let folders = collect_all(&api, &creds.token, &folder_uuid, true).await?;
    let files = collect_all(&api, &creds.token, &folder_uuid, false).await?;

    if output::is_json() {
        output::emit("", json!({ "success": true, "list": { "folders": folders, "files": files } }));
    } else {
        render_items(&folders, &files, extended);
    }
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
        // Personal endpoints return `.folders`/`.files`; workspace endpoints
        // return `.result`.
        let arr = page
            .get(key)
            .or_else(|| page.get("result"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let got = arr.len() as u32;
        for item in arr {
            // Keep EXISTS items; workspace results may omit `status`.
            let status = str_field(&item, "status");
            if status.is_empty() || status == "EXISTS" {
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
            row.push(format_date(&str_field_or(f, "creationTime", "createdAt")));
            row.push(format_date(&str_field_or(f, "modificationTime", "updatedAt")));
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
            row.push(format_date(&str_field_or(f, "creationTime", "createdAt")));
            row.push(format_date(&str_field_or(f, "modificationTime", "updatedAt")));
            row.push(human_file_size(size));
        }
        rows.push(row);
    }

    let headers: Vec<&str> = if extended {
        vec!["Type", "Name", "Id", "Created", "Modified", "Size"]
    } else {
        vec!["Type", "Name", "Id"]
    };
    print_table(&headers, &rows);
}

// ---- folder/file mutations ----

pub async fn create_folder(
    name: &str,
    parent_id: Option<&str>,
    parent_path: Option<&str>,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let resolved = paths::resolve_opt(
        &api,
        &creds.token,
        creds.root_folder(),
        parent_id,
        parent_path,
        Expect::Folder,
    )
    .await?;
    let parent = fallback_root(resolved.as_deref(), creds.root_folder());

    output::status("Creating folder...");
    let folder = api.create_folder(&creds.token, name, &parent).await?;
    let uuid = str_field(&folder, "uuid");
    let plain = str_field(&folder, "plainName");
    output::emit(
        &format!(
            "✓ Folder {} created successfully, view it at {}/folder/{}",
            plain,
            config::drive_web_url(),
            uuid
        ),
        json!({ "success": true, "folder": folder }),
    );
    Ok(())
}

pub async fn move_file(
    id: Option<&str>,
    path: Option<&str>,
    destination: Option<&str>,
    dest_path: Option<&str>,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let root = creds.root_folder();
    let file_id = paths::resolve_opt(&api, &creds.token, root, id, path, Expect::File)
        .await?
        .ok_or_else(|| anyhow!("Provide the file id (--id) or path (--path)"))?;
    let dest = paths::resolve_destination_opt(&api, &creds.token, root, destination, dest_path, Expect::Folder)
        .await?;
    let dest = fallback_root(dest.as_deref(), root);
    let file = api.move_file(&creds.token, &file_id, &dest).await?;
    output::emit(
        &format!("✓ File moved successfully to: {dest}"),
        json!({ "success": true, "file": file }),
    );
    Ok(())
}

pub async fn move_folder(
    id: Option<&str>,
    path: Option<&str>,
    destination: Option<&str>,
    dest_path: Option<&str>,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let root = creds.root_folder();
    let folder_id = paths::resolve_opt(&api, &creds.token, root, id, path, Expect::Folder)
        .await?
        .ok_or_else(|| anyhow!("Provide the folder id (--id) or path (--path)"))?;
    let dest = paths::resolve_destination_opt(&api, &creds.token, root, destination, dest_path, Expect::Folder)
        .await?;
    let dest = fallback_root(dest.as_deref(), root);
    let folder = api.move_folder(&creds.token, &folder_id, &dest).await?;
    output::emit(
        &format!("✓ Folder moved successfully to: {dest}"),
        json!({ "success": true, "folder": folder }),
    );
    Ok(())
}

pub async fn rename_file(id: Option<&str>, path: Option<&str>, new_name: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let file_id = paths::resolve_opt(&api, &creds.token, creds.root_folder(), id, path, Expect::File)
        .await?
        .ok_or_else(|| anyhow!("Provide the file id (--id) or path (--path)"))?;
    paths::validate_name(new_name)?;
    // Split into name + extension like node's path.parse.
    let p = std::path::Path::new(new_name);
    let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or(new_name);
    let ext = p.extension().and_then(|s| s.to_str()).unwrap_or("");
    api.rename_file(&creds.token, &file_id, name, ext).await?;
    let shown = if ext.is_empty() {
        name.to_string()
    } else {
        format!("{name}.{ext}")
    };
    output::emit(
        &format!("✓ File renamed successfully with: {shown}"),
        json!({ "success": true, "file": { "uuid": file_id, "plainName": name, "type": ext } }),
    );
    Ok(())
}

pub async fn rename_folder(id: Option<&str>, path: Option<&str>, new_name: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let folder_id =
        paths::resolve_opt(&api, &creds.token, creds.root_folder(), id, path, Expect::Folder)
            .await?
            .ok_or_else(|| anyhow!("Provide the folder id (--id) or path (--path)"))?;
    api.rename_folder(&creds.token, &folder_id, new_name).await?;
    output::emit(
        &format!("✓ Folder renamed successfully with: {new_name}"),
        json!({ "success": true, "folder": { "uuid": folder_id, "plainName": new_name } }),
    );
    Ok(())
}

// ---- trash ----

pub async fn trash_file(id: Option<&str>, path: Option<&str>) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let file_id = paths::resolve_opt(&api, &creds.token, creds.root_folder(), id, path, Expect::File)
        .await?
        .ok_or_else(|| anyhow!("Provide the file id (--id) or path (--path)"))?;
    api.trash_items(&creds.token, json!([{ "uuid": file_id, "type": "file" }]))
        .await?;
    output::emit(
        "✓ File trashed successfully.",
        json!({ "success": true, "file": { "uuid": file_id } }),
    );
    Ok(())
}

pub async fn trash_folder(id: Option<&str>, path: Option<&str>) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let folder_id =
        paths::resolve_opt(&api, &creds.token, creds.root_folder(), id, path, Expect::Folder)
            .await?
            .ok_or_else(|| anyhow!("Provide the folder id (--id) or path (--path)"))?;
    api.trash_items(&creds.token, json!([{ "uuid": folder_id, "type": "folder" }]))
        .await?;
    output::emit(
        "✓ Folder trashed successfully.",
        json!({ "success": true, "folder": { "uuid": folder_id } }),
    );
    Ok(())
}

pub async fn delete_file(id: Option<&str>, path: Option<&str>, permanent: bool) -> Result<()> {
    if !permanent {
        return trash_file(id, path).await;
    }
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let file_id = paths::resolve_opt(&api, &creds.token, creds.root_folder(), id, path, Expect::File)
        .await?
        .ok_or_else(|| anyhow!("Provide the file id (--id) or path (--path)"))?;
    delete_permanently_file(&file_id).await
}

pub async fn delete_folder(id: Option<&str>, path: Option<&str>, permanent: bool) -> Result<()> {
    if !permanent {
        return trash_folder(id, path).await;
    }
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let folder_id =
        paths::resolve_opt(&api, &creds.token, creds.root_folder(), id, path, Expect::Folder)
            .await?
            .ok_or_else(|| anyhow!("Provide the folder id (--id) or path (--path)"))?;
    delete_permanently_folder(&folder_id).await
}

pub async fn trash_list(extended: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);

    let folders = collect_trash(&api, &creds.token, "folders").await?;
    let files = collect_trash(&api, &creds.token, "files").await?;
    if output::is_json() {
        output::emit("", json!({ "success": true, "list": { "folders": folders, "files": files } }));
    } else {
        render_items(&folders, &files, extended);
    }
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

pub async fn trash_restore_file(
    file_id: &str,
    destination: Option<&str>,
    dest_path: Option<&str>,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let dest = paths::resolve_destination_opt(
        &api,
        &creds.token,
        creds.root_folder(),
        destination,
        dest_path,
        Expect::Folder,
    )
    .await?;
    let dest = fallback_root(dest.as_deref(), creds.root_folder());
    let file = api.move_file(&creds.token, file_id, &dest).await?;
    output::emit(
        &format!("✓ File restored successfully to: {dest}"),
        json!({ "success": true, "file": file }),
    );
    Ok(())
}

pub async fn trash_restore_folder(
    folder_id: &str,
    destination: Option<&str>,
    dest_path: Option<&str>,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let dest = paths::resolve_destination_opt(
        &api,
        &creds.token,
        creds.root_folder(),
        destination,
        dest_path,
        Expect::Folder,
    )
    .await?;
    let dest = fallback_root(dest.as_deref(), creds.root_folder());
    let folder = api.move_folder(&creds.token, folder_id, &dest).await?;
    output::emit(
        &format!("✓ Folder restored successfully to: {dest}"),
        json!({ "success": true, "folder": folder }),
    );
    Ok(())
}

pub async fn trash_clear(force: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    if !force {
        if output::is_json() || output::is_non_interactive() {
            return Err(anyhow!(
                "The \"--force\" flag is required to empty the trash in JSON / non-interactive mode."
            ));
        }
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
    DriveApi::for_credentials(&creds).clear_trash(&creds.token).await?;
    output::emit(
        "✓ Trash emptied successfully.",
        json!({ "success": true, "message": "Trash emptied successfully." }),
    );
    Ok(())
}

pub async fn delete_permanently_file(file_id: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    // Confirm the file exists first (node fetches metadata, errors if missing).
    api.get_file_meta(&creds.token, file_id)
        .await
        .map_err(|_| anyhow!("File not found"))?;
    api.delete_file(&creds.token, file_id).await?;
    output::emit(
        "✓ File permanently deleted successfully",
        json!({ "success": true, "message": "File permanently deleted successfully" }),
    );
    Ok(())
}

pub async fn delete_permanently_folder(folder_id: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    api.get_folder_meta(&creds.token, folder_id)
        .await
        .map_err(|_| anyhow!("Folder not found"))?;
    api.delete_folder(&creds.token, folder_id).await?;
    output::emit(
        "✓ Folder permanently deleted successfully",
        json!({ "success": true, "message": "Folder permanently deleted successfully" }),
    );
    Ok(())
}

/// Regression tests for the exit-code bug: `whoami`/`logout` used to report
/// "not logged in" via a custom `output::emit` call and then return `Ok(())`,
/// so the process exited 0 even though it had just printed a failure. They
/// must now return `Err` (routed through `main()`'s generic error path, which
/// sets a non-zero exit) instead.
///
/// Sandboxes `HOME` to a fresh, empty temp dir so `auth::read_credentials()`/
/// `auth::all_credentials()` see "no accounts" without touching any real
/// `~/.ixr` state — no filesystem mocking needed since these failure paths
/// never reach the network. Shares `auth::ENV_LOCK` with `auth::perm_tests`
/// so the two suites don't race over the same process-global `HOME` var.
#[cfg(all(test, unix))]
mod exit_code_tests {
    use super::*;

    fn sandbox_empty_home(tag: &str) -> std::path::PathBuf {
        let tmp_home =
            std::env::temp_dir().join(format!("ixr-exitcodetest-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_home).unwrap();
        unsafe {
            std::env::set_var("HOME", &tmp_home);
        }
        tmp_home
    }

    #[tokio::test]
    async fn whoami_errs_when_not_logged_in() {
        let _guard = crate::auth::ENV_LOCK.lock().await;
        let tmp_home = sandbox_empty_home("whoami");

        let err = whoami().await.expect_err("expected Err on empty credentials store");
        assert_eq!(err.to_string(), "You are not logged in.");

        std::fs::remove_dir_all(&tmp_home).ok();
    }

    #[tokio::test]
    async fn logout_errs_when_not_logged_in() {
        let _guard = crate::auth::ENV_LOCK.lock().await;
        let tmp_home = sandbox_empty_home("logout");

        let err = logout(false).await.expect_err("expected Err on empty credentials store");
        assert_eq!(err.to_string(), "No user is currently logged in.");

        std::fs::remove_dir_all(&tmp_home).ok();
    }

    #[tokio::test]
    async fn logout_all_errs_when_not_logged_in() {
        let _guard = crate::auth::ENV_LOCK.lock().await;
        let tmp_home = sandbox_empty_home("logout-all");

        let err = logout(true).await.expect_err("expected Err on empty credentials store");
        assert_eq!(err.to_string(), "No user is currently logged in.");

        std::fs::remove_dir_all(&tmp_home).ok();
    }
}

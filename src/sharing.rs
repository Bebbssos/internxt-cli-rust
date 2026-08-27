//! Sharing commands: list what's shared, inspect one item's sharing, see the
//! invitations waiting for you, show the roles/domains a share can use, and
//! revoke a share.
//!
//! New — no official CLI equivalent; sharing lives in drive-web only, so the
//! semantics here follow it. `shared list` makes the same pair of calls its
//! "Shared" root view makes (`/sharings/folders` + `/sharings/files`, which
//! return everything visible to you — items you shared out and items shared
//! with you), while `--by-me`/`--with-me` use the two directional endpoints,
//! which only exist for folders.
//!
//! **Creating** a share is deliberately absent: it wraps the item's key for
//! the recipient (or for a link password), which `internxt-core` doesn't
//! implement yet.
//!
//! Most of these endpoints hand back raw JSON — core returns `Value` for them
//! because the account they were verified against only ever answered with
//! empty collections, so no schema was ever observed. The human tables
//! therefore stick to the few fields og's sdk documents and tolerate every one
//! of them being absent, while `--json` passes the server's response through
//! untouched.

use anyhow::Result;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::auth;
use crate::drive_ops::{format_date, human_file_size, print_table};
use crate::output;
use crate::paths::{self, ItemTarget};
use internxt_core::api::DriveApi;

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// The first of `keys` present and non-empty, or `-`. Every table cell goes
/// through this: the payload shapes are unverified, so a missing field has to
/// render as a dash rather than blow up the row.
fn first_str(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        let s = str_field(v, k);
        if !s.is_empty() {
            return s;
        }
    }
    "-".to_string()
}

fn bool_field(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(|x| x.as_bool())
}

/// `size` is a number on shared folders and a decimal string on shared files
/// (og's sdk types spell it both ways); accept either.
fn size_field(v: &Value) -> Option<f64> {
    match v.get("size") {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// `plainName`, with the file's `type` appended as an extension the way the
/// rest of the CLI displays Drive names. Falls back to the encrypted `name`
/// when the server didn't send a plain one.
fn display_name(v: &Value, is_folder: bool) -> String {
    let base = first_str(v, &["plainName", "name"]);
    if is_folder {
        return base;
    }
    let ext = str_field(v, "type");
    if ext.is_empty() || base.ends_with(&format!(".{ext}")) {
        base
    } else {
        format!("{base}.{ext}")
    }
}

/// The item array of a sharings listing: `{ "folders": [...] }` /
/// `{ "files": [...] }` as og's sdk documents them, also accepting `items` or
/// a bare array so an unverified shape still renders something.
fn items_of(v: &Value, key: &str) -> Vec<Value> {
    if let Some(a) = v.as_array() {
        return a.clone();
    }
    v.get(key)
        .or_else(|| v.get("items"))
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default()
}

/// The per-item sharing endpoints answer `404 Item is not being shared` when
/// an item has no sharing — the normal "not shared" answer rather than a
/// failure. Anything else (auth, network, 5xx) stays an error.
fn not_shared(e: &anyhow::Error) -> bool {
    e.to_string().contains("HTTP 404")
}

/// Which of the three listings `shared list` should make.
#[derive(Clone, Copy, PartialEq)]
pub enum Direction {
    /// Everything visible: `/sharings/folders` + `/sharings/files`.
    All,
    /// `/sharings/shared-by-me/folders` — outbound, folders only.
    ByMe,
    /// `/sharings/shared-with-me/folders` — inbound, folders only.
    WithMe,
}

impl Direction {
    pub fn from_flags(with_me: bool, by_me: bool) -> Self {
        match (with_me, by_me) {
            (true, _) => Direction::WithMe,
            (_, true) => Direction::ByMe,
            _ => Direction::All,
        }
    }
}

/// The resolved `<ITEM>` as JSON, plus whether it turned out to be shared.
/// `paths::resolve_item` does the uuid-or-path resolution — it is shared with
/// `favorites`, whose routes take the same `{item_type}/{uuid}` pair.
fn target_json(target: &ItemTarget, shared: bool) -> Value {
    json!({ "uuid": target.uuid, "type": target.item_type, "shared": shared })
}

/// `roleId` -> role name, so an invitations table can show `EDITOR` instead of
/// a uuid. Best effort: on any failure the raw id is shown instead.
async fn role_names(api: &DriveApi, token: &str) -> HashMap<String, String> {
    match api.get_sharing_roles(token).await {
        Ok(roles) => roles.into_iter().map(|r| (r.id, r.name)).collect(),
        Err(_) => HashMap::new(),
    }
}

fn role_label(roles: &HashMap<String, String>, v: &Value) -> String {
    let id = first_str(v, &["roleId", "roleName", "role"]);
    roles.get(&id).cloned().unwrap_or(id)
}

/// One row of a shared file/folder from `/sharings/files` / `/sharings/folders`.
fn shared_row(v: &Value, is_folder: bool, extended: bool) -> Vec<String> {
    let owner = v
        .get("user")
        .map(|u| first_str(u, &["email"]))
        .unwrap_or_else(|| "-".to_string());
    let mut row = vec![
        if is_folder { "folder" } else { "file" }.to_string(),
        display_name(v, is_folder),
        size_field(v).map(human_file_size).unwrap_or_else(|| "-".to_string()),
        owner,
        format_date(&first_str(v, &["dateShared", "createdAt"])),
    ];
    if extended {
        row.push(first_str(v, &["uuid"]));
    }
    row
}

/// One row of a folder from the directional (`shared-by-me` / `shared-with-me`)
/// listings, which return plain folder records rather than sharing records.
fn folder_row(v: &Value, extended: bool) -> Vec<String> {
    let mut row = vec![
        display_name(v, true),
        format_date(&first_str(v, &["updatedAt", "createdAt"])),
    ];
    if extended {
        row.push(first_str(v, &["uuid"]));
    }
    row
}

pub async fn list(
    direction: Direction,
    page: u32,
    per_page: u32,
    order_by: &str,
    extended: bool,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let token = &creds.token;

    if direction == Direction::All {
        let folders = api.get_shared_folders(token, page, per_page, order_by).await?;
        let files = api.get_shared_files(token, page, per_page, order_by).await?;
        if output::is_json() {
            output::emit(
                "",
                json!({ "success": true, "list": { "folders": folders, "files": files } }),
            );
            return Ok(());
        }
        let folder_items = items_of(&folders, "folders");
        let file_items = items_of(&files, "files");
        if folder_items.is_empty() && file_items.is_empty() {
            output::status("No shared items found.");
            return Ok(());
        }
        let mut rows: Vec<Vec<String>> =
            folder_items.iter().map(|f| shared_row(f, true, extended)).collect();
        rows.extend(file_items.iter().map(|f| shared_row(f, false, extended)));
        let mut headers = vec!["Type", "Name", "Size", "Owner", "Shared"];
        if extended {
            headers.push("Uuid");
        }
        print_table(&headers, &rows);
        return Ok(());
    }

    let resp = if direction == Direction::ByMe {
        api.get_shared_by_me_folders(token, page, per_page).await?
    } else {
        api.get_shared_with_me_folders(token, page, per_page).await?
    };
    if output::is_json() {
        output::emit("", json!({ "success": true, "list": { "folders": resp } }));
        return Ok(());
    }
    let items = items_of(&resp, "folders");
    if items.is_empty() {
        output::status(if direction == Direction::ByMe {
            "No folders shared by you."
        } else {
            "No folders shared with you."
        });
        return Ok(());
    }
    let rows: Vec<Vec<String>> = items.iter().map(|f| folder_row(f, extended)).collect();
    let mut headers = vec!["Name", "Updated"];
    if extended {
        headers.push("Uuid");
    }
    print_table(&headers, &rows);
    Ok(())
}

pub async fn info(item: &str, item_type: Option<&str>) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let token = &creds.token;
    let target = paths::resolve_item(&api, token, creds.root_folder(), item, item_type).await?;

    let info = match api.get_item_sharing_info(token, &target.item_type, &target.uuid).await {
        Ok(v) => Some(v),
        Err(e) if not_shared(&e) => None,
        Err(e) => return Err(e),
    };
    // `/type` 404s for an unshared item exactly like `/info`, so only ask when
    // there is a sharing to describe.
    let sharing_type = match info {
        None => None,
        Some(_) => match api.get_item_sharing_type(token, &target.item_type, &target.uuid).await {
            Ok(v) => Some(v),
            Err(e) if not_shared(&e) => None,
            Err(e) => return Err(e),
        },
    };
    // Unlike the two above, this one answers `200 []` for an unshared item.
    let invites = api.get_item_sharing_invites(token, &target.item_type, &target.uuid).await?;

    if output::is_json() {
        output::emit(
            "",
            json!({
                "success": true,
                "item": target_json(&target, info.is_some()),
                "info": info,
                "sharingType": sharing_type,
                "invites": invites,
            }),
        );
        return Ok(());
    }

    println!("Item:        {} ({})", target.label, target.item_type);
    println!("Uuid:        {}", target.uuid);
    match &info {
        None => println!("Shared:      no"),
        Some(i) => {
            println!("Shared:      yes");
            let kind = match str_field(i, "type") {
                t if !t.is_empty() => t,
                _ => sharing_type.as_ref().map(|t| str_field(t, "type")).unwrap_or_default(),
            };
            println!("Sharing:     {}", if kind.is_empty() { "-".to_string() } else { kind });
            let protected =
                i.get("publicSharing").and_then(|p| bool_field(p, "isPasswordProtected"));
            println!(
                "Password:    {}",
                match protected {
                    Some(true) => "yes",
                    Some(false) => "no",
                    None => "-",
                }
            );
            if let Some(n) = i.get("invitationsCount").and_then(|n| n.as_u64()) {
                println!("Invitations: {n}");
            }
        }
    }

    let invite_items = items_of(&invites, "invites");
    if invite_items.is_empty() {
        return Ok(());
    }
    let roles = role_names(&api, token).await;
    let rows: Vec<Vec<String>> = invite_items
        .iter()
        .map(|i| {
            let email = match first_str(i, &["sharedWith"]).as_str() {
                "-" => i
                    .get("invited")
                    .map(|u| first_str(u, &["email"]))
                    .unwrap_or_else(|| "-".to_string()),
                e => e.to_string(),
            };
            vec![email, role_label(&roles, i), format_date(&first_str(i, &["createdAt"]))]
        })
        .collect();
    println!();
    print_table(&["Shared with", "Role", "Invited"], &rows);
    Ok(())
}

/// Sharing invitations waiting for *you* (`/sharings/invites`), as opposed to
/// the per-item invitations `info` shows. The endpoint rejects a `limit`
/// outside 1-25 (verified live), hence the small default on the flag.
pub async fn invites(limit: u32, offset: u32) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let resp = api.get_sharing_invites(&creds.token, limit, offset).await?;
    if output::is_json() {
        output::emit("", json!({ "success": true, "list": { "invites": resp } }));
        return Ok(());
    }
    let items = items_of(&resp, "invites");
    if items.is_empty() {
        output::status("No sharing invitations.");
        return Ok(());
    }
    let roles = role_names(&api, &creds.token).await;
    let rows: Vec<Vec<String>> = items
        .iter()
        .map(|i| {
            let item_type = first_str(i, &["itemType"]);
            let name = i
                .get("item")
                .map(|it| display_name(it, item_type == "folder"))
                .unwrap_or_else(|| "-".to_string());
            vec![name, item_type, role_label(&roles, i), format_date(&first_str(i, &["createdAt"]))]
        })
        .collect();
    print_table(&["Item", "Type", "Role", "Invited"], &rows);
    Ok(())
}

pub async fn roles() -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let roles = api.get_sharing_roles(&creds.token).await?;
    if output::is_json() {
        // Core parses these into a typed struct, so the objects are rebuilt
        // here rather than passed through; the field names are the wire ones.
        let list: Vec<Value> = roles
            .iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "name": r.name,
                    "createdAt": r.created_at,
                    "updatedAt": r.updated_at,
                })
            })
            .collect();
        output::emit("", json!({ "success": true, "list": { "roles": list } }));
        return Ok(());
    }
    if roles.is_empty() {
        output::status("No sharing roles found.");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = roles.iter().map(|r| vec![r.name.clone(), r.id.clone()]).collect();
    print_table(&["Role", "Id"], &rows);
    Ok(())
}

pub async fn domains() -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let domains = api.get_share_domains(&creds.token).await?;
    if output::is_json() {
        output::emit("", json!({ "success": true, "list": { "domains": domains } }));
        return Ok(());
    }
    if domains.is_empty() {
        output::status("No share domains found.");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = domains.iter().map(|d| vec![d.clone()]).collect();
    print_table(&["Domain"], &rows);
    Ok(())
}

pub async fn revoke(item: &str, item_type: Option<&str>) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let token = &creds.token;
    let target = paths::resolve_item(&api, token, creds.root_folder(), item, item_type).await?;

    // `stop_sharing` is idempotent — revoking an unshared item answers Ok — so
    // ask `/info` first, purely to be able to say which of the two happened
    // instead of implying a share was removed when there was none.
    let shared = match api.get_item_sharing_info(token, &target.item_type, &target.uuid).await {
        Ok(_) => true,
        Err(e) if not_shared(&e) => false,
        Err(e) => return Err(e),
    };
    if !shared {
        let message = format!("{} is not shared; nothing to revoke.", target.label);
        output::emit(
            &message,
            json!({
                "success": true,
                "revoked": false,
                "item": target_json(&target, false),
                "message": message,
            }),
        );
        return Ok(());
    }

    api.stop_sharing(token, &target.item_type, &target.uuid).await?;
    let message = format!("Stopped sharing {}.", target.label);
    output::emit(
        &format!("✓ {message}"),
        json!({
            "success": true,
            "revoked": true,
            "item": target_json(&target, false),
            "message": message,
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_render_as_dashes() {
        let empty = json!({});
        assert_eq!(shared_row(&empty, true, true), vec!["folder", "-", "-", "-", "-", "-"]);
        assert_eq!(folder_row(&empty, false), vec!["-", "-"]);
    }

    #[test]
    fn sizes_come_as_numbers_or_strings() {
        assert_eq!(size_field(&json!({ "size": 1024 })), Some(1024.0));
        assert_eq!(size_field(&json!({ "size": "1024" })), Some(1024.0));
        assert_eq!(size_field(&json!({})), None);
    }

    #[test]
    fn item_arrays_survive_shape_changes() {
        assert_eq!(items_of(&json!({ "folders": [1] }), "folders").len(), 1);
        assert_eq!(items_of(&json!({ "items": [1, 2] }), "folders").len(), 2);
        assert_eq!(items_of(&json!([1, 2, 3]), "folders").len(), 3);
        assert_eq!(items_of(&json!({ "other": [1] }), "folders").len(), 0);
    }

    #[test]
    fn file_names_get_their_extension_back() {
        assert_eq!(display_name(&json!({ "plainName": "a", "type": "pdf" }), false), "a.pdf");
        assert_eq!(display_name(&json!({ "plainName": "a", "type": "pdf" }), true), "a");
        assert_eq!(display_name(&json!({ "plainName": "a.pdf", "type": "pdf" }), false), "a.pdf");
        assert_eq!(display_name(&json!({ "plainName": "a" }), false), "a");
    }
}

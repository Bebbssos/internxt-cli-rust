//! Favorites: `favorites list | add | remove`.
//!
//! New — no official CLI equivalent. Favorites are a drive-web feature backed
//! by three endpoints core already wraps: `GET /favorites?type=…` for one page
//! of favorited files *or* folders, and `PUT`/`DELETE /favorites/{type}/{uuid}`
//! to mark and unmark one item.
//!
//! Two things about the wire shape drive the design here:
//!
//! * The listing returns **one kind per call** — `type=file` or `type=folder`,
//!   never both — so `favorites list` makes two requests by default and
//!   `--type` narrows it to one. `--limit`/`--offset` are per kind, since
//!   that's what the endpoint pages.
//! * Marking is **idempotent server-side**: favoriting an already-favorited
//!   item is a successful no-op. The endpoints answer with the item's
//!   resulting state, which is what gets reported — `add`/`remove` never claim
//!   a change the server didn't confirm.
//!
//! The favorite records come back as raw JSON (core returns `Value` for the
//! listing: folders have no camelCase DTO there), so the table tolerates every
//! field being absent and `--json` passes the server's records through
//! untouched.

use anyhow::Result;
use serde_json::{json, Value};

use internxt_core::api::DriveApi;

use crate::auth;
use crate::drive_ops::{format_date, human_file_size, print_table};
use crate::output;
use crate::paths;

/// Default `--limit`, a screenful. The endpoint's own default isn't
/// documented, so the flag is always sent.
pub const DEFAULT_LIMIT: u32 = 50;

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// The first of `keys` present and non-empty, or `-` — the favorite records
/// are unverified shapes, so a missing field renders as a dash.
fn first_str(v: &Value, keys: &[&str]) -> String {
    for k in keys {
        let s = str_field(v, k);
        if !s.is_empty() {
            return s;
        }
    }
    "-".to_string()
}

/// `size` arrives as a number on some records and a decimal string on others
/// (the Drive DTOs spell it both ways); accept either.
fn size_field(v: &Value) -> Option<f64> {
    match v.get("size") {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// `plainName` with the file's `type` appended as an extension, the way the
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

/// `--sort` accepts the wire values (`uuid`, `plainName`, `updatedAt`) plus the
/// friendlier `name`/`modified` spellings, in any case; this maps whatever was
/// typed onto the exact value the endpoint takes. Unknown values can't reach
/// here — clap rejects them against the same list.
pub fn normalize_sort(sort: &str) -> String {
    match sort.trim().to_ascii_lowercase().as_str() {
        "name" | "plainname" => "plainName",
        "modified" | "updatedat" => "updatedAt",
        _ => "uuid",
    }
    .to_string()
}

/// `--order` is `ASC`/`DESC` on the wire; accept either case from the user.
pub fn normalize_order(order: &str) -> String {
    order.trim().to_ascii_uppercase()
}

/// One table row for a favorited item.
fn row(v: &Value, is_folder: bool, extended: bool) -> Vec<String> {
    let mut row = vec![
        if is_folder { "folder" } else { "file" }.to_string(),
        display_name(v, is_folder),
        // Folder records carry `size: 0` — that's "not counted", not "empty",
        // so it renders as a dash rather than a fake number. `du` is the
        // command that actually measures a folder.
        if is_folder {
            "-".to_string()
        } else {
            size_field(v)
                .map(human_file_size)
                .unwrap_or_else(|| "-".to_string())
        },
        date_cell(v),
    ];
    if extended {
        row.push(first_str(v, &["uuid"]));
    }
    row
}

/// The Modified cell: the item's own modification time when it has one, else
/// the record's `updatedAt`/`createdAt` — same order the other listings use.
fn date_cell(v: &Value) -> String {
    let iso = first_str(v, &["modificationTime", "updatedAt", "createdAt"]);
    if iso == "-" { iso } else { format_date(&iso) }
}

/// `favorites list`: one page of favorited folders, files, or both.
pub async fn list(
    item_type: Option<&str>,
    limit: u32,
    offset: u32,
    sort: Option<&str>,
    order: Option<&str>,
    extended: bool,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let token = &creds.token;

    let sort = sort.map(normalize_sort);
    let order = order.map(normalize_order);
    let (want_folders, want_files) = match item_type {
        Some("folder") => (true, false),
        Some("file") => (false, true),
        // Neither given: the endpoint can't answer for both at once, so ask
        // twice — one page of each.
        _ => (true, true),
    };

    let mut folders: Option<Vec<Value>> = None;
    if want_folders {
        folders = Some(
            api.get_favorites(token, "folder", limit, offset, sort.as_deref(), order.as_deref())
                .await?,
        );
    }
    let mut files: Option<Vec<Value>> = None;
    if want_files {
        files = Some(
            api.get_favorites(token, "file", limit, offset, sort.as_deref(), order.as_deref())
                .await?,
        );
    }

    if output::is_json() {
        // Both keys are always present so a consumer can tell "not requested"
        // (null) from "requested and empty" ([]).
        output::emit(
            "",
            json!({
                "success": true,
                "favorites": { "folders": folders, "files": files },
                "limit": limit,
                "offset": offset,
            }),
        );
        return Ok(());
    }

    let folder_items = folders.unwrap_or_default();
    let file_items = files.unwrap_or_default();
    if folder_items.is_empty() && file_items.is_empty() {
        output::status("No favorites found.");
        return Ok(());
    }

    let mut rows: Vec<Vec<String>> = folder_items.iter().map(|f| row(f, true, extended)).collect();
    rows.extend(file_items.iter().map(|f| row(f, false, extended)));
    let headers: Vec<&str> = if extended {
        vec!["Type", "Name", "Size", "Modified", "Id"]
    } else {
        vec!["Type", "Name", "Size", "Modified"]
    };
    print_table(&headers, &rows);
    Ok(())
}

/// `favorites add` / `favorites remove`. `favorited` is the state we're asking
/// for; the server's answer is what gets reported, so an item that was already
/// in (or already out of) the favorites is described as such rather than as a
/// change that just happened.
pub async fn set(item: &str, item_type: Option<&str>, favorited: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let token = &creds.token;
    let target = paths::resolve_item(&api, token, creds.root_folder(), item, item_type).await?;

    let now = if favorited {
        api.mark_favorite(token, &target.item_type, &target.uuid).await?
    } else {
        api.unmark_favorite(token, &target.item_type, &target.uuid).await?
    };

    let message = result_message(&target.label, favorited, now);
    output::emit(
        &format!("{} {message}", if now == favorited { "✓" } else { "!" }),
        json!({
            "success": true,
            "item": { "uuid": target.uuid, "type": target.item_type },
            // The state the server reports *after* the call — not an echo of
            // what was asked for.
            "favorited": now,
            "message": message,
        }),
    );
    Ok(())
}

/// What to say about a mark/unmark, given what was asked for (`wanted`) and the
/// state the endpoint reported afterwards (`now`).
fn result_message(label: &str, wanted: bool, now: bool) -> String {
    match (wanted, now) {
        (true, true) => format!("Added {label} to favorites."),
        (false, false) => format!("Removed {label} from favorites."),
        // The endpoints are idempotent and always answer with the state they
        // left the item in, so a disagreement means the server declined —
        // never paper over it with a success line.
        (true, false) => format!("{label} is still not favorited — the server reported no change."),
        (false, true) => format!("{label} is still favorited — the server reported no change."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_accepts_the_wire_and_friendly_spellings() {
        assert_eq!(normalize_sort("plainName"), "plainName");
        assert_eq!(normalize_sort("name"), "plainName");
        assert_eq!(normalize_sort("NAME"), "plainName");
        assert_eq!(normalize_sort("updatedAt"), "updatedAt");
        assert_eq!(normalize_sort("modified"), "updatedAt");
        assert_eq!(normalize_sort("uuid"), "uuid");
    }

    #[test]
    fn order_is_upper_cased_for_the_wire() {
        assert_eq!(normalize_order("asc"), "ASC");
        assert_eq!(normalize_order(" desc "), "DESC");
        assert_eq!(normalize_order("DESC"), "DESC");
    }

    #[test]
    fn file_rows_get_their_extension_back() {
        assert_eq!(display_name(&json!({ "plainName": "a", "type": "pdf" }), false), "a.pdf");
        // A folder's `type` (if any) is not an extension.
        assert_eq!(display_name(&json!({ "plainName": "a", "type": "pdf" }), true), "a");
        // Already suffixed: don't double it.
        assert_eq!(display_name(&json!({ "plainName": "a.pdf", "type": "pdf" }), false), "a.pdf");
        assert_eq!(display_name(&json!({ "name": "encrypted" }), false), "encrypted");
    }

    #[test]
    fn sizes_come_as_numbers_or_strings() {
        assert_eq!(size_field(&json!({ "size": 1024 })), Some(1024.0));
        assert_eq!(size_field(&json!({ "size": "1024" })), Some(1024.0));
        assert_eq!(size_field(&json!({})), None);
    }

    #[test]
    fn missing_fields_render_as_dashes() {
        assert_eq!(row(&json!({}), true, true), vec!["folder", "-", "-", "-", "-"]);
        assert_eq!(row(&json!({}), false, false), vec!["file", "-", "-", "-"]);
    }

    #[test]
    fn a_folders_zero_size_is_not_shown_as_a_size() {
        // The favorites listing sends `size: 0` for folders; a file with a
        // real size still gets one.
        let folder = json!({ "plainName": "Docs", "size": 0 });
        assert_eq!(row(&folder, true, false)[2], "-");
        let file = json!({ "plainName": "a", "type": "txt", "size": 26 });
        assert_eq!(row(&file, false, false)[2], "26 B");
    }

    #[test]
    fn the_message_follows_the_server_not_the_request() {
        assert_eq!(result_message("/a", true, true), "Added /a to favorites.");
        assert_eq!(result_message("/a", false, false), "Removed /a from favorites.");
        assert!(result_message("/a", true, false).contains("no change"));
        assert!(result_message("/a", false, true).contains("no change"));
    }
}

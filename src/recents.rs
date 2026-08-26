//! `recents` — the account's most recently modified files across every folder
//! (`GET /files/recents`). Beyond og: the official CLI has no recents command,
//! but drive-web's "Recents" view uses the same endpoint.
//!
//! The endpoint returns entries newest-first by `updatedAt` and already filters
//! out trashed/deleted files, so there's nothing to sort or skip here. It is
//! also the only file read that inlines the parent folder, which is what lets
//! the table say *where* each file lives without a request per row.

use anyhow::Result;
use serde_json::json;

use internxt_core::api::DriveApi;
use internxt_core::models::DriveFileData;

use crate::auth;
use crate::drive_ops::{format_date, human_file_size, print_table};
use crate::output;

/// Default `--limit`. The endpoint's own default is [`MAX_LIMIT`], which is far
/// more than a terminal table is useful for, so we ask for a screenful instead
/// and let `--limit` widen it.
pub const DEFAULT_LIMIT: u32 = 50;

/// Largest `--limit` the endpoint accepts — it answers
/// `limit should be between 1 and 1000` (HTTP 400) above this, and falls back
/// to 1000 when the parameter is missing or unparseable. Enforced by clap so
/// an out-of-range value is a usage error rather than a server round-trip.
pub const MAX_LIMIT: u32 = 1000;

pub async fn recents(limit: u32, extended: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let files = api.get_recent_files(&creds.token, limit).await?;

    // A non-empty result already answers "has this account ever uploaded
    // anything?", so the extra `/users/me/upload-status` round-trip only
    // happens on the empty path. It's a personal-account endpoint with no
    // workspace-scoped variant, so inside a workspace we don't ask (and don't
    // claim): an empty workspace listing says nothing about the personal drive.
    let ever_uploaded = if !files.is_empty() {
        Some(true)
    } else if api.is_workspace() {
        None
    } else {
        api.has_uploaded_files(&creds.token).await.ok()
    };

    if output::is_json() {
        let list: Vec<_> = files
            .iter()
            .map(|f| {
                json!({
                    "uuid": f.uuid,
                    "plainName": f.plain_name,
                    "type": f.file_type,
                    "size": f.size.0,
                    "bucket": f.bucket,
                    "fileId": f.file_id,
                    // Both timestamp pairs are passed through as the API sends
                    // them: `modificationTime`/`creationTime` are the file's
                    // own, `updatedAt`/`createdAt` are the record's, and a
                    // consumer may care which it reads.
                    "modificationTime": f.modification_time,
                    "creationTime": f.creation_time,
                    "createdAt": f.created_at,
                    "updatedAt": f.updated_at,
                    "folderUuid": f.folder_uuid,
                    "folder": f.folder.as_ref().map(|d| json!({
                        "uuid": d.uuid,
                        "plainName": d.plain_name,
                    })),
                })
            })
            .collect();
        output::emit(
            "",
            json!({ "success": true, "recents": list, "hasUploadedFiles": ever_uploaded }),
        );
        return Ok(());
    }

    if files.is_empty() {
        // `Some(false)` is the one case worth spelling out: the account is
        // empty, not merely quiet. `None` (workspace, or the status call
        // failed) falls back to the neutral wording.
        output::status(if ever_uploaded == Some(false) {
            "No recent files — this account has never uploaded anything."
        } else {
            "No recent files."
        });
        return Ok(());
    }

    let rows: Vec<Vec<String>> = files
        .iter()
        .map(|f| {
            let mut row = vec![
                display_name(f.plain_name.as_deref(), f.file_type.as_deref()),
                parent_folder(f),
                // `modified_at` is core's modificationTime-then-updatedAt
                // fallback — the same order `list` uses for its Modified column.
                date_or_dash(f.modified_at()),
            ];
            if extended {
                row.push(date_or_dash(
                    f.creation_time.as_deref().or(f.created_at.as_deref()),
                ));
            }
            row.push(human_file_size(f.size.0 as f64));
            if extended {
                row.push(f.uuid.clone());
            }
            row
        })
        .collect();

    let headers: Vec<&str> = if extended {
        vec!["Name", "Folder", "Modified", "Created", "Size", "Id"]
    } else {
        vec!["Name", "Folder", "Modified", "Size"]
    };
    print_table(&headers, &rows);
    Ok(())
}

/// `plainName` + `.type`, matching how `list` renders a file. An extension-less
/// file has `type: null` (or an empty string on some responses); either way the
/// bare name is used, never a trailing dot.
fn display_name(plain_name: Option<&str>, file_type: Option<&str>) -> String {
    let plain = plain_name.unwrap_or_default();
    match file_type.filter(|t| !t.is_empty()) {
        Some(ext) => format!("{plain}.{ext}"),
        None => plain.to_string(),
    }
}

/// The parent folder's name, from the folder this endpoint inlines.
///
/// Only the name is shown: the whole path would cost a request per ancestor,
/// and `folderUuid` (in `--json`) is what a script would follow anyway. The
/// account root has no `plainName`, so it renders as `/`.
fn parent_folder(f: &DriveFileData) -> String {
    match f.folder.as_ref().and_then(|d| d.plain_name.as_deref()) {
        Some(name) if !name.is_empty() => name.to_string(),
        // A file directly in the root: the endpoint inlines the root folder,
        // which has no plain name of its own.
        _ if f.folder.is_some() => "/".to_string(),
        _ => "-".to_string(),
    }
}

/// A date column, or `-` when the response carried no timestamp at all.
fn date_or_dash(iso: Option<&str>) -> String {
    match iso.filter(|s| !s.is_empty()) {
        Some(s) => format_date(s),
        None => "-".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{date_or_dash, display_name, parent_folder};
    use internxt_core::models::DriveFileData;
    use serde_json::json;

    fn file(v: serde_json::Value) -> DriveFileData {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn parent_folder_shows_the_inlined_folder_name() {
        let f = file(json!({
            "uuid": "file-uuid",
            "folderUuid": "parent-uuid",
            "folder": { "uuid": "parent-uuid", "plainName": "Finance" }
        }));
        assert_eq!(parent_folder(&f), "Finance");
    }

    #[test]
    fn parent_folder_renders_the_account_root_as_a_slash() {
        // The root folder record has no `plainName` — its `name` is the
        // encrypted blob — so a file directly in the root arrives like this.
        let f = file(json!({
            "uuid": "file-uuid",
            "folderUuid": "root-uuid",
            "folder": { "uuid": "root-uuid", "plainName": null, "name": "encrypted-blob" }
        }));
        assert_eq!(parent_folder(&f), "/");
    }

    #[test]
    fn parent_folder_falls_back_to_a_dash_without_an_inlined_folder() {
        let f = file(json!({ "uuid": "file-uuid" }));
        assert_eq!(parent_folder(&f), "-");
    }

    #[test]
    fn date_or_dash_dashes_a_missing_or_empty_timestamp() {
        assert_eq!(date_or_dash(None), "-");
        assert_eq!(date_or_dash(Some("")), "-");
        // A real timestamp is handed to `format_date`, which renders local
        // time — assert only that it isn't the dash placeholder.
        assert_ne!(date_or_dash(Some("2026-08-25T17:19:44.000Z")), "-");
    }

    #[test]
    fn display_name_appends_the_extension() {
        assert_eq!(display_name(Some("report"), Some("pdf")), "report.pdf");
    }

    #[test]
    fn display_name_leaves_an_extension_less_file_bare() {
        assert_eq!(display_name(Some("LICENSE"), None), "LICENSE");
        assert_eq!(display_name(Some("LICENSE"), Some("")), "LICENSE");
    }
}

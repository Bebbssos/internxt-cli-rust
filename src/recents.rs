//! `recents` — the account's most recently modified files across every folder
//! (`GET /files/recents`). Beyond og: the official CLI has no recents command,
//! but drive-web's "Recents" view uses the same endpoint.
//!
//! The endpoint returns entries newest-first by `updatedAt` and already filters
//! out trashed/deleted files, so there's nothing to sort or skip here. It also
//! returns each entry's timestamps and parent folder, but core's typed
//! `DriveFileData` keeps only name/size/id, so this table can't show them.

use anyhow::Result;
use serde_json::json;

use internxt_core::api::DriveApi;

use crate::auth;
use crate::drive_ops::{human_file_size, print_table};
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

pub async fn recents(limit: u32) -> Result<()> {
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
            vec![
                display_name(f.plain_name.as_deref(), f.file_type.as_deref()),
                human_file_size(f.size.0 as f64),
                f.uuid.clone(),
            ]
        })
        .collect();
    print_table(&["Name", "Size", "Id"], &rows);
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

#[cfg(test)]
mod tests {
    use super::display_name;

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

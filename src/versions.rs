//! File version history: `versions list | restore | delete`.
//!
//! Not part of the node CLI — it has no versions command at all. The endpoints
//! behind this (`/files/{uuid}/versions`) are the ones drive-web's "Version
//! history" sidebar uses, and the semantics here mirror it: list the stored
//! versions of a file, restore one as the file's current content, or drop one.
//!
//! Versions are minted server-side — no client asks for one. drive-web calls
//! them "autosave versions" and only offers the sidebar for `pdf`/`docx`/
//! `xlsx`/`csv` files (its `ALLOWED_VERSIONING_EXTENSIONS`) on a plan whose
//! `versioning.enabled` is true. That extension whitelist is enforced by the
//! backend, not just by drive-web's UI: replacing the content of a `.pdf`
//! mints a version, replacing a `.txt` of the same size does not. So an empty
//! list is the normal, expected answer for most files, and `list` says so in
//! words rather than printing an empty table. `usage` reports whether the plan
//! keeps versions at all, and within what caps.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use internxt_core::api::DriveApi;
use internxt_core::models::{Credentials, FileVersion};

use crate::auth;
use crate::drive_ops::{format_date, human_file_size, print_table};
use crate::output;
use crate::paths::{self, Expect};

/// `true` for a canonical 8-4-4-4-12 hex uuid. Used to decide whether the
/// single `<file>` argument is already a uuid or a Drive path to walk — the
/// two are unambiguous in practice: a Drive name that happens to be exactly
/// this shape (36 chars, hyphens in those four positions, hex everywhere else)
/// would also have to carry no extension to be mistaken for one.
fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Resolve the `<file>` argument — a uuid or a Drive path — to a file uuid.
/// Both forms go through the same resolver every other file command uses, so
/// path semantics (including the `//backups/...` / `//drive/...` escapes) are
/// identical here.
async fn resolve_file(api: &DriveApi, creds: &Credentials, file: &str) -> Result<String> {
    let (id, path) = if looks_like_uuid(file.trim()) {
        (Some(file), None)
    } else {
        (None, Some(file))
    };
    paths::resolve_opt(api, &creds.token, creds.root_folder(), id, path, Expect::File)
        .await?
        .ok_or_else(|| anyhow!("Provide the file to inspect, as a Drive path or a uuid"))
}

/// One version as JSON, using the wire field names of `/files/{uuid}/versions`.
/// Built by hand because core's [`FileVersion`] is deserialize-only.
fn version_json(v: &FileVersion) -> Value {
    json!({
        "id": v.id,
        "fileId": v.file_id,
        "networkFileId": v.network_file_id,
        "size": v.size.0,
        "status": v.status,
        "modificationTime": v.modification_time,
        "createdAt": v.created_at,
        "updatedAt": v.updated_at,
        "expiresAt": v.expires_at,
    })
}

/// A date cell for the table: formatted like everywhere else, `-` when absent.
fn date_cell(iso: Option<&String>) -> String {
    match iso {
        Some(s) if !s.is_empty() => format_date(s),
        _ => "-".to_string(),
    }
}

/// `versions list`: the file's stored version history, newest first.
pub async fn list(file: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let uuid = resolve_file(&api, &creds, file).await?;
    let versions = api.get_file_versions(&creds.token, &uuid).await?;

    if output::is_json() {
        let list: Vec<Value> = versions.iter().map(version_json).collect();
        output::emit(
            "",
            json!({ "success": true, "list": { "file": uuid, "versions": list } }),
        );
        return Ok(());
    }

    if versions.is_empty() {
        // An empty table would read as "something went wrong"; this is in fact
        // the expected answer for most files (see the module doc).
        output::status("No versions stored for this file.");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = versions
        .iter()
        .map(|v| {
            vec![
                v.id.clone(),
                human_file_size(v.size.0 as f64),
                date_cell(v.modification_time.as_ref()),
                date_cell(v.created_at.as_ref()),
                date_cell(v.expires_at.as_ref()),
                v.status.clone().unwrap_or_else(|| "-".to_string()),
            ]
        })
        .collect();
    print_table(
        &["Version ID", "Size", "Modified", "Created", "Expires", "Status"],
        &rows,
    );
    Ok(())
}

/// `versions restore`: make a stored version the file's current content. The
/// file keeps its uuid; drive-web warns that newer versions are dropped.
pub async fn restore(file: &str, version_id: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let uuid = resolve_file(&api, &creds, file).await?;
    let restored = api
        .restore_file_version(&creds.token, &uuid, version_id.trim())
        .await?;

    let name = restored
        .plain_name
        .clone()
        .or_else(|| restored.name.clone())
        .unwrap_or_else(|| restored.uuid.clone());
    output::emit(
        &format!("✓ Restored version {version_id} of '{name}'."),
        json!({
            "success": true,
            "message": "Version restored",
            "file": {
                "uuid": restored.uuid,
                "name": name,
                "type": restored.file_type,
                "size": restored.size.0,
                "fileId": restored.file_id,
            },
            "versionId": version_id,
        }),
    );
    Ok(())
}

/// `versions delete`: drop one stored version. The file's current content is
/// untouched; this cannot be undone.
pub async fn delete(file: &str, version_id: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let uuid = resolve_file(&api, &creds, file).await?;
    api.delete_file_version(&creds.token, &uuid, version_id.trim())
        .await?;

    output::emit(
        &format!("✓ Deleted version {version_id}."),
        json!({
            "success": true,
            "message": "Version deleted",
            "file": uuid,
            "versionId": version_id,
        }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::looks_like_uuid;

    #[test]
    fn uuid_vs_path() {
        assert!(looks_like_uuid("2f8a1c40-9b7e-4d3a-8f21-0c5b6e7d1a90"));
        assert!(!looks_like_uuid("/folder/report.pdf"));
        assert!(!looks_like_uuid("report.pdf"));
        // Right length and hyphen positions, but not hex.
        assert!(!looks_like_uuid("zf8a1c40-9b7e-4d3a-8f21-0c5b6e7d1a90"));
        // Hyphens in the wrong places.
        assert!(!looks_like_uuid("2f8a1c409b7e-4d3a-8f21-0c5b6e7d1a90-"));
    }
}

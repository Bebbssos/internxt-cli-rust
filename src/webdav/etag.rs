//! Stable, content-derived WebDAV ETags.
//!
//! Ports og/cli `WebDavUtils.generateETag` / `getItemETag` (added upstream in
//! the change that replaced the old random-uuid etags). The recipe is:
//! take the item's identity + size + timestamps, replace every absent part with
//! `-`, join with `|`, sha256 the result and hex-encode it. The hash is what
//! callers get back here — the surrounding double quotes are added by the
//! caller (`xml::file_response`/`folder_response` already wrap `<D:getetag>`
//! in quotes; header callers use [`quoted`]).
//!
//! Why it matters: a random etag per response tells every WebDAV client that
//! the resource changed on every single request, which defeats client caching
//! and revalidation entirely. A hash of the metadata is byte-identical while
//! the item is untouched and changes as soon as the item's size or timestamps
//! move (an overwrite bumps both).
//!
//! ## Which fields feed the hash
//!
//! Same set as upstream: `uuid`, `size` (files only — folders contribute `-`
//! in that slot, so a file and a folder that shared every other value would
//! still hash differently), `createdAt`, `updatedAt`, `creationTime` and
//! `modificationTime`. Upstream hashes `Date.getTime()`, i.e. epoch
//! milliseconds; the Drive listings hand this crate ISO-8601 strings instead,
//! so [`time_part`] converts them to epoch millis to keep the hashed form
//! identical (a value that won't parse is hashed verbatim rather than being
//! collapsed to `-`, so it still discriminates).
//!
//! Two items are hashed from what the tree walk actually parsed, so a part is
//! `-` when the listing omitted it. The one place that is systematic rather
//! than incidental is the *root* collection: this crate synthesizes it from
//! the configured root uuid plus the `updatedAt` fetched once at startup (see
//! `serve::run::fetch_folder_updated_at`) instead of re-reading its folder
//! meta per request, so the root's etag only carries uuid + that startup
//! `updatedAt`. That mirrors the existing limitation of the root's
//! `getlastmodified`, and every non-root item gets the full field set.

use internxt_core::crypto::sha256;

use crate::serve::tree::{FileItem, FolderItem};

/// Stand-in for an absent part (upstream's `part ?? '-'`).
const ABSENT: &str = "-";

/// sha256 over the `|`-joined parts, hex-encoded, unquoted. Empty parts become
/// `-`.
fn generate(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .map(|p| if p.is_empty() { ABSENT } else { *p })
        .collect::<Vec<_>>()
        .join("|");
    hex::encode(sha256(joined.as_bytes()))
}

/// An ISO-8601 timestamp as the epoch-millisecond string upstream hashes.
/// Empty stays empty (and so becomes `-`); an unparseable value is passed
/// through unchanged so it still distinguishes one item from another.
fn time_part(iso: &str) -> String {
    if iso.is_empty() {
        return String::new();
    }
    match chrono::DateTime::parse_from_rfc3339(iso) {
        Ok(dt) => dt.timestamp_millis().to_string(),
        Err(_) => iso.to_string(),
    }
}

/// Unquoted etag for a file.
pub fn file_etag(f: &FileItem) -> String {
    let size = f.size.to_string();
    generate(&[
        &f.uuid,
        &size,
        &time_part(&f.created_at),
        &time_part(&f.updated_at),
        &time_part(&f.creation_time),
        &time_part(&f.modification_time),
    ])
}

/// Unquoted etag for a folder. The size slot is always absent (`-`), as
/// upstream's `itemType === 'file' ? size : undefined`.
pub fn folder_etag(f: &FolderItem) -> String {
    generate(&[
        &f.uuid,
        ABSENT,
        &time_part(&f.created_at),
        &time_part(&f.updated_at),
        &time_part(&f.creation_time),
        &time_part(&f.modification_time),
    ])
}

/// Wrap an etag for use in an `ETag` response header (RFC 7232 requires the
/// quotes; `xml::*_response` add their own, so they take the bare hash).
pub fn quoted(etag: &str) -> String {
    format!("\"{etag}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file() -> FileItem {
        FileItem {
            uuid: "0f2c2b5a-1c4a-4a1e-9a6e-2b6a1f0d3c11".to_string(),
            plain_name: "report".to_string(),
            file_type: "txt".to_string(),
            size: 1024,
            bucket: "bucket".to_string(),
            file_id: Some("netid".to_string()),
            updated_at: "2026-01-02T03:04:05.000Z".to_string(),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            creation_time: "2026-01-01T00:00:00.000Z".to_string(),
            modification_time: "2026-01-02T03:04:05.000Z".to_string(),
        }
    }

    fn folder() -> FolderItem {
        FolderItem {
            uuid: "0f2c2b5a-1c4a-4a1e-9a6e-2b6a1f0d3c11".to_string(),
            plain_name: "report".to_string(),
            updated_at: "2026-01-02T03:04:05.000Z".to_string(),
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            creation_time: "2026-01-01T00:00:00.000Z".to_string(),
            modification_time: "2026-01-02T03:04:05.000Z".to_string(),
        }
    }

    #[test]
    fn same_input_same_etag() {
        assert_eq!(file_etag(&file()), file_etag(&file()));
        assert_eq!(folder_etag(&folder()), folder_etag(&folder()));
        // 64 hex chars, no quotes — the callers add those.
        let e = file_etag(&file());
        assert_eq!(e.len(), 64);
        assert!(e.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn size_change_changes_etag() {
        let mut bigger = file();
        bigger.size += 1;
        assert_ne!(file_etag(&file()), file_etag(&bigger));
    }

    #[test]
    fn modification_time_change_changes_etag() {
        let mut touched = file();
        touched.modification_time = "2026-01-02T03:04:06.000Z".to_string();
        assert_ne!(file_etag(&file()), file_etag(&touched));

        let mut touched_folder = folder();
        touched_folder.modification_time = "2026-01-02T03:04:06.000Z".to_string();
        assert_ne!(folder_etag(&folder()), folder_etag(&touched_folder));
    }

    #[test]
    fn rename_only_bumps_updated_at_and_changes_etag() {
        // A rename/move leaves size and modificationTime alone but bumps the
        // record's updatedAt, which must still move the etag.
        let mut renamed = file();
        renamed.updated_at = "2026-01-03T00:00:00.000Z".to_string();
        assert_ne!(file_etag(&file()), file_etag(&renamed));
    }

    #[test]
    fn folder_and_file_differ() {
        // Identical uuid and timestamps: only the size slot separates them.
        assert_ne!(file_etag(&file()), folder_etag(&folder()));
    }

    #[test]
    fn absent_parts_become_dashes() {
        // A synthetic root (uuid + updatedAt only) still hashes deterministically.
        let root = FolderItem {
            uuid: "root-uuid".to_string(),
            updated_at: "2026-01-02T03:04:05.000Z".to_string(),
            ..Default::default()
        };
        let millis = time_part("2026-01-02T03:04:05.000Z");
        assert_eq!(
            folder_etag(&root),
            hex::encode(sha256(format!("root-uuid|-|-|{millis}|-|-").as_bytes()))
        );
    }

    #[test]
    fn iso_is_hashed_as_epoch_millis() {
        assert_eq!(time_part("1970-01-01T00:00:01.500Z"), "1500");
        assert_eq!(time_part(""), "");
        // Unparseable values pass through rather than collapsing to `-`.
        assert_eq!(time_part("not-a-date"), "not-a-date");
        // Same instant, different spellings => same hashed part.
        assert_eq!(
            time_part("2026-01-02T03:04:05.000Z"),
            time_part("2026-01-02T04:04:05.000+01:00")
        );
    }

    #[test]
    fn matches_upstream_recipe() {
        // Hand-computed against og/cli `generateETag`:
        // sha256('a|1|2|-|-|-') hex.
        let expected = hex::encode(sha256(b"a|1|2|-|-|-"));
        assert_eq!(generate(&["a", "1", "2", "", "", ""]), expected);
    }
}

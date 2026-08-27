//! `search` — fuzzy name search across the whole Drive (`POST /fuzzy/{query}`).
//!
//! New — no official CLI equivalent; this is the endpoint behind drive-web's
//! search box. Hits are ranked best-first by the backend (Postgres full-text
//! rank, with trigram similarity as the tiebreaker/fallback), and both files
//! and folders can match.
//!
//! og moved this off `GET .../fuzzy/{search}?offset=N` in sdk 1.20.x: the
//! parameters now travel as a JSON body so the filters below have somewhere to
//! live. The filters **AND** together, except the extensions inside `--type`,
//! which OR with each other. Size filters exclude folders by their nature (a
//! folder has no size on the search index).
//!
//! Workspace-aware through the shared API client: with a workspace active the
//! search runs against that workspace's drive, not the personal one.
//!
//! The search index carries only enough of a record to rank and label a hit,
//! so a hit's `item` (bucket / fileId / size / type) is partial and may be
//! absent entirely — the table dashes those cells rather than making numbers
//! up, and `--json` passes the server's records through untouched.

use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, SecondsFormat, Utc};
use serde_json::{json, Value};

use internxt_core::api::DriveApi;
use internxt_core::models::{SearchFilters, SearchResult};

use crate::auth;
use crate::drive_ops::{human_file_size, print_table};
use crate::output;
use crate::upload_limit::parse_size;

/// The reserved `--type` value that includes folders in the results —
/// everything else in that list is a file extension.
const FOLDER_TYPE: &str = "folder";

/// Normalize the `--type` values into what the endpoint's `type` array takes:
/// bare, lower-cased extensions (`pdf`, not `.PDF`), plus the reserved
/// `folder`. Duplicates are dropped, order is preserved, empty entries (from
/// `--type jpg,,png`) are ignored.
pub fn normalize_types(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in raw {
        let t = t.trim().trim_start_matches('.').to_ascii_lowercase();
        if t.is_empty() || out.contains(&t) {
            continue;
        }
        out.push(t);
    }
    out
}

/// Parse a `--modified-after`/`--modified-before` value into the ISO 8601
/// timestamp the endpoint takes.
///
/// Accepted: a full RFC 3339 timestamp (`2026-01-02T03:04:05Z`, offsets
/// included — converted to UTC), a date-and-time without a zone
/// (`2026-01-02 03:04`, seconds optional), or a bare date (`2026-01-02`, taken
/// as that day's midnight). **A value with no zone is read as UTC**, not local
/// time, so the filter means the same thing wherever it runs.
pub fn parse_timestamp(s: &str) -> Result<String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("empty timestamp"));
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt
            .with_timezone(&Utc)
            .to_rfc3339_opts(SecondsFormat::Millis, true));
    }
    for fmt in ["%Y-%m-%dT%H:%M:%S", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(naive.and_utc().to_rfc3339_opts(SecondsFormat::Millis, true));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(date
            .and_hms_opt(0, 0, 0)
            .expect("midnight is a valid time")
            .and_utc()
            .to_rfc3339_opts(SecondsFormat::Millis, true));
    }
    Err(anyhow!(
        "not a date or timestamp: {s:?} — use `2026-01-02`, `2026-01-02 15:04`, \
         or a full `2026-01-02T15:04:05Z` (no timezone means UTC)"
    ))
}

/// A hit's display name: the indexed `name`, with the file's extension from the
/// partial `item` record appended when the index didn't already include it.
fn display_name(hit: &SearchResult) -> String {
    if hit.item_type == FOLDER_TYPE {
        return hit.name.clone();
    }
    let ext = hit
        .item
        .as_ref()
        .and_then(|i| i.get("type"))
        .and_then(|t| t.as_str())
        .unwrap_or("");
    if ext.is_empty() || hit.name.ends_with(&format!(".{ext}")) {
        hit.name.clone()
    } else {
        format!("{}.{}", hit.name, ext)
    }
}

/// A hit's size, from the partial `item` record (number or decimal string).
/// `None` for a folder, or when the index didn't carry the record.
fn size_of(hit: &SearchResult) -> Option<f64> {
    match hit.item.as_ref().and_then(|i| i.get("size")) {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// A hit as JSON, using the wire field names — core's [`SearchResult`] is
/// deserialize-only, so the object is rebuilt rather than passed through.
fn hit_json(hit: &SearchResult) -> Value {
    json!({
        "id": hit.id,
        "itemId": hit.item_id,
        "itemType": hit.item_type,
        "name": hit.name,
        "rank": hit.rank,
        "similarity": hit.similarity,
        "item": hit.item,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn search(
    query: &str,
    types: &[String],
    min_size: Option<&str>,
    max_size: Option<&str>,
    modified_after: Option<&str>,
    modified_before: Option<&str>,
    offset: Option<u32>,
    extended: bool,
) -> Result<()> {
    let query = query.trim();
    if query.is_empty() {
        return Err(anyhow!("Provide something to search for."));
    }

    let filters = SearchFilters {
        offset,
        types: normalize_types(types),
        min_size: min_size.map(parse_size).transpose()?,
        max_size: max_size.map(parse_size).transpose()?,
        modified_after: modified_after.map(parse_timestamp).transpose()?,
        modified_before: modified_before.map(parse_timestamp).transpose()?,
    };
    if let (Some(min), Some(max)) = (filters.min_size, filters.max_size)
        && min > max
    {
        return Err(anyhow!(
            "--min-size ({min} bytes) is larger than --max-size ({max} bytes) — nothing can match"
        ));
    }

    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let hits = api.global_search(&creds.token, query, &filters).await?;

    if output::is_json() {
        output::emit(
            "",
            json!({
                "success": true,
                "query": query,
                // The exact request body that was sent, so a caller can see
                // how its flags were interpreted.
                "filters": serde_json::to_value(&filters).unwrap_or(Value::Null),
                "results": hits.iter().map(hit_json).collect::<Vec<_>>(),
            }),
        );
        return Ok(());
    }

    if hits.is_empty() {
        output::status(&format!("No matches for '{query}'."));
        return Ok(());
    }

    let rows: Vec<Vec<String>> = hits
        .iter()
        .map(|h| {
            let mut row = vec![
                h.item_type.clone(),
                display_name(h),
                size_of(h)
                    .map(human_file_size)
                    .unwrap_or_else(|| "-".to_string()),
            ];
            if extended {
                row.push(format!("{:.2}", h.similarity));
                row.push(h.item_id.clone());
            }
            row
        })
        .collect();
    let headers: Vec<&str> = if extended {
        vec!["Type", "Name", "Size", "Score", "Id"]
    } else {
        vec!["Type", "Name", "Size"]
    };
    print_table(&headers, &rows);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(v: Value) -> SearchResult {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn types_are_normalized_to_bare_lowercase_extensions() {
        let raw: Vec<String> = ["  .PDF ", "jpg", "JPG", "", "folder"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(normalize_types(&raw), vec!["pdf", "jpg", "folder"]);
        assert!(normalize_types(&[]).is_empty());
    }

    #[test]
    fn a_bare_date_becomes_that_days_midnight_utc() {
        assert_eq!(parse_timestamp("2026-01-02").unwrap(), "2026-01-02T00:00:00.000Z");
    }

    #[test]
    fn a_zoneless_timestamp_is_read_as_utc() {
        assert_eq!(parse_timestamp("2026-01-02 15:04").unwrap(), "2026-01-02T15:04:00.000Z");
        assert_eq!(parse_timestamp("2026-01-02T15:04:05").unwrap(), "2026-01-02T15:04:05.000Z");
    }

    #[test]
    fn an_offset_timestamp_is_converted_to_utc() {
        assert_eq!(parse_timestamp("2026-01-02T15:04:05+02:00").unwrap(), "2026-01-02T13:04:05.000Z");
        assert_eq!(parse_timestamp("2026-01-02T15:04:05Z").unwrap(), "2026-01-02T15:04:05.000Z");
    }

    #[test]
    fn nonsense_timestamps_are_rejected() {
        assert!(parse_timestamp("yesterday").is_err());
        assert!(parse_timestamp("2026-13-40").is_err());
        assert!(parse_timestamp("   ").is_err());
    }

    #[test]
    fn a_files_extension_comes_from_the_partial_item_record() {
        let h = hit(json!({
            "id": "1", "itemId": "u", "itemType": "file", "name": "report",
            "similarity": 0.5, "item": { "type": "pdf" }
        }));
        assert_eq!(display_name(&h), "report.pdf");
    }

    #[test]
    fn a_name_that_already_carries_the_extension_is_left_alone() {
        let h = hit(json!({
            "id": "1", "itemId": "u", "itemType": "file", "name": "report.pdf",
            "similarity": 0.5, "item": { "type": "pdf" }
        }));
        assert_eq!(display_name(&h), "report.pdf");
    }

    #[test]
    fn a_folder_hit_and_an_itemless_hit_keep_the_bare_name() {
        let folder = hit(json!({
            "id": "1", "itemId": "u", "itemType": "folder", "name": "Invoices",
            "similarity": 0.5, "item": { "type": "pdf" }
        }));
        assert_eq!(display_name(&folder), "Invoices");
        let bare = hit(json!({
            "id": "1", "itemId": "u", "itemType": "file", "name": "notes", "similarity": 0.5
        }));
        assert_eq!(display_name(&bare), "notes");
        assert_eq!(size_of(&bare), None);
    }

    #[test]
    fn sizes_come_as_numbers_or_strings() {
        let n = hit(json!({
            "id": "1", "itemId": "u", "itemType": "file", "name": "a",
            "similarity": 0.5, "item": { "size": 2048 }
        }));
        let s = hit(json!({
            "id": "1", "itemId": "u", "itemType": "file", "name": "a",
            "similarity": 0.5, "item": { "size": "2048" }
        }));
        assert_eq!(size_of(&n), Some(2048.0));
        assert_eq!(size_of(&s), Some(2048.0));
    }

    #[test]
    fn unset_filters_are_left_out_of_the_request_body() {
        let filters = SearchFilters {
            types: normalize_types(&["jpg".to_string()]),
            max_size: Some(1024),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&filters).unwrap(),
            json!({ "type": ["jpg"], "maxSize": 1024 })
        );
    }
}

//! `du` — a folder's recursive size and file count, from the server.
//!
//! Beyond og: the official CLI has no `du`. Everything here rests on
//! `GET /folders/{uuid}/stats`, which answers with a whole subtree's
//! `fileCount` and `totalSize` in **one** request — no walking, no downloading.
//! That is the entire point of the command: measuring a subtree any other way
//! costs a listing per folder (`tree`, `list`) or a full subtree response.
//!
//! The catch is that the backend estimates for large folders and says which
//! number it estimated via `isFileCountExact` / `isTotalSizeExact`. Those flags
//! are surfaced rather than hidden — an estimate presented as a fact is worse
//! than no number at all.
//!
//! `--children` asks the same question once per direct subfolder (in parallel),
//! which is the "what is eating my space" view; the leftover between the
//! parent's total and its children's is the bytes sitting directly in the
//! folder, shown as its own row.

use anyhow::{anyhow, Context, Result};
use futures_util::stream::{self, StreamExt};
use serde_json::{json, Value};
use std::fmt::Write as _;

use crate::auth;
use crate::drive_ops::{human_file_size, print_table};
use crate::output;
use crate::paths::{self, Expect};
use internxt_core::api::DriveApi;
use internxt_core::models::FolderStats;

/// How many per-child `/stats` requests to keep in flight. Same order as the
/// other metadata fan-outs in the CLI: enough to hide latency, not enough to
/// look like a scrape.
const MAX_CONCURRENT_STATS: usize = 8;

/// One row of the `--children` breakdown.
struct Child {
    name: String,
    uuid: String,
    stats: FolderStats,
}

pub async fn du(folder: Option<&str>, children: bool, bytes: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let token = &creds.token;

    // `//` and `//backups` are groupings, not folders — no uuid, so nothing to
    // ask for stats about.
    if paths::virtual_node_for(folder).is_some() {
        return Err(anyhow!(
            "'{}' is a grouping, not a folder — it has no size of its own. \
             Use `ixr du //drive` or `ixr du //backups/<device>`.",
            folder.unwrap_or_default().trim()
        ));
    }

    let (id, path) = paths::split_id_or_path(folder);
    let uuid = paths::resolve_opt(&api, token, creds.root_folder(), id, path, Expect::Folder)
        .await?
        .unwrap_or_else(|| creds.root_folder().to_string());

    let stats = fetch_stats(&api, token, &uuid, id.is_some()).await?;

    // Label the line with what was asked for: the path verbatim, or `/` for the
    // implicit root. A uuid stays a uuid — naming it would cost a request.
    let label = match path.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => p.to_string(),
        None => match id {
            Some(i) => i.trim().to_string(),
            None => "/".to_string(),
        },
    };

    let children = if children {
        Some(fetch_children(&api, token, &uuid).await?)
    } else {
        None
    };

    let mut human = String::new();
    // `print_table` writes to stdout directly, so the breakdown is rendered
    // only when there is a human to read it — `--json` carries the same rows
    // in the payload below.
    if let Some(rows) = &children
        && !output::is_json()
    {
        render_children(&mut human, rows, &stats, bytes);
    }
    let _ = write!(human, "{}", summary_line(&label, &stats, bytes));

    output::emit(
        human.trim_end(),
        json!({
            "success": true,
            "folder": label,
            "uuid": uuid,
            "size": stats.total_size,
            "files": stats.file_count,
            // The backend estimates for large subtrees. A consumer that treats
            // these as exact regardless should at least have been told.
            "sizeExact": stats.is_total_size_exact,
            "filesExact": stats.is_file_count_exact,
            "children": children.as_ref().map(|rows| {
                rows.iter()
                    .map(|c| json!({
                        "name": c.name,
                        "uuid": c.uuid,
                        "size": c.stats.total_size,
                        "files": c.stats.file_count,
                        "sizeExact": c.stats.is_total_size_exact,
                        "filesExact": c.stats.is_file_count_exact,
                    }))
                    .collect::<Vec<_>>()
            }),
        }),
    );
    Ok(())
}

/// `/stats` for one folder, with the error a bad uuid deserves.
async fn fetch_stats(
    api: &DriveApi,
    token: &str,
    uuid: &str,
    from_id: bool,
) -> Result<FolderStats> {
    match api.get_folder_stats(token, uuid).await {
        Ok(s) => Ok(s),
        Err(e) => {
            // A uuid taken straight from the argument was never checked to be a
            // folder (`resolve_opt` only validates what it resolved from a
            // path), so the failure may just be a file's uuid or a bogus one.
            // Worth the extra requests only here, on the error path.
            if from_id && api.get_folder_meta(token, uuid).await.is_err() {
                return Err(if api.get_file_meta_value(token, uuid).await.is_ok() {
                    anyhow!("'{uuid}' is a file, not a folder — `du` measures folders")
                } else {
                    anyhow!("No such folder with id: {uuid}")
                });
            }
            Err(e).context("Could not fetch the folder's stats")
        }
    }
}

/// One `/stats` per direct subfolder, several at a time.
async fn fetch_children(api: &DriveApi, token: &str, uuid: &str) -> Result<Vec<Child>> {
    let subfolders = list_subfolders(api, token, uuid).await?;
    let mut rows: Vec<Child> = stream::iter(subfolders.into_iter().map(|(name, uuid)| async move {
        let stats = api.get_folder_stats(token, &uuid).await?;
        Ok::<Child, anyhow::Error>(Child { name, uuid, stats })
    }))
    .buffer_unordered(MAX_CONCURRENT_STATS)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<Result<Vec<_>>>()?;

    // Biggest first: the point of the breakdown is finding what to delete.
    rows.sort_by(|a, b| {
        b.stats
            .total_size
            .cmp(&a.stats.total_size)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(rows)
}

/// Direct subfolders as `(name, uuid)`, paginated like every other listing.
async fn list_subfolders(api: &DriveApi, token: &str, uuid: &str) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut offset = 0u32;
    loop {
        let page = api.get_folder_subfolders(token, uuid, offset).await?;
        let items: Vec<&Value> = page
            .get("folders")
            .and_then(|f| f.as_array())
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        let got = items.len() as u32;
        for it in items {
            let status = it.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if !(status.is_empty() || status == "EXISTS") {
                continue;
            }
            let uuid = it.get("uuid").and_then(|s| s.as_str()).unwrap_or("");
            if uuid.is_empty() {
                continue;
            }
            let name = it
                .get("plainName")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(uuid);
            out.push((name.to_string(), uuid.to_string()));
        }
        if got < 50 {
            break;
        }
        offset += got;
    }
    Ok(out)
}

/// `123` with `--bytes`, `1.21 MB` without.
fn size_cell(bytes_flag: bool, size: u64) -> String {
    if bytes_flag {
        size.to_string()
    } else {
        human_file_size(size as f64)
    }
}

/// ` (estimate)` when the backend says it guessed.
fn estimate_note(exact: bool) -> &'static str {
    if exact { "" } else { " (estimate)" }
}

fn summary_line(label: &str, stats: &FolderStats, bytes: bool) -> String {
    format!(
        "{}{}  {} file{}{}  {label}\n",
        size_cell(bytes, stats.total_size),
        estimate_note(stats.is_total_size_exact),
        stats.file_count,
        if stats.file_count == 1 { "" } else { "s" },
        estimate_note(stats.is_file_count_exact),
    )
}

fn render_children(out: &mut String, rows: &[Child], total: &FolderStats, bytes: bool) {
    let mut table: Vec<Vec<String>> = rows
        .iter()
        .map(|c| {
            vec![
                format!(
                    "{}{}",
                    size_cell(bytes, c.stats.total_size),
                    estimate_note(c.stats.is_total_size_exact)
                ),
                format!(
                    "{}{}",
                    c.stats.file_count,
                    estimate_note(c.stats.is_file_count_exact)
                ),
                c.name.clone(),
            ]
        })
        .collect();

    // Whatever the children don't account for sits directly in this folder.
    // Derived, not fetched: the subtree total already includes it, and the
    // listing endpoint would have to be paged through to sum it again. Skipped
    // when any input was an estimate, since the subtraction would then be one
    // guess minus another.
    let children_size: u64 = rows.iter().map(|c| c.stats.total_size).sum();
    let children_files: u64 = rows.iter().map(|c| c.stats.file_count).sum();
    let all_exact = total.is_total_size_exact
        && total.is_file_count_exact
        && rows
            .iter()
            .all(|c| c.stats.is_total_size_exact && c.stats.is_file_count_exact);
    if all_exact && (total.total_size > children_size || total.file_count > children_files) {
        table.push(vec![
            size_cell(bytes, total.total_size.saturating_sub(children_size)),
            total.file_count.saturating_sub(children_files).to_string(),
            ".".to_string(),
        ]);
    }

    if table.is_empty() {
        return;
    }
    print_table(&["Size", "Files", "Name"], &table);
    let _ = writeln!(out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(size: u64, files: u64, exact: bool) -> FolderStats {
        serde_json::from_value(json!({
            "totalSize": size,
            "fileCount": files,
            "isTotalSizeExact": exact,
            "isFileCountExact": exact,
        }))
        .unwrap()
    }

    #[test]
    fn summary_marks_estimated_numbers() {
        let exact = summary_line("/photos", &stats(2048, 3, true), false);
        assert_eq!(exact, "2 KB  3 files  /photos\n");

        let guessed = summary_line("/photos", &stats(2048, 3, false), false);
        assert!(guessed.contains("2 KB (estimate)"), "{guessed}");
        assert!(guessed.contains("3 files (estimate)"), "{guessed}");
    }

    #[test]
    fn summary_uses_raw_bytes_with_the_flag_and_singularises_one_file() {
        assert_eq!(
            summary_line("/x", &stats(2048, 1, true), true),
            "2048  1 file  /x\n"
        );
    }
}

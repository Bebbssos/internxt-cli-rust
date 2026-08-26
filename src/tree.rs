//! `tree`: print a folder subtree.
//!
//! Everything here comes from a **single** `GET /folders/{uuid}/tree` request:
//! the backend answers with the whole subtree (nested folders and their files)
//! at once, unlike the paginated per-folder listings the rest of the CLI walks.
//! `--depth` is therefore only a display filter — the response is the same size
//! either way.
//!
//! The flip side is that the backend builds that response eagerly and gives up
//! on very large subtrees (an upstream 5xx rather than a normal error body), so
//! the failure path points at `list` and at picking a smaller starting folder.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::fmt::Write as _;

use crate::auth;
use crate::drive_ops::human_file_size;
use crate::output;
use crate::paths::{self, Expect};
use internxt_core::api::DriveApi;
use internxt_core::models::{FolderStats, FolderTree};

/// Display name of a tree node: the plain name, falling back to the encrypted
/// one only so an odd record still prints something identifiable.
fn folder_name(node: &FolderTree) -> String {
    node.plain_name
        .clone()
        .filter(|n| !n.is_empty())
        .or_else(|| node.name.clone().filter(|n| !n.is_empty()))
        .unwrap_or_else(|| node.uuid.clone())
}

/// `plainName` + `.type`, like every other file listing in the CLI.
fn file_name(f: &internxt_core::models::DriveFileData) -> String {
    let plain = f.plain_name.clone().unwrap_or_default();
    let plain = if plain.is_empty() {
        f.name.clone().unwrap_or_default()
    } else {
        plain
    };
    match f.file_type.as_deref().filter(|t| !t.is_empty()) {
        Some(t) => format!("{plain}.{t}"),
        None => plain,
    }
}

/// Live children of a node. Trashed/removed subfolders are dropped the way the
/// paginated listings drop them; a node with no `status` at all is kept.
fn live_children(node: &FolderTree) -> Vec<&FolderTree> {
    let mut out: Vec<&FolderTree> = node
        .children
        .iter()
        .filter(|c| match c.status.as_deref() {
            None | Some("") | Some("EXISTS") => true,
            Some(_) => false,
        })
        .collect();
    out.sort_by_key(|c| folder_name(c).to_lowercase());
    out
}

fn sorted_files(node: &FolderTree) -> Vec<&internxt_core::models::DriveFileData> {
    let mut out: Vec<&internxt_core::models::DriveFileData> = node.files.iter().collect();
    out.sort_by_key(|f| file_name(f).to_lowercase());
    out
}

/// Totals of the whole subtree below `node`, counting only live subfolders.
/// Files are counted (and their sizes summed) wherever they sit, so this is the
/// same number `FolderTree::total_files` gives for a tree with no trashed
/// folders in it, plus the byte total that type can't compute.
fn totals(node: &FolderTree) -> (usize, usize, u64) {
    let mut folders = 0usize;
    let mut files = node.files.len();
    let mut size: u64 = node.files.iter().map(|f| f.size.0).sum();
    for child in live_children(node) {
        let (cf, cfi, cs) = totals(child);
        folders += 1 + cf;
        files += cfi;
        size += cs;
    }
    (folders, files, size)
}

/// `" [2 folders, 5 files]"` — what a `--depth` cut hides below a folder.
/// Empty when there is nothing to hide.
fn hidden_suffix(node: &FolderTree, folders_only: bool) -> String {
    let folders = live_children(node).len();
    let files = if folders_only { 0 } else { node.files.len() };
    let mut parts = Vec::new();
    if folders > 0 {
        parts.push(format!("{folders} folder{}", if folders == 1 { "" } else { "s" }));
    }
    if files > 0 {
        parts.push(format!("{files} file{}", if files == 1 { "" } else { "s" }));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" [{}]", parts.join(", "))
    }
}

/// Render options carried through the recursive walk.
struct Render {
    /// Omit files entirely.
    folders_only: bool,
    /// Append each file's size to its line.
    extended: bool,
}

/// Write the children of `node`, prefixed for the box-drawing gutter.
/// `remaining` is how many levels are still allowed *below* the children being
/// written here; `None` means unlimited. Returns `true` if anything was hidden
/// by the depth limit.
fn write_children(
    out: &mut String,
    node: &FolderTree,
    prefix: &str,
    remaining: Option<u32>,
    opts: &Render,
) -> bool {
    let folders = live_children(node);
    let files = if opts.folders_only { Vec::new() } else { sorted_files(node) };
    let total = folders.len() + files.len();
    let mut truncated_any = false;

    for (i, child) in folders.iter().enumerate() {
        let last = i + 1 == total;
        let branch = if last { "└── " } else { "├── " };
        let cut = remaining == Some(0);
        let suffix = if cut { hidden_suffix(child, opts.folders_only) } else { String::new() };
        truncated_any |= !suffix.is_empty();
        let _ = writeln!(out, "{prefix}{branch}{}{suffix}", folder_name(child));
        if !cut {
            let gutter = if last { "    " } else { "│   " };
            truncated_any |= write_children(
                out,
                child,
                &format!("{prefix}{gutter}"),
                remaining.map(|r| r.saturating_sub(1)),
                opts,
            );
        }
    }

    for (i, f) in files.iter().enumerate() {
        let last = folders.len() + i + 1 == total;
        let branch = if last { "└── " } else { "├── " };
        let size = if opts.extended {
            format!("  ({})", human_file_size(f.size.0 as f64))
        } else {
            String::new()
        };
        let _ = writeln!(out, "{prefix}{branch}{}{size}", file_name(f));
    }

    truncated_any
}

/// One node as JSON, honouring `--depth` / `--folders-only` so the machine
/// output shows exactly what the human output does.
fn node_json(node: &FolderTree, remaining: Option<u32>, opts: &Render) -> Value {
    let mut obj = json!({
        "uuid": node.uuid,
        "name": folder_name(node),
        "type": "folder",
    });
    if remaining == Some(0) {
        let folders = live_children(node).len();
        let files = if opts.folders_only { 0 } else { node.files.len() };
        if folders > 0 || files > 0 {
            obj["truncated"] = json!(true);
        }
        return obj;
    }
    let deeper = remaining.map(|r| r.saturating_sub(1));
    obj["folders"] = Value::Array(
        live_children(node)
            .iter()
            .map(|c| node_json(c, deeper, opts))
            .collect(),
    );
    if !opts.folders_only {
        obj["files"] = Value::Array(
            sorted_files(node)
                .iter()
                .map(|f| {
                    json!({
                        "uuid": f.uuid,
                        "name": file_name(f),
                        "plainName": f.plain_name,
                        "type": f.file_type,
                        "size": f.size.0,
                    })
                })
                .collect(),
        );
    }
    obj
}

/// `tree`: print the subtree under `folder` (a path or a uuid; default root).
pub async fn tree(
    folder: Option<&str>,
    depth: Option<u32>,
    folders_only: bool,
    extended: bool,
    stats: bool,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let token = &creds.token;

    // `//` and `//backups` are groupings, not folders — they have no uuid, so
    // there is no single subtree to ask the endpoint for.
    if paths::virtual_node_for(folder).is_some() {
        return Err(anyhow!(
            "'{}' is a grouping, not a folder — it has no subtree of its own. Use `ixr tree //drive` or `ixr tree //backups/<device>`.",
            folder.unwrap_or_default().trim()
        ));
    }

    let (id, path) = paths::split_id_or_path(folder);
    let uuid = paths::resolve_opt(&api, token, creds.root_folder(), id, path, Expect::Folder)
        .await?
        .unwrap_or_else(|| creds.root_folder().to_string());

    let root = match api.get_folder_tree(token, &uuid).await {
        Ok(t) => t,
        Err(e) => {
            // A uuid taken straight from the argument was never checked to be a
            // folder (`resolve_opt` only validates what it resolved from a
            // path), so the failure may just be a file's or a bogus uuid. Only
            // worth two extra requests once we're on the error path anyway.
            if id.is_some() && api.get_folder_meta(token, &uuid).await.is_err() {
                return Err(if api.get_file_meta_value(token, &uuid).await.is_ok() {
                    anyhow!("'{uuid}' is a file, not a folder")
                } else {
                    anyhow!("No such folder with id: {uuid}")
                });
            }
            return Err(e).context(
                "Could not fetch the folder tree. The whole subtree is built server-side for \
                 one response, and very large ones fail there — try a subfolder, or `ixr list` \
                 to walk it one level at a time",
            );
        }
    };

    let (folder_count, file_count, size) = totals(&root);
    let opts = Render { folders_only, extended };

    // Label the root line with what the user asked for: their path verbatim, the
    // folder's own name when they gave a uuid, and `/` for the implicit root.
    let label = match (path.map(str::trim).filter(|p| !p.is_empty()), id) {
        (Some(p), _) => p.to_string(),
        (None, Some(_)) => folder_name(&root),
        (None, None) => "/".to_string(),
    };

    let mut human = String::new();
    let root_cut = depth == Some(0);
    let root_suffix = if root_cut { hidden_suffix(&root, folders_only) } else { String::new() };
    let _ = writeln!(human, "{label}{root_suffix}");
    let mut truncated = !root_suffix.is_empty();
    if !root_cut {
        truncated |= write_children(
            &mut human,
            &root,
            "",
            depth.map(|d| d.saturating_sub(1)),
            &opts,
        );
    }

    let _ = writeln!(
        human,
        "\n{folder_count} folder{}, {file_count} file{}, {}",
        if folder_count == 1 { "" } else { "s" },
        if file_count == 1 { "" } else { "s" },
        human_file_size(size as f64),
    );
    if truncated {
        let d = depth.unwrap_or(0);
        let _ = writeln!(
            human,
            "(showing {d} level{}; the totals above cover the whole subtree)",
            if d == 1 { "" } else { "s" }
        );
    }

    // `--stats` is a second request. The tree already knows the exact counts and
    // sizes, so this is only worth asking for as a cross-check — the endpoint
    // estimates for large folders and says so via its two `*Exact` flags.
    let stats = if stats {
        let s: FolderStats = api.get_folder_stats(token, &uuid).await?;
        let exact = |v: bool| if v { "" } else { " (estimate)" };
        let _ = writeln!(
            human,
            "Stats endpoint: {} file{}{}, {}{}",
            s.file_count,
            if s.file_count == 1 { "" } else { "s" },
            exact(s.is_file_count_exact),
            human_file_size(s.total_size as f64),
            exact(s.is_total_size_exact),
        );
        let mut notes = Vec::new();
        if s.file_count != file_count as u64 {
            notes.push(format!("{} vs {file_count} files", s.file_count));
        }
        if s.total_size != size {
            notes.push(format!(
                "{} vs {}",
                human_file_size(s.total_size as f64),
                human_file_size(size as f64)
            ));
        }
        if !notes.is_empty() {
            let _ = writeln!(
                human,
                "Note: the stats endpoint disagrees with the tree ({}); \
                 its numbers are estimates unless marked exact",
                notes.join(", ")
            );
        }
        Some(s)
    } else {
        None
    };

    let mut payload = json!({
        "success": true,
        "root": label,
        "tree": node_json(&root, depth, &opts),
        "totals": { "folders": folder_count, "files": file_count, "size": size },
    });
    if let Some(s) = stats {
        payload["stats"] = json!({
            "fileCount": s.file_count,
            "totalSize": s.total_size,
            "isFileCountExact": s.is_file_count_exact,
            "isTotalSizeExact": s.is_total_size_exact,
        });
    }
    output::emit(human.trim_end(), payload);
    Ok(())
}

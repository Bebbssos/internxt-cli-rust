//! One-way folder sync: `sync-up` (local → remote) and `sync-down` (remote → local).
//!
//! One-shot reconcile pass, not a daemon. The source side always wins — no
//! bidirectional mode, no conflict policy, no sync-state DB. Change detection
//! compares `size`, then `modificationTime` (±2s FS-granularity tolerance).
//! Tree walk is keyed by POSIX relative path, case-sensitive. Workspace-aware for
//! free via `DriveApi::for_credentials` + the `creds.*()` helpers.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use rand::RngExt;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;

use internxt_core::api::DriveApi;
use crate::auth;
use internxt_core::transfer::{create_folder_with_retry, upload_file_to_network};
use internxt_core::crypto::{self, Ctr};
use internxt_core::models::Credentials;
use internxt_core::network::NetworkApi;
use crate::output;

/// Files whose mtimes differ by more than this many seconds are "changed"
/// (absorbs FS timestamp granularity between local and remote).
const MTIME_TOL_SECS: i64 = 2;
const MAX_CONCURRENT_TRANSFERS: usize = 10;

/// What to do with items that exist only on the destination side.
#[derive(Clone, Copy, PartialEq)]
enum DeleteMode {
    /// Leave destination-only items untouched.
    None,
    /// sync-up: move remote extras to trash. sync-down: not used.
    Trash,
    /// sync-up: permanently delete remote extras.
    Permanent,
    /// sync-down: remove local extra files from disk.
    Remove,
}

/// Parse the optional `--delete[=MODE]` flag for the given direction.
fn parse_delete(delete: Option<&str>, up: bool) -> Result<DeleteMode> {
    match delete {
        None => Ok(DeleteMode::None),
        Some(m) => {
            let m = m.trim().to_lowercase();
            if up {
                match m.as_str() {
                    "" | "default" | "trash" => Ok(DeleteMode::Trash),
                    "permanent" | "permanently" => Ok(DeleteMode::Permanent),
                    other => Err(anyhow!(
                        "invalid --delete mode for sync-up: {other} (use `trash` or `permanent`)"
                    )),
                }
            } else {
                match m.as_str() {
                    "" | "default" | "remove" => Ok(DeleteMode::Remove),
                    "trash" => Err(anyhow!(
                        "--delete=trash (OS trash) is not supported yet; use `--delete` to remove local files"
                    )),
                    other => Err(anyhow!(
                        "invalid --delete mode for sync-down: {other} (use `remove`)"
                    )),
                }
            }
        }
    }
}

// ---- shared tree model ----

struct LocalFile {
    abs: PathBuf,
    size: u64,
    mtime: i64,
}

struct RemoteFile {
    uuid: String,
    size: u64,
    mtime: i64,
    file_id: Option<String>,
    bucket: String,
}

fn systime_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn rfc3339_secs(s: &str) -> i64 {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

fn to_rfc3339(t: SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.to_rfc3339()
}

fn dirname(rel: &str) -> String {
    match rel.rfind('/') {
        Some(i) => rel[..i].to_string(),
        None => String::new(),
    }
}

fn basename(rel: &str) -> &str {
    match rel.rfind('/') {
        Some(i) => &rel[i + 1..],
        None => rel,
    }
}

fn join_rel(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else {
        format!("{base}/{name}")
    }
}

/// True if `rel` sits inside any of `dirs` (i.e. some `dir/` is a prefix). Used to
/// skip files/subfolders already covered by a pruned parent folder.
fn under_any(rel: &str, dirs: &[String]) -> bool {
    dirs.iter().any(|d| rel.starts_with(&format!("{d}/")))
}

/// True if the file content is considered changed (source vs destination).
fn changed(a_size: u64, a_mtime: i64, b_size: u64, b_mtime: i64) -> bool {
    a_size != b_size || (a_mtime - b_mtime).abs() > MTIME_TOL_SECS
}

fn value_size(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

/// Recursively walk a local directory tree, keyed by POSIX rel path (relative to
/// `root`). Skips symlinks. Collects files (with size + mtime) and every
/// subdirectory rel path.
fn walk_local(
    root: &Path,
    current: &Path,
    rel: &str,
    files: &mut HashMap<String, LocalFile>,
    dirs: &mut Vec<String>,
) {
    let entries = match std::fs::read_dir(current) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let child_rel = join_rel(rel, &name);
        if meta.is_dir() {
            dirs.push(child_rel.clone());
            walk_local(root, &path, &child_rel, files, dirs);
        } else if meta.is_file() {
            let mtime = meta.modified().map(systime_secs).unwrap_or(0);
            files.insert(
                child_rel,
                LocalFile {
                    abs: path,
                    size: meta.len(),
                    mtime,
                },
            );
        }
    }
}

/// Extract the EXISTS items from one folder-content page (`.folders`/`.files` for
/// personal, `.result` for workspace), filtering trashed entries.
fn page_items(page: &Value, key: &str) -> Vec<Value> {
    page.get(key)
        .or_else(|| page.get("result"))
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|item| {
            let status = item.get("status").and_then(|s| s.as_str()).unwrap_or("");
            status.is_empty() || status == "EXISTS"
        })
        .collect()
}

/// Recursively build the remote tree rooted at `root_uuid`. Returns a
/// rel-path → RemoteFile map and a rel-path → folder-uuid map (with `""` = root).
async fn build_remote_tree(
    api: &DriveApi,
    token: &str,
    root_uuid: &str,
) -> Result<(HashMap<String, RemoteFile>, HashMap<String, String>)> {
    let mut files: HashMap<String, RemoteFile> = HashMap::new();
    let mut dirs: HashMap<String, String> = HashMap::new();
    dirs.insert(String::new(), root_uuid.to_string());

    let mut stack = vec![(String::new(), root_uuid.to_string())];
    while let Some((rel, uuid)) = stack.pop() {
        // Subfolders (paginated).
        let mut offset = 0u32;
        loop {
            let page = api.get_folder_subfolders(token, &uuid, offset).await?;
            let items = page_items(&page, "folders");
            let got = items.len() as u32;
            for it in &items {
                let name = it.get("plainName").and_then(|s| s.as_str()).unwrap_or("");
                let cuuid = it.get("uuid").and_then(|s| s.as_str()).unwrap_or("");
                if name.is_empty() || cuuid.is_empty() {
                    continue;
                }
                let crel = join_rel(&rel, name);
                dirs.insert(crel.clone(), cuuid.to_string());
                stack.push((crel, cuuid.to_string()));
            }
            if got < 50 {
                break;
            }
            offset += got;
        }
        // Subfiles (paginated).
        let mut offset = 0u32;
        loop {
            let page = api.get_folder_subfiles(token, &uuid, offset).await?;
            let items = page_items(&page, "files");
            let got = items.len() as u32;
            for it in &items {
                let plain = it.get("plainName").and_then(|s| s.as_str()).unwrap_or("");
                let ftype = it.get("type").and_then(|s| s.as_str()).unwrap_or("");
                let fuuid = it.get("uuid").and_then(|s| s.as_str()).unwrap_or("");
                if fuuid.is_empty() {
                    continue;
                }
                let name = if ftype.is_empty() {
                    plain.to_string()
                } else {
                    format!("{plain}.{ftype}")
                };
                let crel = join_rel(&rel, &name);
                let mtime = it
                    .get("modificationTime")
                    .and_then(|s| s.as_str())
                    .or_else(|| it.get("updatedAt").and_then(|s| s.as_str()))
                    .map(rfc3339_secs)
                    .unwrap_or(0);
                files.insert(
                    crel,
                    RemoteFile {
                        uuid: fuuid.to_string(),
                        size: it.get("size").map(value_size).unwrap_or(0),
                        mtime,
                        file_id: it
                            .get("fileId")
                            .and_then(|s| s.as_str())
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string()),
                        bucket: it.get("bucket").and_then(|s| s.as_str()).unwrap_or("").to_string(),
                    },
                );
            }
            if got < 50 {
                break;
            }
            offset += got;
        }
    }
    Ok((files, dirs))
}

// ---- action recording ----

#[derive(Default)]
struct Summary {
    actions: Mutex<Vec<Value>>,
    transferred: AtomicU64,
    deleted: AtomicU64,
    failed: AtomicU64,
}

impl Summary {
    fn record(&self, action: &str, path: &str, ok: bool) {
        self.actions
            .lock()
            .unwrap()
            .push(json!({ "action": action, "path": path, "ok": ok }));
    }
}

fn emit_summary(dry_run: bool, skipped: u64, s: &Summary) {
    let actions = s.actions.lock().unwrap();
    let transferred = s.transferred.load(Ordering::Relaxed);
    let deleted = s.deleted.load(Ordering::Relaxed);
    let failed = s.failed.load(Ordering::Relaxed);
    if output::is_json() {
        output::emit(
            "",
            json!({
                "success": failed == 0,
                "dryRun": dry_run,
                "transferred": transferred,
                "deleted": deleted,
                "skipped": skipped,
                "failed": failed,
                "actions": *actions,
            }),
        );
    } else {
        let verb = if dry_run { "Planned" } else { "Done" };
        output::status(&format!(
            "{verb}: {transferred} transferred, {deleted} deleted, {skipped} unchanged, {failed} failed."
        ));
    }
}

// ---- sync-up: make remote match local ----

#[allow(clippy::too_many_arguments)]
pub async fn sync_up(
    local: &str,
    remote: Option<&str>,
    remote_path: Option<&str>,
    delete: Option<&str>,
    dry_run: bool,
    limit_args: &crate::upload_limit::UploadLimitArgs,
) -> Result<()> {
    let mode = parse_delete(delete, true)?;
    let creds = Arc::new(auth::get_auth_details().await?);
    let limit = crate::upload_limit::resolve(limit_args, &creds).await?;

    let root = Path::new(local);
    let md = std::fs::metadata(root).map_err(|_| anyhow!("Not a directory: {local}"))?;
    if !md.is_dir() {
        return Err(anyhow!("Not a directory: {local}"));
    }
    let remote_uuid = {
        let api = DriveApi::for_credentials(&creds);
        crate::paths::resolve_opt(
            &api,
            &creds.token,
            creds.root_folder(),
            remote,
            remote_path,
            crate::paths::Expect::Folder,
        )
        .await?
        .unwrap_or_else(|| creds.root_folder().to_string())
    };

    let mut local_files = HashMap::new();
    let mut local_dirs = Vec::new();
    walk_local(root, root, "", &mut local_files, &mut local_dirs);

    output::status("Scanning remote tree...");
    let api = DriveApi::for_credentials(&creds);
    let (remote_files, mut remote_dirs) =
        build_remote_tree(&api, &creds.token, &remote_uuid).await?;

    // Classify local files: new uploads vs. changed re-uploads vs. skips.
    let mut to_upload: Vec<(String, u64)> = Vec::new(); // (rel, size)
    let mut reupload_old: HashMap<String, String> = HashMap::new(); // rel -> old remote uuid
    let mut skipped = 0u64;
    for (rel, lf) in &local_files {
        match remote_files.get(rel) {
            Some(rf) => {
                if changed(lf.size, lf.mtime, rf.size, rf.mtime) {
                    reupload_old.insert(rel.clone(), rf.uuid.clone());
                    to_upload.push((rel.clone(), lf.size));
                } else {
                    skipped += 1;
                }
            }
            None => to_upload.push((rel.clone(), lf.size)),
        }
    }
    // Destination-only items → optional prune. Folders first (top-most extra dirs;
    // trashing one cascades its whole subtree), then standalone extra files.
    let mut to_delete: Vec<(String, String, bool)> = Vec::new(); // (rel, uuid, is_folder)
    if mode != DeleteMode::None {
        let local_dir_set: HashSet<&str> = local_dirs.iter().map(|s| s.as_str()).collect();
        let mut pruned_dirs: Vec<String> = Vec::new();
        for (rel, uuid) in &remote_dirs {
            if rel.is_empty() || local_dir_set.contains(rel.as_str()) {
                continue; // root, or a folder that also exists locally
            }
            // Extra folder. Keep only the top-most ones (parent exists locally / is root).
            let parent = dirname(rel);
            if parent.is_empty() || local_dir_set.contains(parent.as_str()) {
                pruned_dirs.push(rel.clone());
                to_delete.push((rel.clone(), uuid.clone(), true));
            }
        }
        for (rel, rf) in &remote_files {
            if local_files.contains_key(rel) || under_any(rel, &pruned_dirs) {
                continue;
            }
            to_delete.push((rel.clone(), rf.uuid.clone(), false));
        }
    }

    if dry_run {
        let s = Summary::default();
        for (rel, _) in &to_upload {
            let kind = if reupload_old.contains_key(rel) { "reupload" } else { "upload" };
            output::status(&format!("{kind}: {rel}"));
            s.record(kind, rel, true);
            s.transferred.fetch_add(1, Ordering::Relaxed);
        }
        for (rel, _, is_folder) in &to_delete {
            let tag = if *is_folder { "delete-remote-folder" } else { "delete-remote" };
            output::status(&format!("{tag}: {rel}"));
            s.record(tag, rel, true);
            s.deleted.fetch_add(1, Ordering::Relaxed);
        }
        emit_summary(true, skipped, &s);
        return Ok(());
    }

    // Ensure every needed remote folder exists (parents before children).
    local_dirs.sort_by_key(|d| d.matches('/').count());
    for dir in &local_dirs {
        if remote_dirs.contains_key(dir) {
            continue;
        }
        let parent = dirname(dir);
        let parent_uuid = match remote_dirs.get(&parent) {
            Some(u) => u.clone(),
            None => {
                output::status(&format!("Parent folder missing for {dir}, skipping"));
                continue;
            }
        };
        match create_folder_with_retry(&api, &creds.token, basename(dir), &parent_uuid).await? {
            Some(uuid) => {
                remote_dirs.insert(dir.clone(), uuid);
            }
            None => output::status(&format!("Folder {dir} already exists but was not listed")),
        }
    }
    let remote_dirs = Arc::new(remote_dirs);
    let reupload_old = Arc::new(reupload_old);

    let total_bytes: u64 = to_upload.iter().map(|(_, s)| *s).sum();
    let pb = output::progress_bar(total_bytes, "Uploading");
    let net = NetworkApi::new(creds.net_user(), creds.net_pass());
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRANSFERS));
    let summary = Arc::new(Summary::default());
    let mut handles = Vec::new();

    for (rel, _size) in to_upload {
        let parent_rel = dirname(&rel);
        let parent_uuid = match remote_dirs.get(&parent_rel) {
            Some(u) => u.clone(),
            None => {
                pb.println(format!("No remote parent for {rel}, skipping"));
                summary.record("upload", &rel, false);
                summary.failed.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let lf = local_files.get(&rel).unwrap();
        let abs = lf.abs.clone();
        let size = lf.size;
        if let Err(e) = limit.check(size) {
            pb.println(format!("Skipping {rel}: {e}"));
            summary.record("upload", &rel, false);
            summary.failed.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        let mtime = to_rfc3339(
            std::fs::metadata(&abs)
                .and_then(|m| m.modified())
                .unwrap_or_else(|_| SystemTime::now()),
        );
        let old_uuid = reupload_old.get(&rel).cloned();

        let permit = sem.clone().acquire_owned().await.unwrap();
        let net = net.clone();
        let creds = creds.clone();
        let pb = pb.clone();
        let summary = summary.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let api = DriveApi::for_credentials(&creds);
            let res = upload_one(
                &net, &api, &creds, &abs, size, &parent_uuid, &rel, &mtime, old_uuid, &pb,
            )
            .await;
            match res {
                Ok(()) => {
                    summary.transferred.fetch_add(1, Ordering::Relaxed);
                    summary.record("upload", &rel, true);
                    pb.println(format!("Uploaded {rel}"));
                }
                Err(e) => {
                    summary.failed.fetch_add(1, Ordering::Relaxed);
                    summary.record("upload", &rel, false);
                    pb.println(format!("Failed {rel}: {e}"));
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    pb.finish_and_clear();

    // Deletes (sequential; low volume, keeps output readable).
    for (rel, uuid, is_folder) in to_delete {
        let ty = if is_folder { "folder" } else { "file" };
        let res = match (mode, is_folder) {
            (DeleteMode::Permanent, true) => api.delete_folder(&creds.token, &uuid).await,
            (DeleteMode::Permanent, false) => api.delete_file(&creds.token, &uuid).await,
            (_, _) => {
                api.trash_items(&creds.token, json!([{ "uuid": uuid, "type": ty }]))
                    .await
            }
        };
        let tag = if is_folder { "delete-remote-folder" } else { "delete-remote" };
        match res {
            Ok(()) => {
                summary.deleted.fetch_add(1, Ordering::Relaxed);
                summary.record(tag, &rel, true);
                output::status(&format!("Deleted remote {ty} {rel}"));
            }
            Err(e) => {
                summary.failed.fetch_add(1, Ordering::Relaxed);
                summary.record(tag, &rel, false);
                output::status(&format!("Failed to delete remote {ty} {rel}: {e}"));
            }
        }
    }

    emit_summary(false, skipped, &summary);
    Ok(())
}

/// Upload a single local file to `parent_uuid`, trashing the old remote entry first
/// on a re-upload (content update loses the old uuid — acceptable v1).
#[allow(clippy::too_many_arguments)]
async fn upload_one(
    net: &NetworkApi,
    api: &DriveApi,
    creds: &Credentials,
    abs: &Path,
    size: u64,
    parent_uuid: &str,
    rel: &str,
    modification: &str,
    old_uuid: Option<String>,
    pb: &indicatif::ProgressBar,
) -> Result<()> {
    if let Some(old) = old_uuid {
        api.trash_items(&creds.token, json!([{ "uuid": old, "type": "file" }]))
            .await?;
    }
    let name = basename(rel);
    let np = Path::new(name);
    let stem = np.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let ftype = np.extension().and_then(|s| s.to_str()).unwrap_or("");

    let mut file_id = String::new();
    if size > 0 {
        file_id = upload_file_to_network(
            net,
            creds.bucket(),
            creds.mnemonic(),
            abs,
            size,
            Some(crate::output::bar_sink(pb)),
        )
        .await?;
    }
    api.create_file_entry(
        &creds.token,
        stem,
        ftype,
        size,
        parent_uuid,
        &file_id,
        creds.bucket(),
        modification,
        modification,
    )
    .await?;
    Ok(())
}

// ---- sync-down: make local match remote ----

pub async fn sync_down(
    local: &str,
    remote: Option<&str>,
    remote_path: Option<&str>,
    delete: Option<&str>,
    dry_run: bool,
) -> Result<()> {
    let mode = parse_delete(delete, false)?;
    let creds = Arc::new(auth::get_auth_details().await?);

    let root = PathBuf::from(local);
    if root.exists() && !root.is_dir() {
        return Err(anyhow!("Not a directory: {local}"));
    }
    let remote_uuid = {
        let api = DriveApi::for_credentials(&creds);
        crate::paths::resolve_opt(
            &api,
            &creds.token,
            creds.root_folder(),
            remote,
            remote_path,
            crate::paths::Expect::Folder,
        )
        .await?
        .unwrap_or_else(|| creds.root_folder().to_string())
    };

    output::status("Scanning remote tree...");
    let api = DriveApi::for_credentials(&creds);
    let (remote_files, remote_dirs) =
        build_remote_tree(&api, &creds.token, &remote_uuid).await?;

    let mut local_files = HashMap::new();
    let mut local_dirs = Vec::new();
    if root.is_dir() {
        walk_local(&root, &root, "", &mut local_files, &mut local_dirs);
    }

    // Classify remote files: new downloads vs. changed re-downloads vs. skips.
    let mut to_download: Vec<String> = Vec::new();
    let mut skipped = 0u64;
    for (rel, rf) in &remote_files {
        match local_files.get(rel) {
            Some(lf) => {
                if changed(rf.size, rf.mtime, lf.size, lf.mtime) {
                    to_download.push(rel.clone());
                } else {
                    skipped += 1;
                }
            }
            None => to_download.push(rel.clone()),
        }
    }
    // Local-only items → optional prune. Top-most extra dirs first (removing one
    // takes its whole subtree), then standalone extra files.
    let mut to_delete: Vec<(String, bool)> = Vec::new(); // (rel, is_folder)
    if mode != DeleteMode::None {
        let remote_dir_set: HashSet<&str> = remote_dirs.keys().map(|s| s.as_str()).collect();
        let mut pruned_dirs: Vec<String> = Vec::new();
        for dir in &local_dirs {
            if remote_dir_set.contains(dir.as_str()) {
                continue; // folder also exists remotely
            }
            let parent = dirname(dir);
            if parent.is_empty() || remote_dir_set.contains(parent.as_str()) {
                pruned_dirs.push(dir.clone());
                to_delete.push((dir.clone(), true));
            }
        }
        for rel in local_files.keys() {
            if remote_files.contains_key(rel) || under_any(rel, &pruned_dirs) {
                continue;
            }
            to_delete.push((rel.clone(), false));
        }
    }

    if dry_run {
        let s = Summary::default();
        for rel in &to_download {
            let kind = if local_files.contains_key(rel) { "redownload" } else { "download" };
            output::status(&format!("{kind}: {rel}"));
            s.record(kind, rel, true);
            s.transferred.fetch_add(1, Ordering::Relaxed);
        }
        for (rel, is_folder) in &to_delete {
            let tag = if *is_folder { "delete-local-folder" } else { "delete-local" };
            output::status(&format!("{tag}: {rel}"));
            s.record(tag, rel, true);
            s.deleted.fetch_add(1, Ordering::Relaxed);
        }
        emit_summary(true, skipped, &s);
        return Ok(());
    }

    // Create local directory tree (parents before children).
    let mut ordered_dirs: Vec<&String> = remote_dirs.keys().filter(|d| !d.is_empty()).collect();
    ordered_dirs.sort_by_key(|d| d.matches('/').count());
    for dir in ordered_dirs {
        let _ = std::fs::create_dir_all(root.join(dir));
    }
    std::fs::create_dir_all(&root)?;

    let total_bytes: u64 = to_download
        .iter()
        .filter_map(|rel| remote_files.get(rel).map(|rf| rf.size))
        .sum();
    let pb = output::progress_bar(total_bytes, "Downloading");
    let net = NetworkApi::new(creds.net_user(), creds.net_pass());
    let remote_files = Arc::new(remote_files);
    let root = Arc::new(root);
    let sem = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRANSFERS));
    let summary = Arc::new(Summary::default());
    let mut handles = Vec::new();

    for rel in to_download {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let net = net.clone();
        let creds = creds.clone();
        let pb = pb.clone();
        let summary = summary.clone();
        let remote_files = remote_files.clone();
        let root = root.clone();
        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let rf = remote_files.get(&rel).unwrap();
            let dest = root.join(&rel);
            let res = download_one(&net, &creds, rf, &dest, &pb).await;
            match res {
                Ok(()) => {
                    summary.transferred.fetch_add(1, Ordering::Relaxed);
                    summary.record("download", &rel, true);
                    pb.println(format!("Downloaded {rel}"));
                }
                Err(e) => {
                    summary.failed.fetch_add(1, Ordering::Relaxed);
                    summary.record("download", &rel, false);
                    pb.println(format!("Failed {rel}: {e}"));
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    pb.finish_and_clear();

    // Deletes (sequential).
    for (rel, is_folder) in to_delete {
        let path = root.join(&rel);
        let res = if is_folder {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        let (tag, what) = if is_folder {
            ("delete-local-folder", "folder")
        } else {
            ("delete-local", "file")
        };
        match res {
            Ok(()) => {
                summary.deleted.fetch_add(1, Ordering::Relaxed);
                summary.record(tag, &rel, true);
                output::status(&format!("Removed local {what} {rel}"));
            }
            Err(e) => {
                summary.failed.fetch_add(1, Ordering::Relaxed);
                summary.record(tag, &rel, false);
                output::status(&format!("Failed to remove local {what} {rel}: {e}"));
            }
        }
    }

    emit_summary(false, skipped, &summary);
    Ok(())
}

/// Download + decrypt one remote file to `dest` via a temp sibling + atomic rename.
async fn download_one(
    net: &NetworkApi,
    creds: &Credentials,
    rf: &RemoteFile,
    dest: &Path,
    pb: &indicatif::ProgressBar,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Empty file: no network shard, just create it.
    if rf.size == 0 || rf.file_id.is_none() {
        tokio::fs::File::create(dest).await?;
        set_mtime(dest, rf.mtime);
        return Ok(());
    }
    let file_id = rf.file_id.as_ref().unwrap();
    let bucket = if rf.bucket.is_empty() {
        creds.bucket().to_string()
    } else {
        rf.bucket.clone()
    };

    let links = net.get_download_links(&bucket, file_id).await?;
    if matches!(links.version, None | Some(1)) {
        return Err(anyhow!("File version 1 not supported"));
    }
    let index = hex::decode(&links.index)?;
    let iv = &index[0..16];
    let key = crypto::generate_file_key(creds.mnemonic(), &bucket, &index)?;
    let mut shards = links.shards.clone();
    shards.sort_by_key(|s| s.index);

    let mut rnd = [0u8; 8];
    rand::rng().fill(&mut rnd);
    let tmp = dest.with_file_name(format!(
        ".{}.inxt-{}.part",
        dest.file_name().and_then(|s| s.to_str()).unwrap_or("file"),
        hex::encode(rnd)
    ));

    let mut ctr = Ctr::new(&key, iv);
    {
        let mut out = tokio::fs::File::create(&tmp).await?;
        for shard in &shards {
            let resp = net.download_shard_stream(&shard.url).await?;
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let mut bytes: Vec<u8> = match chunk {
                    Ok(b) => b.to_vec(),
                    Err(e) => {
                        let _ = std::fs::remove_file(&tmp);
                        return Err(e.into());
                    }
                };
                ctr.apply(&mut bytes);
                if let Err(e) = out.write_all(&bytes).await {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(e.into());
                }
                pb.inc(bytes.len() as u64);
            }
        }
        out.flush().await?;
    }
    tokio::fs::rename(&tmp, dest).await?;
    // Stamp the remote modificationTime so subsequent syncs see it as unchanged.
    set_mtime(dest, rf.mtime);
    Ok(())
}

/// Set a file's mtime (best effort) to `secs` epoch, so downloaded files carry the
/// remote modificationTime and don't look "changed" on the next sync pass.
fn set_mtime(path: &Path, secs: i64) {
    let ft = filetime::FileTime::from_unix_time(secs, 0);
    let _ = filetime::set_file_mtime(path, ft);
}

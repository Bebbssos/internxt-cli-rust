//! `compare file` / `compare folder`: verify a local file/folder is byte-identical
//! to its counterpart on Internxt Drive, without transferring anything anywhere.
//!
//! File: compares size first (mismatch short-circuits — no point streaming); if
//! sizes match, streams both sides and stops at the first differing byte.
//! `--metadata-only` skips the streaming step entirely (size, and mtime with
//! `--check-modified`, only). `--check-modified` is independent of content: it's
//! an extra check layered on top, so a byte-identical file with a differing
//! mtime is still reported as different.
//!
//! Folder: recursively walks both trees (reusing `sync`'s tree-walk — same
//! code sync-up/down already trust) and diffs file-by-file. Stops at the first
//! difference found (file or folder, either side) unless `--list` is given, in
//! which case every difference is collected and reported together.
//!
//! Both commands exit non-zero when a difference is found (and zero when
//! identical) — this is a comparison result, not a command failure, so it's
//! reported via `output::emit` + `std::process::exit`, not `Err`, mirroring how
//! `sync_up`/`sync_down` report their own per-item failures.

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::collections::HashMap;
use std::path::Path;
use tokio::io::AsyncReadExt;

use crate::auth;
use crate::output;
use crate::sync;
use internxt_core::api::DriveApi;
use internxt_core::crypto::{self, Ctr};
use internxt_core::models::Credentials;
use internxt_core::network::NetworkApi;

fn value_size(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn rfc3339_secs(s: &str) -> i64 {
    DateTime::parse_from_rfc3339(s).map(|d| d.timestamp()).unwrap_or(0)
}

fn fmt_ts(secs: i64) -> String {
    DateTime::<Utc>::from_timestamp(secs, 0)
        .map(|d| d.to_rfc3339())
        .unwrap_or_else(|| secs.to_string())
}

/// One reported difference.
struct Difference {
    kind: &'static str,
    path: String,
    detail: String,
}

/// Print the outcome (identical, or every difference found) and exit non-zero
/// if any difference was found. Never returns when differences exist.
fn finish(diffs: Vec<Difference>) -> Result<()> {
    if diffs.is_empty() {
        output::emit(
            "Identical.",
            json!({ "success": true, "identical": true, "differences": [] }),
        );
        return Ok(());
    }
    let json_diffs: Vec<Value> = diffs
        .iter()
        .map(|d| json!({ "type": d.kind, "path": d.path, "detail": d.detail }))
        .collect();
    if output::is_json() {
        output::emit(
            "",
            json!({ "success": false, "identical": false, "differences": json_diffs }),
        );
    } else {
        output::status(&format!("Differences found ({}):", diffs.len()));
        for d in &diffs {
            if d.path.is_empty() {
                output::status(&format!("  [{}] {}", d.kind, d.detail));
            } else {
                output::status(&format!("  [{}] {}: {}", d.kind, d.path, d.detail));
            }
        }
    }
    std::process::exit(1);
}

/// Stream-decrypt a remote file and compare it byte-for-byte against a local
/// file, returning the byte offset of the first mismatch (or `None` if
/// identical). Assumes the caller already confirmed both sides have equal
/// size — a size mismatch would otherwise surface here as a confusing
/// "local file is shorter" error instead of the clearer size check.
async fn content_diff_offset(
    net: &NetworkApi,
    creds: &Credentials,
    file_id: &str,
    bucket: &str,
    local_path: &Path,
    pb: Option<&indicatif::ProgressBar>,
) -> Result<Option<u64>> {
    let links = net.get_download_links(bucket, file_id).await?;
    if matches!(links.version, None | Some(1)) {
        return Err(anyhow!("File version 1 not supported"));
    }
    let index = hex::decode(&links.index)?;
    let iv = &index[0..16];
    let key = crypto::generate_file_key(creds.mnemonic(), bucket, &index)?;
    let mut shards = links.shards.clone();
    shards.sort_by_key(|s| s.index);

    let mut local = tokio::fs::File::open(local_path).await?;
    let mut ctr = Ctr::new(&key, iv);
    let mut offset: u64 = 0;
    for shard in &shards {
        let resp = net.download_shard_stream(&shard.url).await?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let mut bytes = chunk?.to_vec();
            ctr.apply(&mut bytes);
            let mut local_buf = vec![0u8; bytes.len()];
            local.read_exact(&mut local_buf).await.map_err(|_| {
                anyhow!("local file is shorter than remote (truncated at offset {offset})")
            })?;
            if let Some(pos) = bytes.iter().zip(local_buf.iter()).position(|(a, b)| a != b) {
                return Ok(Some(offset + pos as u64));
            }
            offset += bytes.len() as u64;
            if let Some(pb) = pb {
                pb.inc(bytes.len() as u64);
            }
        }
    }
    // Defensive: sizes were already checked equal, so local should be at EOF too.
    let mut probe = [0u8; 1];
    if local.read(&mut probe).await? > 0 {
        return Ok(Some(offset));
    }
    Ok(None)
}

/// Compare one local file against one remote file's metadata (+ content,
/// unless `metadata_only`). Returns every reason it differs (possibly more
/// than one, e.g. size *and* mtime) — empty means identical under the
/// requested checks.
#[allow(clippy::too_many_arguments)]
async fn diff_reasons_for_file(
    local_abs: &Path,
    local_size: u64,
    local_mtime: i64,
    remote_size: u64,
    remote_mtime: i64,
    remote_file_id: Option<&str>,
    remote_bucket: &str,
    metadata_only: bool,
    check_modified: bool,
    net: &NetworkApi,
    creds: &Credentials,
    pb: Option<&indicatif::ProgressBar>,
) -> Result<Vec<String>> {
    let mut reasons = Vec::new();
    let size_differs = local_size != remote_size;
    if size_differs {
        reasons.push(format!(
            "size differs: local {local_size} bytes, remote {remote_size} bytes"
        ));
    }
    if check_modified && (local_mtime - remote_mtime).abs() > sync::MTIME_TOL_SECS {
        reasons.push(format!(
            "modified time differs: local {}, remote {}",
            fmt_ts(local_mtime),
            fmt_ts(remote_mtime)
        ));
    }
    // A size mismatch already proves the files differ; streaming content on
    // top of that would only produce a confusing "local file is shorter"
    // error rather than useful information.
    if metadata_only || size_differs || remote_size == 0 {
        return Ok(reasons);
    }
    let file_id = match remote_file_id {
        Some(id) => id,
        None => {
            reasons.push("remote file has no network fileId; cannot compare content".to_string());
            return Ok(reasons);
        }
    };
    if let Some(offset) = content_diff_offset(net, creds, file_id, remote_bucket, local_abs, pb).await? {
        reasons.push(format!("content differs at byte offset {offset}"));
    }
    Ok(reasons)
}

/// `compare file`: compare a local file against a remote Drive file.
pub async fn compare_file(
    local: &str,
    id: Option<&str>,
    path: Option<&str>,
    metadata_only: bool,
    check_modified: bool,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let uuid = crate::paths::resolve_opt(
        &api,
        &creds.token,
        creds.root_folder(),
        id,
        path,
        crate::paths::Expect::File,
    )
    .await?
    .ok_or_else(|| anyhow!("Provide the remote file id (--id) or path (--path)"))?;

    let local_path = Path::new(local);
    let local_meta =
        std::fs::metadata(local_path).map_err(|_| anyhow!("Local file not found: {local}"))?;
    if !local_meta.is_file() {
        return Err(anyhow!("Not a file: {local}"));
    }
    let local_size = local_meta.len();
    let local_mtime = local_meta
        .modified()
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0))
        .unwrap_or(0);

    output::status("Getting remote file metadata...");
    let meta = api.get_file_meta_value(&creds.token, &uuid).await?;
    let remote_size = meta.get("size").map(value_size).unwrap_or(0);
    let remote_mtime = meta
        .get("modificationTime")
        .and_then(|v| v.as_str())
        .or_else(|| meta.get("updatedAt").and_then(|v| v.as_str()))
        .map(rfc3339_secs)
        .unwrap_or(0);
    let remote_file_id = meta.get("fileId").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let remote_bucket = meta
        .get("bucket")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| creds.bucket().to_string());

    let pb = if metadata_only || local_size != remote_size {
        None
    } else {
        Some(output::progress_bar(remote_size, "Comparing"))
    };
    let net = crate::net_client::network_api(creds.net_user(), creds.net_pass());
    let reasons = diff_reasons_for_file(
        local_path,
        local_size,
        local_mtime,
        remote_size,
        remote_mtime,
        remote_file_id,
        &remote_bucket,
        metadata_only,
        check_modified,
        &net,
        &creds,
        pb.as_ref(),
    )
    .await?;
    if let Some(pb) = &pb {
        pb.finish_and_clear();
    }

    let name = local_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(local)
        .to_string();
    let diffs = reasons
        .into_iter()
        .map(|detail| Difference { kind: "file", path: name.clone(), detail })
        .collect();
    finish(diffs)
}

/// Compare one local/remote file pair already resolved from the folder tree
/// walk (see `sync::LocalFile`/`sync::RemoteFile`).
#[allow(clippy::too_many_arguments)]
async fn compare_one_file(
    lf: &sync::LocalFile,
    rf: &sync::RemoteFile,
    metadata_only: bool,
    check_modified: bool,
    net: &NetworkApi,
    creds: &Credentials,
    pb: Option<&indicatif::ProgressBar>,
) -> Result<Vec<String>> {
    let bucket = if rf.bucket.is_empty() { creds.bucket() } else { &rf.bucket };
    diff_reasons_for_file(
        &lf.abs,
        lf.size,
        lf.mtime,
        rf.size,
        rf.mtime,
        rf.file_id.as_deref(),
        bucket,
        metadata_only,
        check_modified,
        net,
        creds,
        pb,
    )
    .await
}

/// `compare folder`: recursively compare a local folder against a remote
/// Drive folder. Stops at the first difference found unless `list_all`.
pub async fn compare_folder(
    local: &str,
    id: Option<&str>,
    path: Option<&str>,
    metadata_only: bool,
    check_modified: bool,
    list_all: bool,
) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);

    let root = Path::new(local);
    let md = std::fs::metadata(root).map_err(|_| anyhow!("Not a directory: {local}"))?;
    if !md.is_dir() {
        return Err(anyhow!("Not a directory: {local}"));
    }

    // `//` / `//backups`: compare each real child against its own local
    // subdir (`drive/`, `backups/<device>/`), same fan-out as `sync down`/
    // `download folder`, then report every leaf's diffs together — a plain
    // per-leaf `compare_folder` call would `finish()`/exit after the first
    // leaf instead of covering the whole virtual tree, so the diffing core
    // is factored out into `diff_one_folder` (no `finish`/exit) instead.
    if let Some(node) = crate::paths::virtual_node_for(path) {
        let leaves = crate::paths::flatten_virtual(&api, &creds.token, creds.root_folder(), &node).await?;
        let mut diffs = Vec::new();
        for (subdir, uuid) in leaves {
            let mut sub_diffs =
                diff_one_folder(&root.join(&subdir), &uuid, metadata_only, check_modified, list_all, &creds, &api)
                    .await?;
            // Prefix with the leaf so diffs from different children (e.g.
            // `drive` vs. a device) stay distinguishable once merged.
            for d in &mut sub_diffs {
                d.path = if d.path.is_empty() { subdir.clone() } else { format!("{subdir}/{}", d.path) };
            }
            let found = !sub_diffs.is_empty();
            diffs.extend(sub_diffs);
            if !list_all && found {
                break;
            }
        }
        return finish(diffs);
    }

    let remote_uuid = crate::paths::resolve_opt(
        &api,
        &creds.token,
        creds.root_folder(),
        id,
        path,
        crate::paths::Expect::Folder,
    )
    .await?
    .unwrap_or_else(|| creds.root_folder().to_string());

    let diffs = diff_one_folder(root, &remote_uuid, metadata_only, check_modified, list_all, &creds, &api).await?;
    finish(diffs)
}

/// Diff one local folder against one remote folder, returning every
/// difference found (or just the first one, when `!list_all`) — the core of
/// `compare_folder`, minus the `finish`/exit call, so it can be run once per
/// leaf when comparing against a virtual (`//`/`//backups`) grouping.
async fn diff_one_folder(
    root: &Path,
    remote_uuid: &str,
    metadata_only: bool,
    check_modified: bool,
    list_all: bool,
    creds: &Credentials,
    api: &DriveApi,
) -> Result<Vec<Difference>> {
    let mut local_files: HashMap<String, sync::LocalFile> = HashMap::new();
    let mut local_dirs: Vec<String> = Vec::new();
    sync::walk_local(root, root, "", false, &mut local_files, &mut local_dirs);

    output::status("Scanning remote tree...");
    let (remote_files, remote_dirs) = sync::build_remote_tree(api, &creds.token, remote_uuid).await?;

    let net = crate::net_client::network_api(creds.net_user(), creds.net_pass());
    let mut diffs: Vec<Difference> = Vec::new();

    // 1. Directory presence (excluding "" == the compared root itself).
    let local_dir_set: BTreeSet<&str> = local_dirs.iter().map(|s| s.as_str()).collect();
    let remote_dir_set: BTreeSet<&str> =
        remote_dirs.keys().map(|s| s.as_str()).filter(|s| !s.is_empty()).collect();
    let mut all_dirs: BTreeSet<&str> = BTreeSet::new();
    all_dirs.extend(local_dir_set.iter().copied());
    all_dirs.extend(remote_dir_set.iter().copied());
    for d in all_dirs {
        let in_local = local_dir_set.contains(d);
        let in_remote = remote_dir_set.contains(d);
        if in_local && !in_remote {
            diffs.push(Difference {
                kind: "folder",
                path: d.to_string(),
                detail: "folder exists locally but not on Drive".to_string(),
            });
            if !list_all {
                return Ok(diffs);
            }
        } else if in_remote && !in_local {
            diffs.push(Difference {
                kind: "folder",
                path: d.to_string(),
                detail: "folder exists on Drive but not locally".to_string(),
            });
            if !list_all {
                return Ok(diffs);
            }
        }
    }

    // 2. File presence + comparison, in deterministic (sorted) order.
    let mut all_files: BTreeSet<&String> = BTreeSet::new();
    all_files.extend(local_files.keys());
    all_files.extend(remote_files.keys());
    for rel in all_files {
        match (local_files.get(rel), remote_files.get(rel)) {
            (Some(_), None) => {
                diffs.push(Difference {
                    kind: "file",
                    path: rel.clone(),
                    detail: "file exists locally but not on Drive".to_string(),
                });
                if !list_all {
                    return Ok(diffs);
                }
            }
            (None, Some(_)) => {
                diffs.push(Difference {
                    kind: "file",
                    path: rel.clone(),
                    detail: "file exists on Drive but not locally".to_string(),
                });
                if !list_all {
                    return Ok(diffs);
                }
            }
            (Some(lf), Some(rf)) => {
                output::status(&format!("Comparing {rel}..."));
                // Content is only actually streamed when sizes already match (a
                // mismatch short-circuits inside `diff_reasons_for_file`), so
                // only bother with a bar in that case — otherwise it would show
                // 0/N and never move before the comparison already returned.
                let pb = if !metadata_only && rf.size > 0 && lf.size == rf.size {
                    let pb = output::progress_bar(rf.size, "Comparing");
                    pb.set_message(format!("Comparing {rel}"));
                    Some(pb)
                } else {
                    None
                };
                let reasons =
                    compare_one_file(lf, rf, metadata_only, check_modified, &net, creds, pb.as_ref())
                        .await?;
                if let Some(pb) = &pb {
                    pb.finish_and_clear();
                }
                if !reasons.is_empty() {
                    for detail in reasons {
                        diffs.push(Difference { kind: "file", path: rel.clone(), detail });
                    }
                    if !list_all {
                        return Ok(diffs);
                    }
                }
            }
            (None, None) => unreachable!("rel came from the union of both keysets"),
        }
    }

    Ok(diffs)
}

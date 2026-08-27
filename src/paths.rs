//! Path <-> uuid resolution for Drive items.
//!
//! Turns a slash path like `/a/b/file.txt` into a Drive uuid and back: a uuid
//! becomes a path via the folders `ancestors` endpoint, and a path becomes a
//! uuid either in one request (`GET /files|folders/meta?path=`, when it's an
//! ordinary path from the account root) or by walking the folder tree one
//! listing per component (workspace-aware, like `serve::tree` but cache-free
//! and available without the serve features). The walk stays the source of
//! truth: the single-request lookup is a pure optimization that hands back to
//! it whenever it can't answer — see [`resolve_via_meta`].
//!
//! Only *live* (non-trashed) items are reachable by path — the walk uses the
//! same paginated `subfolders`/`subfiles` listings the rest of the CLI uses.
//!
//! A leading `//` escapes into an explicit namespace instead of the given
//! root: `//backups/<device>/...` walks a backup device's folder (personal
//! account only — see `backups.rs`) and `//drive/...` is an explicit alias
//! for the default `/...` walk from the given root (mostly for symmetry with
//! `//backups/`). A single leading `/` (or none) always means the given
//! root, same as before. The reverse direction (`path_from_id`) prints
//! `//backups/<device>/...` for anything found to live under a device, and
//! plain `/...` otherwise — `//drive/` is input-only sugar, never emitted.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::auth;
use crate::output;
use internxt_core::api::DriveApi;

/// What kind of item a path is expected to resolve to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Expect {
    File,
    Folder,
    Any,
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

/// Split a Drive path into name components, ignoring empty segments so leading /
/// trailing / doubled slashes and a bare `/` are all fine. Root = empty vec.
fn components(path: &str) -> Vec<String> {
    path.split('/')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Display name of a file listing/meta entry: `plainName` + `.type` when typed.
fn file_display_name(v: &Value) -> String {
    let plain = str_field(v, "plainName");
    let ftype = str_field(v, "type");
    if ftype.is_empty() {
        plain
    } else {
        format!("{plain}.{ftype}")
    }
}

/// GET /backup/deviceAsFolder — the account's backup devices (personal only;
/// callers must check `api.is_workspace()` first). Shared by the `//backups/`
/// path escape here and by `backups.rs`'s own device commands.
pub(crate) async fn fetch_backup_devices(api: &DriveApi, token: &str) -> Result<Vec<Value>> {
    let resp = api.get_backup_devices(token).await?;
    Ok(resp.as_array().cloned().unwrap_or_default())
}

/// Resolve a user-supplied device (uuid or name) against `devices`. Exact
/// uuid match wins; otherwise a case-insensitive exact match on `plainName`
/// (erroring on ambiguity).
pub(crate) fn resolve_backup_device<'a>(devices: &'a [Value], needle: &str) -> Result<&'a Value> {
    if let Some(d) = devices.iter().find(|d| str_field(d, "uuid") == needle) {
        return Ok(d);
    }
    let matches: Vec<&Value> = devices
        .iter()
        .filter(|d| str_field(d, "plainName").eq_ignore_ascii_case(needle))
        .collect();
    match matches.len() {
        0 => {
            let available: Vec<String> = devices.iter().map(|d| str_field(d, "plainName")).collect();
            Err(anyhow!(
                "No backup device found matching '{needle}'. Available devices: {}",
                if available.is_empty() { "(none)".to_string() } else { available.join(", ") }
            ))
        }
        1 => Ok(matches[0]),
        _ => Err(anyhow!(
            "Multiple backup devices are named '{needle}'; use its id (from `backups devices list`) instead."
        )),
    }
}

/// One page of subfolders (`folders`/`result`), following pagination.
async fn list_folders(api: &DriveApi, token: &str, folder_uuid: &str) -> Result<Vec<Value>> {
    list_children(api, token, folder_uuid, true).await
}

/// All subfiles (`files`/`result`) of a folder, following pagination.
async fn list_files(api: &DriveApi, token: &str, folder_uuid: &str) -> Result<Vec<Value>> {
    list_children(api, token, folder_uuid, false).await
}

async fn list_children(
    api: &DriveApi,
    token: &str,
    folder_uuid: &str,
    folders: bool,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut offset: u32 = 0;
    loop {
        let page = if folders {
            api.get_folder_subfolders(token, folder_uuid, offset).await?
        } else {
            api.get_folder_subfiles(token, folder_uuid, offset).await?
        };
        let key = if folders { "folders" } else { "files" };
        // Personal endpoints return `.folders`/`.files`; workspace ones `.result`.
        let arr = page
            .get(key)
            .or_else(|| page.get("result"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let got = arr.len() as u32;
        for item in arr {
            let status = str_field(&item, "status");
            if status.is_empty() || status == "EXISTS" {
                out.push(item);
            }
        }
        if got < 50 {
            break;
        }
        offset += got;
    }
    Ok(out)
}

/// A path resolved to a concrete Drive item.
pub struct Resolved {
    pub uuid: String,
    pub is_folder: bool,
}

/// Walk pre-split `comps` from `root`. Shared by the default `/...` walk and
/// the `//backups/<device>/...` walk (rooted at the device instead of the
/// account/workspace root) — `display` is only used to phrase error messages.
async fn walk_components(
    api: &DriveApi,
    token: &str,
    root: &str,
    comps: &[String],
    expect: Expect,
    display: &str,
) -> Result<Resolved> {
    if comps.is_empty() {
        if expect == Expect::File {
            return Err(anyhow!("Path '{display}' is a folder, not a file"));
        }
        return Ok(Resolved {
            uuid: root.to_string(),
            is_folder: true,
        });
    }

    let last = comps.len() - 1;
    let mut current = root.to_string();
    for (i, comp) in comps.iter().enumerate() {
        let folders = list_folders(api, token, &current).await?;
        if let Some(f) = folders.iter().find(|f| &str_field(f, "plainName") == comp) {
            current = str_field(f, "uuid");
            if i == last {
                if expect == Expect::File {
                    return Err(anyhow!("'{display}' is a folder, not a file"));
                }
                return Ok(Resolved {
                    uuid: current,
                    is_folder: true,
                });
            }
            continue;
        }
        // Last component may be a file when a file is acceptable.
        if i == last && expect != Expect::Folder {
            let files = list_files(api, token, &current).await?;
            if let Some(f) = files.iter().find(|f| &file_display_name(f) == comp) {
                return Ok(Resolved {
                    uuid: str_field(f, "uuid"),
                    is_folder: false,
                });
            }
        }
        let what = if i == last { "item" } else { "folder" };
        return Err(anyhow!("No such {what} '{comp}' at path: {display}"));
    }
    unreachable!()
}

/// Resolve `//backups/<device>/...rest` (the part after the leading `//`, so
/// `rest` starts with `backups`) to a Drive item, rooted at the named
/// device's folder. `//drive/...` is handled here too, as a plain alias for
/// `root`.
async fn resolve_namespaced_path(
    api: &DriveApi,
    token: &str,
    root: &str,
    rest: &str,
    expect: Expect,
) -> Result<Resolved> {
    let comps = components(rest);
    let display = format!("//{rest}");
    match comps.first().map(String::as_str) {
        Some("drive") => walk_components(api, token, root, &comps[1..], expect, &display).await,
        Some("backups") => {
            if comps.len() < 2 {
                return Err(anyhow!("Specify a backup device: //backups/<device>[/path...]"));
            }
            if api.is_workspace() {
                return Err(anyhow!("Backups are personal-account only; not available in an active workspace"));
            }
            let devices = fetch_backup_devices(api, token).await?;
            let device = resolve_backup_device(&devices, &comps[1])?;
            let device_uuid = str_field(device, "uuid");
            walk_components(api, token, &device_uuid, &comps[2..], expect, &display).await
        }
        _ => Err(anyhow!(
            "Unknown path escape '{display}': only '//backups/<device>/...' and '//drive/...' are supported"
        )),
    }
}

/// `true` when a core API error is the server's definite "nothing at this
/// path" answer. Core surfaces failures as `"<ctx> failed: HTTP <status>:
/// <body>"`, so a 404 is recognizable by string; anything else (a timeout, a
/// 5xx, a response that no longer deserializes) is *not* an answer about the
/// path and must never be treated as one.
fn is_not_found(err: &anyhow::Error) -> bool {
    err.to_string().contains("HTTP 404")
}

/// The absolute path spelling the `?path=` endpoints want, rebuilt from the
/// already-split components: exactly one leading slash, none trailing or
/// doubled. The leading `/` is not cosmetic — without it the server answers
/// `400 Invalid path provided`.
fn meta_path(comps: &[String]) -> String {
    format!("/{}", comps.join("/"))
}

/// Resolve a whole path from the account root in one request, via
/// `GET /folders/meta?path=` / `GET /files/meta?path=`, instead of one listing
/// per component. `None` means "no answer" and the caller falls back to
/// [`walk_components`].
///
/// Not every path is eligible, and an ineligible one is `None` without any
/// request: a `//`-namespaced path (`//backups/<device>/...` is rooted at a
/// device, `//drive/...` deliberately means the walk), the root itself (which
/// costs no request at all to "resolve"), and anything under an active
/// workspace — these endpoints have no workspace-scoped variant (og exposes
/// none), so there they would answer about the personal drive, which is a
/// wrong item rather than a slow one.
///
/// The endpoints want an absolute path, and rebuilding it from [`components`]
/// normalizes away the leading, trailing and doubled slashes callers may pass.
/// A file's last component carries its extension (`/dir/notes.txt`), which is
/// exactly what the walk matches via [`file_display_name`], so the two agree
/// on the same spelling.
///
/// `None` also covers a definite 404, not just an unexpected failure: the walk
/// names the component that is actually missing (`No such folder 'b' at path:
/// /a/b/c`) and can tell "missing" from "that's a folder, not a file", neither
/// of which a whole-path 404 can express — and keeping those messages matters
/// more than saving requests on a lookup that is about to fail anyway. Handing
/// back on *every* unhappy answer also means a server-side change to these
/// endpoints can only ever cost speed, never correctness.
async fn resolve_via_meta(api: &DriveApi, token: &str, path: &str, expect: Expect) -> Option<Resolved> {
    if path.starts_with("//") || api.is_workspace() {
        return None;
    }
    let comps = components(path);
    if comps.is_empty() {
        return None;
    }
    let path = meta_path(&comps);
    // Folders first when either kind will do: the walk matches a subfolder
    // before a subfile at every component, so a name that exists as both has
    // to resolve to the folder — asking about the file first would silently
    // flip that precedence. It costs nothing either: a hit is one request
    // whichever kind matches and a miss two, both below the walk's one request
    // per component for any path deeper than a single name.
    if expect != Expect::File {
        match api.get_folder_by_path(token, &path).await {
            Ok(meta) if !meta.deleted && !meta.removed && !meta.uuid.is_empty() => {
                return Some(Resolved { uuid: meta.uuid, is_folder: true })
            }
            // Only a definite "no such folder" leaves room for the path to be
            // a file instead. Everything else — a transient failure, or a
            // `deleted`/`removed` record, which would be a trashed item the
            // walk (EXISTS entries only) doesn't consider reachable by path —
            // goes back to the walk.
            Err(e) if is_not_found(&e) => {}
            _ => return None,
        }
    }
    if expect == Expect::Folder {
        return None;
    }
    match api.get_file_by_path(token, &path).await {
        Ok(file) if !file.uuid.is_empty() => Some(Resolved { uuid: file.uuid, is_folder: false }),
        _ => None,
    }
}

/// Resolve `path` from the account/workspace root to a Drive item, in one
/// request where [`resolve_via_meta`] can and by walking the folder tree
/// otherwise. A leading `//` escapes into an explicit namespace instead — see
/// the module doc.
///
/// `root` must be the account's (or the active workspace's) root folder, since
/// that is what the single-request `?path=` lookup resolves against. To
/// resolve a path inside an arbitrary subtree — a backup device's folder —
/// use [`resolve_path_in_subtree`], which only ever walks.
pub async fn resolve_path(
    api: &DriveApi,
    token: &str,
    root: &str,
    path: &str,
    expect: Expect,
) -> Result<Resolved> {
    if let Some(hit) = resolve_via_meta(api, token, path, expect).await {
        return Ok(hit);
    }
    resolve_path_in_subtree(api, token, root, path, expect).await
}

/// The always-walk half of [`resolve_path`]: its fallback, and the entry point
/// for callers whose `root` is *not* the account/workspace root but an
/// arbitrary subtree (a backup device's folder). Such a root has to walk — the
/// `?path=` endpoints only understand paths from the account root, and would
/// happily answer about a same-named path over there instead.
pub async fn resolve_path_in_subtree(
    api: &DriveApi,
    token: &str,
    root: &str,
    path: &str,
    expect: Expect,
) -> Result<Resolved> {
    if let Some(rest) = path.strip_prefix("//") {
        return resolve_namespaced_path(api, token, root, rest, expect).await;
    }
    let comps = components(path);
    let display = if path.trim().is_empty() { "/" } else { path };
    walk_components(api, token, root, &comps, expect, display).await
}

/// Sentinel `uuid` for `//` smuggled through every serve backend's ordinary
/// `String` uuid field (inode tables, handle maps, ...) so none of them need
/// a parallel virtual-aware type — only `serve::tree` decodes it, right
/// before it would otherwise feed the string to a real API call. Encodes the
/// real account/workspace root uuid so the sentinel alone is enough to
/// produce a real `drive` child. A leading NUL makes collision with a real
/// uuid or plainName impossible.
const VIRTUAL_ROOT_PREFIX: &str = "\0virtual-root:";
/// Sentinel `uuid` for `//backups` — self-sufficient (its children, backup
/// devices, are looked up by account, not by anything embedded in the
/// string), so unlike `VIRTUAL_ROOT_PREFIX` this needs no payload.
pub const VIRTUAL_BACKUPS_UUID: &str = "\0virtual-backups";

/// Encode `real_root` (the account/workspace root) into a `//`-sentinel uuid.
pub fn encode_virtual_root(real_root: &str) -> String {
    format!("{VIRTUAL_ROOT_PREFIX}{real_root}")
}

/// `Some(real_root)` when `uuid` is a `//`-sentinel from [`encode_virtual_root`].
pub fn decode_virtual_root(uuid: &str) -> Option<&str> {
    uuid.strip_prefix(VIRTUAL_ROOT_PREFIX)
}

/// A synthetic (non-real, no Drive uuid) grouping folder. Only reachable via
/// [`resolve_path_or_virtual`] — every other path-resolving entry point
/// (`resolve_path`, `resolve_opt`, ...) keeps hard-erroring on `//` and
/// `//backups` bare, since move/mkdir/compare/upload-destination/etc. have
/// nothing sensible to do with a grouping that isn't a real folder.
#[derive(Clone, Debug)]
pub enum VirtualNode {
    /// `//` — children: `drive` (the given root) and, personal accounts
    /// only, `backups`.
    Root,
    /// `//backups` — children: one per backup device.
    BackupsRoot,
}

/// Either a real, uuid-backed resolution or a synthetic grouping.
pub enum PathTarget {
    Real(Resolved),
    Virtual(VirtualNode),
}

/// One child of a [`VirtualNode`]: either a real folder (`uuid` usable
/// anywhere a Drive folder id is), or another, nested virtual grouping.
pub enum VirtualEntry {
    Real { name: String, uuid: String },
    Nested { name: String, node: VirtualNode },
}

/// `Some` when `path` is exactly (after trimming) `//` or `//backups` — the
/// two virtual groupings — decided without any API call. `None` otherwise,
/// including a real `//backups/<device>` path (that resolves normally).
pub fn virtual_node_for(path: Option<&str>) -> Option<VirtualNode> {
    match path.map(str::trim) {
        Some("//") => Some(VirtualNode::Root),
        Some("//backups") => Some(VirtualNode::BackupsRoot),
        _ => None,
    }
}

/// List the immediate children of a virtual node — one level, like a normal
/// folder listing (a `Nested` entry is shown but not expanded; the caller
/// lists it again to go further, same as any other folder).
pub async fn virtual_entries(api: &DriveApi, token: &str, root: &str, node: &VirtualNode) -> Result<Vec<VirtualEntry>> {
    match node {
        VirtualNode::Root => {
            let mut out = vec![VirtualEntry::Real { name: "drive".to_string(), uuid: root.to_string() }];
            if !api.is_workspace() {
                out.push(VirtualEntry::Nested { name: "backups".to_string(), node: VirtualNode::BackupsRoot });
            }
            Ok(out)
        }
        VirtualNode::BackupsRoot => {
            if api.is_workspace() {
                return Err(anyhow!("Backups are personal-account only; not available in an active workspace"));
            }
            let devices = fetch_backup_devices(api, token).await?;
            Ok(devices
                .iter()
                .map(|d| VirtualEntry::Real { name: str_field(d, "plainName"), uuid: str_field(d, "uuid") })
                .collect())
        }
    }
}

/// Fully flatten a virtual node into `(local-subdir, uuid)` leaf pairs,
/// recursing through nested virtual groupings — for operations that
/// materialize a whole tree (download, sync) rather than list one level.
/// Nesting only ever goes one level deep today (`Root` -> `BackupsRoot`), so
/// this doesn't need to recurse past a `Nested` entry's own children.
pub async fn flatten_virtual(api: &DriveApi, token: &str, root: &str, node: &VirtualNode) -> Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    for entry in virtual_entries(api, token, root, node).await? {
        match entry {
            VirtualEntry::Real { name, uuid } => out.push((name, uuid)),
            VirtualEntry::Nested { name, node: nested } => {
                for inner in virtual_entries(api, token, root, &nested).await? {
                    if let VirtualEntry::Real { name: inner_name, uuid } = inner {
                        out.push((format!("{name}/{inner_name}"), uuid));
                    }
                }
            }
        }
    }
    Ok(out)
}

/// Like [`resolve_path`], but a bare `//` or `//backups` resolves to a
/// [`VirtualNode`] instead of erroring. Only for callers that can sensibly
/// act on a synthetic grouping (`list`, `download`, `sync down`, `serve`);
/// everything else should keep using `resolve_path`/`resolve_opt` so it
/// keeps hard-erroring there.
pub async fn resolve_path_or_virtual(
    api: &DriveApi,
    token: &str,
    root: &str,
    path: &str,
    expect: Expect,
) -> Result<PathTarget> {
    if let Some(rest) = path.strip_prefix("//") {
        let comps = components(rest);
        if comps.is_empty() {
            if expect == Expect::File {
                return Err(anyhow!("'//' is a virtual folder, not a file"));
            }
            return Ok(PathTarget::Virtual(VirtualNode::Root));
        }
        if comps.len() == 1 && comps[0] == "backups" {
            if expect == Expect::File {
                return Err(anyhow!("'//backups' is a virtual folder, not a file"));
            }
            if api.is_workspace() {
                return Err(anyhow!("Backups are personal-account only; not available in an active workspace"));
            }
            return Ok(PathTarget::Virtual(VirtualNode::BackupsRoot));
        }
    }
    resolve_path(api, token, root, path, expect).await.map(PathTarget::Real)
}

/// Build a `/a/b` folder path from an ancestors array (target first → root last):
/// drop the root entry, take `plainName`s, reverse to root-first order.
fn folder_path_from_ancestors(anc: &Value, root: &str) -> String {
    let arr = anc.as_array().cloned().unwrap_or_default();
    let mut names: Vec<String> = arr
        .iter()
        .filter(|e| str_field(e, "uuid") != root)
        .map(|e| str_field(e, "plainName"))
        .collect();
    names.reverse();
    if names.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", names.join("/"))
    }
}

/// Given a folder's ancestors array (target first → root last), check
/// whether a backup device appears in the chain; if so, return the
/// `//backups/<device>/...` path for the target instead of a root-relative
/// one. `None` when no device is found (or a workspace is active — backups
/// are personal-account only) so the caller falls back to
/// `folder_path_from_ancestors`.
async fn backups_relative_path(api: &DriveApi, token: &str, anc: &Value) -> Result<Option<String>> {
    if api.is_workspace() {
        return Ok(None);
    }
    let arr = anc.as_array().cloned().unwrap_or_default();
    let devices = fetch_backup_devices(api, token).await?;
    let Some(idx) = arr.iter().position(|e| {
        let uuid = str_field(e, "uuid");
        devices.iter().any(|d| str_field(d, "uuid") == uuid)
    }) else {
        return Ok(None);
    };
    let device_uuid = str_field(&arr[idx], "uuid");
    let device = devices.iter().find(|d| str_field(d, "uuid") == device_uuid).unwrap();
    // Entries above the device (target + intermediate ancestors, device excluded), root-first.
    let mut names: Vec<String> = arr[..idx].iter().map(|e| str_field(e, "plainName")).collect();
    names.reverse();
    let mut path = format!("//backups/{}", str_field(device, "plainName"));
    for n in names {
        path.push('/');
        path.push_str(&n);
    }
    Ok(Some(path))
}

/// Reconstruct the full path of an item (file or folder) from its uuid.
/// Returns `(path, is_folder)`. Prints `//backups/<device>/...` when the
/// item lives under a backup device instead of the account/workspace root.
pub async fn path_from_id(
    api: &DriveApi,
    token: &str,
    root: &str,
    id: &str,
) -> Result<(String, bool)> {
    // A folder? `/folders/{uuid}/meta` 404s for a file uuid, so success ⇒ folder.
    if let Ok(meta) = api.get_folder_meta(token, id).await {
        if !str_field(&meta, "uuid").is_empty() {
            if id == root {
                return Ok(("/".to_string(), true));
            }
            let anc = api.get_folder_ancestors(token, id).await?;
            if let Some(p) = backups_relative_path(api, token, &anc).await? {
                return Ok((p, true));
            }
            return Ok((folder_path_from_ancestors(&anc, root), true));
        }
    }
    // Otherwise a file: its dir is the ancestors of its parent folder.
    let fmeta = api
        .get_file_meta_value(token, id)
        .await
        .map_err(|_| anyhow!("No file or folder found with id: {id}"))?;
    let folder_uuid = str_field(&fmeta, "folderUuid");
    let name = file_display_name(&fmeta);
    let dir = if folder_uuid.is_empty() || folder_uuid == root {
        "/".to_string()
    } else {
        let anc = api.get_folder_ancestors(token, &folder_uuid).await?;
        match backups_relative_path(api, token, &anc).await? {
            Some(p) => p,
            None => folder_path_from_ancestors(&anc, root),
        }
    };
    let full = if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    };
    Ok((full, false))
}

/// Validate a value that is meant to be an opaque Drive item *name* (as opposed to
/// a `/a/b` Drive *path*, which is what `resolve_path`/`components` above parse).
/// Names must not contain `/`: Drive paths use it as the component separator (see
/// `components`), so a name containing `/` is ambiguous with path syntax. It's also
/// the character `std::path::Path::file_stem`/`.extension()` split on — callers that
/// derive a stored name from user input via those (filesystem-path) parsers must
/// call this first, or a name like `"a/b"` silently gets truncated down to `"b"`
/// while still reporting success as if the exact requested name was stored.
pub fn validate_name(name: &str) -> Result<()> {
    if name.contains('/') {
        return Err(anyhow!(
            "Name '{name}' must not contain '/' (that's a path separator, not a valid Drive name)"
        ));
    }
    Ok(())
}

/// `true` if `s` has the shape of a Drive uuid (8-4-4-4-12 hex).
///
/// Used by the commands that take one positional argument meaning *either* a
/// uuid or a Drive path, to decide which it is. A path can always be forced by
/// writing it with a leading `/` — no path with one is uuid-shaped.
pub fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// Split a positional "uuid or path" argument into the `(id, path)` pair
/// [`resolve_opt`] takes: a uuid-shaped value is an id, anything else a path.
pub fn split_id_or_path(arg: Option<&str>) -> (Option<&str>, Option<&str>) {
    match arg {
        Some(a) if looks_like_uuid(a.trim()) => (Some(a), None),
        other => (None, other),
    }
}

/// A file *or* folder the caller resolved from one positional argument, for the
/// endpoints that take the kind in the route (`/sharings/{item_type}/...`,
/// `/favorites/{item_type}/...`) and therefore have to know which it is.
pub struct ItemTarget {
    pub uuid: String,
    /// `"file"` or `"folder"`.
    pub item_type: String,
    /// What the user typed, for messages — a path reads better than a uuid.
    pub label: String,
}

/// Nothing in a uuid says whether it points at a file or a folder, and the
/// routes above need to know. Probe it the way `path-from-id` does:
/// `/folders/{uuid}/meta` 404s for a file uuid, so success ⇒ folder.
pub async fn type_of_uuid(api: &DriveApi, token: &str, uuid: &str) -> Result<String> {
    if let Ok(meta) = api.get_folder_meta(token, uuid).await
        && !str_field(&meta, "uuid").is_empty()
    {
        return Ok("folder".to_string());
    }
    api.get_file_meta_value(token, uuid)
        .await
        .map(|_| "file".to_string())
        .map_err(|_| anyhow!("No file or folder found with id: {uuid}"))
}

/// Resolve a positional `<ITEM>` — a Drive path or a bare uuid — to the
/// `(uuid, item_type)` pair those routes take. `forced` is the caller's
/// optional `--type`, which also saves the [`type_of_uuid`] probe.
pub async fn resolve_item(
    api: &DriveApi,
    token: &str,
    root: &str,
    item: &str,
    forced: Option<&str>,
) -> Result<ItemTarget> {
    let item = item.trim();
    if item.is_empty() {
        return Err(anyhow!("No file or folder given."));
    }
    if looks_like_uuid(item) {
        let item_type = match forced {
            Some(t) => t.to_string(),
            None => type_of_uuid(api, token, item).await?,
        };
        return Ok(ItemTarget {
            uuid: item.to_string(),
            item_type,
            label: item.to_string(),
        });
    }
    let expect = match forced {
        Some("file") => Expect::File,
        Some("folder") => Expect::Folder,
        _ => Expect::Any,
    };
    let resolved = resolve_path(api, token, root, item, expect).await?;
    Ok(ItemTarget {
        item_type: if resolved.is_folder { "folder" } else { "file" }.to_string(),
        uuid: resolved.uuid,
        label: item.to_string(),
    })
}

/// Resolve the mutually-exclusive `--id` / `--path` options to a uuid. `None`
/// only when both are absent (the caller decides: root default, or required).
///
/// Note: a `Some("")` or `Some("   ")` value is treated the same as `None` here
/// (both fall through to "absent"). That's the right behavior for *source*
/// selection (e.g. `list`, `download`, `create-folder`'s parent) where "not
/// provided" legitimately means "use the default root" and an accidentally
/// blank string is harmless. It is the *wrong* behavior for a *destination*
/// pair on a mutating command, where silently defaulting to the Drive root
/// is surprising and potentially destructive — use `resolve_destination_opt`
/// for those call sites instead.
pub async fn resolve_opt(
    api: &DriveApi,
    token: &str,
    root: &str,
    id: Option<&str>,
    path: Option<&str>,
    expect: Expect,
) -> Result<Option<String>> {
    let id = id.filter(|s| !s.trim().is_empty());
    let path = path.filter(|s| !s.trim().is_empty());
    match (id, path) {
        (Some(i), None) => Ok(Some(i.trim().to_string())),
        (None, Some(p)) => Ok(Some(resolve_path(api, token, root, p, expect).await?.uuid)),
        (Some(_), Some(_)) => Err(anyhow!("Provide either an id or a path, not both")),
        (None, None) => Ok(None),
    }
}

/// `true` if the flag was explicitly passed (`Some`) but its value is empty or
/// whitespace-only after trimming. `None` (flag never passed at all) is not
/// blank — clap gives `None` only when the argument was omitted entirely, so
/// this is exactly the distinction erased by `resolve_opt`'s `.filter()`.
fn is_blank_but_provided(v: Option<&str>) -> bool {
    matches!(v, Some(s) if s.trim().is_empty())
}

/// Like [`resolve_opt`], but for a *destination* `--id`/`--path` pair on a
/// mutating command (move/upload/trash-restore): an explicitly-provided but
/// empty or whitespace-only value is a hard error rather than being silently
/// treated as "not provided". Without this guard, `--dest-path ""` (e.g. from
/// an unset shell variable interpolated into the flag) is indistinguishable
/// from omitting the flag entirely, and callers then default to the Drive
/// account root — moving/uploading into root with no warning, which is
/// exactly the kind of silent, surprising, destructive behavior a destination
/// argument must never have.
pub async fn resolve_destination_opt(
    api: &DriveApi,
    token: &str,
    root: &str,
    id: Option<&str>,
    path: Option<&str>,
    expect: Expect,
) -> Result<Option<String>> {
    if is_blank_but_provided(id) {
        return Err(anyhow!("--destination was provided but is empty"));
    }
    if is_blank_but_provided(path) {
        return Err(anyhow!("--dest-path was provided but is empty"));
    }
    resolve_opt(api, token, root, id, path, expect).await
}

// ---- commands ----

/// `get-id`: print the uuid of the item at `path`.
pub async fn cmd_id_from_path(path: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let item = resolve_path(&api, &creds.token, creds.root_folder(), path, Expect::Any).await?;
    let kind = if item.is_folder { "folder" } else { "file" };
    output::emit(
        &item.uuid,
        json!({ "success": true, "uuid": item.uuid, "isFolder": item.is_folder, "type": kind }),
    );
    Ok(())
}

/// `get-path`: print the full path of the item with uuid `id`.
pub async fn cmd_path_from_id(id: &str) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::for_credentials(&creds);
    let (path, is_folder) = path_from_id(&api, &creds.token, creds.root_folder(), id).await?;
    let kind = if is_folder { "folder" } else { "file" };
    output::emit(
        &path,
        json!({ "success": true, "path": path, "isFolder": is_folder, "type": kind }),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_blank_but_provided_true_for_empty_and_whitespace() {
        assert!(is_blank_but_provided(Some("")));
        assert!(is_blank_but_provided(Some("   ")));
        assert!(is_blank_but_provided(Some("\t\n ")));
    }

    #[test]
    fn is_blank_but_provided_false_for_none_and_real_values() {
        assert!(!is_blank_but_provided(None));
        assert!(!is_blank_but_provided(Some("abc")));
        assert!(!is_blank_but_provided(Some("  x  ")));
        assert!(!is_blank_but_provided(Some("/a/b")));
    }

    #[test]
    fn meta_path_normalizes_what_components_tolerates() {
        // Leading, trailing and doubled slashes are all fine on input; the
        // endpoint gets one canonical absolute path either way.
        assert_eq!(meta_path(&components("/a/b")), "/a/b");
        assert_eq!(meta_path(&components("a/b")), "/a/b");
        assert_eq!(meta_path(&components("/a//b/")), "/a/b");
        // A file keeps its extension — the same spelling `file_display_name`
        // builds for the walk to match on.
        assert_eq!(meta_path(&components("/dir/notes.txt")), "/dir/notes.txt");
    }

    #[test]
    fn is_not_found_only_matches_a_404() {
        assert!(is_not_found(&anyhow!("getFolderByPath failed: HTTP 404 Not Found: {{}}")));
        assert!(!is_not_found(&anyhow!("getFolderByPath failed: HTTP 502 Bad Gateway: {{}}")));
        assert!(!is_not_found(&anyhow!("error decoding response body")));
    }

    #[test]
    fn validate_name_rejects_a_slash() {
        let err = validate_name("a/b").unwrap_err();
        assert!(err.to_string().contains('/'));
    }

    #[test]
    fn validate_name_rejects_a_slash_anywhere_in_the_string() {
        assert!(validate_name("/leading").is_err());
        assert!(validate_name("trailing/").is_err());
        assert!(validate_name("mid/dle").is_err());
    }

    #[test]
    fn looks_like_uuid_only_matches_the_canonical_shape() {
        assert!(looks_like_uuid("00000000-0000-0000-0000-000000000000"));
        assert!(looks_like_uuid("A0B1C2D3-0000-4000-8000-000000000000"));
        // A leading slash is how a path that would otherwise be uuid-shaped is
        // forced to stay a path.
        assert!(!looks_like_uuid("/00000000-0000-0000-0000-000000000000"));
        assert!(!looks_like_uuid("/Docs/report.pdf"));
        assert!(!looks_like_uuid("report.pdf"));
        assert!(!looks_like_uuid("zzzzzzzz-0000-0000-0000-000000000000"));
        assert!(!looks_like_uuid("00000000-0000-0000-0000-0000000000000"));
    }

    #[test]
    fn split_id_or_path_routes_each_form_to_the_right_slot() {
        assert_eq!(
            split_id_or_path(Some("00000000-0000-0000-0000-000000000000")),
            (Some("00000000-0000-0000-0000-000000000000"), None)
        );
        assert_eq!(split_id_or_path(Some("/a/b")), (None, Some("/a/b")));
        assert_eq!(split_id_or_path(None), (None, None));
        // Surrounding whitespace doesn't change what the value *is*; the
        // untrimmed original is passed on, since the resolver trims too.
        assert_eq!(
            split_id_or_path(Some(" 00000000-0000-0000-0000-000000000000 ")),
            (Some(" 00000000-0000-0000-0000-000000000000 "), None)
        );
    }

    #[test]
    fn validate_name_accepts_ordinary_names() {
        assert!(validate_name("report.pdf").is_ok());
        assert!(validate_name("a b (1).txt").is_ok());
        // Not a path separator on this (Linux-first) codebase's path model — see
        // `components`, which only splits on `/` — so a backslash is left as an
        // ordinary, legal character in an opaque Drive name.
        assert!(validate_name("weird\\name").is_ok());
    }
}

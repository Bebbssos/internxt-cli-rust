//! Path <-> uuid resolution for Drive items.
//!
//! Turns a slash path like `/a/b/file.txt` into a Drive uuid by walking the
//! folder tree from the account/workspace root (workspace-aware, like
//! `serve::tree` but cache-free and available without the serve features), and
//! turns a uuid back into a path via the folders `ancestors` endpoint.
//!
//! Only *live* (non-trashed) items are reachable by path — the walk uses the
//! same paginated `subfolders`/`subfiles` listings the rest of the CLI uses.

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

/// Resolve `path` (relative to `root`) to a Drive item, walking the folder tree.
pub async fn resolve_path(
    api: &DriveApi,
    token: &str,
    root: &str,
    path: &str,
    expect: Expect,
) -> Result<Resolved> {
    let comps = components(path);
    if comps.is_empty() {
        if expect == Expect::File {
            return Err(anyhow!("Path '/' is the root folder, not a file"));
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
                    return Err(anyhow!("'{path}' is a folder, not a file"));
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
        return Err(anyhow!("No such {what} '{comp}' at path: {path}"));
    }
    unreachable!()
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

/// Reconstruct the full path of an item (file or folder) from its uuid.
/// Returns `(path, is_folder)`.
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
        folder_path_from_ancestors(&anc, root)
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
    fn validate_name_accepts_ordinary_names() {
        assert!(validate_name("report.pdf").is_ok());
        assert!(validate_name("a b (1).txt").is_ok());
        // Not a path separator on this (Linux-first) codebase's path model — see
        // `components`, which only splits on `/` — so a backslash is left as an
        // ordinary, legal character in an opaque Drive name.
        assert!(validate_name("weird\\name").is_ok());
    }
}

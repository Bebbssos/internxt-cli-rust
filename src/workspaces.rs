//! Workspace commands: list, use, unset. Mirrors og/cli workspaces-* commands
//! and workspace.service. Decrypts each workspace mnemonic with the user's
//! ecc/kyber private keys (see [`internxt_core::crypto::decrypt_workspace_key`]).
//!
//! On top of those three, `info`/`members`/`teams`/`usage`/`invitations` are
//! read-only administration views with no official CLI equivalent (og only
//! exposes them in drive-web). See the "workspace administration" section
//! below for why their rendering is deliberately defensive.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use internxt_core::api::DriveApi;
use crate::auth;
use internxt_core::crypto;
use crate::drive_ops::{format_date, human_file_size, print_table};
use internxt_core::models::{Credentials, WorkspaceContext};
use crate::output;

/// `availableWorkspaces` entries from GET /workspaces/.
pub(crate) fn available_workspaces(resp: &Value) -> Vec<Value> {
    resp["availableWorkspaces"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

fn used_space(ws: &Value) -> f64 {
    let drive = ws["workspaceUser"]["driveUsage"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    let backups = ws["workspaceUser"]["backupsUsage"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    drive + backups
}

fn space_limit(ws: &Value) -> f64 {
    ws["workspaceUser"]["spaceLimit"]
        .as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0)
}

pub async fn list(extended: bool) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let resp = DriveApi::new().get_workspaces(&creds.token).await?;
    let workspaces = available_workspaces(&resp);

    if !output::is_json() {
        let mut rows = Vec::new();
        for ws in &workspaces {
            let name = ws["workspace"]["name"].as_str().unwrap_or("").to_string();
            let id = ws["workspace"]["id"].as_str().unwrap_or("").to_string();
            let used = human_file_size(used_space(ws));
            let avail = human_file_size(space_limit(ws));
            let mut row = vec![name, id, used, avail];
            if extended {
                row.push(ws["workspace"]["ownerId"].as_str().unwrap_or("").to_string());
                row.push(ws["workspace"]["address"].as_str().unwrap_or("").to_string());
                row.push(format_date(ws["workspace"]["createdAt"].as_str().unwrap_or("")));
            }
            rows.push(row);
        }
        let mut headers = vec!["Name", "Workspace ID", "Used space", "Available space"];
        if extended {
            headers.push("Owner ID");
            headers.push("Address");
            headers.push("Created at");
        }
        if rows.is_empty() {
            output::status("No workspaces found.");
        } else {
            print_table(&headers, &rows);
        }
    }
    output::emit("", json!({ "success": true, "list": { "workspaces": workspaces } }));
    Ok(())
}

/// Build the active-workspace context for `workspace_id`: fetch credentials and
/// decrypt the workspace mnemonic with the user's keys.
pub(crate) async fn build_context(
    creds: &Credentials,
    workspaces: &[Value],
    workspace_id: &str,
) -> Result<WorkspaceContext> {
    let selected = workspaces
        .iter()
        .find(|w| w["workspace"]["id"].as_str() == Some(workspace_id))
        .ok_or_else(|| anyhow!("Workspace {workspace_id} not found."))?;

    let ecc = creds.user.ecc_private_key.as_deref().ok_or_else(|| {
        anyhow!("Your stored credentials have no private keys; run `ixr login` again to enable workspaces.")
    })?;
    let encrypted_mnemonic = selected["workspaceUser"]["key"]
        .as_str()
        .ok_or_else(|| anyhow!("workspace has no encryption key"))?;
    let mnemonic = crypto::decrypt_workspace_key(
        encrypted_mnemonic,
        ecc,
        creds.user.kyber_private_key.as_deref(),
    )
    .map_err(|e| anyhow!("Failed to decrypt workspace mnemonic: {e}"))?;
    if !crypto::validate_mnemonic(&mnemonic) {
        return Err(anyhow!("Decrypted workspace mnemonic is invalid."));
    }

    let cred = DriveApi::new()
        .get_workspace_credentials(&creds.token, workspace_id)
        .await?;

    Ok(WorkspaceContext {
        id: workspace_id.to_string(),
        name: selected["workspace"]["name"].as_str().unwrap_or("").to_string(),
        token: cred["tokenHeader"]
            .as_str()
            .ok_or_else(|| anyhow!("no tokenHeader in workspace credentials"))?
            .to_string(),
        bucket: cred["bucket"].as_str().unwrap_or("").to_string(),
        network_user: cred["credentials"]["networkUser"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        network_pass: cred["credentials"]["networkPass"]
            .as_str()
            .unwrap_or("")
            .to_string(),
        mnemonic,
        root_folder_id: selected["workspaceUser"]["rootFolderId"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    })
}

/// Resolve what the user typed to a workspace id. Accepts the workspace uuid,
/// its name (case-insensitive, and only when unambiguous), or the 1-based index
/// of the row `workspaces list` printed. Every command that takes a workspace
/// argument goes through here so they all accept the same three forms.
pub(crate) fn resolve_workspace_id(workspaces: &[Value], selector: &str) -> Result<String> {
    let selector = selector.trim();
    let id_of = |ws: &Value| ws["workspace"]["id"].as_str().unwrap_or("").to_string();

    if workspaces.is_empty() {
        return Err(anyhow!("You have no workspaces."));
    }
    if let Some(ws) = workspaces.iter().find(|w| id_of(w) == selector) {
        return Ok(id_of(ws));
    }
    if let Ok(index) = selector.parse::<usize>() {
        return match index.checked_sub(1).and_then(|i| workspaces.get(i)) {
            Some(ws) => Ok(id_of(ws)),
            None => Err(anyhow!(
                "No workspace #{index}; you have {}. Run `ixr workspaces list`.",
                workspaces.len()
            )),
        };
    }
    let by_name: Vec<&Value> = workspaces
        .iter()
        .filter(|w| {
            w["workspace"]["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(selector))
        })
        .collect();
    match by_name.as_slice() {
        [ws] => Ok(id_of(ws)),
        [] => Err(anyhow!(
            "Workspace '{selector}' not found. Pass an id, a name or a number from `ixr workspaces list`."
        )),
        _ => Err(anyhow!(
            "'{selector}' matches {} workspaces; pass the id instead.",
            by_name.len()
        )),
    }
}

pub async fn use_workspace(id: Option<&str>, personal: bool) -> Result<()> {
    if personal {
        return unset().await;
    }
    let mut creds = auth::get_auth_details().await?;
    let resp = DriveApi::new().get_workspaces(&creds.token).await?;
    let workspaces = available_workspaces(&resp);

    let workspace_id = match id {
        Some(i) if !i.trim().is_empty() => resolve_workspace_id(&workspaces, i)?,
        _ => select_workspace_id(&workspaces)?,
    };

    let context = build_context(&creds, &workspaces, &workspace_id).await?;
    let summary = json!({
        "id": context.id,
        "name": context.name,
        "bucket": context.bucket,
        "rootFolderId": context.root_folder_id,
    });
    creds.workspace = Some(context);
    auth::save_credentials(&creds)?;

    output::emit(
        &format!(
            "✓ Workspace {workspace_id} selected. All subsequent commands operate within this workspace until changed or unset."
        ),
        json!({ "success": true, "workspace": summary }),
    );
    Ok(())
}

/// Interactive workspace picker (errors in json / non-interactive mode).
fn select_workspace_id(workspaces: &[Value]) -> Result<String> {
    if output::is_json() || output::is_non_interactive() {
        return Err(anyhow!(
            "No value provided for required flag: id (use `workspaces list` to view ids)."
        ));
    }
    if workspaces.is_empty() {
        return Err(anyhow!("You have no workspaces."));
    }
    use std::io::Write;
    println!("Available workspaces:");
    for (i, ws) in workspaces.iter().enumerate() {
        let name = ws["workspace"]["name"].as_str().unwrap_or("");
        let id = ws["workspace"]["id"].as_str().unwrap_or("");
        println!("  [{}] {name} ({id})", i + 1);
    }
    print!("Which workspace do you want to use? (number) ");
    std::io::stdout().flush().ok();
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    let idx: usize = s
        .trim()
        .parse()
        .map_err(|_| anyhow!("Invalid selection."))?;
    let ws = workspaces
        .get(idx.wrapping_sub(1))
        .ok_or_else(|| anyhow!("Selection out of range."))?;
    ws["workspace"]["id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("Invalid workspace."))
}

pub async fn unset() -> Result<()> {
    let mut creds = auth::get_auth_details().await?;
    creds.workspace = None;
    auth::save_credentials(&creds)?;
    output::emit(
        "✓ Personal drive space selected successfully.",
        json!({ "success": true, "message": "Personal drive space selected successfully." }),
    );
    Ok(())
}

// ---- workspace administration (read-only, beyond og) ----
//
// The endpoints behind these views hand back a raw `Value` from core on
// purpose: the account they could be probed with belongs to no workspace at
// all, so the only responses ever seen were empty. The field names used below
// therefore come from og's SDK types (`og/sdk/src/workspaces/types.ts`) and
// drive-web's admin screens, not from a payload we watched arrive. Everything
// here is consequently written to degrade rather than guess: a missing field
// renders as `-`, and a response that doesn't look like the expected shape is
// printed verbatim instead of being silently reduced to an empty table.
// `--json` always passes the response straight through, so a populated
// workspace stays inspectable even if the human view turns out to be wrong.
//
// What was confirmed live: every route here answers (`/workspaces/{id}`,
// `/members`, `/teams`, `/usage` all return a 404 "Workspace not found" for an
// id the caller isn't a member of, rather than a routing error), and
// `/workspaces/invitations` and `/workspaces/pending-setup` answer `[]` on an
// account with none. What was never seen is a populated body.

/// A string field, or "" when it's absent or not a string.
fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// A byte count. og is inconsistent about these — `WorkspaceUser` carries them
/// as decimal strings, `WorkspaceUsage` as numbers — so accept either.
fn bytes(v: &Value, key: &str) -> Option<f64> {
    match v.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse::<f64>().ok(),
        _ => None,
    }
}

/// Human-readable size for a byte count, or "-" when the field isn't there.
fn size_cell(v: &Value, key: &str) -> String {
    bytes(v, key)
        .map(human_file_size)
        .unwrap_or_else(|| "-".to_string())
}

/// The array inside `value`: the value itself when it already is one, else the
/// `key` property of an object wrapping it. Empty when it's neither.
fn as_list(value: &Value, key: &str) -> Vec<Value> {
    value
        .as_array()
        .or_else(|| value.get(key).and_then(Value::as_array))
        .cloned()
        .unwrap_or_default()
}

/// A response we couldn't map onto the expected shape — print it as-is instead
/// of dropping it, so an unforeseen payload stays readable without `--json`.
fn print_raw(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
    );
}

/// A `field: value` block, aligned like the one `usage` prints.
fn print_fields(fields: &[(&str, String)]) {
    let width = fields.iter().map(|(k, _)| k.len()).max().unwrap_or(0) + 2;
    for (key, value) in fields {
        println!("{:<width$}{value}", format!("{key}:"), width = width);
    }
}

/// A `8-4-4-4-12` hex id. Used to decide whether a selector that matched no
/// workspace is still worth handing to the server (see `target_workspace`).
fn looks_like_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(len, g)| g.len() == *len && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The workspace an admin view targets: the selector when one was given, else
/// the active workspace (`workspaces use`, or `IXR_WORKSPACE_ID` for this
/// invocation only — `get_auth_details` has already applied it to `creds`).
/// Returns the id together with the list it was resolved against.
///
/// A uuid that isn't in that list is passed through to the server rather than
/// rejected here: a workspace you own but haven't set up yet is reported under
/// `pendingWorkspaces`, not `availableWorkspaces`, so `info` can print its id
/// while resolution can't see it. Anything else has to resolve locally.
async fn target_workspace(
    creds: &Credentials,
    selector: Option<&str>,
) -> Result<(String, Vec<Value>)> {
    let resp = DriveApi::new().get_workspaces(&creds.token).await?;
    let workspaces = available_workspaces(&resp);
    let id = match selector {
        Some(s) if !s.trim().is_empty() => match resolve_workspace_id(&workspaces, s) {
            Ok(id) => id,
            Err(_) if looks_like_uuid(s.trim()) => s.trim().to_string(),
            Err(e) => return Err(e),
        },
        _ => match creds.workspace.as_ref().map(|w| w.id.clone()) {
            Some(id) => id,
            None if workspaces.is_empty() => return Err(anyhow!("You have no workspaces.")),
            None => {
                return Err(anyhow!(
                    "No workspace given and none active. Pass a workspace (id, name, or the number from `ixr workspaces list`), or run `ixr workspaces use` first."
                ))
            }
        },
    };
    Ok((id, workspaces))
}

/// The `availableWorkspaces` entry for `id`, when the caller is a member of it.
fn entry_for<'a>(workspaces: &'a [Value], id: &str) -> Option<&'a Value> {
    workspaces
        .iter()
        .find(|w| w["workspace"]["id"].as_str() == Some(id))
}

/// `workspaces info` — the single-workspace record, plus any workspace the
/// caller owns that still needs setting up.
pub async fn info(selector: Option<&str>) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let api = DriveApi::new();
    let (id, workspaces) = target_workspace(&creds, selector).await?;

    let pending = api
        .get_pending_setup_workspaces(&creds.token)
        .await
        .unwrap_or(Value::Null);
    let pending_list = as_list(&pending, "pendingWorkspaces");

    // `GET /workspaces/{id}` has no counterpart in og's SDK — drive-web reads
    // the record out of `GET /workspaces/` instead — so it may not exist
    // server-side at all. Fall back to whichever list the workspace shows up
    // in rather than failing.
    let (record, from_list) = match api.get_workspace(&creds.token, &id).await {
        Ok(v) if v.is_object() => (v, false),
        _ => {
            let listed = entry_for(&workspaces, &id)
                .map(|entry| entry["workspace"].clone())
                .or_else(|| {
                    pending_list
                        .iter()
                        .find(|w| w.get("id").and_then(Value::as_str) == Some(id.as_str()))
                        .cloned()
                });
            match listed {
                Some(record) => (record, true),
                None => return Err(anyhow!("Workspace {id} not found.")),
            }
        }
    };

    if !output::is_json() {
        // Some routes nest the record under a wrapper; unwrap when the top
        // level clearly isn't the workspace itself.
        let ws = record
            .get("workspace")
            .filter(|w| w.is_object())
            .unwrap_or(&record);
        if ws.is_object() {
            let mut fields = vec![("Name", text(ws, "name")), ("Workspace ID", text(ws, "id"))];
            for (label, key) in [
                ("Description", "description"),
                ("Address", "address"),
                ("Owner ID", "ownerId"),
                ("Default team ID", "defaultTeamId"),
                ("Root folder ID", "rootFolderId"),
            ] {
                let value = text(ws, key);
                if !value.is_empty() {
                    fields.push((label, value));
                }
            }
            if let Some(done) = ws.get("setupCompleted").and_then(Value::as_bool) {
                fields.push((
                    "Setup completed",
                    if done { "yes" } else { "no" }.to_string(),
                ));
            }
            for (label, key) in [("Created at", "createdAt"), ("Updated at", "updatedAt")] {
                let value = text(ws, key);
                if !value.is_empty() {
                    fields.push((label, format_date(&value)));
                }
            }
            print_fields(&fields);
            if from_list {
                output::status(
                    "\n(taken from `workspaces list` — this server has no single-workspace endpoint)",
                );
            }
        } else {
            print_raw(&record);
        }

        if !pending_list.is_empty() {
            println!("\nWorkspaces awaiting setup:");
            let rows: Vec<Vec<String>> = pending_list
                .iter()
                .map(|w| {
                    vec![
                        text(w, "name"),
                        text(w, "id"),
                        format_date(&text(w, "createdAt")),
                    ]
                })
                .collect();
            print_table(&["Name", "Workspace ID", "Created at"], &rows);
        }
    }
    output::emit(
        "",
        json!({
            "success": true,
            "workspaceId": id,
            "workspace": record,
            "pendingSetup": pending,
        }),
    );
    Ok(())
}

/// Display name for a member: "name lastname", falling back to the account's
/// username and finally its email.
fn member_name(member: &Value) -> String {
    let full = format!("{} {}", text(member, "name"), text(member, "lastname"));
    let full = full.trim().to_string();
    if !full.is_empty() {
        return full;
    }
    let username = text(member, "username");
    if username.is_empty() {
        text(member, "email")
    } else {
        username
    }
}

/// One row of the members table. `user` is a `WorkspaceUser` — the membership
/// record — with the account it points at nested under `member`.
fn member_row(user: &Value) -> Vec<String> {
    let member = user.get("member").cloned().unwrap_or(Value::Null);
    let role = if user.get("isOwner").and_then(Value::as_bool) == Some(true) {
        "owner"
    } else if user.get("isManager").and_then(Value::as_bool) == Some(true) {
        "manager"
    } else {
        "member"
    };
    let used = bytes(user, "driveUsage").unwrap_or(0.0) + bytes(user, "backupsUsage").unwrap_or(0.0);
    vec![
        member_name(&member),
        text(&member, "email"),
        role.to_string(),
        human_file_size(used),
        size_cell(user, "spaceLimit"),
        text(user, "memberId"),
    ]
}

/// `workspaces members` — who belongs to the workspace.
pub async fn members(selector: Option<&str>) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let (id, _) = target_workspace(&creds, selector).await?;
    let resp = DriveApi::new()
        .get_workspace_members(&creds.token, &id)
        .await?;

    if !output::is_json() {
        // og splits members into activated and disabled; an array would be a
        // different shape than documented, but is still renderable as one group.
        let groups: Vec<(&str, Vec<Value>)> =
            if resp.get("activatedUsers").is_some() || resp.get("disabledUsers").is_some() {
                vec![
                    ("Active members", as_list(&resp, "activatedUsers")),
                    ("Deactivated members", as_list(&resp, "disabledUsers")),
                ]
            } else if resp.is_array() {
                vec![("", as_list(&resp, ""))]
            } else {
                Vec::new()
            };

        if groups.is_empty() {
            print_raw(&resp);
        } else if groups.iter().all(|(_, g)| g.is_empty()) {
            output::status("No members found.");
        } else {
            let headers = [
                "Name",
                "Email",
                "Role",
                "Used space",
                "Space limit",
                "Member ID",
            ];
            for (i, (label, group)) in groups.iter().filter(|(_, g)| !g.is_empty()).enumerate() {
                if i > 0 {
                    println!();
                }
                if !label.is_empty() {
                    println!("{label}:");
                }
                let rows: Vec<Vec<String>> = group.iter().map(member_row).collect();
                print_table(&headers, &rows);
            }
        }
    }
    output::emit(
        "",
        json!({ "success": true, "workspaceId": id, "members": resp }),
    );
    Ok(())
}

/// `workspaces teams` — the teams defined inside the workspace.
pub async fn teams(selector: Option<&str>) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let (id, _) = target_workspace(&creds, selector).await?;
    let resp = DriveApi::new().get_workspace_teams(&creds.token, &id).await?;
    let list = as_list(&resp, "teams");

    if !output::is_json() {
        if list.is_empty() {
            if resp.is_array() || resp.is_null() {
                output::status("No teams found.");
            } else {
                print_raw(&resp);
            }
        } else {
            // og wraps each team as `{ membersCount, team }`; tolerate a bare
            // team object too.
            let rows: Vec<Vec<String>> = list
                .iter()
                .map(|entry| {
                    let team = entry.get("team").filter(|t| t.is_object()).unwrap_or(entry);
                    let count = entry
                        .get("membersCount")
                        .and_then(Value::as_u64)
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "-".to_string());
                    vec![
                        text(team, "name"),
                        text(team, "id"),
                        count,
                        text(team, "managerId"),
                        format_date(&text(team, "createdAt")),
                    ]
                })
                .collect();
            print_table(
                &["Name", "Team ID", "Members", "Manager ID", "Created at"],
                &rows,
            );
        }
    }
    output::emit(
        "",
        json!({ "success": true, "workspaceId": id, "teams": resp }),
    );
    Ok(())
}

/// `workspaces usage` — the workspace's space totals plus the caller's own
/// slice of them. There is no per-member lookup here: og's
/// `GET /workspaces/{id}/usage/member` reports whoever is asking, and the
/// remaining members' quotas come from `workspaces members`.
pub async fn usage(selector: Option<&str>) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let (id, workspaces) = target_workspace(&creds, selector).await?;
    let api = DriveApi::new();
    let (total, member) = tokio::join!(
        api.get_workspace_usage(&creds.token, &id),
        api.get_workspace_member_usage(&creds.token, &id),
    );
    let total = total?;
    // Best-effort: an owner who isn't a member has no member usage, and the
    // route is unverified — fall back to the membership record from the list.
    let member = member.ok().filter(Value::is_object);

    if !output::is_json() {
        if total.is_object() {
            let capacity = bytes(&total, "totalWorkspaceSpace");
            let assigned = bytes(&total, "spaceAssigned");
            let used = bytes(&total, "spaceUsed");
            let percent_of = |value: Option<f64>, whole: Option<f64>, fallback: String| match (
                value, whole,
            ) {
                (Some(v), Some(w)) if w > 0.0 => {
                    format!("{} ({:.1}%)", human_file_size(v), v / w * 100.0)
                }
                _ => fallback,
            };
            print_fields(&[
                ("Total space", size_cell(&total, "totalWorkspaceSpace")),
                (
                    "Assigned",
                    percent_of(assigned, capacity, size_cell(&total, "spaceAssigned")),
                ),
                (
                    "Used",
                    percent_of(used, assigned, size_cell(&total, "spaceUsed")),
                ),
            ]);
        } else {
            print_raw(&total);
        }

        let own = member
            .clone()
            .or_else(|| entry_for(&workspaces, &id).map(|e| e["workspaceUser"].clone()))
            .filter(Value::is_object);
        if let Some(own) = own {
            println!("\nYour usage in this workspace:");
            print_fields(&[
                ("Drive", size_cell(&own, "driveUsage")),
                ("Backups", size_cell(&own, "backupsUsage")),
                ("Space limit", size_cell(&own, "spaceLimit")),
            ]);
        }
    }
    output::emit(
        "",
        json!({
            "success": true,
            "workspaceId": id,
            "usage": total,
            "memberUsage": member,
        }),
    );
    Ok(())
}

/// `workspaces invitations` — invitations waiting for the caller to accept or
/// decline. Account-scoped, so it takes no workspace argument.
pub async fn invitations(limit: u32, offset: u32) -> Result<()> {
    let creds = auth::get_auth_details().await?;
    let resp = DriveApi::new()
        .get_workspace_invitations(&creds.token, limit, offset)
        .await?;
    let list = as_list(&resp, "invitations");

    if !output::is_json() {
        if list.is_empty() {
            if resp.is_array() || resp.is_null() {
                output::status("No pending invitations.");
            } else {
                print_raw(&resp);
            }
        } else {
            let rows: Vec<Vec<String>> = list
                .iter()
                .map(|invite| {
                    let workspace = invite.get("workspace").cloned().unwrap_or(Value::Null);
                    vec![
                        text(&workspace, "name"),
                        text(invite, "workspaceId"),
                        size_cell(invite, "spaceLimit"),
                        format_date(&text(invite, "createdAt")),
                        text(invite, "id"),
                    ]
                })
                .collect();
            print_table(
                &[
                    "Workspace",
                    "Workspace ID",
                    "Space limit",
                    "Invited at",
                    "Invitation ID",
                ],
                &rows,
            );
        }
    }
    output::emit("", json!({ "success": true, "invitations": resp }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Value> {
        vec![
            json!({ "workspace": { "id": "11111111-1111-4111-8111-111111111111", "name": "Alpha" } }),
            json!({ "workspace": { "id": "22222222-2222-4222-8222-222222222222", "name": "Beta" } }),
            json!({ "workspace": { "id": "33333333-3333-4333-8333-333333333333", "name": "Beta" } }),
        ]
    }

    #[test]
    fn resolves_a_workspace_by_id() {
        let id = resolve_workspace_id(&sample(), "22222222-2222-4222-8222-222222222222").unwrap();
        assert_eq!(id, "22222222-2222-4222-8222-222222222222");
    }

    #[test]
    fn resolves_a_workspace_by_name_ignoring_case() {
        let id = resolve_workspace_id(&sample(), "alpha").unwrap();
        assert_eq!(id, "11111111-1111-4111-8111-111111111111");
    }

    #[test]
    fn resolves_a_workspace_by_one_based_index() {
        let id = resolve_workspace_id(&sample(), "2").unwrap();
        assert_eq!(id, "22222222-2222-4222-8222-222222222222");
        assert!(resolve_workspace_id(&sample(), "0").is_err());
        assert!(resolve_workspace_id(&sample(), "4").is_err());
    }

    #[test]
    fn rejects_an_ambiguous_name() {
        let err = resolve_workspace_id(&sample(), "Beta").unwrap_err().to_string();
        assert!(err.contains("matches 2"), "{err}");
    }

    #[test]
    fn rejects_an_unknown_selector_and_an_empty_list() {
        assert!(resolve_workspace_id(&sample(), "Gamma").is_err());
        assert!(resolve_workspace_id(&[], "Alpha").is_err());
    }

    #[test]
    fn recognises_uuid_shaped_selectors() {
        assert!(looks_like_uuid("11111111-1111-4111-8111-111111111111"));
        assert!(!looks_like_uuid("Alpha"));
        assert!(!looks_like_uuid("11111111-1111-4111-8111"));
        assert!(!looks_like_uuid("zzzzzzzz-1111-4111-8111-111111111111"));
    }

    #[test]
    fn reads_byte_counts_as_numbers_or_strings() {
        let v = json!({ "a": 1024, "b": "2048", "c": null, "d": "nope" });
        assert_eq!(bytes(&v, "a"), Some(1024.0));
        assert_eq!(bytes(&v, "b"), Some(2048.0));
        assert_eq!(bytes(&v, "c"), None);
        assert_eq!(bytes(&v, "d"), None);
        assert_eq!(bytes(&v, "missing"), None);
        assert_eq!(size_cell(&v, "missing"), "-");
        assert_eq!(size_cell(&v, "a"), "1 KB");
    }

    #[test]
    fn unwraps_a_list_from_either_shape() {
        assert_eq!(as_list(&json!([1, 2]), "teams").len(), 2);
        assert_eq!(as_list(&json!({ "teams": [1] }), "teams").len(), 1);
        assert!(as_list(&json!({ "other": [1] }), "teams").is_empty());
        assert!(as_list(&Value::Null, "teams").is_empty());
    }

    #[test]
    fn names_a_member_with_whatever_the_record_carries() {
        assert_eq!(
            member_name(&json!({ "name": "Ada", "lastname": "Lovelace" })),
            "Ada Lovelace"
        );
        assert_eq!(member_name(&json!({ "name": "Ada" })), "Ada");
        assert_eq!(
            member_name(&json!({ "username": "ada", "email": "e" })),
            "ada"
        );
        assert_eq!(member_name(&json!({ "email": "e" })), "e");
        assert_eq!(member_name(&Value::Null), "");
    }

    #[test]
    fn member_row_survives_a_record_missing_everything() {
        let row = member_row(&Value::Null);
        assert_eq!(row.len(), 6);
        assert_eq!(row[2], "member");
    }

    #[test]
    fn member_row_labels_owners_and_managers() {
        assert_eq!(member_row(&json!({ "isOwner": true, "isManager": true }))[2], "owner");
        assert_eq!(member_row(&json!({ "isManager": true }))[2], "manager");
    }
}

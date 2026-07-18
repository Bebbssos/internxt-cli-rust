# internxt-cli (`ixr`)

A Rust port of Internxt's official CLI, aiming to be a fast, low-memory, single
static binary with fully streaming transfers.

The headline difference from the official CLI: **it works on any account type**,
including Free. The official CLI gates most functionality to the Ultimate plan
server-side; `ixr` doesn't hit that gate.

## Compatibility with the official Internxt CLI

This is intended to be a **mostly drop-in replacement**. For ported commands the
names, flags, endpoints, payloads and crypto all match, so the two behave the
same for everyday login / upload / download / list / move / rename / trash
workflows. Credentials are **not** shared between the two — `ixr` stores its
own session at `~/.ixr/credentials`, separate from the official CLI's
`~/.internxt-cli`, so each needs its own `login`.

The official CLI's commands (built with [oclif](https://oclif.io)) are named with
a `topic:command` colon style, e.g. `upload:file`, `move:file`, `sync:up`. `ixr`
keeps every one of those as an alias, but makes a hyphenated form
(`upload-file`, `move-file`, `sync-up`) the primary spelling, since it doesn't
need shell quoting/escaping. Both spellings always work.

Known differences:

- **`login` defaults to SSO**, matching the official CLI's default. Built with
  the default `sso` feature, `login` runs the web-browser callback flow;
  `login-legacy` forces email + password + optional 2FA, `login-sso` forces SSO.
  Building `--no-default-features` drops the `sso` feature: `login` then falls
  back to legacy and `login-sso` errors. The SSO flow can't carry the Kyber
  private key, so hybrid-Kyber workspaces need `login-legacy`.
- **`--json` output schema differs.** `ixr` emits a simplified `{ "success": true, ... }`
  object per command rather than the official CLI's exact JSON envelope. Field
  names mostly match, but don't assume a byte-identical structure — see each
  command's JSON output below.
- **No interactive prompting for missing flags**, with two exceptions:
  `login-legacy` (email/password/2FA) and `trash-clear` (confirmation, unless
  `--force`). Everywhere else, a missing required flag is a clap usage error.
- **Plain-text table output** uses simple aligned columns rather than the
  official CLI's boxed tables. Use `--json` for stable machine-readable output.
- **`serve webdav` runs in the foreground**, options passed inline, rather than
  as a pm2-managed background service configured through a separate
  `webdav-config` command. The `webdav-config` / `webdav start|stop|status` /
  `add-cert` daemon-management commands aren't ported — the WebDAV server itself
  is, as `serve webdav`.
- **Not yet ported:** `config`, `logs`.
- **New, with no official equivalent:** `usage`, `id-from-path`, `path-from-id`,
  the `thumbnail` command family, `mount`, and the `fuse`/`smb`/`nfs`/`sftp`
  `serve` backends (the official CLI only serves WebDAV). See the
  [command reference](#command-reference) below for details on each.

## Install

Not yet published to a package registry, AUR, or similar — install from source
for now (see [Build](#build) below).

## Build

```sh
cargo build --release
# binary at target/release/ixr (SSO + WebDAV over HTTP + FUSE enabled by default)

# add HTTPS support to the WebDAV server (pulls in a rustls TLS stack):
cargo build --release --features webdav-tls

# add the SMB / NFS / SFTP backends (all off by default, all experimental):
cargo build --release --features smb          # SMB2/3 share
cargo build --release --features nfs          # NFSv3 export
cargo build --release --features sftp         # SFTP over SSH (pulls in russh)

# smaller binary without SSO login, WebDAV or FUSE (drops axum + open + fuser):
cargo build --release --no-default-features
```

See [Features](#features) below for what each `--features`/`--no-default-features`
flag enables and disables.

A multi-arch Docker image is available — see the [`Dockerfile`](../Dockerfile)
at the repo root.

## Features

Cargo feature flags gate optional command surface, mainly to keep the default
binary small and dependency-light. `default = ["sso", "webdav", "fuse"]`.

| Feature | Default | Enables | Notes |
|---|---|---|---|
| `sso` | on | Web-based SSO flow for `login`/`login-sso` (local callback server + browser launch) | Without it, `login` falls back to the legacy flow and `login-sso` errors. Pulls in `axum` + `open`. |
| `webdav` | on | `serve webdav` over plain HTTP | Pulls in `axum` + `tokio-util` + `mime_guess`. |
| `webdav-tls` | off | HTTPS for `serve webdav` (`--webdav-https`) | Requires `webdav`. Pulls in `axum-server` + `rustls-pemfile` + `rcgen` (self-signed or your own cert/key). |
| `fuse` | on (Unix only) | `mount`, `serve fuse` | Needs `libfuse3-dev` + `pkg-config` at build time and a FUSE driver at runtime (fuse3/macFUSE/`fusefs-libs3`). Inert on Windows — default builds still compile there, just without these commands. |
| `smb` | off | `serve smb` — SMB2/3 share | Experimental. All platforms. Built on a fork of the `smb-server` crate. |
| `nfs` | off | `serve nfs` — NFSv3 export | Experimental. All platforms. |
| `sftp` | off | `serve sftp` — SFTP over SSH | Experimental. All platforms. Pulls in `russh` + `russh-sftp`. |
| `termimage` | off | `thumbnail display` — inline terminal image rendering | Pulls in `viuer` + `image`. Kitty/iTerm2 graphics protocol, with a Unicode half-block fallback. |

## Global flags

Every command accepts:

- `--json` — print a single JSON result object and suppress progress/status
  output. See each command's "JSON output" below for its shape.
- `-x, --non-interactive` (env `INXT_NONINTERACTIVE`) — never prompt for
  input; error out instead when a required value is missing.

IDs are Drive UUIDs. Most commands that take an id also accept a `--path`
(or `--dest-path` / `--remote-path`) alternative — give one or the other, not
both. Where a destination/parent folder id is optional, leaving it empty
targets your root folder (or workspace root, if a workspace is active).

## Commands

| Command | Description | Feature(s) | Official CLI compatibility |
|---|---|---|---|
| [`login`](#login) | Log in to your Internxt account. | none (flow varies with `sso`, default on) | Same command; SSO by default here matches the official default. |
| [`login-legacy`](#login-legacy) | Log in with email + password (legacy flow). | none | Mirrors `login:legacy`, kept as alias. |
| [`login-sso`](#login-sso) | Log in via the web-based SSO flow. | `sso` (default on) | Mirrors `login:sso`, kept as alias. |
| [`logout`](#logout) | Log out the current user. | none | Same command. |
| [`whoami`](#whoami) | Show the currently logged-in user. | none | Same command. |
| [`usage`](#usage) | Show account plan, used space, and upload limit. | none | New — no official equivalent. |
| [`list`](#list) | List a folder's contents. | none | Same command; adds `--path`. |
| [`create-folder`](#create-folder) | Create a folder. | none | Mirrors `create:folder`, kept as alias; adds `--path`. |
| [`upload-file`](#upload-file) | Upload a file. | none | Mirrors `upload:file`, kept as alias; adds `--dest-path`, `--stdin`, `--size`. |
| [`upload-folder`](#upload-folder) | Recursively upload a folder tree. | none | Mirrors `upload:folder`, kept as alias; adds `--dest-path`. |
| [`download-file`](#download-file) | Download + decrypt a file. | none | Mirrors `download:file`, kept as alias; adds `--path`, `--stdout`. |
| [`move-file`](#move-file--move-folder) | Move a file into a destination folder. | none | Mirrors `move:file`, kept as alias; adds `--path`/`--dest-path`. |
| [`move-folder`](#move-file--move-folder) | Move a folder into a destination folder. | none | Mirrors `move:folder`, kept as alias; adds `--path`/`--dest-path`. |
| [`rename-file`](#rename-file--rename-folder) | Rename a file. | none | Mirrors `rename:file`, kept as alias; adds `--path`. |
| [`rename-folder`](#rename-file--rename-folder) | Rename a folder. | none | Mirrors `rename:folder`, kept as alias; adds `--path`. |
| [`trash-file`](#trash-file--trash-folder) | Move a file to the trash. | none | Mirrors `trash:file`, kept as alias; adds `--path`. |
| [`trash-folder`](#trash-file--trash-folder) | Move a folder to the trash. | none | Mirrors `trash:folder`, kept as alias; adds `--path`. |
| [`trash-list`](#trash-list) | List the contents of the trash. | none | Mirrors `trash:list`, kept as alias. |
| [`trash-restore-file`](#trash-restore-file--trash-restore-folder) | Restore a trashed file. | none | Mirrors `trash:restore:file`, kept as alias; adds `--dest-path`. |
| [`trash-restore-folder`](#trash-restore-file--trash-restore-folder) | Restore a trashed folder. | none | Mirrors `trash:restore:folder`, kept as alias; adds `--dest-path`. |
| [`trash-clear`](#trash-clear) | Empty the trash permanently. | none | Mirrors `trash:clear`, kept as alias. |
| [`delete-permanently-file`](#delete-permanently-file--delete-permanently-folder) | Permanently delete a file. | none | Mirrors `delete:permanently:file`, kept as alias. |
| [`delete-permanently-folder`](#delete-permanently-file--delete-permanently-folder) | Permanently delete a folder. | none | Mirrors `delete:permanently:folder`, kept as alias. |
| [`workspaces-list`](#workspaces-list) | List the workspaces you belong to. | none | Mirrors `workspaces:list`, kept as alias. |
| [`workspaces-use`](#workspaces-use) | Set the active workspace. | none | Mirrors `workspaces:use`, kept as alias. |
| [`workspaces-unset`](#workspaces-unset) | Unset the active workspace. | none | Mirrors `workspaces:unset`, kept as alias. |
| [`sync-up`](#sync-up--sync-down) | One-way sync, local → remote (push). | none | Mirrors `sync:up`, kept as alias; adds `--remote-path`. |
| [`sync-down`](#sync-up--sync-down) | One-way sync, remote → local (pull). | none | Mirrors `sync:down`, kept as alias; adds `--remote-path`. |
| [`serve`](#serve) | Serve Drive over WebDAV / FUSE / SMB / NFS / SFTP (foreground). | at least one of `webdav`, `fuse` (unix), `smb`, `nfs`, `sftp` — each protocol needs its own feature; `webdav`+`fuse` default on | WebDAV mirrors the official server, run inline instead of as a daemon. FUSE/SMB/NFS/SFTP are new. |
| [`mount`](#mount) | Mount Drive as a local filesystem via FUSE (Unix). | `fuse` (default on, Unix only) | New — no official equivalent. |
| [`id-from-path`](#id-from-path) | Print the uuid of the item at a Drive path. | none | New — no official equivalent. |
| [`path-from-id`](#path-from-id) | Print the Drive path of an item given its uuid. | none | New — no official equivalent. |
| [`thumbnail generate\|upload\|download`](#thumbnail) | Manage a file's thumbnail. | none | New — the official CLI only generates thumbnails automatically on upload; it has no management commands. |
| [`thumbnail display`](#thumbnail) | Show a file's thumbnail inline in the terminal. | `termimage` (default off) | New — no official equivalent. |

## Command reference

### `login`

Logs in. Uses the web-based SSO flow when built with the `sso` feature
(default); otherwise falls back to the legacy email/password flow. Use
`login-sso` or `login-legacy` to force a specific flow.

Flags: `--host <HOST>`, `--port <PORT>` (SSO callback server address/port,
default 127.0.0.1 / a random free port); `-e/--email`, `-p/--password`,
`-w/--twofactor`, `-t/--twofactortoken` (used for the legacy fallback).

```sh
ixr login                                  # SSO: opens a browser to authenticate
ixr login --host 0.0.0.0 --port 4000       # cross-device SSO (e.g. inside a container)
```

JSON output: `{ "success": true, "message": "...", "login": <credentials> }`
on success. `login` (JSON credentials, tokens, keys) is sensitive — treat it
like a secret. On failure: `{ "success": false, "message": "..." }`.

### `login-legacy`

Alias: `login:legacy`. Logs in with email + password (+ 2FA if the account
requires it). Prompts for any missing value unless `-x/--non-interactive`.

Flags: `-e/--email`, `-p/--password`, `-w/--twofactor`, `-t/--twofactortoken`
(takes priority over `--twofactor` when both are given).

```sh
ixr login-legacy --email you@example.com     # prompts for password (+ 2FA)
ixr login-legacy -e you@example.com -p '...' -w 123456
```

JSON output: same shape as [`login`](#login).

### `login-sso`

Alias: `login:sso`. Forces the web-based SSO flow. Errors if built without the
`sso` feature.

Flags: `--host <HOST>`, `--port <PORT>`.

JSON output: same shape as [`login`](#login).

### `logout`

Invalidates the session server-side and clears local credentials. No flags.

JSON output: `{ "success": true, "message": "User logged out successfully." }`,
or `{ "success": false, "message": "No user is currently logged in." }`.

### `whoami`

Shows the currently logged-in user. Refreshes the session token if it's near
expiry; if the session is dead, clears local credentials (matching the
official CLI's behaviour of logging out on a dead session).

JSON output: `{ "success": true, "message": "...", "login": <credentials> }`,
or `{ "success": false, "message": "You are not logged in." }`.

### `usage`

Aliases: `account`, `account-info`. Not an official CLI command — it fans out
the same drive-gateway endpoints the official CLI uses internally
(`/users/usage`, `/users/limit`, `/files/limits`) plus a best-effort plan
lookup on the payments API.

```
Plan:               Free
Used:               3.89 TB / 10 TB (38.9%)
  Drive:            3.89 TB
  Backups:          0 B
Space limit:        10 TB
Upload file limit:  10 GB
```

The plan name reads `Tier (Type)` (e.g. `Pro (Subscription)`), collapsing to
one value when they agree. Legacy lifetime accounts show `Free (Lifetime)` —
the tier endpoint mislabels old plans as `free`, but `(Lifetime)` still
signals it's a paid plan; the space limit is always correct. If the payments
API is unreachable the plan shows `unknown`.

JSON output:

```json
{
  "success": true,
  "usage": {
    "plan": "Pro (Subscription)",
    "planLabel": "pro",
    "subscriptionType": "subscription",
    "used": 123456789,
    "drive": 123000000,
    "backups": 456789,
    "spaceLimit": 1000000000000,
    "spaceLimitInfinite": false,
    "usedPercent": 12.3,
    "uploadFileLimit": 10737418240
  }
}
```

### `list`

Lists a folder's contents.

Flags: `-i/--id <FOLDER_ID>` (default: root), `-p/--path <PATH>` (alternative
to `--id`), `-e/--extended` (adds created/modified date + size to the
human-readable view).

```sh
ixr list -e                            # root folder, extended view
ixr list -i <folder-uuid> --json       # machine-readable output
ixr list -p /Documents/Reports
```

JSON output: `{ "success": true, "list": { "folders": [...], "files": [...] } }`
— always the full (non-extended) item objects, regardless of `--extended`
(that flag only affects the human-readable table).

### `create-folder`

Alias: `create:folder`. Creates a folder.

Flags: `-n/--name <NAME>` (required), `-i/--id <PARENT_ID>` (default: root),
`-p/--path <PATH>` (alternative to `--id`).

```sh
ixr create-folder -n "Reports" -i <parent-uuid>
ixr create-folder -n "Reports" -p /Documents
```

JSON output: `{ "success": true, "folder": <DriveFolderData> }`.

### `upload-file`

Alias: `upload:file`. Uploads a single file (streaming; single-part or
multipart depending on size).

Flags: `-f/--file <PATH>` (omit when using `--stdin`), `-i/--destination
<FOLDER_ID>` (default: root), `--dest-path <PATH>` (alternative to
`--destination`), `--stdin` (read the body from stdin instead of `--file`,
requires `--name`), `-n/--name <NAME>` (Drive filename; required with
`--stdin`, otherwise overrides the name/extension from `--file`'s path),
`-s/--size <BYTES>` (exact stdin length — streams directly if given,
otherwise stdin is spooled to a temp file to learn its size), plus the
[upload-limit flags](#upload-size-limit).

```sh
ixr upload-file -f ./file.bin -i <folder-uuid>
ixr upload-file -f ./big.iso --max-upload-size 20GB     # override the per-file cap
ixr upload-file -f ./big.iso --no-upload-limit          # disable the cap
tar -c ./dir | ixr upload-file --stdin --name dir.tar --dest-path /Backups
```

A thumbnail is generated automatically for image sources (best-effort, never
fails the upload) — see [`thumbnail`](#thumbnail).

JSON output: `{ "success": true, "file": { "uuid": "..." } }`.

### `upload-folder`

Alias: `upload:folder`. Recursively uploads a folder tree (concurrent file
uploads).

Flags: `-f/--folder <PATH>` (required), `-i/--destination <FOLDER_ID>`
(default: root), `--dest-path <PATH>` (alternative to `--destination`), plus
the [upload-limit flags](#upload-size-limit).

```sh
ixr upload-folder -f ./my-folder                # -i for a destination folder
ixr upload-folder -f ./my-folder --dest-path /Backups
```

JSON output: `{ "success": true, "folder": { "uuid": "..." }, "totalBytes": N, "uploadTimeMs": N }`.

### `download-file`

Alias: `download:file`. Downloads and decrypts a file, streaming to disk (or
stdout).

Flags: `-i/--id <FILE_ID>`, `-p/--path <PATH>` (alternative to `--id`),
`-d/--directory <DIR>` (default: current dir), `-o/--overwrite`, `--stdout`
(write decrypted bytes to stdout instead of a file; status goes to stderr so
it never mixes into piped data).

```sh
ixr download-file -i <file-uuid> -d ./out --overwrite
ixr download-file -p /Documents/report.pdf -d ./out
ixr download-file -i <file-uuid> --stdout > file.bin
```

JSON output: `{ "success": true, "path": "<local path>" }` when written to
disk. **With `--stdout`, no JSON object is emitted at all** (only a status
line on stderr in non-JSON mode) — the file bytes own stdout instead.

### `move-file` / `move-folder`

Aliases: `move:file` / `move:folder`. Moves a file or folder into a
destination folder.

Flags: `-i/--id <ID>`, `-p/--path <PATH>` (alternative to `--id`),
`-d/--destination <FOLDER_ID>` (default: root), `--dest-path <PATH>`
(alternative to `--destination`).

```sh
ixr move-file -i <file-uuid> -d <folder-uuid>
ixr move-folder -p /Old/Name -d <folder-uuid>
```

JSON output: `move-file` → `{ "success": true, "file": <DriveFileData> }`.
`move-folder` → `{ "success": true, "folder": <DriveFolderData> }`.

### `rename-file` / `rename-folder`

Aliases: `rename:file` / `rename:folder`. Renames a file or folder (for
files, name/extension are split automatically).

Flags: `-i/--id <ID>`, `-p/--path <PATH>` (alternative to `--id`), `-n/--name
<NAME>` (required).

```sh
ixr rename-file -i <file-uuid> -n "new-name.txt"
ixr rename-folder -p /Old/Name -n "New Name"
```

JSON output: `rename-file` → `{ "success": true, "file": { "uuid", "plainName", "type" } }`.
`rename-folder` → `{ "success": true, "folder": { "uuid", "plainName" } }`.

### `trash-file` / `trash-folder`

Aliases: `trash:file` / `trash:folder`. Moves a file or folder to the trash.

Flags: `-i/--id <ID>`, `-p/--path <PATH>` (alternative to `--id`).

JSON output: `{ "success": true, "file": { "uuid": "..." } }` or
`{ "success": true, "folder": { "uuid": "..." } }`.

### `trash-list`

Alias: `trash:list`. Lists the contents of the trash.

Flags: `-e/--extended`.

JSON output: `{ "success": true, "list": { "folders": [...], "files": [...] } }`
(same shape as [`list`](#list)).

### `trash-restore-file` / `trash-restore-folder`

Aliases: `trash:restore:file` / `trash:restore:folder`. Restores a trashed
file or folder into a destination folder.

Flags: `-i/--id <ID>`, `-d/--destination <FOLDER_ID>` (default: root),
`--dest-path <PATH>` (alternative to `--destination`).

JSON output: `{ "success": true, "file": <DriveFileData> }` or
`{ "success": true, "folder": <DriveFolderData> }`.

### `trash-clear`

Alias: `trash:clear`. Empties the trash permanently — **cannot be undone**.
Prompts for confirmation unless `--force` (required in `--json`/non-interactive
mode).

Flags: `-f/--force`.

```sh
ixr trash-clear --force
```

JSON output: `{ "success": true, "message": "Trash emptied successfully." }`.

### `delete-permanently-file` / `delete-permanently-folder`

Aliases: `delete:permanently:file` / `delete:permanently:folder`.
Permanently deletes a file or folder — **cannot be undone**.

Flags: `-i/--id <ID>`.

JSON output: `{ "success": true, "message": "File permanently deleted successfully" }`
or `{ "success": true, "message": "Folder permanently deleted successfully" }`.

### `workspaces-list`

Alias: `workspaces:list`. Lists the workspaces you belong to.

Flags: `-e/--extended` (owner, address, created-at in the human-readable view).

JSON output: `{ "success": true, "list": { "workspaces": [...] } }` (always the
full objects, regardless of `--extended`).

### `workspaces-use`

Alias: `workspaces:use`. Sets the active workspace for subsequent commands —
switches where drive calls and transfers route (its own bucket, network
credentials and mnemonic).

Flags: `-i/--id <WORKSPACE_ID>` (use `workspaces-list` to find ids),
`-p/--personal` (switch back to your personal drive space; conflicts with
`--id`).

```sh
ixr workspaces-use -i <workspace-id>
ixr workspaces-use --personal
```

JSON output: `{ "success": true, "workspace": { "id", "name", "bucket", "rootFolderId" } }`.

### `workspaces-unset`

Alias: `workspaces:unset`. Unsets the active workspace (equivalent to
`workspaces-use --personal`). No flags.

JSON output: `{ "success": true, "message": "Personal drive space selected successfully." }`.

### `sync-up` / `sync-down`

Aliases: `sync:up` / `sync:down`. A single **one-way** reconcile pass, then
exit (not a daemon). The source side always wins — no bidirectional mode, no
conflict resolution. Files are keyed by relative path; change detection
compares size, then modification time (±2s tolerance). Downloaded files are
stamped with the remote modification time so repeat `sync-down` runs are
idempotent.

Flags (`sync-up`): `-l/--local <DIR>` (required), `-r/--remote <FOLDER_ID>`
(default: root), `--remote-path <PATH>` (alternative to `--remote`),
`--delete[=trash|permanent]` (opt-in; prunes extra remote files **and**
folders — deleting the top-most extra folder cascades its subtree), `--dry-run`,
plus the [upload-limit flags](#upload-size-limit).

Flags (`sync-down`): `-l/--local <DIR>` (required), `-r/--remote <FOLDER_ID>`,
`--remote-path <PATH>`, `--delete[=remove]` (OS-trash delete mode not yet
supported), `--dry-run`.

```sh
ixr sync-up   -l ./my-folder -r <folder-uuid> --dry-run   # preview a push
ixr sync-up   -l ./my-folder -r <folder-uuid> --delete    # push, trashing remote extras
ixr sync-down -l ./my-folder --remote-path /Backups       # pull new/changed files
```

JSON output:

```json
{
  "success": true,
  "dryRun": false,
  "transferred": 12,
  "deleted": 1,
  "skipped": 40,
  "failed": 0,
  "actions": [{ "action": "upload", "path": "notes.txt", "ok": true }]
}
```

### `serve`

Runs one or more Drive backends in the **foreground** until Ctrl-C. Pass a
comma-separated protocol list: `webdav`, `fuse` (Unix), `smb`, `nfs`, `sftp`.
Running several at once shares one set of credentials, one folder-listing
cache and one global upload limit.

The **WebDAV** backend mirrors the official CLI's WebDAV server; the official
CLI runs it as a pm2-managed background service configured through a separate
`webdav-config` command, while `ixr` runs it inline as a normal foreground
command instead. **FUSE, SMB, NFS and SFTP have no official equivalent** — the
official CLI only serves WebDAV. `smb`, `nfs` and `sftp` are experimental and
off by default (build with `--features smb`/`nfs`/`sftp`).

Shared flags (bare): `-i/--folder-uuid <UUID>` (root to expose), `-d
/--delete-permanently` (hard-delete instead of trash), `--read-only`,
`-v/--verbose` (log every per-op request across all backends), `--spool`
(spool uploads to a temp file before uploading; FUSE always spools),
`--spool-dir <DIR>`, `--max-concurrent-uploads <N>` (0 = unlimited),
`--cache-ttl <SECS>` (default 5; also the FUSE kernel attr/entry TTL),
`--no-cache`, plus the [upload-limit flags](#upload-size-limit).

Protocol-specific flags are prefixed:

- **WebDAV** (`--webdav-*`): `--webdav-host` (default `127.0.0.1`),
  `--webdav-port` (default `3005`), `--webdav-https` (needs `webdav-tls`
  feature), `--webdav-cert`/`--webdav-key` (custom TLS cert/key, both
  required together), `--webdav-timeout <MINS>` (default 60; accepted but not
  yet wired to a request-timeout layer), `--webdav-create-full-path`
  (auto-create missing parent folders on `PUT`/`MKCOL`), `--webdav-custom-auth`
  + `--webdav-username`/`--webdav-password` (require HTTP Basic auth).
- **FUSE** (`--fuse-*`): `--fuse-mountpoint <DIR>` (required when `fuse` is
  served), `--fuse-allow-other`.
- **SMB** (`--smb-*`): `--smb-host` (default `127.0.0.1`), `--smb-port`
  (default `4445` — port 445 needs root/admin), `--smb-share` (default
  `internxt`), `--smb-username` (default `internxt`), `--smb-password` (omit
  for anonymous/guest — most clients, Windows especially, refuse it).
- **NFS** (`--nfs-*`): `--nfs-host` (default `127.0.0.1`), `--nfs-port`
  (default `12049` — port 2049 needs root/admin).
- **SFTP** (`--sftp-*`): `--sftp-host` (default `127.0.0.1`), `--sftp-port`
  (default `2022` — port 22 needs root/admin), `--sftp-username` (default
  `internxt`), `--sftp-password` (omit to accept any password), `--sftp-host-key
  <PATH>` (persistent host key; omit and one is generated once under
  `~/.ixr/sftp_host_key`).

```sh
ixr serve webdav                                             # http://127.0.0.1:3005
ixr serve webdav --webdav-host 0.0.0.0 --webdav-port 8080     # accept LAN clients
ixr serve fuse --fuse-mountpoint ~/drive
ixr serve smb --smb-password secret                           # needs --features smb
ixr serve webdav,fuse --fuse-mountpoint ~/drive                # both at once, shared cache/creds
ixr serve webdav --read-only -i <folder-uuid>                  # read-only, rooted at a subfolder
```

WebDAV supported methods: `OPTIONS`, `PROPFIND`, `GET`/`HEAD` (with `Range`),
`PUT`, `MKCOL`, `DELETE`, `MOVE`, `LOCK`/`UNLOCK`. `COPY` and `PROPPATCH`
return `501 Not Implemented`, matching the official server. `DELETE` trashes
items by default (`--delete-permanently` for a hard delete).

`serve`/`mount` run until interrupted — there's no terminal JSON result
object to speak of; `--json` mainly suppresses the startup/progress banner.

Set `INTERNXT_WEBDAV_DEBUG=1` (or pass `--verbose`) to dump each WebDAV
request/response, headers included, to stderr.

### `mount`

New — no official equivalent (the official CLI has no filesystem-mount mode).
Unix only. A thin wrapper over `serve fuse` where the shared flags use their
bare names (no `fuse-` prefix).

Flags: `-i/--folder-uuid <UUID>`, `--read-only`, `-d/--delete-permanently`,
`--spool-dir <DIR>`, `--max-concurrent-uploads <N>`, `--cache-ttl <SECS>` /
`--no-cache`, `--allow-other`, `-v/--verbose`, plus the
[upload-limit flags](#upload-size-limit).

```sh
mkdir -p ~/drive && ixr mount ~/drive              # Ctrl-C to unmount
ixr mount ~/drive --read-only                      # browse/read only
ixr mount ~/drive -i <folder-uuid>                 # mount a subfolder as root
```

Needs `libfuse3-dev` + `pkg-config` at build time and a FUSE driver at runtime
(fuse3 on Linux, macFUSE on macOS, `fusefs-libs3` on FreeBSD). Reads stream
and decrypt lazily; writes buffer to a temp file and upload in full when the
file is closed (Internxt has no partial-update API), replacing the old Drive
entry.

### `id-from-path`

Aliases: `get-id`, `id:from:path`. New — no official equivalent. Prints the
uuid of the Drive file/folder at a given path.

Flags: `-p/--path <PATH>` (required).

```sh
ixr id-from-path -p /Documents/report.pdf
```

JSON output: `{ "success": true, "uuid": "...", "isFolder": false, "type": "file" }`.

### `path-from-id`

Aliases: `get-path`, `path:from:id`. New — no official equivalent. Prints the
full Drive path of a file/folder given its uuid.

Flags: `-i/--id <UUID>` (required).

```sh
ixr path-from-id -i <uuid>
```

JSON output: `{ "success": true, "path": "/Documents/report.pdf", "isFolder": false, "type": "file" }`.

### `thumbnail`

Alias: `thumbnails`. New — the official CLI generates a thumbnail
automatically on upload (which `ixr` also does) but has no user-facing
management commands for it. Only image sources (jpg/png/webp/gif/tiff) are
supported; PDF thumbnails are not generated (matching the official CLI).

Every subcommand takes `-i/--id <UUID>` or `-p/--path <PATH>` (one or the
other) to identify the file.

- **`thumbnail generate`** — regenerate a thumbnail from the file's own image
  content. JSON: `{ "success": true, "thumbnail": { "id": "...", "size": N } }`.
- **`thumbnail upload`** — `-f/--file <PATH>` (required): upload a custom
  image as the thumbnail. `--raw` uploads it as-is instead of resizing to a
  300x300 PNG. JSON: same shape as `generate`.
- **`thumbnail download`** — `-d/--directory <DIR>` (default: current dir),
  `-o/--overwrite`, `--index <N>` (0-based, for files with several
  thumbnails). JSON: `{ "success": true, "path": "<local path>" }`.
- **`thumbnail display`** (alias `show`, needs the `termimage` feature) —
  renders inline in the terminal (Kitty/iTerm2 graphics protocol, or a
  Unicode half-block fallback). `--index <N>`, `-w/--width`, `-H/--height`
  (max render size in terminal cells). Not meaningful with `--json` — it
  renders to the terminal rather than emitting a result object.

```sh
ixr thumbnail generate -p /Photos/cat.jpg
ixr thumbnail upload -i <file-uuid> -f ./custom-thumb.png
ixr thumbnail download -i <file-uuid> -d ./out
ixr thumbnail display -p /Photos/cat.jpg          # needs --features termimage
```

Automatic thumbnailing (on `upload-file`, `upload-folder`, and any `serve`
backend write) can be disabled everywhere with `INTERNXT_THUMBNAILS=0`.

## Upload size limit

Uploads are validated against a per-file size cap before transferring — except
there is **no** hard-coded default: when your plan sets no cap, uploads are
unbounded. The cap is resolved in this order (first match wins):

1. `--no-upload-limit` — disable the check entirely.
2. `--max-upload-size <SIZE>` — a custom cap (`5GB`, `500M`, `1073741824`, …
   binary units).
3. `INTERNXT_MAX_UPLOAD_SIZE` env var — universal override for every upload
   command. A size string sets a cap; `off`/`none`/`unlimited`/`0` disables it.
4. Otherwise, your plan's `maxUploadFileSize` (from `/files/limits`; unlimited
   if unset).

These flags apply to `upload-file`, `upload-folder`, `sync-up`, and the
`serve`/`mount` backends. Over-limit files are rejected up front (folder/sync
uploads skip the offending file and continue; WebDAV `PUT` returns `413`;
FUSE/SMB/NFS/SFTP writes fail accordingly).

## Configuration

API endpoints and app constants default to the public Internxt values (see
`crates/internxt-core/src/config.rs`) and can be overridden via environment
variables of the same name (`DRIVE_NEW_API_URL`, `NETWORK_URL`,
`PAYMENTS_API_URL`, etc).

Credentials are stored AES-encrypted at `~/.ixr/credentials` — its own
directory, separate from the official CLI's `~/.internxt-cli`.

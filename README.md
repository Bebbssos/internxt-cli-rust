# internxt-cli-rust (`ixr`)

A Rust port of Internxt's official CLI, aiming to be a fast, low-memory, single
static binary with fully streaming transfers.

Also includes a client for **Internxt VPN** — `vpn locations` / `vpn proxy`
(a local HTTP(S)/SOCKS5 proxy, not a full tunnel). No official CLI
equivalent; see [`vpn proxy`](#vpn-proxy).

**Works on any account type, including Free.** The official CLI refuses to
run unless your plan has server-side CLI access (bundled with Ultimate);
`ixr` skips that check.

Everything else about your plan still applies — this only removes the
CLI-access gate. In particular: the per-file upload cap from
`/files/limits` (see [Upload size limit](#upload-size-limit)), and
Free/legacy plans rejecting empty (0-byte) files outright (`HTTP 402`).

> Not affiliated with or endorsed by Internxt.

> Written mostly by [Claude Code](https://claude.com/claude-code), porting the
> behaviour of Internxt's official Node/TypeScript packages.

The Drive engine (auth, crypto, transfers) lives in a separate crate,
[`internxt-core`](https://github.com/Bebbssos/internxt-core-rust) — this repo is the
CLI front-end built on top of it.

## Contents

- [Install](#install) · [Build](#build) · [Features](#features)
- [FUSE/WinFSP mount support](#fusewinfsp-mount-support)
- [Global flags](#global-flags)
- [Path syntax](#path-syntax) — `//drive`, `//backups/<device>`, and the
  virtual `//` / `//backups` groupings
- [Commands](#commands) — the full command table
- [Command reference](#command-reference) — per-command flags and JSON output
- [Upload size limit](#upload-size-limit)
- [Configuration](#configuration)
- [Multiple accounts](#multiple-accounts)
- [Compatibility with the official Internxt CLI](#compatibility-with-the-official-internxt-cli)
- [License](#license)

## Install

- **Docker**: `docker run ghcr.io/bebbssos/ixr <command>` — see
  [docs/DOCKER.md](docs/DOCKER.md) for compose examples (serving WebDAV/SMB/SFTP at
  once, one-shot upload containers, etc).
- **Cargo**: `cargo install internxt-cli` (crate name; the installed binary is
  still `ixr`), or `cargo binstall internxt-cli` for a prebuilt binary with no
  local compile. Plain `cargo install` only builds the
  [default feature set](#features) (SSO, WebDAV over HTTP, FUSE) —
  the prebuilt binaries above ship with almost every feature on, so to match
  them pass `--features` explicitly, e.g. `cargo install internxt-cli --features
  webdav-tls,smb,nfs,sftp,vpn,termimage,self-update`. See [Features](#features) for
  the full flag list. `self-update` is deliberately never a default — see its
  entry in [Features](#features) for why.
- **AUR** (Arch Linux): `ixr-bin`, e.g. `yay -S ixr-bin`.
- **Prebuilt binary**: download an archive from the
  [releases page](https://github.com/Bebbssos/internxt-cli-rust/releases) for your
  platform, or grab the latest directly:

  | OS | Downloads |
  |---|---|
  | Windows | [x86_64](https://github.com/Bebbssos/internxt-cli-rust/releases/latest/download/ixr-x86_64-pc-windows-msvc.zip) · [ARM64](https://github.com/Bebbssos/internxt-cli-rust/releases/latest/download/ixr-aarch64-pc-windows-msvc.zip) |
  | Linux (glibc) | [x86_64](https://github.com/Bebbssos/internxt-cli-rust/releases/latest/download/ixr-x86_64-unknown-linux-gnu.tar.gz) · [ARM64](https://github.com/Bebbssos/internxt-cli-rust/releases/latest/download/ixr-aarch64-unknown-linux-gnu.tar.gz) |
  | Linux (musl) | [x86](https://github.com/Bebbssos/internxt-cli-rust/releases/latest/download/ixr-i686-unknown-linux-musl.tar.gz) · [ARMv7](https://github.com/Bebbssos/internxt-cli-rust/releases/latest/download/ixr-armv7-unknown-linux-musleabihf.tar.gz) · [ARMv6](https://github.com/Bebbssos/internxt-cli-rust/releases/latest/download/ixr-arm-unknown-linux-musleabihf.tar.gz) |
  | macOS | [Apple Silicon](https://github.com/Bebbssos/internxt-cli-rust/releases/latest/download/ixr-aarch64-apple-darwin.tar.gz) · [Intel](https://github.com/Bebbssos/internxt-cli-rust/releases/latest/download/ixr-x86_64-apple-darwin.tar.gz) |
  | FreeBSD | [x86_64](https://github.com/Bebbssos/internxt-cli-rust/releases/latest/download/ixr-x86_64-unknown-freebsd.tar.gz) |

  These links always resolve to the latest stable release. Every build ships
  with the full feature set (SSO, WebDAV+TLS, SMB/NFS/SFTP servers, VPN
  proxy, self-update, in-terminal thumbnails, FUSE/WinFSP mount) except: Windows drops in-terminal
  thumbnails (no Kitty/iTerm2 graphics protocol on Windows). See
  [FUSE/mount support](#fusewinfsp-mount-support) for the full build/runtime
  matrix — mount still needs the OS driver package installed locally to
  actually run (macFUSE on macOS, WinFsp on Windows), same as any FUSE-based
  tool. `ixr update` self-updates in place afterwards (standalone-binary
  installs only — the other methods above manage updates themselves).
- **From source**: see [Build](#build) below.

## Build

```sh
cargo build --release
# binary at target/release/ixr (SSO + WebDAV over HTTP + FUSE enabled by default)

# stack multiple features on top of the defaults with a comma-separated list —
# here: HTTPS for WebDAV, plus the SMB and SFTP serve backends (all off by default):
cargo build --release --features webdav-tls,smb,sftp

# --no-default-features drops the defaults (sso, webdav, fuse, dotenv)
# instead of adding to them — combine with --features to build a minimal
# binary with only what you want, e.g. WebDAV-only, no SSO/FUSE:
cargo build --release --no-default-features --features webdav
```

See [Features](#features) below for what each `--features`/`--no-default-features`
flag enables and disables.

A multi-arch Docker image is available — see the [`Dockerfile`](Dockerfile)
at the repo root for how it's built, or [docs/DOCKER.md](docs/DOCKER.md) for usage
examples.

## Features

Cargo feature flags gate optional command surface, mainly to keep the default
binary small and dependency-light. `default = ["sso", "webdav", "fuse",
"dotenv"]`.

| Feature | Default | Enables | Notes |
|---|---|---|---|
| `dotenv` | on | Loads a `.env` file from the current directory at startup, before arg/env parsing | Lets `IXR_*` env-backed flags live in `.env` instead of the real environment. Never overrides vars already set in the environment. Pulls in `dotenvy`. |
| `sso` | on | Web-based SSO flow for `login`/`login-sso` (local callback server + browser launch) | Without it, `login` falls back to the legacy flow and `login-sso` errors. Pulls in `axum` + `open`. |
| `webdav` | on | `serve webdav` over plain HTTP | Pulls in `axum` + `tokio-util` + `mime_guess`. |
| `webdav-tls` | off | HTTPS for `serve webdav` (`--webdav-https`) | Requires `webdav`. Pulls in `axum-server` + `rustls-pemfile` + `rcgen` (self-signed or your own cert/key). |
| `fuse` | on | `mount`, `serve fuse` | One feature, all platforms — picks `fuser` (Unix) or `winfsp_wrs`/WinFSP (Windows) per target_os. Build/runtime requirements differ sharply by OS; see [FUSE/WinFSP mount support](#fusewinfsp-mount-support). |
| `smb` | off | `serve smb` — SMB2/3 share | Experimental. All platforms. Built on a fork of the `smb-server` crate. |
| `nfs` | off | `serve nfs` — NFSv3 export | Experimental. All platforms. |
| `sftp` | off | `serve sftp` — SFTP over SSH | Experimental. All platforms. Pulls in `russh` + `russh-sftp`. |
| `vpn` | off | `vpn locations`, `vpn proxy` — a local HTTP(S)/SOCKS5 proxy through the Internxt VPN | New — no official equivalent (ships as a browser extension only). No extra deps. See [`vpn proxy`](#vpn-proxy) for what this is (and isn't). |
| `termimage` | off | `thumbnail display` — inline terminal image rendering | Pulls in `viuer` + `image`. Kitty/iTerm2 graphics protocol, with a Unicode half-block fallback. |
| `self-update` | off | `ixr update` — replace the running binary with the latest GitHub release | Off by default — a self-built binary should update by rebuilding. The GitHub release workflow turns it on for every standalone-binary target; AUR's `ixr-bin` reuses that binary so it has it too, while Docker builds its own binary without it. Pulls in `self_update` + `semver`. |

## FUSE/WinFSP mount support

`mount`/`serve fuse` is one Cargo feature (`fuse`) everywhere, but the library
backing it — and what it needs to build and run — differs sharply by OS. None
of these are things `ixr` can bundle into the binary: they're kernel-level
filesystem drivers, so the OS driver package has to be installed on the
machine regardless of how `ixr` itself was built or installed (same
constraint every FUSE-based tool has — rclone, sshfs, etc.).

| OS | To build | To run | Notes |
|---|---|---|---|
| Linux (glibc & musl) | Nothing extra — `fuser`'s pure-rust mount path talks to `/dev/fuse` directly, no libfuse headers needed. | [`fuse3`](https://github.com/libfuse/libfuse) package (provides `/dev/fuse` + the `fusermount3` helper); most distros ship it, install via your package manager (e.g. `apt install fuse3`, `dnf install fuse3`, `pacman -S fuse3`). | CI-verified on Linux x86_64/ARM64 (glibc) and the three musl cross targets. |
| macOS | [macFUSE](https://osxfuse.github.io/) headers (provides the `fuse.pc` pkg-config file `fuser`'s build.rs probes for) — `brew install --cask macfuse` (no reboot/approval needed just to build). | macFUSE, plus approving its kernel extension once in System Settings → Privacy & Security (a GUI step — can't be scripted). | CI-built into the official release binaries (`brew install --cask macfuse` on the `macos-latest` runner, same as upstream `fuser`'s own CI). |
| FreeBSD | Nothing extra — same pure-rust path as Linux. | [`fusefs-libs3`](https://cgit.freebsd.org/ports/tree/sysutils/fusefs-libs3) (`pkg install fusefs-libs3`) + `kldload fusefs` if the module isn't auto-loaded. | Official release binaries build with `fuse` enabled. |
| Windows | [WinFsp](https://winfsp.dev/) installed (its build.rs reads the `WinFsp\InstallDir` registry key) — build must run natively on Windows, this cannot be cross-compiled from another OS. | [WinFsp](https://winfsp.dev/) installed (the driver + `winfsp-x64.dll`, delay-loaded so `ixr` finds it via the registry rather than needing it next to the exe). | Uses [`winfsp_wrs`](https://github.com/Scille/winfsp_wrs) (MIT), not SnowflakePowered's `winfsp`/`winfsp-sys` crates (GPL-3.0 — wrong license for this project). Mount target can be a drive letter (`X:`) or an empty directory. |

If the driver is missing at runtime, `mount`/`serve fuse` doesn't crash — it
prints a normal `✕ Error: failed to mount at <path>: ...` and exits 1, with a
hint pointing back to this table when the underlying error looks like a
missing-driver situation (as opposed to e.g. a bad mountpoint path).

## Global flags

Every command accepts:

- `--json` — print a single JSON result object and suppress progress/status
  output. See each command's "JSON output" below for its shape.
- `-x, --non-interactive` (env `IXR_NONINTERACTIVE`) — never prompt for
  input; error out instead when a required value is missing.
- `--no-timeout` (env `IXR_NO_TIMEOUTS`) — disable the idle-read timeout on
  network transfers (uploads/downloads). Use if a slow `--stdin` producer or
  `--stdout` consumer trips a false timeout on an otherwise-healthy transfer
  over a slow link. Connect timeout stays on regardless — a hung connection
  attempt is unrelated to transfer speed and should still fail fast.

IDs are Drive UUIDs. Most commands that take an id also accept a `--path`
(or `--dest-path` / `--remote-path`) alternative — give one or the other, not
both. Where a destination/parent folder id is optional, leaving it empty
targets your root folder (or workspace root, if a workspace is active).

## Path syntax

A normal Drive path looks like `/Documents/report.pdf` (the leading `/`
is optional — `Documents/report.pdf` means the same thing). A path like this
resolves in one request whatever its depth (two when the command accepts either
a file or a folder there); the `//` forms below are walked one folder at a time
instead, so they cost one request per component.

A path starting with `//` is a special root with two entries: `drive` (your
normal Drive root) and `backups` (your [backup devices](#backups-devices-list)).
Walk into either like any other folder:

- `//drive/Documents/report.pdf` — same as `/Documents/report.pdf`.
- `//backups/<device>/Documents/report.pdf` — walks into a specific backup
  device's folder tree. `<device>` matches by uuid first, then
  case-insensitively by name. `id-from-path`/`path-from-id` (`get-id`/
  `get-path`) understand this in both directions.

`//` and `//backups` **on their own** (no further path) are virtual,
**read-only** groupings with no real Drive id — `//` lists `drive`/`backups`,
`//backups` lists your devices. Supported by [`list`](#list),
[`download folder`](#download-folder),
[`compare folder`](#compare-file--compare-folder),
[`sync down`](#sync-up--sync-down), [`mount`](#mount), and
[`serve`](#serve), fanning out into one subfolder per real child (`drive/`,
`backups/<device>/`). Anything that writes (`move`, `create folder`,
`sync up`, an upload destination, …) errors on them — there's no real
folder to write to. A real folder reached *through* one, e.g.
`//backups/<device>/Documents`, is a normal, fully writable Drive folder.

## Commands

The **space-separated** form (`ixr upload file`) is the primary syntax. Every
command grouped under a parent below also has an equivalent flat/hyphenated
**alias** (`ixr upload-file`) for drop-in compatibility with the official CLI —
both always work. Parent rows (in **bold**) just print help for their group.

| Command | Description | Aliases | Notes |
|---|---|---|---|
| [`login`](#login) | Log in — alias for `login-sso` (or `login-legacy` without `sso`). | — | Matches official's SSO-only `login`. |
| [`login-legacy`](#login-legacy) | Log in with email + password. | — | |
| [`login-sso`](#login-sso) | Force the web-based SSO flow. | — | New; needs `sso` (default on). |
| [`logout`](#logout) | Log out the current/targeted account. | — | |
| [`whoami`](#whoami) | Show the active/targeted account. | — | |
| **[`accounts`](#accounts-list)** | Manage logged-in accounts | | New — no official equivalent. |
| &nbsp;&nbsp;[`accounts list`](#accounts-list) | List every logged-in account. | — | |
| &nbsp;&nbsp;[`accounts switch`](#accounts-switch) | Switch the active account. | — | |
| [`usage`](#usage) | Plan, used space, upload limit. | `account`, `account-info` | New — no official equivalent. |
| [`list`](#list) | List a folder's contents. | — | |
| [`recents`](#recents) | Most recently modified files, account-wide, with the folder each lives in. | `recent` | New — no official equivalent (drive-web has a "Recents" view). |
| [`tree`](#tree) | Print a folder's whole subtree, indented. | — | New — no official equivalent. One request for the whole subtree. |
| [`du`](#du) | A folder's recursive size and file count. | — | New — no official equivalent. Counted server-side, one request. |
| **[`create`](#create-folder)** | Create a folder | | |
| &nbsp;&nbsp;[`create folder`](#create-folder) | Create a folder. | `create-folder` | |
| **[`upload`](#upload-file)** | Upload a file or folder | | |
| &nbsp;&nbsp;[`upload file`](#upload-file) | Upload a single file (streaming). | `upload-file` | |
| &nbsp;&nbsp;[`upload folder`](#upload-folder) | Recursively upload a folder tree. | `upload-folder` | |
| **[`download`](#download-file)** | Download a file or folder | | |
| &nbsp;&nbsp;[`download file`](#download-file) | Download + decrypt a file. | `download-file` | |
| &nbsp;&nbsp;[`download folder`](#download-folder) | Recursively download a folder tree. | `download-folder` | New — official downloads single files only. |
| **[`move`](#move-file--move-folder)** | Move a file or folder | | |
| &nbsp;&nbsp;[`move file`](#move-file--move-folder) | Move a file into a folder. | `move-file` | |
| &nbsp;&nbsp;[`move folder`](#move-file--move-folder) | Move a folder into a folder. | `move-folder` | |
| **[`rename`](#rename-file--rename-folder)** | Rename a file or folder | | |
| &nbsp;&nbsp;[`rename file`](#rename-file--rename-folder) | Rename a file. | `rename-file` | |
| &nbsp;&nbsp;[`rename folder`](#rename-file--rename-folder) | Rename a folder. | `rename-folder` | |
| **[`trash`](#trash-file--trash-folder)** | Manage the trash | | |
| &nbsp;&nbsp;[`trash file`](#trash-file--trash-folder) | Move a file to the trash. | `trash-file` | |
| &nbsp;&nbsp;[`trash folder`](#trash-file--trash-folder) | Move a folder to the trash. | `trash-folder` | |
| &nbsp;&nbsp;[`trash list`](#trash-list) | List the trash contents. | `trash-list` | |
| &nbsp;&nbsp;[`trash restore file`](#trash-restore-file--trash-restore-folder) | Restore a trashed file. | `trash-restore-file` | |
| &nbsp;&nbsp;[`trash restore folder`](#trash-restore-file--trash-restore-folder) | Restore a trashed folder. | `trash-restore-folder` | |
| &nbsp;&nbsp;[`trash clear`](#trash-clear) | Empty the trash permanently. | `trash-clear` | |
| **[`delete`](#delete-file--delete-folder)** | Delete a file or folder | | |
| &nbsp;&nbsp;[`delete file`](#delete-file--delete-folder) | Trash a file (`--permanent` to hard-delete). | `delete-file` | New — trash alias + `--permanent`. |
| &nbsp;&nbsp;[`delete folder`](#delete-file--delete-folder) | Trash a folder (`--permanent` to hard-delete). | `delete-folder` | New — trash alias + `--permanent`. |
| &nbsp;&nbsp;[`delete permanently file`](#delete-permanently-file--delete-permanently-folder) | Permanently delete a file. | `delete-permanently-file` | |
| &nbsp;&nbsp;[`delete permanently folder`](#delete-permanently-file--delete-permanently-folder) | Permanently delete a folder. | `delete-permanently-folder` | |
| **[`workspaces`](#workspaces-list)** | Manage workspaces | | |
| &nbsp;&nbsp;[`workspaces list`](#workspaces-list) | List your workspaces. | `workspaces-list` | |
| &nbsp;&nbsp;[`workspaces use`](#workspaces-use) | Set the active workspace. | `workspaces-use` | |
| &nbsp;&nbsp;[`workspaces unset`](#workspaces-unset) | Unset the active workspace. | `workspaces-unset` | |
| &nbsp;&nbsp;[`workspaces info`](#workspaces-info) | Show a workspace's details. | — | New — no official equivalent. |
| &nbsp;&nbsp;[`workspaces members`](#workspaces-members) | List a workspace's members. | — | New — no official equivalent. |
| &nbsp;&nbsp;[`workspaces teams`](#workspaces-teams) | List a workspace's teams. | — | New — no official equivalent. |
| &nbsp;&nbsp;[`workspaces usage`](#workspaces-usage) | Show a workspace's space usage. | — | New — no official equivalent. |
| &nbsp;&nbsp;[`workspaces invitations`](#workspaces-invitations) | List invitations awaiting your response. | — | New — no official equivalent. |
| **[`sync`](#sync-up--sync-down)** | One-way sync | | New — no official equivalent. |
| &nbsp;&nbsp;[`sync up`](#sync-up--sync-down) | Push: local → remote. | `sync-up` | |
| &nbsp;&nbsp;[`sync down`](#sync-up--sync-down) | Pull: remote → local. | `sync-down` | |
| **[`compare`](#compare-file--compare-folder)** | Compare local vs. remote | | New — no official equivalent. |
| &nbsp;&nbsp;[`compare file`](#compare-file--compare-folder) | Compare a local file against a remote file. | — | |
| &nbsp;&nbsp;[`compare folder`](#compare-file--compare-folder) | Recursively compare a local folder against a remote folder. | — | |
| **[`backups`](#backups-devices-list)** | Manage backup devices; browse/download what's backed up | | New — no official equivalent (backups are desktop-app-only there). |
| &nbsp;&nbsp;[`backups devices list`](#backups-devices-list) | List your backup devices. | — | |
| &nbsp;&nbsp;[`backups devices create`](#backups-devices-create) | Create a backup device. | — | |
| &nbsp;&nbsp;[`backups devices rename`](#backups-devices-rename) | Rename a backup device. | — | |
| &nbsp;&nbsp;[`backups devices delete`](#backups-devices-delete) | Delete a backup device and everything backed up to it. | — | **Cannot be undone** — backups have no trash. |
| &nbsp;&nbsp;[`backups list`](#backups-list--backups-download--backups-get-id) | List what's backed up to a device (or a subfolder inside it). | — | |
| &nbsp;&nbsp;[`backups download`](#backups-list--backups-download--backups-get-id) | Download everything backed up to a device (or a subfolder). | — | |
| &nbsp;&nbsp;[`backups get-id`](#backups-list--backups-download--backups-get-id) | Print the uuid of a device or an item inside it. | — | |
| **[`shared`](#shared-list)** | Inspect and revoke sharing | | New — no official equivalent (sharing is web-app-only there). Read-only apart from `revoke`; **creating** a share isn't supported yet. |
| &nbsp;&nbsp;[`shared list`](#shared-list) | List shared items — both directions, or one with `--by-me`/`--with-me`. | — | |
| &nbsp;&nbsp;[`shared info`](#shared-info) | Show how one file/folder is shared, and who's invited. | — | |
| &nbsp;&nbsp;[`shared invites`](#shared-invites) | List the sharing invitations waiting for you. | — | |
| &nbsp;&nbsp;[`shared roles`](#shared-roles--shared-domains) | List the roles a share recipient can be given. | — | |
| &nbsp;&nbsp;[`shared domains`](#shared-roles--shared-domains) | List the domains public share links are served from. | — | |
| &nbsp;&nbsp;[`shared revoke`](#shared-revoke) | Stop sharing a file or folder. | — | |
| [`serve`](#serve) | Serve Drive over WebDAV/FUSE/SMB/NFS/SFTP (foreground). | — | Needs ≥1 of `webdav`,`fuse`,`smb`,`nfs`,`sftp`. WebDAV mirrors official; rest new. |
| [`mount`](#mount) | Mount Drive as a local FS via FUSE/WinFSP. | — | New; needs `fuse` (default on). |
| **[`vpn`](#vpn-locations)** | Internxt VPN: locations, local proxy | | New — no official equivalent (ships as a browser extension only). Needs `vpn` (off by default in source builds, on in Docker/prebuilt binaries). |
| &nbsp;&nbsp;[`vpn locations`](#vpn-locations) | List VPN locations available on your plan. | — | |
| &nbsp;&nbsp;[`vpn proxy`](#vpn-proxy) | Run a local HTTP(S)/SOCKS5 proxy through the VPN (foreground). | — | Not a full tunnel — see below. |
| [`id-from-path`](#id-from-path) | Print the uuid at a Drive path. | `get-id` | New — no official equivalent. |
| [`path-from-id`](#path-from-id) | Print the Drive path of a uuid. | `get-path` | New — no official equivalent. |
| **[`thumbnail`](#thumbnail)** | Manage a file's thumbnail | `thumbnails` | New — official auto-generates only. |
| &nbsp;&nbsp;[`thumbnail generate`](#thumbnail) | (Re)generate from the file's image. | — | |
| &nbsp;&nbsp;[`thumbnail upload`](#thumbnail) | Upload a custom thumbnail image. | — | |
| &nbsp;&nbsp;[`thumbnail download`](#thumbnail) | Download the current thumbnail. | — | |
| &nbsp;&nbsp;[`thumbnail display`](#thumbnail) | Render inline in the terminal. | — | Needs `termimage` (default off). |
| **[`versions`](#versions)** | Browse a file's version history | `version` | New — no official equivalent (drive-web feature). |
| &nbsp;&nbsp;[`versions list`](#versions) | List a file's stored versions. | — | |
| &nbsp;&nbsp;[`versions restore`](#versions) | Make a stored version the current content. | — | **Cannot be undone** — drops newer versions. |
| &nbsp;&nbsp;[`versions delete`](#versions) | Permanently delete one stored version. | — | **Cannot be undone.** |
| [`update`](#update) | Update the running binary to the latest GitHub release. | — | New — no official equivalent. Standalone-binary installs only; needs `self-update` (off by default, on in the prebuilt release binaries). |

## Command reference

### `login`

Logs in. An **alias for [`login-sso`](#login-sso)** when built with the `sso`
feature (default, matching the official CLI's SSO-only `login`); otherwise an
**alias for [`login-legacy`](#login-legacy)**. Use `login-sso` or `login-legacy`
directly to force a specific flow.

Flags: `--host <HOST>`, `--port <PORT>` (SSO callback server address/port,
default 127.0.0.1 / a random free port); `-e/--email`, `-p/--password`,
`-w/--twofactor`, `-t/--twofactortoken` (legacy flow). The set that applies to
the active flow is used; the others are accepted but ignored. Plus
`--add`/`--replace` (see [Multiple accounts](#multiple-accounts)).

```sh
ixr login                                  # SSO: opens a browser to authenticate
ixr login --host 0.0.0.0 --port 4000       # cross-device SSO (e.g. inside a container)
```

JSON output: `{ "success": true, "message": "...", "login": <credentials> }`
on success. `login` (JSON credentials, tokens, keys) is sensitive — treat it
like a secret. On failure: `{ "success": false, "message": "..." }`.

### `login-legacy`

Same command name as the official CLI's `login-legacy`. Logs in with email +
password (+ 2FA if the account requires it). Prompts for any missing value
unless `-x/--non-interactive`.

Flags: `-e/--email`, `-p/--password`, `-w/--twofactor`, `-t/--twofactortoken`
(takes priority over `--twofactor` when both are given); `--add`/`--replace`
(see [Multiple accounts](#multiple-accounts)).

```sh
ixr login-legacy --email you@example.com     # prompts for password (+ 2FA)
ixr login-legacy -e you@example.com -p '...' -w 123456
ixr login-legacy -e another@example.com -p '...' --add   # keep both accounts
```

JSON output: same shape as [`login`](#login).

### `login-sso`

New — no official equivalent (the official CLI's plain `login` is already
SSO-only, so it has no separate `login-sso`). Forces the web-based SSO flow.
Errors if built without the `sso` feature.

Flags: `--host <HOST>`, `--port <PORT>`, `--add`/`--replace` (see
[Multiple accounts](#multiple-accounts)).

JSON output: same shape as [`login`](#login).

### `logout`

Invalidates the session server-side and removes the resolved account (the
one targeted by `IXR_USER`, else the active one — see
[Multiple accounts](#multiple-accounts)) from local storage. If it was the
active account and other accounts remain, none of them is auto-selected —
run `accounts switch` to pick one.

Flags: `--all` (log out of every stored account instead of just the resolved
one).

```sh
ixr logout
ixr logout --all
```

JSON output: `{ "success": true, "message": "User logged out successfully." }`,
or `{ "success": false, "message": "No user is currently logged in." }`.
With `--all`: `{ "success": true, "message": "Logged out of all accounts.", "accounts": [...] }`.

### `whoami`

Shows the resolved account (`IXR_USER`, else the active one). Refreshes the
session token if it's near expiry; if the session is dead, removes that
account from local storage (matching the official CLI's behaviour of logging
out on a dead session) without touching any other stored account.

JSON output: `{ "success": true, "message": "...", "login": <credentials> }`,
or `{ "success": false, "message": "You are not logged in." }`.

### `accounts list`

New — no official equivalent (the official CLI supports one account at a
time). Lists every account currently logged in on this machine, marking the
active one with `*` in the human-readable view.

No flags.

```sh
ixr accounts list
```

JSON output: `{ "success": true, "accounts": ["a@example.com", "b@example.com"], "active": "a@example.com" }`.

### `accounts switch`

New — no official equivalent. Sets the active account for subsequent
commands (until changed again, independent of any `IXR_USER` override on a
given invocation — see [Multiple accounts](#multiple-accounts)).

Flags: `-e/--email <EMAIL>` (omit for an interactive picker; errors in
`--json`/`-x` mode if omitted).

```sh
ixr accounts switch -e b@example.com
ixr accounts switch                    # interactive picker
```

JSON output: `{ "success": true, "active": "b@example.com" }`.

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
File versioning:    Enabled (up to 10 versions per file, files up to 25 MB, kept 15 days)
```

`File versioning` is the plan's entitlement, as reported by `/files/limits` —
the policy [`versions`](#versions) works within. Plans without it read
`Not available on this plan`. Each cap is dropped from the line (and reported
as `null` in JSON) when the backend doesn't state it, so a plan that only says
"enabled" prints just `Enabled`.

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
    "uploadFileLimit": 10737418240,
    "versioning": {
      "enabled": true,
      "maxVersions": 10,
      "maxFileSize": 26214400,
      "retentionDays": 15
    }
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

### `recents`

Alias: `recent`. Not an official CLI command — drive-web has a "Recents" view,
but the official CLI never exposed it. Lists the account's most recently
modified files, newest first, across every folder, in a single request
(`/files/recents`). Trashed and deleted files are filtered out server-side, so
everything listed still exists. Like the other listing commands it runs through
the workspace-scoped API client, so an active workspace applies.

Flags: `-l/--limit <N>` — how many files to list, 1 to 1000, default 50. The
endpoint's own default is 1000, which is far more than a terminal table is
useful for; raise `--limit` when you need more. Out-of-range values are a usage
error rather than a request that the server rejects. `-e/--extended` adds the
created date and the file's id, like [`list -e`](#list).

```sh
ixr recents                # the 50 most recently modified files
ixr recents --limit 200
ixr recents --extended
ixr recents --json
```

```
Name            Folder      Modified                  Size
--------------  ----------  ------------------------  --------
quarterly.xlsx  Finance     25 August, 2026 at 17:19  1.21 MB
notes.md        /           25 August, 2026 at 09:02  4.02 KB
diagram.png     Designs     24 August, 2026 at 20:08  318.5 KB
```

The `Folder` column is the **name of the containing folder**, not a full path —
this endpoint inlines the parent folder with each entry (the only file read that
does), so the name is free, while a whole path would cost a request per
ancestor. A file sitting in the account root shows `/`. Use
[`path-from-id`](#path-from-id) when you need a single file's full path, and
`--json`'s `folderUuid` when a script needs to address the folder.

JSON output:

```json
{
  "success": true,
  "recents": [
    {
      "uuid": "00000000-0000-0000-0000-000000000001",
      "plainName": "quarterly",
      "type": "xlsx",
      "size": 1268776,
      "bucket": "0000000000000000000000000",
      "fileId": "000000000000000000000000",
      "modificationTime": "2026-08-25T17:19:44.000Z",
      "creationTime": "2026-08-25T17:19:44.000Z",
      "createdAt": "2026-08-25T17:19:45.114Z",
      "updatedAt": "2026-08-25T17:19:45.114Z",
      "folderUuid": "00000000-0000-0000-0000-0000000000ff",
      "folder": {
        "uuid": "00000000-0000-0000-0000-0000000000ff",
        "plainName": "Finance"
      }
    }
  ],
  "hasUploadedFiles": true
}
```

`hasUploadedFiles` separates a genuinely empty account from one that simply has
nothing recent — the human-readable view says
`No recent files — this account has never uploaded anything.` in that case. A
non-empty list answers it for free; only an empty list costs the extra
`/users/me/upload-status` request. Inside a workspace the field is `null` and
the plain `No recent files.` wording is used: that endpoint is personal-account
only, so an empty workspace listing says nothing about it.
### `du`

Shows how much a folder holds — total size and file count for the **whole
subtree**, counted server-side in a single request. Nothing is walked and
nothing is downloaded, so it costs the same on a folder with three files as on
one with thirty thousand.

Argument: `[FOLDER]` — a Drive path (`/Documents`) or a folder id (uuid);
defaults to the account (or workspace) root. A uuid-shaped value is taken as an
id, so write a leading `/` if a folder is literally named like a uuid. The
`//drive` and `//backups/<device>` [path escapes](#path-syntax) work here too;
the bare `//` and `//backups` groupings don't — they aren't folders and have no
size of their own.

Flags: `-c/--children` (break the total down by direct subfolder, largest
first), `-b/--bytes` (raw byte counts instead of human-readable sizes).

```sh
ixr du                       # the whole account
ixr du /Documents
ixr du /Documents --children
ixr du /Documents --bytes --json
```

```
$ ixr du /Documents
1.21 GB  482 files  /Documents

$ ixr du /Documents --children
Size      Files  Name
--------  -----  --------
900 MB    310    Reports
280 MB    140    Archive
30.5 MB   28     Scans
9.5 MB    4      .
1.21 GB  482 files  /Documents
```

The `.` row is what sits **directly** in the folder rather than in a subfolder:
the parent's total minus its children's. It's derived from numbers already
fetched, not an extra request, and is left out entirely when any of those
numbers is an estimate — one estimate minus another isn't worth printing.

`--children` costs one request per direct subfolder (run several at a time), so
it's the one part of this command that scales with folder count. The plain form
is always a single request.

**Estimates:** the endpoint estimates for large folders and says which number it
guessed. Those are marked `(estimate)` in the human output and reported as
`sizeExact` / `filesExact` in `--json` — the account root of a large account
typically comes back estimated. Numbers without the marker are exact.
[`tree --stats`](#tree) prints the same endpoint's answer next to a
tree-derived count, which is a way to see how far off an estimate is.

JSON output:

```json
{
  "success": true,
  "folder": "/Documents",
  "uuid": "00000000-0000-0000-0000-000000000001",
  "size": 1298765432,
  "files": 482,
  "sizeExact": true,
  "filesExact": true,
  "children": [
    {
      "name": "Reports",
      "uuid": "00000000-0000-0000-0000-000000000002",
      "size": 943718400,
      "files": 310,
      "sizeExact": true,
      "filesExact": true
    }
  ]
}
```

`children` is `null` without `--children`. Sizes are always raw bytes in JSON;
`--bytes` only affects the human-readable table.

### `tree`

Prints a folder's whole subtree as an indented tree. Unlike [`list`](#list),
which pages through one folder at a time, the entire subtree — nested folders
and their files, however deep — arrives in a **single request**.

Argument: `[FOLDER]` — a Drive path (`/Documents/Reports`) or a folder id
(uuid); defaults to the root folder. A uuid-shaped value is taken as an id, so
write a leading `/` if a folder is literally named like a uuid. The `//drive`
and `//backups/<device>` [path escapes](#path-syntax) work here too; the bare
`//` and `//backups` groupings don't — they aren't folders, so they have no
subtree of their own.

Flags: `-d/--depth <N>` (print only N levels below the starting folder),
`--folders-only` (leave files out of the tree; they still count towards the
totals), `-e/--extended` (append each file's size), `--stats` (see below).

`--depth` is purely a display filter: the whole subtree is fetched either way,
so it saves output, not requests. A folder whose contents are cut off shows
what's hidden in brackets.

```sh
ixr tree                                     # the whole account root — but see "Large subtrees"
ixr tree /Documents/Reports
ixr tree /Documents --depth 2 --folders-only
ixr tree <folder-uuid> --stats --json
```

```
/Documents
├── Reports
│   ├── 2024
│   │   └── summary.pdf
│   └── draft.md
├── archive.zip
└── notes.txt

2 folders, 4 files, 5.1 MB
```

The totals always cover the whole subtree, whatever `--depth` and
`--folders-only` leave on screen. They're computed from the tree that was
already fetched, so they're exact and cost nothing extra. `--stats` adds one
request to the folder-stats endpoint and prints its file count and total size
too; that endpoint estimates for large folders (marked `(estimate)`), and when
the two sources disagree the difference is printed rather than hidden.

**Large subtrees:** the server builds the whole response eagerly and gives up
on very big ones — an upstream error or a timeout, not a short answer. The
root folder of a large account is a realistic example. When that happens,
start from a subfolder, or use `list` to walk a level at a time.

JSON output: `{ "success": true, "root": "<folder>", "tree": { "uuid", "name",
"type": "folder", "folders": [...], "files": [{ "uuid", "name", "plainName",
"type", "size" }] }, "totals": { "folders", "files", "size" } }`, plus
`"stats"` with `--stats`. `--depth` and `--folders-only` filter the JSON the
same way they filter the text, and a folder cut off by `--depth` carries
`"truncated": true` in place of its children.

### `create folder`

Also invocable via the flat alias `create-folder`. Creates a folder.

Flags: `-n/--name <NAME>` (required), `-i/--id <PARENT_ID>` (default: root),
`-p/--path <PATH>` (alternative to `--id`).

```sh
ixr create-folder -n "Reports" -i <parent-uuid>
ixr create-folder -n "Reports" -p /Documents
```

JSON output: `{ "success": true, "folder": <DriveFolderData> }`.

### `upload file`

Also invocable via the flat alias `upload-file`. Uploads a single file
(streaming; single-part or multipart depending on size).

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

### `upload folder`

Also invocable via the flat alias `upload-folder`. Recursively uploads a
folder tree (concurrent file uploads).

Flags: `-f/--folder <PATH>` (required), `-i/--destination <FOLDER_ID>`
(default: root), `--dest-path <PATH>` (alternative to `--destination`),
`--exclude-empty-files` (skip 0-byte files client-side instead of uploading
them — see below), plus the [upload-limit flags](#upload-size-limit).

```sh
ixr upload-folder -f ./my-folder                # -i for a destination folder
ixr upload-folder -f ./my-folder --dest-path /Backups
ixr upload-folder -f ./my-folder --exclude-empty-files   # skip 0-byte files
```

Empty (0-byte) files are included by default and uploaded like any other
file. Internxt's free/legacy plans reject them server-side (`HTTP 402
Payment Required`); on Ultimate/paid plans they upload fine. If any file
fails for any reason — including that 402 — the command exits non-zero and
reports which file and why, instead of silently omitting it. Pass
`--exclude-empty-files` to skip 0-byte files up front and avoid that failure
entirely on plans that don't support them.

JSON output: `{ "success": true, "folder": { "uuid": "..." }, "totalBytes": N, "uploadTimeMs": N }`
on full success. If one or more files failed to upload, the command instead
exits with a non-zero status and an error message naming each failed file
and its reason (`{ "success": false, "message": "..." }` in `--json` mode).

### `download file`

Also invocable via the flat alias `download-file`. Downloads and decrypts
a file, streaming to disk (or stdout).

Flags: `-i/--id <FILE_ID>`, `-p/--path <PATH>` (alternative to `--id`),
`-d/--directory <DIR>` (default: current dir), `-o/--overwrite`, `--stdout`
(write decrypted bytes to stdout instead of a file; status goes to stderr so
it never mixes into piped data), `--legacy-write` (see below).

By default (when writing to disk, i.e. not `--stdout`), the download streams
into a temp sibling file (`.<name>.inxt-<random>.part`, next to the
destination) and is renamed into place only once it completes successfully;
if anything fails partway (network drop, a bad chunk, Ctrl-C), the temp file
is removed instead of being left behind. This means the destination path
either doesn't exist or holds a complete file — never a silent truncated
one. Pass `--legacy-write` to restore the old behavior (also the official
CLI's behavior): write directly to the destination path with no cleanup on
error, so an interrupted download can leave a partial file exactly at the
filename you expected the complete download at.

```sh
ixr download-file -i <file-uuid> -d ./out --overwrite
ixr download-file -p /Documents/report.pdf -d ./out
ixr download-file -i <file-uuid> --stdout > file.bin
ixr download-file -i <file-uuid> -d ./out --legacy-write
```

JSON output: `{ "success": true, "path": "<local path>" }` when written to
disk. **With `--stdout`, no JSON object is emitted at all** (only a status
line on stderr in non-JSON mode) — the file bytes own stdout instead.

### `download folder`

Also invocable via the flat alias `download-folder`. New — the official CLI only downloads
single files. Recursively downloads and decrypts a folder tree into a
subfolder named after the Drive folder (it reuses the `sync-down` engine
under the hood).

Flags: `-i/--id <FOLDER_ID>`, `-p/--path <PATH>` (alternative to `--id`),
`-d/--directory <DIR>` (default: current dir — a subfolder named after the
Drive folder is created inside it), `-o/--overwrite` (merge into an
already-existing, non-empty destination folder). It enumerates the remote
folder the way [`sync`](#sync-up--sync-down) does, `IXR_FOLDER_TREE=0`
included.

```sh
ixr download-folder -i <folder-uuid> -d ./out
ixr download-folder -p /Documents/Reports --overwrite
```

JSON output: the [`sync-down`](#sync-up--sync-down) result object
(`transferred`, `deleted`, `skipped`, `failed`, `actions`, …).

### `move file` / `move folder`

Also invocable via the flat aliases `move-file`/`move-folder`.
Moves a file or folder into a destination folder.

Flags: `-i/--id <ID>`, `-p/--path <PATH>` (alternative to `--id`),
`-d/--destination <FOLDER_ID>` (default: root), `--dest-path <PATH>`
(alternative to `--destination`).

```sh
ixr move-file -i <file-uuid> -d <folder-uuid>
ixr move-folder -p /Old/Name -d <folder-uuid>
```

JSON output: `move-file` → `{ "success": true, "file": <DriveFileData> }`.
`move-folder` → `{ "success": true, "folder": <DriveFolderData> }`.

### `rename file` / `rename folder`

Also invocable via the flat aliases `rename-file`/`rename-folder`. Renames a file or folder (for files, name/extension are split
automatically).

Flags: `-i/--id <ID>`, `-p/--path <PATH>` (alternative to `--id`), `-n/--name
<NAME>` (required).

```sh
ixr rename-file -i <file-uuid> -n "new-name.txt"
ixr rename-folder -p /Old/Name -n "New Name"
```

JSON output: `rename-file` → `{ "success": true, "file": { "uuid", "plainName", "type" } }`.
`rename-folder` → `{ "success": true, "folder": { "uuid", "plainName" } }`.

### `trash file` / `trash folder`

Also invocable via the flat aliases `trash-file`/`trash-folder`.
Moves a file or folder to the trash.

Flags: `-i/--id <ID>`, `-p/--path <PATH>` (alternative to `--id`).

JSON output: `{ "success": true, "file": { "uuid": "..." } }` or
`{ "success": true, "folder": { "uuid": "..." } }`.

### `trash list`

Also invocable via the flat alias `trash-list`. Lists the contents of the
trash.

Flags: `-e/--extended`.

JSON output: `{ "success": true, "list": { "folders": [...], "files": [...] } }`
(same shape as [`list`](#list)).

### `trash restore file` / `trash restore folder`

Also invocable via the flat aliases `trash-restore-file`/`trash-restore-folder`. Restores a trashed file or folder into
a destination folder.

Flags: `-i/--id <ID>`, `-d/--destination <FOLDER_ID>` (default: root),
`--dest-path <PATH>` (alternative to `--destination`).

JSON output: `{ "success": true, "file": <DriveFileData> }` or
`{ "success": true, "folder": <DriveFolderData> }`.

### `trash clear`

Also invocable via the flat alias `trash-clear`. Empties the trash
permanently — **cannot be undone**.
Prompts for confirmation unless `--force` (required in `--json`/non-interactive
mode).

Flags: `-f/--force`.

```sh
ixr trash-clear --force
```

JSON output: `{ "success": true, "message": "Trash emptied successfully." }`.

### `delete permanently file` / `delete permanently folder`

Also invocable via the flat aliases `delete-permanently-file`/`delete-permanently-folder`.
Permanently deletes a file or folder — **cannot be undone**.

Flags: `-i/--id <ID>`.

JSON output: `{ "success": true, "message": "File permanently deleted successfully" }`
or `{ "success": true, "message": "Folder permanently deleted successfully" }`.

### `delete file` / `delete folder`

Also invocable via the flat aliases `delete-file`/`delete-folder`. New — a convenience alias
that trashes a file or folder by default, or permanently deletes it with
`--permanent`. The official CLI has `delete permanently file|folder` but no
plain `delete file|folder`. Without `--permanent` this is equivalent to
[`trash-file`/`trash-folder`](#trash-file--trash-folder); with it, to
[`delete-permanently-file`/`delete-permanently-folder`](#delete-permanently-file--delete-permanently-folder).

Flags: `-i/--id <ID>`, `-p/--path <PATH>` (alternative to `--id`),
`--permanent` (hard-delete instead of trashing — **cannot be undone**).

```sh
ixr delete-file -p /Documents/old.txt              # move to trash
ixr delete-folder -i <folder-uuid> --permanent     # hard-delete, no undo
```

JSON output: trashing → `{ "success": true, "file": { "uuid": "..." } }` /
`{ "success": true, "folder": { "uuid": "..." } }`; with `--permanent` → the
`delete-permanently-*` message shape above.

### `workspaces list`

Also invocable via the flat alias `workspaces-list`. Lists the
workspaces you belong to.

Flags: `-e/--extended` (owner, address, created-at in the human-readable view).

JSON output: `{ "success": true, "list": { "workspaces": [...] } }` (always the
full objects, regardless of `--extended`).

### `workspaces use`

Also invocable via the flat alias `workspaces-use`. Sets the active
workspace for subsequent commands —
switches where drive calls and transfers route (its own bucket, network
credentials and mnemonic).

Flags: `-i/--id <WORKSPACE>` (the workspace id, its name, or the number of its
row in `workspaces list`), `-p/--personal` (switch back to your personal drive
space; conflicts with `--id`).

```sh
ixr workspaces-use -i <workspace-id>
ixr workspaces-use -i "Acme Design"    # by name
ixr workspaces-use -i 2                # by row number in `workspaces list`
ixr workspaces-use --personal
```

JSON output: `{ "success": true, "workspace": { "id", "name", "bucket", "rootFolderId" } }`.

### `workspaces unset`

Also invocable via the flat alias `workspaces-unset`. Unsets the active
workspace (equivalent to
`workspaces-use --personal`). No flags.

JSON output: `{ "success": true, "message": "Personal drive space selected successfully." }`.

### `workspaces info`

New — no official equivalent (the official CLI stops at `list`/`use`/`unset`;
these views exist only in the web app). Shows one workspace's record, followed
by any workspace you own that still has to be set up.

Takes an optional positional `WORKSPACE` — its id, its name, or the number of
its row in `workspaces list`. Omit it to use the active workspace (whatever
`workspaces use` selected, or `IXR_WORKSPACE_ID` for a single invocation). With
no argument and no active workspace, the command errors instead of guessing.
No other flags.

```sh
ixr workspaces info                 # the active workspace
ixr workspaces info "Acme Design"   # by name
ixr workspaces info 2               # by row number in `workspaces list`
```

```
Name:             Acme Design
Workspace ID:     11111111-2222-3333-4444-555555555555
Description:      Design team space
Owner ID:         66666666-7777-8888-9999-000000000000
Setup completed:  yes
Created at:       3 March, 2026 at 10:15
```

JSON output: `{ "success": true, "workspaceId": "...", "workspace": {...},
"pendingSetup": [...] }` — `workspace` and `pendingSetup` are the API responses
passed straight through.

### `workspaces members`

New — no official equivalent. Lists who belongs to a workspace, split into
active and deactivated members, with each member's role and space usage. Takes
the same optional `WORKSPACE` argument as
[`workspaces info`](#workspaces-info). No other flags.

```sh
ixr workspaces members
ixr workspaces members "Acme Design"
```

```
Active members:
Name          Email                 Role     Used space  Space limit  Member ID
------------  --------------------  -------  ----------  -----------  ------------------------------------
Ada Chen      ada@example.com       owner    12.4 GB     1 TB         aaaaaaaa-1111-2222-3333-444444444444
Rob Iyer      rob@example.com       member   3.1 GB      200 GB       bbbbbbbb-1111-2222-3333-444444444444
```

JSON output: `{ "success": true, "workspaceId": "...", "members": {...} }`.

### `workspaces teams`

New — no official equivalent. Lists the teams defined inside a workspace and
how many members each has. Takes the same optional `WORKSPACE` argument as
[`workspaces info`](#workspaces-info). No other flags.

```sh
ixr workspaces teams
```

```
Name       Team ID                               Members  Manager ID                            Created at
---------  ------------------------------------  -------  ------------------------------------  ---------------------
Designers  cccccccc-1111-2222-3333-444444444444  4        aaaaaaaa-1111-2222-3333-444444444444  3 March, 2026 at 10:20
```

JSON output: `{ "success": true, "workspaceId": "...", "teams": [...] }`.

### `workspaces usage`

New — no official equivalent. Shows the workspace's total space, how much of it
is handed out to members, and how much is actually in use — followed by your own
share of it. There is no per-member lookup: the API reports usage for whoever is
asking, and everyone else's quotas show up in
[`workspaces members`](#workspaces-members). Takes the same optional
`WORKSPACE` argument as [`workspaces info`](#workspaces-info). No other flags.

```sh
ixr workspaces usage
```

```
Total space:  1 TB
Assigned:     600 GB (60.0%)
Used:         84 GB (14.0%)

Your usage in this workspace:
Drive:        12.1 GB
Backups:      300 MB
Space limit:  200 GB
```

JSON output: `{ "success": true, "workspaceId": "...", "usage": {...},
"memberUsage": {...} }` — `memberUsage` is `null` when the API doesn't report
one for you.

### `workspaces invitations`

New — no official equivalent. Lists workspace invitations waiting for you to
accept or decline. Account-scoped, so it takes no workspace argument. Accepting
or declining isn't supported yet — do that from the web app.

Flags: `-l/--limit <N>` (default 25; the server rejects anything above 25),
`-o/--offset <N>` (default 0, for paging).

```sh
ixr workspaces invitations
ixr workspaces invitations --limit 10 --offset 10
```

```
Workspace    Workspace ID                          Space limit  Invited at              Invitation ID
-----------  ------------------------------------  -----------  ----------------------  ------------------------------------
Acme Design  11111111-2222-3333-4444-555555555555  200 GB       4 March, 2026 at 09:00  dddddddd-1111-2222-3333-444444444444
```

JSON output: `{ "success": true, "invitations": [...] }`.

> The five views above read endpoints the official CLI never touches, and the
> account they were developed against belongs to no workspace — so only their
> error and empty-collection paths were exercised against the live API. The
> human-readable tables follow the field names in Internxt's own SDK types; if a
> response ever turns out not to match, the command prints it verbatim rather
> than dropping it, and `--json` always passes the raw response through.

### `sync up` / `sync down`

New — no official equivalent. A single **one-way**
reconcile pass, then exit (not a daemon). The source side always wins — no
bidirectional mode, no
conflict resolution. Files are keyed by relative path; change detection
compares size, then modification time (±2s tolerance). Downloaded files are
stamped with the remote modification time so repeat `sync-down` runs are
idempotent.

Flags (`sync-up`): `-l/--local <DIR>` (required), `-r/--remote <FOLDER_ID>`
(default: root), `--remote-path <PATH>` (alternative to `--remote`),
`--delete[=trash|permanent]` (opt-in; prunes extra remote files **and**
folders — deleting the top-most extra folder cascades its subtree), `--dry-run`,
`--exclude-empty-files` (skip 0-byte local files instead of uploading them —
see below), plus the [upload-limit flags](#upload-size-limit).

Flags (`sync-down`): `-l/--local <DIR>` (required), `-r/--remote <FOLDER_ID>`,
`--remote-path <PATH>`, `--delete[=remove]` (OS-trash delete mode not yet
supported), `--dry-run`.

```sh
ixr sync-up   -l ./my-folder -r <folder-uuid> --dry-run   # preview a push
ixr sync-up   -l ./my-folder -r <folder-uuid> --delete    # push, trashing remote extras
ixr sync-up   -l ./my-folder -r <folder-uuid> --exclude-empty-files
ixr sync-down -l ./my-folder --remote-path /Backups       # pull new/changed files
```

`sync-up` uploads empty (0-byte) local files by default, same as
[`upload folder`](#upload-folder). Internxt's free/legacy plans reject them
server-side (`HTTP 402 Payment Required`), which counts as a normal per-file
failure (reflected in `failed` below, and a non-zero exit code) rather than
being silently skipped. Pass `--exclude-empty-files` to skip them client-side
instead.

If `failed` is non-zero (for any reason, not just empty files), the command
exits non-zero — check the `actions` list (or the non-JSON status lines) for
which paths failed and why.

**How the remote side is enumerated.** Before transferring anything, both
directions build an inventory of the remote folder. When the server can answer
one, that inventory — every folder *and* every file — comes from a **single
subtree request** (the same endpoint [`tree`](#tree) uses), no matter how deep
or wide the folder is. Large subtrees are built server-side and time out there,
so the size is checked first and anything too big — plus any failure — falls
back to listing every folder one by one, sequentially: two requests per folder.
Either way the resulting inventory is identical, including trashed items, which
both paths leave out. Set `IXR_FOLDER_TREE=0` (also `false`/`no`/`off`) to force
the folder-by-folder listing everywhere it's used: `sync up`, `sync down`,
[`download folder`](#download-folder) and
[`compare folder`](#compare-file--compare-folder).

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

### `compare file` / `compare folder`

New — no official equivalent. Verifies a local file/folder is byte-identical
to its Drive counterpart, without transferring anything anywhere. Exits
non-zero (and prints every difference found) when they differ; exits `0` and
prints `Identical.` when they match.

`compare file` checks size first — a mismatch is reported immediately, with
no need to read either side. If sizes match, both sides are streamed
(decrypting the remote file on the fly) and compared byte-for-byte, stopping
at the first difference and reporting its byte offset.

`compare folder` recursively enumerates both trees (the remote side exactly as
[`sync`](#sync-up--sync-down) does, `IXR_FOLDER_TREE=0` included) and
diffs file-by-file — missing files/folders on either side count as
differences too. By default it **stops at the first difference found**
(file or folder, either side); pass `--list` to keep going and report every
difference instead. `--path` also accepts the virtual `//`/`//backups`
[groupings](#path-syntax) — each real child is compared against its own
local subfolder (`drive/`, `backups/<device>/`), with diffs from every
child reported together (`compare file` doesn't accept them: there's no
single file to compare a grouping against).

`--metadata-only` skips content streaming entirely on both commands — only
size (and modification time, with `--check-modified`) is checked.
`--check-modified` is independent of content: it's an extra check layered on
top, so a byte-identical file with a differing modification time (±2s
tolerance, local FS mtime vs. remote `modificationTime`) still counts as a
difference.

Flags (`compare file`): `-l/--local <FILE>` (required), `-i/--id <FILE_ID>`,
`-p/--path <PATH>` (alternative to `--id`; one of the two is required),
`--metadata-only`, `--check-modified`.

Flags (`compare folder`): `-l/--local <DIR>` (required), `-i/--id <FOLDER_ID>`
(default: root), `-p/--path <PATH>` (alternative to `--id`),
`--metadata-only`, `--check-modified`, `--list`.

```sh
ixr compare file   -l ./report.pdf --path /Documents/report.pdf
ixr compare file   -l ./report.pdf --path /Documents/report.pdf --metadata-only --check-modified
ixr compare folder -l ./my-folder -i <folder-uuid>              # stop at first diff
ixr compare folder -l ./my-folder -i <folder-uuid> --list        # list every diff
```

JSON output:

```json
{ "success": true, "identical": true, "differences": [] }
```

```json
{
  "success": false,
  "identical": false,
  "differences": [
    { "type": "file", "path": "notes.txt", "detail": "size differs: local 120 bytes, remote 118 bytes" }
  ]
}
```

### `backups devices list`

New — no official equivalent (backups are a desktop-app-only feature there;
the desktop app represents each backed-up device as a special Drive folder —
"device as folder" — and `ixr` ports device management plus browsing/
downloading what's already backed up, not the continuous watch-local-
folders-and-upload daemon itself, which has no background-service model in
a CLI). Lists your backup devices.

Flags: `-e/--extended` (created-at + active/removed status in the
human-readable view), `--all` (include removed devices too, hidden by
default).

```sh
ixr backups devices list
ixr backups devices list --all --json
```

JSON output: `{ "success": true, "list": { "devices": [...] } }` (always the
full device objects, regardless of `--extended`).

### `backups devices create`

New — no official equivalent. Registers a new backup device (a
`POST /backup/deviceAsFolder` call — the same one the desktop app makes the
first time it configures backups on a machine). Useful for a headless/
scripted `ixr` upload that wants to show up as a device in the desktop/
mobile Backups UI without ever running the desktop app: create the device
once, then `upload folder`/`sync up` into it like any other Drive folder.

Flags: `-n/--name <NAME>` (required — prompts if omitted, unless
`--json`/`-x`).

```sh
ixr backups devices create -n "CI Runner"
```

JSON output: `{ "success": true, "device": <created device> }`.

### `backups devices rename`

New — no official equivalent. Renames a backup device.

Flags: `<DEVICE>` (positional — id or name, see
[`backups devices list`](#backups-devices-list)), `-n/--name <NAME>`
(required — prompts if omitted, unless `--json`/`-x`).

```sh
ixr backups devices rename "My-Laptop" -n "Work-Laptop"
```

JSON output: `{ "success": true, "device": <updated device> }`.

### `backups devices delete`

New — no official equivalent. Deletes a backup device and everything backed
up to it — **cannot be undone** (unlike Drive files/folders, backups have no
trash to recover from). Prompts for confirmation unless `--force` (required
in `--json`/non-interactive mode).

Flags: `<DEVICE>` (positional — id or name), `-f/--force`.

```sh
ixr backups devices delete "Old-Laptop" --force
```

JSON output: `{ "success": true, "message": "Backup device '...' deleted." }`.

### `backups list` / `backups download` / `backups get-id`

New — no official equivalent. Thin, device-scoped wrappers around
[`list`](#list) / [`download folder`](#download-folder) /
[`id-from-path`](#id-from-path): `<DEVICE>` (positional — id or name, see
[`backups devices list`](#backups-devices-list)) resolves to the device's
Drive folder, and `-p/--path <PATH>` (optional on all three) walks a
subfolder inside it — same effect as
[`//backups/<device>/<path>`](#path-syntax) on the equivalent generic
command, just without needing the device's uuid in hand first.

Flags (`backups list`): `-p/--path <PATH>`, `-e/--extended`.

Flags (`backups download`): `-p/--path <PATH>`, `-d/--directory <DIR>`
(default: current dir), `-o/--overwrite`.

Flags (`backups get-id`): `-p/--path <PATH>`.

```sh
ixr backups list "My-Laptop"                          # device root
ixr backups list "My-Laptop" -p Documents/Reports -e
ixr backups download "My-Laptop" -d ./restored
ixr backups get-id "My-Laptop" -p Documents            # -> a uuid, for scripting
ixr mount ~/laptop-backup --folder-uuid $(ixr backups get-id "My-Laptop")
```

JSON output: `backups list` → same shape as [`list`](#list); `backups
download` → same shape as [`sync down`](#sync-up--sync-down)'s result
object; `backups get-id` → `{ "success": true, "uuid": "...", "isFolder": ..., "type": "file"|"folder" }`.

### `shared list`

New — no official CLI equivalent (sharing lives in the web app, so the
semantics here follow drive-web). Lists what is shared. With no direction flag
it makes the same two calls drive-web's "Shared" view makes
(`/sharings/folders` + `/sharings/files`), which return everything visible to
you — items you shared out *and* items shared with you, told apart by the
owner column. `--with-me` / `--by-me` switch to the two directional endpoints,
which the API only provides for **folders**.

**Creating** a share is not supported: it wraps the item's key for the
recipient's public key (or for a link password), and the engine crate doesn't
implement that wrapping yet. Everything under `shared` is read-only except
[`shared revoke`](#shared-revoke).

Flags: `--with-me`, `--by-me` (mutually exclusive), `--page <N>` (0-based,
default 0), `--per-page <N>` (default 50), `--order-by <FIELD:DIRECTION>`
(default `createdAt:DESC`; og's sdk documents `createdAt` and `views`, `ASC`
or `DESC` — ignored by the directional listings, whose endpoints don't take
it), `-e/--extended` (adds the uuid column).

```sh
ixr shared list
ixr shared list --with-me -e
ixr shared list --page 1 --per-page 20 --order-by createdAt:ASC --json
```

An account with nothing shared prints `No shared items found.` (or
`No folders shared with you.` / `No folders shared by you.`).

JSON output:
`{ "success": true, "list": { "folders": <response>, "files": <response> } }`
— each value is the server's reply passed through verbatim. `list.files` is
absent for the directional listings, which have no file half.

### `shared info`

New — no official CLI equivalent. Shows how one file or folder is shared, and
who has been invited to it (`/sharings/{type}/{id}/info`, `.../type` and
`.../invites` in one go).

Flags: `<ITEM>` (positional — a Drive path like `/Docs/report.pdf`, or a
uuid; a value shaped exactly like a uuid is taken as one, so write it as a
path if a Drive item is genuinely named that way), `-t/--type file|folder`
(only saves the extra metadata lookup that works this out when `<ITEM>` is a
uuid).

```sh
ixr shared info /Docs/report.pdf
ixr shared info 11111111-2222-3333-4444-555555555555 -t folder
```

An item that isn't shared prints `Shared: no` — the API reports that as a 404
on the sharing endpoints, which is a normal answer here, not an error. A
shared one looks like this (invented data):

```
Item:        /Photos/Trip (folder)
Uuid:        11111111-2222-3333-4444-555555555555
Shared:      yes
Sharing:     public
Password:    no
Invitations: 1

Shared with          Role    Invited
-------------------  ------  ------------------------
someone@example.com  READER  1 January, 2026 at 12:00
```

JSON output: `{ "success": true, "item": { "uuid": "...", "type":
"file"|"folder", "shared": true|false }, "info": <response|null>,
"sharingType": <response|null>, "invites": <response> }` — the three
server replies verbatim, `null` where the item isn't shared.

### `shared invites`

New — no official CLI equivalent. Lists the sharing invitations waiting for
*you* (`/sharings/invites`), as opposed to the per-item invitations
[`shared info`](#shared-info) shows. Accepting or declining one isn't
implemented.

Flags: `--limit <N>` (default 25 — the endpoint only accepts 1-25),
`--offset <N>` (default 0).

```sh
ixr shared invites --limit 10 --json
```

JSON output: `{ "success": true, "list": { "invites": <response> } }`.

### `shared roles` / `shared domains`

New — no official CLI equivalent. `shared roles` lists the roles a share
recipient can be given (the human table also uses them to turn the role ids
in the invitation listings into names); `shared domains` lists the domains
public share links are served from. Neither takes any flags.

```sh
ixr shared roles
ixr shared domains --json
```

JSON output: `{ "success": true, "list": { "roles": [ { "id": "...", "name":
"...", "createdAt": "...", "updatedAt": "..." } ] } }` — rebuilt from the
typed values the engine crate parses, with the wire field names — and
`{ "success": true, "list": { "domains": ["https://example.invalid", ...] } }`.

### `shared revoke`

New — no official CLI equivalent. Stops sharing a file or folder
(`DELETE /sharings/{type}/{id}`), removing the public link and every
invitation to it.

Flags: `<ITEM>` (positional — path or uuid, as in
[`shared info`](#shared-info)), `-t/--type file|folder`.

```sh
ixr shared revoke /Docs/report.pdf
ixr shared revoke 11111111-2222-3333-4444-555555555555 --json
```

The API call itself is idempotent — revoking something that isn't shared
succeeds instead of failing — so `ixr` checks first and says which of the two
happened rather than implying a share was removed when there was none:
`<item> is not shared; nothing to revoke.`

JSON output: `{ "success": true, "revoked": true|false, "item": { "uuid":
"...", "type": "file"|"folder", "shared": false }, "message": "..." }`.

### `serve`

Runs one or more Drive backends in the **foreground** until Ctrl-C. Pass a
comma-separated protocol list: `webdav`, `fuse`, `smb`, `nfs`, `sftp`.
Running several at once shares one set of credentials, one folder-listing
cache and one global upload limit.

The **WebDAV** backend mirrors the official CLI's WebDAV server; the official
CLI runs it as a pm2-managed background service configured through a separate
`webdav-config` command, while `ixr` runs it inline as a normal foreground
command instead. **FUSE, SMB, NFS and SFTP have no official equivalent** — the
official CLI only serves WebDAV. `smb`, `nfs` and `sftp` are experimental and
off by default (build with `--features smb`/`nfs`/`sftp`).

Shared flags (bare): `-i/--folder-uuid <UUID>` (root to expose; mutually
exclusive with `-p/--path`), `-p/--path <PATH>` (root to expose, as a
[Drive path](#path-syntax) instead of a uuid — also accepts the virtual `//`
and `//backups` groupings, exposed **read-only** across every backend: `mkdir`/
write/rename/delete anywhere directly under `//` or `//backups` is rejected,
while a real folder reached *through* one, e.g. `//backups/My-Laptop/Documents`,
is a normal, fully writable Drive folder), `-d
/--delete-permanently` (hard-delete instead of trash), `--read-only`,
`-v/--verbose` (log every per-op request across all backends), `--spool`
(spool uploads to a temp file before uploading; FUSE always spools),
`--spool-dir <DIR>`, `--max-concurrent-uploads <N>` (0 = unlimited),
`--cache-ttl <SECS>` (default 300 — matches rclone's own `--dir-cache-time`;
also the FUSE kernel attr/entry TTL), `--no-cache`, `--recent-window <BYTES>`
(default 4194304 — trailing-stream retention on the read path for
FUSE/SMB/NFS/SFTP, see below; 0 disables it), plus the
[upload-limit flags](#upload-size-limit).

Protocol-specific flags are prefixed:

- **WebDAV** (`--webdav-*`): `--webdav-host` (default `127.0.0.1`),
  `--webdav-port` (default `3005`), `--webdav-https` (needs `webdav-tls`
  feature), `--webdav-cert`/`--webdav-key` (custom TLS cert/key, both
  required together), `--webdav-timeout <MINS>` (default 60; accepted but not
  yet wired to a request-timeout layer), `--webdav-create-full-path`
  (auto-create missing parent folders on `PUT`/`MKCOL`), `--webdav-custom-auth`
  + `--webdav-username`/`--webdav-password` (require HTTP Basic auth).
- **FUSE** (`--fuse-*`): `--fuse-mountpoint <DIR>` (required when `fuse` is
  served — a directory on Unix, a drive letter like `X:` or a directory on
  Windows), `--fuse-allow-other` (Unix only — no WinFSP equivalent).
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
ixr serve webdav,fuse --fuse-mountpoint ~/all -p '//'           # browse Drive + all backup devices at once
```

WebDAV supported methods: `OPTIONS`, `PROPFIND`, `GET`/`HEAD` (with `Range`),
`PUT`, `MKCOL`, `DELETE`, `MOVE`, `LOCK`/`UNLOCK`. `COPY` and `PROPPATCH`
return `501 Not Implemented`, matching the official server. `DELETE` trashes
items by default (`--delete-permanently` for a hard delete).

`serve`/`mount` run until interrupted — there's no terminal JSON result
object to speak of; `--json` mainly suppresses the startup/progress banner.

FUSE/SMB/NFS/SFTP each serve reads from one lazily-started decrypt stream per
open file; a small backward/forward re-read (e.g. a media player re-visiting
a container-index box — an MP4 `moov` atom, MKV cues — while probing a file)
would otherwise force a full stream restart, a fresh network round trip.
`--recent-window <BYTES>` keeps that many recently-streamed bytes per open
file so those re-reads are served from memory instead; `--recent-window 0`
disables it (every non-sequential read restarts the stream, trading the
per-file memory for none of the retention). WebDAV's GET is one-shot per
HTTP request and doesn't use this.

Pass `--verbose` to dump each WebDAV request/response, headers included, to
stderr.

### `mount`

New — no official equivalent (the official CLI has no filesystem-mount mode).
A thin wrapper over `serve fuse` where the shared flags use their bare names
(no `fuse-` prefix).

Flags: `-i/--folder-uuid <UUID>` (mutually exclusive with `-p/--path`),
`-p/--path <PATH>` (root to mount, as a [Drive path](#path-syntax) instead
of a uuid — also accepts the virtual `//`/`//backups` groupings, mounted
**read-only**; see [`serve`](#serve) above for the exact rule), `--read-only`,
`-d/--delete-permanently`, `--spool-dir <DIR>`, `--max-concurrent-uploads
<N>`, `--cache-ttl <SECS>` / `--no-cache`, `--recent-window <BYTES>` (see
[`serve`](#serve) above), `--allow-other` (Unix only — no WinFSP
equivalent), `-v/--verbose`, plus the [upload-limit flags](#upload-size-limit).

```sh
mkdir -p ~/drive && ixr mount ~/drive              # Ctrl-C to unmount (Unix)
ixr mount X: --read-only                           # drive letter (Windows)
ixr mount ~/drive -i <folder-uuid>                 # mount a subfolder as root
ixr mount ~/backups -p '//backups'                 # browse every backup device at once
ixr mount ~/laptop -p '//backups/My-Laptop'         # mount just one device
```

See [FUSE/WinFSP mount support](#fusewinfsp-mount-support) for what each
platform needs to build and run this. Reads stream and decrypt lazily; writes
buffer to a temp file and upload in full when the file is closed (Internxt has
no partial-update API), replacing the old Drive entry.

### `vpn locations`

New — no official equivalent (the VPN otherwise ships as a browser extension
only). Needs the `vpn` feature (off by default in source builds; on in the
Docker image and prebuilt release binaries). Lists the VPN locations your
plan can use, server-enforced — not hardcoded here, so a plan change on
Internxt's side doesn't need a client update.

No flags.

```sh
ixr vpn locations
```

```
Code  Location
----  --------------
FR    France
DE    Germany
PL    Poland
CA    Canada
UK    United Kingdom
```

JSON output: `{ "success": true, "list": { "locations": [{ "code": "FR", "label": "France" }, ...] } }`.
A location code this build doesn't have a name for yet shows up with
`"label": null` (`-` in the table) rather than being dropped.

### `vpn proxy`

New — no official equivalent. Runs a local proxy that tunnels through the
Internxt VPN, in the **foreground** until Ctrl-C — same run-until-interrupted
model as [`serve`](#serve).

**This is a proxy, not a full VPN tunnel.** Only traffic explicitly pointed
at the local listener is routed through it (`HTTPS_PROXY`/`ALL_PROXY`, a
browser's proxy setting, an app's `--proxy` flag, …) — it doesn't touch your
system's default routing, and it never carries UDP (no DNS-over-UDP,
QUIC/HTTP3, games, VoIP — the upstream proxy is CONNECT-only).

On the wire, Internxt runs one shared proxy server for every location — the
location is selected per-connection via the Proxy-Authorization *username*,
not by connecting to a different host. The password is your existing Drive
session token, the same one every other `ixr` command already uses — no
separate VPN login. The proxy hop itself is **plain, unencrypted HTTP**, not
TLS. See `internxt-core`'s `vpn` module and
`config::vpn_api_url`/`vpn_proxy_host`/`vpn_proxy_port` (all three
env-overridable, same as every other endpoint — see
[Configuration](#configuration)) if any of this ever drifts.

Pass a comma-separated list of local listeners to run — `https`, `socks5`,
or both at once (same mechanism as `serve`'s protocol list; both share the
one upstream connection scheme, just a different local wire format).
`socks5` never resolves DNS locally — domain targets are forwarded to the
proxy as-is, so resolution happens server-side (the "h" in socks5h, always
on).

Flags: `<PROTOCOLS>` (positional, required — `https`, `socks5`, or
`https,socks5`), `-l/--location <CODE>` (default `FR` — see
[`vpn locations`](#vpn-locations) for what your plan allows; an unrecognized
code is still accepted and used as-is, not rejected client-side — the server
is the real authority), `--https-host`/`--https-port` (default
`127.0.0.1`/`1080`), `--socks5-host`/`--socks5-port` (default
`127.0.0.1`/`1081`), `-v/--verbose` (log every accepted connection's
destination — never payload bytes).

At startup, a best-effort check against `vpn locations` warns (doesn't
block) if `--location` isn't on your plan, so a typo shows up immediately
instead of as a wall of per-connection failures. Per-connection failures
(wrong/unauthorized location, upstream refusal, a malformed request) are
always logged regardless of `-v` — only the noisy per-connection *access*
log (`-v`'s "CONNECT host:port" lines) is gated on it. A connection that
closes without sending anything at all (a browser speculatively
opening/abandoning a connection, a health check, connection-pool probes —
routine, not an error) is never logged either way.

```sh
ixr vpn proxy https                                   # 127.0.0.1:1080, location FR
ixr vpn proxy https,socks5 -l DE                       # both listeners, Germany
ixr vpn proxy socks5 --socks5-port 9050 -v             # verbose access log
HTTPS_PROXY=http://127.0.0.1:1080 curl https://ifconfig.me
curl --socks5-hostname 127.0.0.1:1081 https://ifconfig.me
```

`vpn proxy` runs until interrupted — there's no terminal JSON result object;
`--json` mainly suppresses the startup banner.

### `id-from-path`

Alias: `get-id`. New — no official equivalent. Prints the
uuid of the Drive file/folder at a given path. Understands the full
[path syntax](#path-syntax), including `//backups/<device>/...` (the virtual
bare `//`/`//backups` groupings excepted — those have no id of their own).

Flags: `-p/--path <PATH>` (required).

```sh
ixr id-from-path -p /Documents/report.pdf
ixr id-from-path -p //backups/My-Laptop/Documents/report.pdf
```

JSON output: `{ "success": true, "uuid": "...", "isFolder": false, "type": "file" }`.

### `path-from-id`

Alias: `get-path`. New — no official equivalent. Prints the
full Drive path of a file/folder given its uuid. Prints
`//backups/<device>/...` (see [path syntax](#path-syntax)) instead of a
misleading root-relative path for anything living inside a backup device.

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
backend write) can be disabled everywhere with `IXR_THUMBNAILS=0`.

### `versions`

Alias: `version`. New — the official CLI has no versions command at all;
version history is a drive-web feature, and these are the same
`/files/{uuid}/versions` endpoints its "Version history" sidebar uses.

Every subcommand takes the file as a single positional argument, which may be
either a **Drive path** or a **uuid** — a 36-character 8-4-4-4-12 hex string is
read as a uuid, anything else is resolved as a path (including the `//drive` /
`//backups/<device>` [escapes](#path-syntax)).

- **`versions list <FILE>`** — the file's stored versions, newest first, as a
  table of version id, size, modified/created/expiry dates and status. Prints
  `No versions stored for this file.` when there are none, which is the normal
  answer for most files (see below).
- **`versions restore <FILE> <VERSION_ID>`** — make that version the file's
  current content. The file keeps its uuid, so links and paths to it stay
  valid. **Cannot be undone**, and versions newer than the restored one are
  dropped — download them first if you want to keep them.
- **`versions delete <FILE> <VERSION_ID>`** — permanently delete one stored
  version. The file's current content is untouched. **Cannot be undone.**

```sh
ixr versions list /Reports/quarterly.pdf
ixr versions list 00000000-0000-0000-0000-000000000000 --json
ixr versions restore /Reports/quarterly.pdf <version-id>
ixr versions delete /Reports/quarterly.pdf <version-id>
```

**What creates a version.** Nothing in this CLI (or in any official Internxt
client) asks for one — versions are minted server-side, which is why drive-web
calls them "autosave versions". Two conditions have to hold before one appears:

1. **The plan must allow versioning.** [`usage`](#usage) reports this as
   `File versioning`, along with how many versions are kept per file, the
   largest file eligible, and how long a version is retained before the
   retention policy drops it.
2. **The file's extension must be one the backend versions.** drive-web only
   offers the sidebar for `pdf`, `docx`, `xlsx` and `csv`, and the backend
   agrees: replacing the content of a `.pdf` produces a version, while
   replacing an otherwise identical `.txt` produces none.

With both satisfied, a version is created when a file's **content is replaced
in place** — what every [`serve`](#serve) backend does on a write to an
existing file (`PUT`, or a write through the mount). Uploading a *new* file
creates no version, and neither does renaming, moving, or trashing one.

Outside those conditions `versions list` is simply empty. That's the expected
result, not an error — so it says so in words rather than printing an empty
table.

JSON output of `versions list`:

```json
{
  "success": true,
  "list": {
    "file": "00000000-0000-0000-0000-000000000000",
    "versions": [
      {
        "id": "11111111-1111-1111-1111-111111111111",
        "fileId": "00000000-0000-0000-0000-000000000000",
        "networkFileId": "0123456789abcdef01234567",
        "size": 20480,
        "status": "EXISTS",
        "modificationTime": "2025-01-02T10:00:00.000Z",
        "createdAt": "2025-01-02T10:05:00.000Z",
        "updatedAt": "2025-01-02T10:05:00.000Z",
        "expiresAt": "2025-01-17T10:05:00.000Z"
      }
    ]
  }
}
```

`restore` emits `{ "success": true, "message": "Version restored", "file": {
"uuid": ..., "name": ..., "type": ..., "size": ..., "fileId": ... },
"versionId": ... }`; `delete` emits `{ "success": true, "message": "Version
deleted", "file": "<file uuid>", "versionId": ... }`.

There is no `versions download`: a version's bytes live in the network under
its own `networkFileId`, in the same bucket as the file, and nothing in the
CLI exposes a bucket-level fetch by network id yet. Restoring a version and
downloading the file is the way to get old content back for now.

### `update`

New — no official equivalent. Needs the `self-update` feature (off by
default; the GitHub release workflow enables it for every standalone-binary
target, and AUR's `ixr-bin` reuses that same binary). Replaces the running
binary in place with the latest GitHub release. Meant for the standalone
binary distribution — a package-manager install (AUR) or a self-built binary
(plain `cargo install`/`cargo build`) should still update via the package
manager or a rebuild instead, since this would fight them for ownership of
the file. Docker isn't affected either way — its image is built without the
feature.

By default, targets the true latest release regardless of how big the
version jump is (e.g. `0.2.0` -> `0.3.0` in one hop). Only stable releases
are considered unless `--pre-release` is given; prerelease tags (e.g.
`v0.2.0-rc.1`) are skipped otherwise. `--check` and the actual install
always agree on which version is targeted.

Flags: `--check` (report whether a newer release exists, without installing
it), `-y/--yes` (skip the confirmation prompt — required under `--json` or
`--non-interactive`), `--pre-release` (consider prerelease tags too),
`--patch-only` (restrict to the current minor version — patch bumps only,
e.g. `0.2.0` -> `0.2.1` but not `0.3.0`; conflicts with `--version`),
`--version <VER>` (install this exact version instead of the latest — can
also downgrade; conflicts with `--patch-only`).

```sh
ixr update --check
ixr update -y
ixr update --pre-release -y
ixr update --patch-only -y      # stay within the current minor version
ixr update --version 0.2.0 -y   # install (or downgrade to) an exact version
```

## Upload size limit

Uploads are validated against a per-file size cap before transferring — except
there is **no** hard-coded default: when your plan sets no cap, uploads are
unbounded. The cap is resolved in this order (first match wins):

1. `--no-upload-limit` — disable the check entirely.
2. `--max-upload-size <SIZE>` — a custom cap (`5GB`, `500M`, `1073741824`, …
   binary units).
3. `IXR_MAX_UPLOAD_SIZE` env var — universal override for every upload
   command. A size string sets a cap; `off`/`none`/`unlimited`/`0` disables it.
4. Otherwise, your plan's `maxUploadFileSize` (from `/files/limits`; unlimited
   if unset).

These flags apply to `upload-file`, `upload-folder`, `sync-up`, and the
`serve`/`mount` backends. Over-limit files are rejected up front (folder/sync
uploads skip the offending file and continue; WebDAV `PUT` returns `413`;
FUSE/SMB/NFS/SFTP writes fail accordingly).

This is purely a **local, client-side pre-check** for fast failure — Internxt's
servers independently enforce the same `maxUploadFileSize` on every upload.
`--no-upload-limit` / `--max-upload-size` / `IXR_MAX_UPLOAD_SIZE` only change
what `ixr` checks *before* sending; they cannot raise, bypass, or otherwise
affect the server-side cap your plan actually has. An upload past the real
server limit still fails server-side even with the local check disabled.

## Configuration

API endpoints and app constants default to the public Internxt values (defined in the
`internxt-core` crate's `config` module) and can be overridden via environment
variables of the same name (`DRIVE_NEW_API_URL`, `NETWORK_URL`,
`PAYMENTS_API_URL`, etc). This includes `VPN_API_URL`, `VPN_PROXY_HOST` and
`VPN_PROXY_PORT` (needs the `vpn` feature) — see [`vpn proxy`](#vpn-proxy).

Two `IXR_*` switches turn off behaviour that is otherwise automatic:
`IXR_THUMBNAILS=0` disables thumbnail generation on every upload path (see
[`thumbnail`](#thumbnail)), and `IXR_FOLDER_TREE=0` forces the
folder-by-folder listing when a command enumerates a remote subtree (see
[`sync up` / `sync down`](#sync-up--sync-down)). Both also accept
`false`/`no`/`off`.

Credentials are stored AES-encrypted at `~/.ixr/credentials` — its own
directory, separate from the official CLI's `~/.internxt-cli`. The file holds
every logged-in account (see below), not just one.

## Multiple accounts

Unlike the official CLI (one account at a time), `ixr` can hold several
logged-in accounts in `~/.ixr/credentials` and lets you pick which one a given
command acts on.

**Adding / replacing.** `login`/`login-legacy`/`login-sso` only prompt or need
a flag when a *different* account is already active:

- Logging in again as the same active account just refreshes it (no prompt).
- Logging in as a new account while a different one is active: interactively,
  you're asked to add (keep both, switch to the new one) or replace (log out
  the old one, switch to the new one); non-interactively (`-x`), pass
  `--add` or `--replace` explicitly or it errors.

**Switching.** `accounts switch` sets the active account for every subsequent
command until changed again; `accounts list` shows what's stored and which one
is active.

**Targeting one account for a single command, without switching.** Set
`IXR_USER=<email>` — every command resolves credentials for that account
instead of the active one, without persisting any change to `accounts switch`'s
active pointer. If that account isn't logged in yet, also set `IXR_PASSWORD`
(and `IXR_TWOFACTORCODE`/`IXR_OTPTOKEN` if it has 2FA) and the command
transparently logs it in first and stores it (still without making it active)
— the built-in equivalent of `og/cli/docker/entrypoint.sh`'s shell-level
auto-login, useful for containers/CI that always want to act as one specific
account:

```sh
IXR_USER=ci@example.com IXR_PASSWORD=... ixr whoami --json   # auto-logs in ci@example.com, first time only
IXR_USER=ci@example.com ixr upload-file -f ./report.csv       # every later invocation just uses the stored session
```

Add `IXR_NO_PERSIST` (any value) to make that one invocation leave no trace on
disk at all: the `IXR_PASSWORD` auto-login result (and any refreshed token) is
kept in memory for this command only, never written to `~/.ixr/credentials`.
Every invocation re-authenticates from scratch — useful for one-shot/CI runs
that shouldn't leave a session file behind:

```sh
IXR_USER=ci@example.com IXR_PASSWORD=... IXR_NO_PERSIST=1 ixr upload-file -f ./report.csv
```

These replace the official CLI's `INXT_USER`/`INXT_PASSWORD`/
`INXT_TWOFACTORCODE`/`INXT_OTPTOKEN` env vars (which only filled in
`login`'s/`login-legacy`'s own flags) — `ixr` has no equivalent on `login`
itself; use `IXR_USER`/`IXR_PASSWORD` for the env-driven auto-login case
instead, on any command.

## Compatibility with the official Internxt CLI

This is intended to be a **mostly drop-in replacement**. For ported commands the
names, flags, endpoints, payloads and crypto all match, so the two behave the
same for everyday login / upload / download / list / move / rename / trash
workflows. Credentials are **not** shared between the two — `ixr` stores its
own session(s) at `~/.ixr/credentials`, separate from the official CLI's
`~/.internxt-cli`, so each needs its own `login`. Unlike the official CLI,
`ixr` supports several logged-in accounts at once — see
[Multiple accounts](#multiple-accounts).

The official CLI's commands (built with [oclif](https://oclif.io)) are
hyphenated (`upload-file`, `move-file`, `create-folder`, …) — `ixr` uses the
exact same primary names. Most of them can also be invoked as separate
space-separated words (`internxt upload file`), and `ixr` matches that with
real nested subcommands, so `ixr upload file` works the same way. Both forms
work on both CLIs, for every command listed above as also having a space
form. `login-legacy`, `login-sso` and `sync-up`/`sync-down` are the
exceptions: `login-sso` and `sync-up`/`sync-down` don't exist in the official
CLI at all, so only the hyphenated form applies to any of the three.

Known differences:

- **`login` is an alias, not its own flow.** The official CLI's `login` is
  SSO-only (no email/password flags at all) and `login-legacy` is the separate
  email/password command. `ixr`'s `login` is an alias for `login-sso` when built
  with the default `sso` feature (same SSO-only behaviour as the official CLI),
  or for `login-legacy` when built `--no-default-features` (which drops `sso`
  entirely). `login-sso` (forces SSO, errors without the `sso` feature) doesn't
  exist in the official CLI at all — it's here so you can force a specific flow
  explicitly instead of relying on which feature `login` was built with. The
  SSO flow can't carry the Kyber private key, so hybrid-Kyber workspaces need
  `login-legacy`.
- **`--json` output schema differs.** `ixr` emits a simplified `{ "success": true, ... }`
  object per command rather than the official CLI's exact JSON envelope. Field
  names mostly match, but don't assume a byte-identical structure — see each
  command's JSON output above.
- **No interactive prompting for missing flags**, with three exceptions:
  `login-legacy` (email/password/2FA), `login`/`login-legacy`/`login-sso`'s
  add-vs-replace prompt when a different account is already active (unless
  `--add`/`--replace` is given — see [Multiple accounts](#multiple-accounts)),
  and `trash-clear` (confirmation, unless `--force`). Everywhere else, a
  missing required flag is a clap usage error.
- **Plain-text table output** uses simple aligned columns rather than the
  official CLI's boxed tables. Use `--json` for stable machine-readable output.
- **`serve webdav` runs in the foreground**, options passed inline, rather than
  as a pm2-managed background service configured through a separate
  `webdav-config` command. The `webdav-config` / `webdav start|stop|status` /
  `add-cert` daemon-management commands aren't ported — the WebDAV server itself
  is, as `serve webdav`.
- **Not yet ported:** `config`, `logs`, `autocomplete`.
- **New, with no official equivalent:** `usage`, `login-sso`, `accounts list`,
  `accounts switch`, `recents`, `download-folder`, `delete-file`/`delete-folder` (the
  official CLI has `delete permanently file|folder` but no plain trash-alias
  `delete file|folder`), `tree`, `du`, `sync-up`, `sync-down`, `id-from-path`, `path-from-id`,
  the `thumbnail` command family, the `versions` command family (file version
  history is a drive-web feature there), the `backups` command family (backups are a
  desktop-app-only feature there), the `shared` command family (sharing is
  web-app-only there; `ixr` covers the read side plus revoking, not creating
  a share), the read-only workspace admin views
  `workspaces info|members|teams|usage|invitations` (the official CLI stops at
  `workspaces list|use|unset`; these exist only in the web app), `mount`, the
  `fuse`/`smb`/`nfs`/`sftp`
  `serve` backends (the official CLI only serves WebDAV, and only supports one
  logged-in account), and the `vpn` command family (the VPN otherwise ships
  as a browser extension only). See the [command reference](#command-reference)
  above for details on each.

## License

[MIT](LICENSE).

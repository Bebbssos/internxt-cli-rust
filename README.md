# internxt-cli-rust

An unofficial Rust port of the [Internxt CLI](https://github.com/internxt/cli), focused on speed and low memory use.

> Not affiliated with or endorsed by Internxt.

> This port was written mostly by [Claude Code](https://claude.com/claude-code), porting the behaviour of the official Node/TypeScript CLI.

## Status

Implemented: authentication (legacy + web-based SSO), streaming file transfers, recursive folder upload, one-way folder sync (`sync-up` / `sync-down`), workspaces (list/use/unset with workspace-scoped transfers), a foreground **WebDAV server** (`webdav`), automatic token refresh near expiry, and the full set of drive-management commands (list, create/move/rename, trash + restore, permanent delete). Every command supports `--json` for scripting.

Crypto is byte-for-byte compatible with the node CLI (verified by cross-check tests, see `cargo test`).

Not yet ported: thumbnails and the `config` command.

## Compatibility with `@internxt/cli`

This is intended to be a **mostly drop-in replacement** for the official [`@internxt/cli`](https://www.npmjs.com/package/@internxt/cli): for the implemented commands the names, aliases, flags, endpoints, payloads, credential file location/format (`~/.internxt-cli/.inxtcli`) and crypto all match, so the two are interchangeable for everyday `login` / `upload` / `download` / list / move / rename / trash workflows.

It is *not* a 100% replacement — anything in "Not yet ported" above is simply absent. The **WebDAV server** works differently by design (see the [WebDAV](#webdav) section): it runs in the foreground with options passed inline, rather than as a pm2-managed background service configured via `webdav-config`. Beyond that, the known **behavioural differences (breaking changes)** are:

- **`login` defaults to SSO.** Built with the default `sso` feature, `login` runs the web-browser callback flow (like the official CLI); `login-legacy` runs email + password + optional 2FA, and `login-sso` forces SSO. Build `--no-default-features` for a smaller binary where `login` falls back to legacy and `login-sso` errors. The SSO flow drops the kyber private key (hybrid-Kyber workspaces need `login-legacy`).
- **`--json` output schema differs.** We emit a simplified `{ "success": true, ... }` object per command rather than the exact oclif JSON envelope. Field names mostly match, but don't assume byte-identical structure.
- **No interactive prompting for missing flags.** Required arguments that are absent produce a clap usage error instead of an interactive prompt. The exceptions that still prompt: `login-legacy` (email/password/2FA) and `trash-clear` (confirmation, unless `--force`).
- **No thumbnail generation** on upload.
- **Plain-text table output** uses simple aligned columns rather than the official CLI's boxed tables. Use `--json` for stable machine-readable output.

## Build

```sh
cargo build --release
# binary at target/release/internxt (SSO + WebDAV over HTTP enabled by default)

# add HTTPS support to the WebDAV server (pulls in a rustls TLS stack):
cargo build --release --features webdav-tls

# smaller binary without SSO login or WebDAV (drops axum + open):
cargo build --release --no-default-features
```

Feature flags: `sso` (web-based login) and `webdav` (WebDAV server, HTTP) are on by default; `webdav-tls` adds HTTPS. Disable any of them for a smaller binary.

## Commands

All commands accept the global `--json` flag, which prints a single JSON result object and suppresses progress output. IDs are Drive UUIDs. Where a destination/parent folder id is optional, leaving it empty targets your root folder.

| Command | Aliases | Arguments | Description |
|---|---|---|---|
| `login` | | `--host <HOST>`, `--port <PORT>` (SSO); or legacy flags when built without `sso` | Log in. SSO web-browser flow by default (`--host`/`--port` for cross-device); falls back to legacy when built `--no-default-features`. |
| `login-sso` | `login:sso` | `--host <HOST>`, `--port <PORT>` | Web-based SSO login (opens a browser, local callback server). Errors if built without the `sso` feature. |
| `login-legacy` | `login:legacy` | `-e, --email <EMAIL>`, `-p, --password <PASSWORD>`, `-w, --twofactor <CODE>` | Log in with email + password. Prompts for any missing value; 2FA prompted if the account requires it. |
| `logout` | | | Invalidate the session server-side and clear local credentials. |
| `whoami` | | | Show the currently logged-in user. |
| `workspaces-list` | `workspaces:list` | `-e, --extended` | List the workspaces you belong to. |
| `workspaces-use` | `workspaces:use` | `-i, --id <WORKSPACE_ID>`, `-p, --personal` | Set the active workspace for subsequent commands (`--personal` switches back to your personal drive). |
| `workspaces-unset` | `workspaces:unset` | | Unset the active workspace (operate within your personal drive space). |
| `upload-file` | `upload:file` | `-f, --file <PATH>`, `-i, --destination <FOLDER_ID>` | Upload a single file (streaming; single-part or multipart). |
| `upload-folder` | `upload:folder` | `-f, --folder <PATH>`, `-i, --destination <FOLDER_ID>` | Recursively upload a folder tree (concurrent file uploads). |
| `download-file` | `download:file` | `-i, --id <FILE_ID>`, `-d, --directory <DIR>`, `-o, --overwrite` | Download + decrypt a file by id, streaming to disk. |
| `list` | | `-i, --id <FOLDER_ID>`, `-e, --extended` | List a folder's contents. `--extended` adds modified date + size. |
| `create-folder` | `create:folder` | `-n, --name <NAME>`, `-i, --id <PARENT_ID>` | Create a folder. |
| `move-file` | `move:file` | `-i, --id <FILE_ID>`, `-d, --destination <FOLDER_ID>` | Move a file into a destination folder. |
| `move-folder` | `move:folder` | `-i, --id <FOLDER_ID>`, `-d, --destination <FOLDER_ID>` | Move a folder into a destination folder. |
| `rename-file` | `rename:file` | `-i, --id <FILE_ID>`, `-n, --name <NAME>` | Rename a file (name + extension split automatically). |
| `rename-folder` | `rename:folder` | `-i, --id <FOLDER_ID>`, `-n, --name <NAME>` | Rename a folder. |
| `trash-file` | `trash:file` | `-i, --id <FILE_ID>` | Move a file to the trash. |
| `trash-folder` | `trash:folder` | `-i, --id <FOLDER_ID>` | Move a folder to the trash. |
| `trash-list` | `trash:list` | `-e, --extended` | List the contents of the trash. |
| `trash-restore-file` | `trash:restore:file` | `-i, --id <FILE_ID>`, `-d, --destination <FOLDER_ID>` | Restore a trashed file into a destination folder. |
| `trash-restore-folder` | `trash:restore:folder` | `-i, --id <FOLDER_ID>`, `-d, --destination <FOLDER_ID>` | Restore a trashed folder into a destination folder. |
| `trash-clear` | `trash:clear` | `-f, --force` | Empty the trash permanently. Prompts unless `--force` (required in `--json` mode). |
| `delete-permanently-file` | `delete:permanently:file` | `-i, --id <FILE_ID>` | Permanently delete a file. Cannot be undone. |
| `delete-permanently-folder` | `delete:permanently:folder` | `-i, --id <FOLDER_ID>` | Permanently delete a folder. Cannot be undone. |
| `sync-up` | `sync:up` | `-l, --local <DIR>`, `-r, --remote <FOLDER_ID>`, `--delete[=trash\|permanent]`, `--dry-run` | Make a remote folder match a local one (push): upload new/changed files, optionally trash/delete remote extras. |
| `sync-down` | `sync:down` | `-l, --local <DIR>`, `-r, --remote <FOLDER_ID>`, `--delete`, `--dry-run` | Make a local folder match a remote one (pull): download new/changed files, optionally remove local extras. |
| `webdav` | | `-l, --host <HOST>`, `-p, --port <PORT>`, `-s, --https`, `--cert/--key <PEM>`, `-c, --create-full-path`, `-a, --custom-auth` + `-u/-w`, `-d, --delete-permanently` | Serve your Drive over WebDAV in the foreground until Ctrl-C. Requires the `webdav` feature (on by default). |

### Sync

`sync-up` and `sync-down` do a single **one-way** reconcile pass then exit (not a daemon). The source side always wins — there is no bidirectional mode and no conflict resolution. Files are keyed by relative path; change detection compares size, then `modificationTime` (±2s tolerance). `--dry-run` prints the plan without transferring; `--json` emits a summary object with counts + per-action list. Downloaded files are stamped with the remote modification time so repeat runs are idempotent. `--delete` is opt-in and off by default; it prunes both extra files **and** extra folders (deleting the top-most extra folder cascades its whole subtree).

### WebDAV

`webdav` serves your Drive (or the active workspace) over WebDAV so it can be mounted by any WebDAV client (Finder, Windows Explorer, `rclone`, Cyberduck, …). Unlike the official CLI — which runs the server as a pm2-managed background service configured through a separate `webdav-config` command — this port runs it **in the foreground as a normal command**: all options are passed inline, and the server runs until you stop it with Ctrl-C.

```sh
internxt webdav                                 # http://127.0.0.1:3005
internxt webdav --host 0.0.0.0 --port 8080      # accept clients on your LAN
internxt webdav --create-full-path              # auto-create missing parent folders on upload
internxt webdav --custom-auth -u alice -w secret  # require HTTP Basic auth from clients
internxt webdav --https                         # HTTPS with a self-signed cert (needs `webdav-tls`)
internxt webdav --https --cert cert.pem --key key.pem   # HTTPS with your own certificate
```

Supported methods: `OPTIONS`, `PROPFIND`, `GET`/`HEAD` (with `Range`), `PUT`, `MKCOL`, `DELETE`, `MOVE`, `LOCK`/`UNLOCK`. `COPY` and `PROPPATCH` return `501 Not Implemented` (as upstream). Transfers stream through the same encrypt/decrypt path as `upload-file`/`download-file`, so large files never load into RAM. `DELETE` trashes items by default (`--delete-permanently` to hard-delete). Paths are resolved by walking the folder tree, so it stays workspace-aware when a workspace is active.

Notes / current limitations: HTTP by default (enable HTTPS with the `webdav-tls` feature); no local database cache (og uses sqlite); `--timeout` is accepted for parity but not yet wired to a request-timeout layer. A background task refreshes the session token hourly (same near-expiry refresh as the other commands), so a long-running server keeps working. The `webdav-config` / `webdav start|stop|status` subcommands are intentionally left for a possible future daemon mode.

## Usage examples

```sh
internxt login                                  # SSO: opens a browser to authenticate
internxt login-legacy --email you@example.com   # email/password; prompts for password (+ 2FA if enabled)
internxt upload-file -f ./file.bin -i <folder-uuid>
internxt upload-folder -f ./my-folder           # recursive; -i for a destination folder
internxt download-file -i <file-uuid> -d ./out --overwrite
internxt list -e                                # root folder, extended view
internxt list -i <folder-uuid> --json           # machine-readable output
internxt create-folder -n "Reports" -i <parent-uuid>
internxt trash-file -i <file-uuid>
internxt trash-clear --force
internxt sync-up   -l ./my-folder -r <folder-uuid> --dry-run   # preview a push
internxt sync-up   -l ./my-folder -r <folder-uuid> --delete    # push, trashing remote extras
internxt sync-down -l ./my-folder -r <folder-uuid>             # pull new/changed files
internxt webdav --host 0.0.0.0 --port 8080                     # serve Drive over WebDAV (Ctrl-C to stop)
```

Credentials are stored AES-encrypted at `~/.internxt-cli/.inxtcli` (same location/format as the node CLI).

## Architecture

- `src/crypto.rs` — password hashing, CryptoJS-compatible AES, file-key derivation, AES-256-CTR.
- `src/auth.rs` — login flow + credential persistence + token/workspace refresh.
- `src/sso.rs` — web-based SSO login (feature `sso`): local callback server + browser.
- `src/api.rs` — Drive REST client.
- `src/network.rs` — bridge (network) client: start/PUT/finish + download links/shards.
- `src/commands.rs` — streaming upload/download + recursive folder upload.
- `src/sync.rs` — one-way folder sync (`sync-up` / `sync-down`): tree diff + reconcile.
- `src/webdav/` — WebDAV server (feature `webdav`): axum server, method handlers, path resolution, XML.
- `src/drive_ops.rs` — drive-management commands (list, folder/file ops, trash).
- `src/workspaces.rs` — workspaces list/use/unset + workspace mnemonic decrypt.
- `src/output.rs` — global `--json` / human output switch.

### Streaming / memory

- Upload < 100MB: single presigned PUT, body streamed disk → encrypt → HTTP (~1MB RAM).
- Upload ≥ 100MB: multipart, 15MB parts, up to 10 concurrent PUTs (~150MB RAM cap regardless of file size).
- Download: shard response streamed → decrypt → disk.

## Configuration

API endpoints and app constants default to the public Internxt values (see `src/config.rs`) and can be overridden via environment variables of the same name (`DRIVE_NEW_API_URL`, `NETWORK_URL`, etc.).

## Credits

Protocol and crypto behaviour reverse-engineered from the official Internxt packages
(`@internxt/cli`, `@internxt/sdk`, `@internxt/lib` — MIT; `@internxt/inxt-js`). See `LICENSE`.

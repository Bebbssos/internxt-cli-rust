# internxt-cli-rust

An unofficial Rust port of the [Internxt CLI](https://github.com/internxt/cli), focused on speed and low memory use. File transfers are fully streaming — large files (100GB+) are uploaded and downloaded without ever being loaded into RAM.

> Not affiliated with or endorsed by Internxt.

> This port was written mostly by [Claude Code](https://claude.com/claude-code), porting the behaviour of the official Node/TypeScript CLI.

## Status

Implemented: authentication, streaming file transfers, recursive folder upload, one-way folder sync (`sync-up` / `sync-down`), and the full set of drive-management commands (list, create/move/rename, trash + restore, permanent delete). Every command supports `--json` for scripting.

Crypto is byte-for-byte compatible with the node CLI (verified by cross-check tests, see `cargo test`).

Not yet ported: SSO login, workspaces, thumbnails, token refresh on expiry, the `config` command, and WebDAV server mode.

## Compatibility with `@internxt/cli`

This is intended to be a **mostly drop-in replacement** for the official [`@internxt/cli`](https://www.npmjs.com/package/@internxt/cli): for the implemented commands the names, aliases, flags, endpoints, payloads, credential file location/format (`~/.internxt-cli/.inxtcli`) and crypto all match, so the two are interchangeable for everyday `login` / `upload` / `download` / list / move / rename / trash workflows.

It is *not* a 100% replacement — anything in "Not yet ported" above is simply absent. Beyond missing commands, the known **behavioural differences (breaking changes)** are:

- **`login` is legacy-only.** Our `login` is the official CLI's `login-legacy` flow (email + password + optional 2FA). The official `login` (SSO / web browser callback) is not implemented.
- **`--json` output schema differs.** We emit a simplified `{ "success": true, ... }` object per command rather than the exact oclif JSON envelope. Field names mostly match, but don't assume byte-identical structure.
- **No interactive prompting for missing flags.** Required arguments that are absent produce a clap usage error instead of an interactive prompt. The exceptions that still prompt: `login` (email/password/2FA) and `trash-clear` (confirmation, unless `--force`).
- **Personal drive only.** Workspaces are unsupported; there is no `workspaces-*` command and no workspace credential handling.
- **No token auto-refresh.** A stored token is used as-is; on expiry you must `login` again (the official CLI refreshes near-expiry).
- **No thumbnail generation** on upload.
- **Only the mnemonic is persisted** from the decrypted account keys (not the ecc/kyber private keys).
- **Plain-text table output** uses simple aligned columns rather than the official CLI's boxed tables. Use `--json` for stable machine-readable output.

## Build

```sh
cargo build --release
# binary at target/release/internxt
```

## Commands

All commands accept the global `--json` flag, which prints a single JSON result object and suppresses progress output. IDs are Drive UUIDs. Where a destination/parent folder id is optional, leaving it empty targets your root folder.

| Command | Aliases | Arguments | Description |
|---|---|---|---|
| `login` | | `-e, --email <EMAIL>`, `-p, --password <PASSWORD>`, `-w, --twofactor <CODE>` | Log in with email + password (legacy flow). Prompts for any missing value; 2FA prompted if the account requires it. |
| `logout` | | | Invalidate the session server-side and clear local credentials. |
| `whoami` | | | Show the currently logged-in user. |
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

### Sync

`sync-up` and `sync-down` do a single **one-way** reconcile pass then exit (not a daemon). The source side always wins — there is no bidirectional mode and no conflict resolution. Files are keyed by relative path; change detection compares size, then `modificationTime` (±2s tolerance). `--dry-run` prints the plan without transferring; `--json` emits a summary object with counts + per-action list. Downloaded files are stamped with the remote modification time so repeat runs are idempotent. `--delete` is opt-in and off by default; it prunes both extra files **and** extra folders (deleting the top-most extra folder cascades its whole subtree).

## Usage examples

```sh
internxt login --email you@example.com          # prompts for password (+ 2FA if enabled)
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
```

Credentials are stored AES-encrypted at `~/.internxt-cli/.inxtcli` (same location/format as the node CLI).

## Architecture

- `src/crypto.rs` — password hashing, CryptoJS-compatible AES, file-key derivation, AES-256-CTR.
- `src/auth.rs` — login flow + credential persistence.
- `src/api.rs` — Drive REST client.
- `src/network.rs` — bridge (network) client: start/PUT/finish + download links/shards.
- `src/commands.rs` — streaming upload/download + recursive folder upload.
- `src/sync.rs` — one-way folder sync (`sync-up` / `sync-down`): tree diff + reconcile.
- `src/drive_ops.rs` — drive-management commands (list, folder/file ops, trash).
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

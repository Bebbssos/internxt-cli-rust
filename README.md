# internxt-cli-rust

An unofficial Rust port of the [Internxt CLI](https://github.com/internxt/cli), focused on speed and low memory use. File transfers are fully streaming — large files (100GB+) are uploaded and downloaded without ever being loaded into RAM.

> Not affiliated with or endorsed by Internxt.

## Status

Implemented: authentication, streaming file transfers, recursive folder upload, and the full set of drive-management commands (list, create/move/rename, trash + restore, permanent delete). Every command supports `--json` for scripting.

Crypto is byte-for-byte compatible with the node CLI (verified by cross-check tests, see `cargo test`).

Not yet ported: SSO login, workspaces, thumbnails, token refresh on expiry, the `config` command, and WebDAV server mode.

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
```

Credentials are stored AES-encrypted at `~/.internxt-cli/.inxtcli` (same location/format as the node CLI).

## Architecture

- `src/crypto.rs` — password hashing, CryptoJS-compatible AES, file-key derivation, AES-256-CTR.
- `src/auth.rs` — login flow + credential persistence.
- `src/api.rs` — Drive REST client.
- `src/network.rs` — bridge (network) client: start/PUT/finish + download links/shards.
- `src/commands.rs` — streaming upload/download + recursive folder upload.
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

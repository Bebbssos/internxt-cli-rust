# TODO / Roadmap

Status of the Rust port vs the official node Internxt CLI.

## Upstream baseline (what we ported from)

Reference sources live in `./og` (git-ignored). Fetch them with
`./scripts/fetch-og-sources.sh`. The port is based on these exact commits —
diff upstream against them to find changes worth pulling in.

| Package | Repo | Pinned commit | Tag / version | Commit date |
|---|---|---|---|---|
| cli | github.com/internxt/cli | `166fb5a77dab27aea3e9cdb1af0e1713d9dde04e` | v1.6.5 | 2026-06-16 |
| inxt-js | github.com/internxt/inxt-js | `a27cc91cde65700ebca088ebba3870d9bbf2a94f` | v3.3.1 | 2026-06-16 |
| lib | github.com/internxt/lib | `22eaae309ad17a8c39c03b742bca631feca0a8f9` | (master) v1.4.2 | 2026-06-17 |
| sdk | github.com/internxt/sdk | `aa97c980562926b3425a290f8ca39ea5c1f45a15` | v1.17.9 | 2026-06-17 |

> Note: the **released** CLI (v1.6.5) actually runs older published deps —
> `@internxt/inxt-js@3.2.2`, `@internxt/sdk@1.17.5`, `@internxt/lib@1.4.2`
> (present in `og/node_modules`). The source repos above are slightly ahead.
> Behaviour we ported matches the runtime versions; check both if something differs.

## Commands

### Done
- [x] `login` — legacy email/password + 2FA (node `login-legacy`)
- [x] `upload-file` — streaming, single-part + multipart
- [x] `download-file` — streaming decrypt-to-disk
- [x] `logout` — best-effort `/auth/logout` + clear local credentials
- [x] `whoami`
- [x] `list` — list folder contents (`-e` extended; paginated)
- [x] `create-folder`
- [x] `move-file`, `move-folder`
- [x] `rename-file`, `rename-folder`
- [x] `trash-file`, `trash-folder`, `trash-list`, `trash-restore-file`, `trash-restore-folder`, `trash-clear`
- [x] `delete-permanently-file`, `delete-permanently-folder`

> Most of the above live in `src/drive_ops.rs` (REST helpers in `src/api.rs`).
> Verified end-to-end against a real account (full file + folder lifecycles).
- [x] `upload-folder` — recursive folder upload (tree created sequentially, files
      uploaded with bounded concurrency; in `src/commands.rs`)

### Missing commands
- [ ] `login` (SSO) — node `login.ts` web-based flow (local callback server + browser)
- [ ] `config` — show/set config
- [ ] `logs`
- [ ] `webdav`, `webdav-config`, `add-cert` — WebDAV server mode (big; express + sqlite + TLS)
- [x] `workspaces-list`, `workspaces-use`, `workspaces-unset` (`src/workspaces.rs`).
      `use` decrypts the workspace mnemonic (PGP ecc + Kyber512 hybrid) and stores
      the workspace context; all drive/network commands then operate within it.

## Beyond-og features (not in the official CLI)

These three are extensions we add on top of node parity. Design below; defaults
chosen pragmatically (flag them if you disagree before implementing).

### 1. `upload-file` from stdin — **[x] done**

Pipe bytes in instead of reading a path. Filename is required (stdin has none).

Implemented: `--stdin` + `--name` (required) + optional `--size`. With `--size`,
stdin streams directly (single-part); without it, stdin is spooled to a self-deleting
temp file (`TempSpool`) to learn the length, then uploaded normally (single/multipart).
`upload_single` is now generic over `AsyncRead`; `upload_stream_to_network` is the
stdin entry point. `src/commands.rs`.

- New flags on `upload-file`: `--stdin` (read body from stdin) and `--name <NAME>`
  (required with `--stdin`; supplies the Drive name + extension split, like a path).
  Optional `--size <BYTES>`.
- **Why a size is needed at all:** the network `start` call needs the total size
  and part count *up front*, and each S3 presigned PUT needs a `Content-Length`.
  Unknown-length streaming is therefore impossible end-to-end.
- **Decision (default):** if `--size` is given, stream stdin straight through CTR
  with that length (true streaming, no temp file). Otherwise **spool stdin to a
  temp file** (`config::tmp dir` / same FS as creds), `fsync`, read `len`, upload
  normally, delete temp. RAM stays bounded either way.
- Refactor: extract the network-upload core to accept a generic
  `AsyncRead + size` source rather than only `&Path`. `upload_file_to_network`,
  `upload_single`, `upload_multipart` all currently take `path: &Path` + `size`;
  generalise to a reader factory (multipart re-opens / re-seeks, so for the
  `--size` direct-stream path we either disallow multipart or buffer per-part —
  single-part is the simple v1; multipart-from-temp works for the spool path).
- `create_file_entry`: `modificationTime`/`creationTime` = now (no fs mtime).

### 2. `download-file` to stdout — **[x] done**

Stream decrypted bytes to stdout instead of a file.

Implemented: `--stdout` flag. Output target is `Box<dyn AsyncWrite>` (file or
`tokio::io::stdout()`); all status + progress routed to stderr (`output::status_err`,
`eprint!`) so the data stream stays clean; rejected in `--json` mode. `src/commands.rs`.

- New: `--stdout` flag (or accept `-d -`). Mutually exclusive with `--directory`.
- Refactor `download_file` core to write to a `Box<dyn AsyncWrite + Unpin + Send>`
  (file today, `tokio::io::stdout()` for this). Decrypt loop is unchanged.
- Progress bar + all status text must go to **stderr** (never stdout) so the piped
  data is clean. In `--json` mode `--stdout` is rejected (binary + JSON conflict).
- No `--overwrite` semantics for stdout.

### 3. One-way folder sync — **[x] done** (not in og)

Implemented in `src/sync.rs` as `sync-up` / `sync-down` (aliases `sync:up` /
`sync:down`), verified end-to-end against a real account (up, down, idempotent
re-runs both directions, change detection, `--delete` both ways, roundtrip byte
integrity). Notes vs. the design below:
- Shared tree walk: `walk_local` (rel-path keyed, skips symlinks) + `build_remote_tree`
  (recurses `get_folder_subfolders`/`subfiles`, paginated, EXISTS-filtered).
- Change detection as designed (size, then `modificationTime` ±2s). Uploads set
  `modificationTime` = local mtime; **downloads stamp the local file's mtime to the
  remote value** (`filetime` crate) so repeat runs are idempotent.
- `--delete` is opt-in (off by default). sync-up: `--delete`/`--delete=trash` trashes
  remote extras, `--delete=permanent` deletes them. sync-down: `--delete` removes
  local extras (`--delete=trash` for OS trash still unimplemented — errors clearly).
- `--dry-run` prints the plan and exits; `--json` emits a summary object (counts +
  per-action list). Transfers use the bounded semaphore (10 concurrent).
- **Folder pruning:** `--delete` prunes extra *folders* too, not just files. Only
  the top-most extra dir in a chain is deleted (trash/remove cascades its subtree);
  files/subfolders under it are skipped to avoid double-work. A folder is never
  "extra" if any synced file lives under it (local files always register all their
  parent dirs), so pruning can't remove needed content.
- **Scope cut for v1:** re-upload of a changed file trashes the old remote entry +
  creates a new one (loses the remote uuid — no in-place content-replace API).

**One-shot, not a daemon.** Each command does a single reconcile pass when invoked,
then exits. No filesystem watcher, no long-running process, no polling loop.

**Two separate commands, one direction each. No bidirectional mode, no conflict
policy** — the source side always wins. This sidesteps the "changed on both sides"
ambiguity (no sync-state DB needed).

- `sync-up   --local <DIR> --remote <FOLDER-UUID>` — make **remote match local**
  (push). Upload new/changed local files; optionally trash remote extras.
- `sync-down --local <DIR> --remote <FOLDER-UUID>` — make **local match remote**
  (pull). Download new/changed remote files; optionally delete local extras.

Aliases `sync:up` / `sync:down` for colon-style parity.

**Tree walk** (keyed by **relative path**, POSIX-normalised, case-sensitive):
- local: reuse `scan_dir` (pre-orders folders before files, skips symlinks).
- remote: recurse `get_folder_subfolders` / `get_folder_subfiles` from the root uuid
  into a `rel-path -> {uuid, size, modificationTime, fileId, bucket}` map. Cache
  folder uuids so file ops target/create the right parent.

**Change detection** (same for both commands): compare `size` first; if equal,
compare `modificationTime` (remote stores it — set on every `create_file_entry`;
present in `og .../storage/types.ts DriveFileData.modificationTime`). Local mtime
from fs metadata; both as epoch seconds, ±2s tolerance for FS granularity. Different
size ⇒ changed; equal size + mtime within tolerance ⇒ unchanged (skip).

**`sync-up` (local → remote):**
| state | action (default) | `--delete` |
|---|---|---|
| local only | upload | upload |
| remote only | — (leave) | trash remote (`--delete=permanent` ⇒ delete-permanently) |
| both, changed | re-upload (local wins) | same |
| both, equal | skip | skip |
- Re-upload of a changed file: trash old remote entry + `create_file_entry` for new
  content (no in-place content-replace API in use today; loses remote uuid —
  acceptable v1). Remote-missing parent folders → `create_folder` (retry helper).

**`sync-down` (remote → local):**
| state | action (default) | `--delete` |
|---|---|---|
| remote only | download | download |
| local only | — (leave) | remove local file (`--delete=trash` ⇒ OS trash if a lib is added) |
| both, changed | re-download (remote wins) | same |
| both, equal | skip | skip |
- Re-download: stream to a temp sibling, fsync, atomic rename over target. Missing
  local parent dirs → `create_dir_all`. Default local delete = **remove**
  (no cross-platform trash lib in v1).

**Shared infra.** Reuse the bounded-semaphore concurrency from `upload_folder`
(`MAX_CONCURRENT_FILE_UPLOADS`). `--dry-run` prints planned actions and exits.
`--json` ⇒ single summary object (counts + per-action list). Workspace-aware free via
`DriveApi::for_credentials` + `creds.*()`. Factor the tree-diff (build local map,
build remote map, classify each rel-path) into one shared helper; the two commands
only differ in which side drives and which delete path runs.

**Open questions (implement time):**
- Local cache DB later (node uses sqlite) for true deletion detection / faster diffs?
  Out of scope for v1.
- Case-insensitive FS (macOS/Windows) collision handling.
- Preserve remote uuid on content update (needs in-place replace path).

---

## Feature gaps in already-ported commands

### Auth
- [x] Token expiry check + refresh (`getAuthDetails` / `refreshUserToken`) —
      `auth::get_auth_details()` decodes the JWT `exp`, errors if expired, and
      refreshes via `GET /users/cli/refresh` when within 2 days of expiry (then
      persists the new token). All network commands call it instead of
      `read_credentials`.
- [x] Workspace credential handling + refresh. `Credentials::net_user/net_pass/
      bucket/mnemonic/root_folder` pick workspace vs personal; `get_auth_details`
      re-fetches workspace credentials (`/workspaces/{id}/credentials`) when the
      workspace token nears expiry.
- [x] TOTP secret → code generation (node `--twofactortoken`/`-t` flag via otpauth;
      `crypto::totp_now`, RFC 6238 SHA-1/30s/6-digit, base32 secret).
- [ ] Login with generated PGP/Kyber keys (`/auth/cli/login/access`). We use
      `/auth/login/access` WITHOUT keys; fine for existing accounts (the server
      returns the stored keys), but registration / key-update paths are unsupported.
- [x] Decrypt + persist private keys (ecc/kyber). Login decrypts `keys.ecc/kyber`
      (lib AES-GCM) and stores them base64; used to decrypt workspace mnemonics
      (`crypto::decrypt_workspace_key`: OpenPGP ecc + Kyber512 KEM + blake3 XOF).

### Upload
- [ ] Thumbnail generation + upload (`ThumbnailService`).
- [ ] Retry with backoff on transient failures (`uploadFileWithRetry`, MAX_RETRIES/DELAYS_MS).
- [ ] Upload size limit check (node enforces a per-file limit; see CLI README "40GB").
- [x] Workspace uploads — `createFileEntry` routes to `/workspaces/{id}/files`
      and uses the workspace bucket + network creds + mnemonic when active.
- [ ] Real progress bar (currently minimal prints).
- [ ] HMAC on upload (sdk now stores hmac on upload — see sdk commit; node inxt-js
      passes `hmac: undefined`, so we skip it. Revisit if server starts requiring it.)

### Download
- [ ] Range / resume support (`RangeOptions`, partial download with CTR offset IV math).
- [ ] Shared-link / token download (`getDownloadLinks(..., token)`).
- [ ] Multi-shard parallel download (we download shards sequentially).
- [x] Workspace downloads — uses workspace bucket + network creds + mnemonic.

### Infrastructure / parity
- [ ] `.env` loading (node uses dotenv). We hardcode public defaults in `src/config.rs`,
      overridable via env vars. Decide if a `.env` file should be supported.
- [ ] SDK-style request retry layer (node `SdkManager` maxRetries: 3).
- [ ] Local drive cache (node uses better-sqlite3 / typeorm `internxt-cli-drive.db`).
- [x] JSON output mode (`--json`) for scripting parity — global flag; each command
      emits a single JSON object on success/error (`src/output.rs`).
- [x] `--non-interactive` flag semantics (global `-x`/`INXT_NONINTERACTIVE`; never
      prompts, errors on missing required value — login email/password/2FA, trash-clear).

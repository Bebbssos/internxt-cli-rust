# CLAUDE.md

Context for working on this repo. Read this first.

## What this is

An unofficial **Rust port of the Internxt CLI** (the official one is Node/TypeScript).
Goal: faster, lower memory, single static binary. Implements **login** (legacy
email/password + web-based **SSO** universal-link flow), **upload-file,
download-file** with fully streaming transfers (handles 100GB+ files without loading
them into RAM), plus **upload-folder** (recursive) and the drive-management commands
(**logout, whoami, list, create-folder, move/rename/trash/restore file+folder,
trash-list, trash-clear, delete-permanently**), **workspaces** (list/use/unset,
with workspace-scoped upload/download/list/etc), one-way folder **sync**
(**sync-up** / **sync-down**), a foreground multi-protocol **serve** command
(**serve webdav,fuse,…** — runs one or more Drive-exposing backends at once,
sharing creds/cache/upload-limit) covering a **WebDAV server** and a **FUSE mount**
(Unix only) that exposes the Drive as a local read-write filesystem, plus a
top-level **mount** convenience command (FUSE only). All commands support a global
**`--json`** flag. Not affiliated with Internxt.

Roadmap + what's missing: see [TODO.md](TODO.md).

## Reference (original) sources — `./og`

The node sources we port from live in `./og` (**git-ignored**, not pushed).
Recreate with:

```sh
./scripts/fetch-og-sources.sh
```

Layout once fetched:
- `og/cli/`     — official CLI source (`src/commands`, `src/services`, `src/utils`)
- `og/sdk/`     — `@internxt/sdk` source (auth, drive/storage, network upload/download)
- `og/inxt-js/` — `@internxt/inxt-js` source (network upload V2 + multipart, file-key crypto)
- `og/lib/`     — `@internxt/lib` source (aes-gcm helpers)
- `og/node_modules/@internxt/*` — the published deps the released CLI actually runs

Pinned upstream commits/versions are recorded in [TODO.md](TODO.md). Diff against
those to find upstream changes worth porting.

### Where each ported piece comes from
- Password hash / CryptoJS AES / credentials file → `og/cli/src/services/crypto.service.ts`, `config.service.ts`
- Private-key AES-GCM decrypt → `og/lib/src/aes/`
- Login flow + endpoints → `og/sdk/src/auth/index.ts`, `og/cli/src/services/auth.service.ts`
- SSO / universal-link login (callback server + browser) → `og/cli/src/commands/login.ts`, `og/cli/src/services/universal-link.service.ts`
- File-key derivation (`GenerateFileKey`), shard hash (`ripemd160(sha256)`) → `og/inxt-js/src/lib/utils/crypto/crypto.ts`, `og/inxt-js/src/lib/utils/streams/Hasher.ts`
- Upload start/PUT/finish + multipart → `og/sdk/src/network/`, `og/inxt-js/src/lib/core/upload/`
- Drive REST (createFileEntry, file meta) → `og/sdk/src/drive/storage/index.ts`, `og/cli/src/services/drive/`
- Workspaces (list/use/unset, creds, folder/trash routing) → `og/cli/src/commands/workspaces-*.ts`, `og/cli/src/services/drive/workspace.service.ts`, `og/sdk/src/workspaces/`
- Workspace mnemonic decrypt (PGP ecc + Kyber512 hybrid + blake3) → `og/cli/src/services/keys.service.ts`, `og/cli/src/utils/crypto.utils.ts`

## Rust layout

| File | Responsibility |
|---|---|
| `src/main.rs` | clap CLI dispatch (all subcommands) |
| `src/config.rs` | API URLs + app constants (public; env-overridable), paths |
| `src/crypto.rs` | passToHash, CryptoJS AES-CBC, lib AES-GCM, GenerateFileKey, AES-256-CTR, hashes, workspace-key decrypt (PGP/Kyber512/blake3) |
| `src/auth.rs` | legacy login flow (+ ecc/kyber key decrypt), SSO credential build, credential save/read, token + workspace-cred refresh |
| `src/sso.rs` | web-based SSO login (feature `sso`): local axum callback server + browser open, universal-link flow |
| `src/api.rs` | Drive REST client (`DRIVE_NEW_API_URL`); workspace-aware via `for_credentials` (`x-internxt-workspace` header + `/workspaces/{id}/…` routing) |
| `src/network.rs` | bridge/network client (`NETWORK_URL`), streaming PUT/GET |
| `src/commands.rs` | upload-file/download-file/upload-folder orchestration (streaming + multipart + recursive) |
| `src/sync.rs` | sync-up/sync-down: one-way folder reconcile (local↔remote tree diff, size+mtime change detection, optional `--delete`) |
| `src/serve/` | code shared by the serve backends (webdav + fuse): `run.rs` (the `serve` **orchestrator**: parses the comma-list of protocols, builds one `Shared` bundle — `SharedCreds`+cache+global upload semaphore+root — fetched/created once, spawns each backend as a task, and owns the single Ctrl-C that shuts them all down via a `watch` flag), `tree.rs` (protocol-agnostic Drive-tree walk: `FileItem`/`FolderItem`/`DriveItem`, `list_folders`/`list_files`/`resolve_folder`), `cache.rs` (`FolderCache` TTL listing cache), `creds.rs` (`SharedCreds` + hourly token refresh) |
| `src/webdav/` | WebDAV server (feature `webdav`): `mod.rs` (axum server + dispatch + auth + TLS), `handlers.rs` (method handlers), `resource.rs` (WebDAV URL parsing + `resolve_item`; re-exports `serve::tree`), `xml.rs` (response XML) |
| `src/fuse/` | FUSE mount (feature `fuse`, Unix only): `mod.rs` (`MountConfig` + `fuser::spawn_mount2` + Ctrl-C unmount), `fs.rs` (`fuser::Filesystem` impl: inode table, sync-callback→tokio bridge, streaming reads, temp-file write buffering) |
| `src/drive_ops.rs` | logout, whoami, list, create/move/rename/trash/delete folder+file |
| `src/workspaces.rs` | workspaces list/use/unset; decrypts workspace mnemonic |
| `src/output.rs` | global `--json` vs human output switch (`emit`/`status`/`emit_error`) |
| `src/models.rs` | serde DTOs; `Credentials` workspace/keys + net/bucket/mnemonic helpers |

## Key facts / decisions

- **Crypto is byte-for-byte compatible with node.** Verified by tests in `src/crypto.rs`
  against `scripts/ref.js`, plus `tests/keys_crypto.rs` against `scripts/ref-keys.json`
  (workspace key crypto: PGP ecc + Kyber512 + blake3). Run `cargo test`.
  **If you touch crypto, re-run these.** Regenerate the keys vectors with
  `NODE_PATH=og/node_modules node scripts/ref-keys.js > scripts/ref-keys.json`
  (the file is git-ignored / non-deterministic; the keys test skips if it is absent).
- **Login** (legacy) uses `/auth/login/access` WITHOUT *sending* keys, but reads the
  stored `keys.ecc/kyber` the server returns, decrypts them (lib AES-GCM), and persists
  them base64 — needed to decrypt workspace mnemonics. 2FA prompted when required.
- **Subcommands**: `login` = SSO when built with the `sso` feature (default) else legacy;
  `login-legacy` = always email/password; `login-sso` = always SSO (errors if built
  `--no-default-features`). aliases `login:legacy` / `login:sso`.
- **SSO login** (`src/sso.rs`, feature `sso`): binds a local axum server on `0.0.0.0:port`
  (random port if unset), opens the browser at `{DRIVE_WEB_URL}/login?universalLink=true&redirectUri={b64}`,
  waits for the `/callback` GET carrying base64 `mnemonic`/`newToken`/`privateKey`,
  redirects the browser to `auth-link-ok`/`auth-link-error`, then builds creds via
  `refreshUserCredentials` (`GET /users/cli/refresh`). `--host` sets the address the
  browser uses (for cross-device login); `--port` fixes the callback port. **Kyber key is
  dropped in SSO** (link never carries it, refresh returns it encrypted, no password to
  decrypt) — ecc-only workspaces work; hybrid-Kyber workspaces need `login-legacy`.
- **Workspaces**: `workspaces use` decrypts the workspace mnemonic
  (`crypto::decrypt_workspace_key`: ecc-only or `HybridMode$kyberCt$eccCt`) and stores a
  `WorkspaceContext`. When active, drive calls carry `x-internxt-workspace` and route to
  `/workspaces/{id}/…` for list/create-folder/file-entry/trash; transfers use the
  workspace bucket + network creds (`sha256(networkPass)`) + workspace mnemonic.
- **Kyber512** (`safe_pqc_kyber`, kyber512 feature) interops with node's
  `@dashlane/pqc-kem-kyber512`. Stored kyber key is coerced to the raw 1632-byte secret
  key (handles both base64(raw) and node's double-base64 form). **`pgp` crate needs
  aes-gcm's re-exported `aes` (`aes_gcm::aes::Aes256`), not the top-level `aes` 0.9.**
- **Network auth**: bridge basic auth = `user : sha256(pass).hex` — personal
  `pass = userId`, workspace `pass = networkPass`.
- **File key**: `sha512( sha512(seed || bucketIdBytes)[0..32] || index )[0..32]`,
  where `seed = bip39 mnemonicToSeed(mnemonic)`, `iv = index[0..16]`, cipher = AES-256-CTR.
- **Shard hash** (sent in finish): `ripemd160(sha256(ciphertext))`, computed incrementally.
- **Multipart** threshold 100MB, 15MB parts, 10 concurrent PUTs. CTR stream is continuous
  across the whole file, then sliced. RAM bounded (~150MB) regardless of file size.
- **Credentials**: AES-encrypted (CryptoJS-compatible, `APP_CRYPTO_SECRET`) at
  `~/.internxt-cli/.inxtcli` — same path/format as the node CLI.
- **No secrets in the repo.** `APP_CRYPTO_SECRET` / `DESKTOP_HEADER` in `config.rs` are
  public per-client constants from the upstream `.env.template`, not user data.
  `og/` and `target/` are git-ignored.

- **serve orchestration** (`src/serve/run.rs`): `serve` takes a **comma-separated
  positional protocol list** (`serve webdav`, `serve webdav,fuse`) — *not* a clap
  subcommand — so several backends run in one foreground process. **Shared** knobs are
  bare flags (`-i/--folder-uuid`, `--cache-ttl`/`--no-cache`, `-d/--delete-permanently`,
  `--spool`, `--spool-dir`, `--max-concurrent-uploads`, `--read-only`) and back one shared
  `SharedCreds` (+one refresh task), one `FolderCache`, and one **global** upload
  `Semaphore` across all backends; **protocol-specific** knobs are prefixed
  (`--webdav-host/-port/-https/-cert/-key/-timeout/-create-full-path/-custom-auth/-username/-password`,
  `--fuse-mountpoint/-allow-other`). `--read-only` and `--spool` are now shared and honored
  by both backends (WebDAV rejects mutating verbs — PUT/MKCOL/DELETE/MOVE/COPY/PROPPATCH — with
  403 when read-only; FUSE writes always spool so `--spool` is a no-op there). Each backend is
  `webdav::serve` / `fuse::serve`, which take the `Arc<Shared>` bundle + a shutdown future
  (built from a `watch` flag) instead of creating their own creds/cache/refresh or owning
  Ctrl-C. The top-level `mount` command is a thin wrapper that calls the same orchestrator
  with a single-`fuse` protocol list. Unknown / not-yet-implemented (`smb`/`sftp`) / feature-off
  protocols in the list error at parse time.
- **WebDAV** (`src/webdav/`, feature `webdav`, default-on): a **foreground** WebDAV
  backend under `serve` (`serve webdav`, runs until Ctrl-C) — deliberately *not* og's pm2
  daemon + `webdav-config`. axum `Router::fallback` catches all methods (incl. custom
  verbs PROPFIND/MKCOL/MOVE/LOCK/…) and dispatches by `req.method()`. Path→item
  resolution walks the folder tree via `get_folder_subfolders/subfiles` (workspace-aware),
  no sqlite cache — but subfolder listings are cached in-process (`webdav/cache.rs`,
  `FolderCache`, keyed by folder uuid) for `--cache-ttl` seconds (default 5, `--no-cache`
  / `0` disables) to collapse the repeated tree walks a burst of requests does; file
  listings stay live (no stale-file→duplicate-upload), and folder mutations invalidate.
  Concurrent creation of the same folder (many parallel PUTs with `--create-full-path`, or
  racing MKCOLs) is conflict-tolerant: `get_or_create_child` re-lists and adopts the
  winner's folder instead of surfacing a 409/500 — important because an early error on a
  large-bodied PUT makes hyper drop the (undrained) TCP connection, which WinSCP reports as
  "connection aborted / Could not read status line". GET streams through the shared
  `commands::download_file_to_writer` (RAM-bounded) and honours HTTP `Range:` (`Accept-Ranges:
  bytes`) — a range request is a **true ranged download**: `download_file_to_writer`'s `range`
  arg seeks the CTR keystream (`Ctr::seek`) to the window start and fetches only the covering
  shards, byte-ranging the boundary shards over HTTP (`NetworkApi::download_shard_range_stream`,
  expects `206`). Shards are one continuous CTR stream sliced by ordered `index`; per-shard
  `size` (now deserialized on `DownloadShard`) maps a file offset to shard + in-shard byte range.
  Works for **single- and multi-shard** files (og only did single-shard ranges); if a multi-shard
  file's per-shard sizes are absent it falls back to decrypt-from-0-and-skip-prefix. PUT has two
  upload strategies:
  **default streams** the live client body straight to network storage
  (`upload_stream_to_network`, Content-Length known; falls back to spooling when no length
  is declared) — RAM-bounded, no temp disk, lowest latency. **`--spool`** instead writes
  the whole request body to a temp file (`spool_body`, dir = `--spool-dir` or system temp)
  then uploads from disk (`upload_file_to_network`). Spooling is opt-in because it costs
  temp disk + latency, but is more robust for concurrent/slow clients (WinSCP): it (a)
  fully drains the client body up front, so a storage-side failure can't leave an undrained
  body (that undrained body is what makes hyper abort the TCP connection — WinSCP "Could
  not send request body / connection aborted"), and (b) feeds the storage PUT from local
  disk continuously, avoiding the S3 `RequestTimeout` ("socket not read from or written to
  within the timeout period") that a stalled / executor-starved live stream trips under a
  concurrent upload burst. Temp file removed on drop. **`--max-concurrent-uploads N`**
  (0 = unlimited) gates the PUT transfer section via a `tokio::Semaphore` on `Ctx`
  (`acquire_upload`); `1` serializes uploads so a client that fans out many parallel PUTs
  (WinSCP) can't overwhelm the storage backend — the permit is taken *before* the body is
  read, so queued requests hold no open storage socket. DELETE trashes unless
  `--delete-permanently`. Default **HTTP**; **HTTPS** is the
  separate **`webdav-tls`** feature (rustls via `axum-server` + `rcgen` self-signed or
  `--cert`/`--key`). `COPY`/`PROPPATCH` → 501, `UNLOCK`/unknown verbs handled (mirrors og).
  **Every method that returns without consuming its request body drains it first**
  (`handlers::drain_body` / `unsupported`; also `LOCK`) — WinSCP reuses connections and
  *pipelines* (PUT → PROPPATCH → PROPPATCH → PROPFIND), and an undrained PROPPATCH body
  makes hyper close the socket after the 501 (it can't resync a pipelined stream), which
  RSTs WinSCP's next in-flight request → "connection aborted / Could not send request
  body". This — not the earlier folder-race / storage-timeout theories — was the actual
  cause of the WinSCP aborts; confirmed via `tshark` (server FIN/RST right after each
  PROPPATCH 501). Response XML is hand-built
  in `xml.rs` to match og's `D:`-namespaced shape. Creds live behind the shared
  `serve::creds::SharedCreds` (`RwLock<Arc<Credentials>>`); `serve::creds::spawn_refresh`
  calls `get_auth_details` hourly and swaps in the refreshed token, and each request
  snapshots via `ctx.creds()` — so a long-running server survives token expiry.
  `webdav-config` / `webdav start|stop|status` names are left free for a future daemon mode.

- **FUSE mount** (`src/fuse/`, feature `fuse`, default-on, **Unix only**): a **foreground**
  `mount <MOUNTPOINT>` command (runs until Ctrl-C, then unmounts) that exposes the Drive as
  a local **read-write** filesystem via the `fuser` crate. `fuse` is default-on but declared
  under `[target.'cfg(unix)'.dependencies]` and all code is gated `#[cfg(all(unix, feature =
  "fuse"))]`, so Windows default builds still compile (feature inert there). `fuser` links
  **libfuse** (default fuser features) — the only linkage that covers Linux **and** macOS +
  FreeBSD — so a build needs `libfuse3-dev`+`pkg-config` (macFUSE on macOS, `fusefs-libs3` on
  FreeBSD) and a FUSE driver at runtime. `fuser`'s `Filesystem` callbacks are **synchronous**
  (run on its own session thread); each network-touching method clones the shared `Arc<Inner>`
  and `tokio::runtime::Handle::spawn`s a task that does the async work and calls `reply.*`
  (the `Reply*` objects are `Send`), so the session thread never blocks and ops run concurrent.
  **Inodes**: Drive items are keyed by uuid but the kernel needs stable `u64` inodes, so `fs.rs`
  keeps a two-way `InodeTable` (`ino→NodeData`, `(parent_ino,name)→ino`); root is inode 1,
  children interned lazily on `lookup`/`readdir`. **Reads** use a per-handle sequential
  streaming reader (`ReadHandle`): a spawned `download_file_to_writer` decrypts into a duplex
  pipe, `read_at` serves forward reads from it (small forward gaps are skipped in-stream) and
  only a **backward** seek restarts the stream — so a sequential read of a huge file is one
  continuous decrypt (verified: a 40 MB multi-shard file reads back byte-exact). A restart is a
  **true ranged seek** (see `download_file_to_writer` range path below): the CTR keystream is
  seeked to the offset and only the covering shards are fetched (boundary shards byte-ranged over
  HTTP), so a backward/random seek into a huge file no longer re-downloads the prefix. **Writes**
  use the whole-file model (Internxt
  has no partial update): a write/`create` handle is backed by a temp file (`--spool-dir` or
  system temp); existing content is **materialized lazily** (downloaded into the temp file on
  first read/write, skipped when the open is immediately truncated to 0 — so `>file` costs no
  download), writes patch the temp file, and on the final `release` the temp is uploaded whole
  (`upload_file_to_network`) and a **new** Drive file entry replaces the old (create-then-trash,
  or delete permanently with `--delete-permanently`). `mkdir`/`rmdir`/`unlink`/`rename` map to
  the Drive REST ops (workspace-aware); mutations invalidate the shared `FolderCache`. Uses
  the shared serve flags: `--cache-ttl`/`--no-cache` (also the kernel attr/entry TTL),
  `-d/--delete-permanently`, `--spool-dir`, `--max-concurrent-uploads`, `-i/--folder-uuid`
  (mount a subfolder, root default), `--read-only`; plus fuse-only `--fuse-allow-other` (needs
  `user_allow_other` in `/etc/fuse.conf`; only then is `AutoUnmount` added, since it requires an
  allow-other ACL). Under `mount` these are the bare flag names (`--allow-other`, etc.); under
  `serve` the fuse-only ones are `--fuse-`-prefixed. `serve smb`/`serve sftp` would add a
  `smb`/`sftp` arm to `serve::run` and a `serve::serve`-style backend reusing
  `serve::{run::Shared,tree,cache,creds}` the same way.

## Build / test / run

```sh
cargo build --release        # -> target/release/internxt (SSO + WebDAV-over-HTTP + FUSE on by default; FUSE needs libfuse3-dev+pkg-config on Unix)
cargo build --release --features webdav-tls   # + HTTPS for the WebDAV server
cargo build --release --no-default-features   # smaller binary, legacy login only (no axum/open/webdav/fuse)
cargo test                   # crypto cross-check vs node (requires scripts/ref.js values; no network)
node scripts/ref.js          # regenerate reference crypto values (needs og/ fetched)

target/release/internxt login                       # SSO (opens browser); use --host/--port for another device
target/release/internxt login-legacy --email you@example.com   # email/password
target/release/internxt upload-file -f ./file -i <folder-uuid>
target/release/internxt download-file -i <file-uuid> -d ./out --overwrite

# workspaces: list, switch into one (subsequent commands scope to it), switch back
target/release/internxt workspaces-list
target/release/internxt workspaces-use -i <workspace-uuid>
target/release/internxt workspaces-use --personal   # or: workspaces-unset

# serve: one or more protocols in the foreground (comma-list; Ctrl-C stops all)
target/release/internxt serve webdav --webdav-host 0.0.0.0 --webdav-port 8080
target/release/internxt serve webdav,fuse --fuse-mountpoint ~/drive   # both at once, shared cache/creds
target/release/internxt serve webdav,fuse --fuse-mountpoint ~/drive --read-only -i <folder-uuid>
cargo build --release --features webdav-tls   # add HTTPS (--webdav-https, self-signed or --webdav-cert/--webdav-key)

# mount: expose Drive as a local filesystem via FUSE (Unix only; Ctrl-C to unmount)
mkdir -p ~/drive && target/release/internxt mount ~/drive
target/release/internxt mount ~/drive --read-only            # browse/read only
target/release/internxt mount ~/drive -i <folder-uuid>       # mount a subfolder as root
```

## Conventions

- Match the node CLI's observable behaviour (endpoints, payloads, file locations) so the
  two are interchangeable. When in doubt, read `og/` and mirror it.
- Keep transfers streaming — never read a whole file into memory.
- Endpoints/constants belong in `src/config.rs`, env-overridable.

## License

AGPL-3.0 (see `LICENSE`, `NOTICE`). Chosen because `@internxt/inxt-js` ships an AGPL-3.0
license file (its `package.json` says ISC — upstream conflict). `sdk`/`lib`/`cli` are MIT.

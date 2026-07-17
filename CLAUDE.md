# CLAUDE.md

Context for working on this repo. Read this first.

## What this is

An unofficial **Rust port of the Internxt CLI** (official one is Node/TypeScript).
Goal: faster, lower memory, single static binary. Fully streaming transfers (handles
100GB+ files without loading them into RAM). Not affiliated with Internxt.

Commands: **login** (legacy email/password + web **SSO**), **upload-file /
download-file / upload-folder**, drive management (**logout, whoami, list,
create-folder, move/rename/trash/restore, trash-list/clear, delete-permanently**),
**workspaces** (list/use/unset, workspace-scoped), one-way **sync** (sync-up/down),
a foreground multi-protocol **serve** (`serve webdav,fuse,smb,…`) exposing Drive
over **WebDAV**, a **FUSE mount** (Unix) and an **SMB/CIFS** share, plus a
top-level **mount** convenience command. Global **`--json`** flag on all commands.

Roadmap + what's missing, pinned upstream commits: see [TODO.md](TODO.md).

## Reference (original) sources — `./og`

Node sources we port from live in `./og` (**git-ignored**, not pushed). Recreate:

```sh
./scripts/fetch-og-sources.sh
```

- `og/cli/` — official CLI (`src/commands`, `src/services`, `src/utils`)
- `og/sdk/` — `@internxt/sdk` (auth, drive/storage, network up/download)
- `og/inxt-js/` — `@internxt/inxt-js` (network upload V2 + multipart, file-key crypto)
- `og/lib/` — `@internxt/lib` (aes-gcm helpers)
- `og/node_modules/@internxt/*` — the published deps the released CLI runs

Where each ported piece comes from:
- Password hash / CryptoJS AES / creds file → `og/cli/src/services/crypto.service.ts`, `config.service.ts`
- Private-key AES-GCM decrypt → `og/lib/src/aes/`
- Login flow + endpoints → `og/sdk/src/auth/index.ts`, `og/cli/src/services/auth.service.ts`
- SSO / universal-link login → `og/cli/src/commands/login.ts`, `og/cli/src/services/universal-link.service.ts`
- File-key derivation, shard hash → `og/inxt-js/src/lib/utils/crypto/crypto.ts`, `.../streams/Hasher.ts`
- Upload start/PUT/finish + multipart → `og/sdk/src/network/`, `og/inxt-js/src/lib/core/upload/`
- Drive REST → `og/sdk/src/drive/storage/index.ts`, `og/cli/src/services/drive/`
- Workspaces → `og/cli/src/commands/workspaces-*.ts`, `.../workspace.service.ts`, `og/sdk/src/workspaces/`
- Workspace mnemonic decrypt (PGP ecc + Kyber512 + blake3) → `og/cli/src/services/keys.service.ts`, `.../utils/crypto.utils.ts`

## Workspace layout

Cargo **workspace**, two crates (bin `internxt` built by the cli crate):

- **`crates/internxt-core`** (lib `internxt_core`) — the Drive **engine**: *how
  Internxt works* (endpoints, payloads, encryption) as a protocol-agnostic library.
  No terminal output, clap, indicatif, prompts, or browser — a CLI, a WebDAV/FUSE
  server, or a future GUI all build on it.
- **`crates/internxt-cli`** (bin `internxt`) — the **front-end**: clap dispatch, the
  transfer UX, drive-ops/workspaces wrappers, sync, WebDAV + FUSE + serve backends.

**Core↔front-end seams** (core leaves these to the caller):
- byte progress → [`ProgressSink`] trait (`inc(bytes)`); cli wraps indicatif via
  `output::bar_sink`. Transfers take `Option<Arc<dyn ProgressSink>>` (`None` = discard).
- `auth::login` 2FA code → injected `on_need_2fa` closure.
- `sso::login` browser-open + URL → injected `on_login_url` closure.
- refresh warnings → injected `on_warn` closure on `auth::refresh_credentials`.
- **credential persistence** → entirely cli. Core `refresh_credentials(creds, on_warn)`
  is pure (returns refreshed creds + `changed` flag, no fs). cli `auth.rs` owns the file
  (`~/.internxt-cli/.inxtcli`), read/save, and the read→refresh→save `get_auth_details`.
- **native transfers** → core `fs` feature (default on): path-based
  `upload_file_to_network` / multipart / `create_folder_with_retry` (`tokio::fs`/`spawn`/`time`).
  `--no-default-features` leaves the reader/writer surface (crypto/api/network + generic
  streaming `upload_stream_to_network` / `download_file_to_writer`). cli always enables `fs`.
- cli keeps thin shim modules `auth`/`sso` wrapping the core fns with these callbacks +
  persistence, so the rest of the cli calls `auth::*` / `sso::*` unchanged.

| File | Responsibility |
|---|---|
| **core** `config.rs` | API URLs + app constants (public; env-overridable) |
| **core** `crypto.rs` | passToHash, CryptoJS AES-CBC, lib AES-GCM, GenerateFileKey, AES-256-CTR, hashes, workspace-key decrypt (PGP/Kyber512/blake3) |
| **core** `auth.rs` | legacy login (+ ecc/kyber key decrypt; `on_need_2fa`), SSO cred build, pure `refresh_credentials` (token + workspace-cred refresh; `on_warn`; no fs) |
| **core** `sso.rs` | web SSO flow logic (no axum/tokio-net): build login URL, decode callback, build creds. Local transport behind `SsoCallbackServer` trait; `on_login_url` callback |
| **core** `api.rs` | Drive REST client; workspace-aware via `for_credentials` (`x-internxt-workspace` + `/workspaces/{id}/…`) |
| **core** `network.rs` | bridge/network client, streaming PUT/GET |
| **core** `transfer.rs` | streaming transfer primitives + `ProgressSink`. Always on: generic `upload_stream_to_network` / `download_file_to_writer` (ranged). `fs` feature: path-based `upload_file_to_network`, multipart, `create_folder_with_retry` |
| **core** `progress.rs` | `ProgressSink` trait + `noop_sink` |
| **core** `models.rs` | serde DTOs; `Credentials` workspace/keys + net/bucket/mnemonic helpers |
| **cli** `main.rs` | clap dispatch (all subcommands) |
| **cli** `auth.rs` / `sso.rs` | shims over core: cred persistence, 2FA prompt, browser-open, refresh warnings |
| **cli** `commands.rs` | upload/download/folder-upload orchestration (args, progress bars, `--json`) |
| **cli** `sync.rs` | sync-up/down: one-way folder reconcile (tree diff, size+mtime detection, optional `--delete`) |
| **cli** `serve/` | shared serve code: `run.rs` (orchestrator: parse protocol comma-list, build one `Shared` bundle — creds+cache+global upload semaphore+root — spawn each backend, own the single Ctrl-C via a `watch` flag), `tree.rs` (Drive-tree walk), `cache.rs` (`FolderCache` TTL cache), `creds.rs` (`SharedCreds` + hourly refresh) |
| **cli** `webdav/` | WebDAV server (feature `webdav`): `mod.rs` (axum + dispatch + auth + TLS), `handlers.rs`, `resource.rs` (URL parse + resolve), `xml.rs` |
| **cli** `fuse/` | FUSE mount (feature `fuse`, Unix): `mod.rs` (mount + Ctrl-C unmount), `fs.rs` (`fuser::Filesystem`: inode table, sync→tokio bridge, streaming reads, temp-file writes) |
| **cli** `smb/` | SMB/CIFS server (feature `smb`, default-off): `mod.rs` (`SmbConfig` + `serve`: build `SmbServer`, wire shutdown), `fs.rs` (Drive-backed `ShareBackend`/`Handle`: path resolve, streaming reads, temp-file writes). SMB2/3 wire protocol from the `smb-server` git-dep fork |
| **cli** `drive_ops.rs` | logout, whoami, list, create/move/rename/trash/delete |
| **cli** `workspaces.rs` | workspaces list/use/unset; decrypts workspace mnemonic |
| **cli** `output.rs` | `--json` vs human switch (`emit`/`status`/`emit_error`); `bar_sink` |

## Key facts / decisions

- **Crypto is byte-for-byte compatible with node.** Verified by tests in
  `crates/internxt-core/src/crypto.rs` vs `scripts/ref.js`, plus
  `crates/internxt-core/tests/keys_crypto.rs` vs `scripts/ref-keys.json` (workspace key
  crypto). **If you touch crypto, run `cargo test`.** Regenerate keys vectors:
  `NODE_PATH=og/node_modules node scripts/ref-keys.js > scripts/ref-keys.json` (git-ignored;
  the keys test skips if absent).
- **Login** (legacy) uses `/auth/login/access` WITHOUT *sending* keys, but reads the
  returned `keys.ecc/kyber`, decrypts them (lib AES-GCM), and persists base64 — needed to
  decrypt workspace mnemonics. 2FA prompted when required.
- **Subcommands**: `login` = SSO when built with `sso` (default) else legacy;
  `login-legacy` = always email/password; `login-sso` = always SSO. Aliases `login:legacy` / `login:sso`.
- **SSO login** (cli feature `sso`): flow logic in core `sso.rs` builds
  `{DRIVE_WEB_URL}/login?universalLink=true&redirectUri={b64}`, decodes the callback's
  base64 `mnemonic`/`newToken`/`privateKey`, builds creds via `refreshUserCredentials`. Local
  callback transport behind the `SsoCallbackServer` trait; native impl (temp axum server on
  `0.0.0.0:port`, redirects browser to `auth-link-ok`/`-error`) + browser-open live in cli
  `sso.rs`. `--host` sets the browser-facing address (cross-device); `--port` fixes the
  callback port. **Kyber key dropped in SSO** (link never carries it, refresh returns it
  encrypted, no password) — ecc-only workspaces work; hybrid-Kyber ones need `login-legacy`.
- **Workspaces**: `workspaces use` decrypts the mnemonic (`crypto::decrypt_workspace_key`:
  ecc-only or `HybridMode$kyberCt$eccCt`), stores a `WorkspaceContext`. When active, drive
  calls carry `x-internxt-workspace` + route to `/workspaces/{id}/…`; transfers use the
  workspace bucket + network creds (`sha256(networkPass)`) + workspace mnemonic.
- **Kyber512** (`safe_pqc_kyber`) interops with node's `@dashlane/pqc-kem-kyber512`. Stored
  key coerced to raw 1632-byte secret (handles base64(raw) and node's double-base64). **`pgp`
  crate needs aes-gcm's re-exported `aes` (`aes_gcm::aes::Aes256`), not top-level `aes` 0.9.**
- **Network auth**: bridge basic auth = `user : sha256(pass).hex` — personal `pass = userId`,
  workspace `pass = networkPass`.
- **File key**: `sha512( sha512(seed || bucketIdBytes)[0..32] || index )[0..32]`, where
  `seed = bip39 mnemonicToSeed(mnemonic)`, `iv = index[0..16]`, cipher = AES-256-CTR.
- **Shard hash** (sent in finish): `ripemd160(sha256(ciphertext))`, incremental.
- **Multipart**: threshold 100MB, 15MB parts, 10 concurrent PUTs. Continuous CTR stream
  sliced into parts. RAM bounded (~150MB) regardless of file size.
- **Ranged download** (`download_file_to_writer` `range` arg): with per-shard sizes, seeks the
  CTR keystream to the window start and fetches only the covering shards (boundary shards
  byte-ranged over HTTP, expects `206`). Single- and multi-shard; falls back to
  decrypt-from-0-and-skip-prefix when a multi-shard file's per-shard sizes are absent.
- **Credentials**: AES-encrypted (CryptoJS-compatible, `APP_CRYPTO_SECRET`) at
  `~/.internxt-cli/.inxtcli` — same path/format as the node CLI.
- **No secrets in the repo.** `APP_CRYPTO_SECRET` / `DESKTOP_HEADER` in `config.rs` are public
  per-client constants from upstream `.env.template`, not user data. `og/`, `target/` git-ignored.

### serve / WebDAV / FUSE / SMB

- **serve** (`serve/run.rs`): `serve` takes a **comma-separated positional protocol list**
  (`serve webdav,fuse,smb`) — not a clap subcommand — so several backends run in one foreground
  process (Ctrl-C stops all). **Shared** flags (`-i/--folder-uuid`, `--cache-ttl`/`--no-cache`,
  `-d/--delete-permanently`, `--spool`, `--spool-dir`, `--max-concurrent-uploads`,
  `--read-only`) back one `SharedCreds`, one `FolderCache`, one global upload `Semaphore`.
  **Protocol-specific** flags are prefixed (`--webdav-*`, `--fuse-*`, `--smb-*`). `mount` is a
  thin wrapper calling the orchestrator with a single-`fuse` list. Unknown / not-implemented
  (`sftp`) / feature-off protocols error at parse time.
- **WebDAV** (feature `webdav`, default-on): foreground backend, not og's pm2 daemon. axum
  `Router::fallback` dispatches by method. Path→item walks the folder tree (workspace-aware),
  subfolder listings cached in-process (`--cache-ttl`, default 5s); file listings stay live.
  Concurrent same-folder creation is conflict-tolerant (`get_or_create_child` re-lists and
  adopts the winner). GET streams through `download_file_to_writer` (RAM-bounded) and honours
  HTTP `Range:`. PUT: **default** streams the live body straight to storage
  (`upload_stream_to_network`, needs Content-Length; else spools); **`--spool`** writes the
  body to a temp file first then uploads from disk (more robust for slow/concurrent clients,
  costs temp disk + latency). `--max-concurrent-uploads N` (0 = unlimited) gates PUTs via a
  semaphore; the permit is taken *before* the body is read. DELETE trashes unless
  `--delete-permanently`. **HTTPS** = separate `webdav-tls` feature (rustls, self-signed or
  `--cert`/`--key`). **Every method that returns without consuming its body drains it first**
  — WinSCP pipelines requests and an undrained body makes hyper drop the connection (the actual
  cause of WinSCP aborts; see `handlers.rs` for the full story). Response XML hand-built in
  `xml.rs` to match og's `D:`-namespaced shape.
- **FUSE mount** (feature `fuse`, default-on, **Unix only**): foreground `mount <MOUNTPOINT>`
  exposing Drive as a **read-write** filesystem via `fuser`. Declared under
  `[target.'cfg(unix)'.dependencies]`, code gated `cfg(all(unix, feature = "fuse"))` — Windows
  default builds still compile (feature inert). `fuser` links **libfuse** → build needs
  `libfuse3-dev`+`pkg-config` (macFUSE on macOS) and a FUSE driver at runtime. `Filesystem`
  callbacks are synchronous; each network op spawns a tokio task and replies async, so ops run
  concurrent. Stable `u64` inodes via a two-way `InodeTable`. **Reads**: per-handle sequential
  streaming reader — one continuous decrypt for a forward read; a backward seek restarts as a
  true ranged seek (fetches only covering shards). **Writes**: whole-file model (Internxt has no
  partial update) — a temp file backs the handle, existing content materialized lazily (skipped
  on truncate-to-0), on `release` the temp uploads whole and a **new** Drive file entry replaces
  the old. `mkdir`/`rmdir`/`unlink`/`rename` → Drive REST; mutations invalidate the cache. Uses
  shared serve flags; fuse-only `--fuse-allow-other` (needs `user_allow_other` in
  `/etc/fuse.conf`). Under `mount` the shared flags use bare names; under `serve` fuse-only ones
  are `--fuse-`-prefixed.
- **SMB/CIFS** (feature `smb`, **default-OFF**, all platforms): foreground `serve smb` exposing
  Drive as a **read-write** SMB2/3 share (`net use` on Windows, `mount -t cifs` on Linux/macOS).
  The wire protocol + NTLM auth + signing come from the `smb-server` crate (MIT), pulled as a
  **git dependency** on a fork (`github.com/Bebbssos/rust-smb-server`) — upstream 0.4.1 doesn't
  re-export its own trait types and returned a per-open volatile id from QUERY_INFO (stale-handle
  on the Linux cifs client); the fork fixes both, pending an upstream PR. We only implement its
  `ShareBackend`/`Handle`
  over Drive in `smb/fs.rs` — the SMB analog of `fuser::Filesystem`. **Path-based** (no inode
  table): every op re-resolves the path via the shared `tree` walk. **Reads** reuse FUSE's
  sequential streaming reader (forward-skip, backward-seek → ranged download). **Writes** reuse
  FUSE's whole-file temp model (materialize existing lazily, upload whole on `close`, replace the
  Drive entry). Delete = CREATE `delete_on_close` / SET_INFO disposition → backend `unlink`
  (trash unless `-d`); `rename` = SET_INFO → Drive move+rename. `--smb-host/-port/-share/
  -username/-password` (port default 4445, since 445 needs root/admin; a password is recommended
  — Windows refuses anonymous). The `smb-server` git-dep is optional, only pulled when the `smb`
  feature is on, so a default `cargo build` never fetches or compiles it.

## Build / test / run

```sh
cargo build --release        # -> target/release/internxt (SSO + WebDAV-over-HTTP + FUSE default; FUSE needs libfuse3-dev+pkg-config on Unix)
cargo build --release --features webdav-tls    # + HTTPS for WebDAV
cargo build --release --features smb           # + SMB/CIFS share (default-off; pulls smb-server fork)
cargo build --release --no-default-features    # smaller: legacy login only (no axum/open/webdav/fuse)
cargo test                                     # crypto cross-check vs node (no network)

target/release/internxt login                                  # SSO (opens browser); --host/--port for another device
target/release/internxt login-legacy --email you@example.com
target/release/internxt upload-file -f ./file -i <folder-uuid>
target/release/internxt download-file -i <file-uuid> -d ./out --overwrite
target/release/internxt workspaces-use -i <workspace-uuid>     # scope later commands; --personal to unset
target/release/internxt serve webdav,fuse --fuse-mountpoint ~/drive   # both, shared cache/creds
target/release/internxt serve smb --smb-password secret               # SMB share (needs --features smb); mount -t cifs //host/internxt /mnt -o port=4445
target/release/internxt mount ~/drive                          # FUSE only (Unix; Ctrl-C to unmount)
```

## Conventions

- Match the node CLI's observable behaviour (endpoints, payloads, file locations) so the two
  are interchangeable. When in doubt, read `og/` and mirror it.
- Keep transfers streaming — never read a whole file into memory.
- Endpoints/constants belong in `crates/internxt-core/src/config.rs`, env-overridable.

## License

AGPL-3.0 (see `LICENSE`, `NOTICE`). Chosen because `@internxt/inxt-js` ships an AGPL-3.0
license file (its `package.json` says ISC — upstream conflict). `sdk`/`lib`/`cli` are MIT.

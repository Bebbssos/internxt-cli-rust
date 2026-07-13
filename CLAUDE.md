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
(**sync-up** / **sync-down**), and a foreground **WebDAV server** (**webdav**).
All commands support a global **`--json`** flag. Not affiliated with Internxt.

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
| `src/webdav/` | WebDAV server (feature `webdav`): `mod.rs` (axum server + dispatch + auth + TLS), `handlers.rs` (method handlers), `resource.rs` (path→Drive-item resolution via tree walk), `xml.rs` (response XML) |
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

- **WebDAV** (`src/webdav/`, feature `webdav`, default-on): a **foreground** `webdav`
  command (runs until Ctrl-C) — deliberately *not* og's pm2 daemon + `webdav-config`.
  Options are inline flags. axum `Router::fallback` catches all methods (incl. custom
  verbs PROPFIND/MKCOL/MOVE/LOCK/…) and dispatches by `req.method()`. Path→item
  resolution walks the folder tree via `get_folder_subfolders/subfiles` (workspace-aware),
  no sqlite cache. GET/PUT stream through the shared `commands::{download_file_to_writer,
  upload_stream_to_network}` (RAM-bounded); PUT with no Content-Length spools to a temp
  file. DELETE trashes unless `--delete-permanently`. Default **HTTP**; **HTTPS** is the
  separate **`webdav-tls`** feature (rustls via `axum-server` + `rcgen` self-signed or
  `--cert`/`--key`). `COPY`/`PROPPATCH` → 501 (mirrors og). Response XML is hand-built
  in `xml.rs` to match og's `D:`-namespaced shape. Creds live behind an
  `RwLock<Arc<Credentials>>`; a background task calls `get_auth_details` hourly and
  swaps in the refreshed token, and each request snapshots via `ctx.creds()` — so a
  long-running server survives token expiry. `webdav-config` / `webdav
  start|stop|status` names are left free for a future daemon mode.

## Build / test / run

```sh
cargo build --release        # -> target/release/internxt (SSO + WebDAV-over-HTTP on by default)
cargo build --release --features webdav-tls   # + HTTPS for the WebDAV server
cargo build --release --no-default-features   # smaller binary, legacy login only (no axum/open/webdav)
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

# webdav: serve Drive over WebDAV in the foreground (Ctrl-C to stop)
target/release/internxt webdav --host 0.0.0.0 --port 8080
cargo build --release --features webdav-tls   # add HTTPS (--https, self-signed or --cert/--key)
```

## Conventions

- Match the node CLI's observable behaviour (endpoints, payloads, file locations) so the
  two are interchangeable. When in doubt, read `og/` and mirror it.
- Keep transfers streaming — never read a whole file into memory.
- Endpoints/constants belong in `src/config.rs`, env-overridable.

## License

AGPL-3.0 (see `LICENSE`, `NOTICE`). Chosen because `@internxt/inxt-js` ships an AGPL-3.0
license file (its `package.json` says ISC — upstream conflict). `sdk`/`lib`/`cli` are MIT.

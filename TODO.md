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
- [ ] `workspaces-list`, `workspaces-use`, `workspaces-unset`

## Feature gaps in already-ported commands

### Auth
- [ ] Token expiry check + refresh (`getAuthDetails` / `refreshUserToken`) — currently
      stored token is used as-is; no renewal on near-expiry.
- [ ] Workspace credential handling + refresh (`getNetworkCreds` workspace branch).
- [x] TOTP secret → code generation (node `--twofactortoken`/`-t` flag via otpauth;
      `crypto::totp_now`, RFC 6238 SHA-1/30s/6-digit, base32 secret).
- [ ] Login with generated PGP/Kyber keys (`/auth/cli/login/access`). We use
      `/auth/login/access` WITHOUT keys; fine for existing accounts, but registration
      or key-update paths are unsupported.
- [ ] Decrypt + persist private keys (ecc/kyber) — currently only mnemonic is decrypted.

### Upload
- [ ] Thumbnail generation + upload (`ThumbnailService`).
- [ ] Retry with backoff on transient failures (`uploadFileWithRetry`, MAX_RETRIES/DELAYS_MS).
- [ ] Upload size limit check (node enforces a per-file limit; see CLI README "40GB").
- [ ] Workspace uploads (`createFileEntry` via workspaces client).
- [ ] Real progress bar (currently minimal prints).
- [ ] HMAC on upload (sdk now stores hmac on upload — see sdk commit; node inxt-js
      passes `hmac: undefined`, so we skip it. Revisit if server starts requiring it.)

### Download
- [ ] Range / resume support (`RangeOptions`, partial download with CTR offset IV math).
- [ ] Shared-link / token download (`getDownloadLinks(..., token)`).
- [ ] Multi-shard parallel download (we download shards sequentially).
- [ ] Workspace downloads.

### Infrastructure / parity
- [ ] `.env` loading (node uses dotenv). We hardcode public defaults in `src/config.rs`,
      overridable via env vars. Decide if a `.env` file should be supported.
- [ ] SDK-style request retry layer (node `SdkManager` maxRetries: 3).
- [ ] Local drive cache (node uses better-sqlite3 / typeorm `internxt-cli-drive.db`).
- [x] JSON output mode (`--json`) for scripting parity — global flag; each command
      emits a single JSON object on success/error (`src/output.rs`).
- [x] `--non-interactive` flag semantics (global `-x`/`INXT_NONINTERACTIVE`; never
      prompts, errors on missing required value — login email/password/2FA, trash-clear).

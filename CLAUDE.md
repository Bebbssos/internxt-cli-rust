# CLAUDE.md

Context for working on this repo. Read this first.

## What this is

An unofficial **Rust port of the Internxt CLI** (the official one is Node/TypeScript).
Goal: faster, lower memory, single static binary. Implements **login, upload-file,
download-file** with fully streaming transfers (handles 100GB+ files without loading
them into RAM), plus **upload-folder** (recursive) and the drive-management commands
(**logout, whoami, list, create-folder, move/rename/trash/restore file+folder,
trash-list, trash-clear, delete-permanently**) and **workspaces** (list/use/unset,
with workspace-scoped upload/download/list/etc). All commands support a global
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
| `src/auth.rs` | login flow (+ ecc/kyber key decrypt), credential save/read, token + workspace-cred refresh |
| `src/api.rs` | Drive REST client (`DRIVE_NEW_API_URL`); workspace-aware via `for_credentials` (`x-internxt-workspace` header + `/workspaces/{id}/…` routing) |
| `src/network.rs` | bridge/network client (`NETWORK_URL`), streaming PUT/GET |
| `src/commands.rs` | upload-file/download-file/upload-folder orchestration (streaming + multipart + recursive) |
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
- **Login** uses `/auth/login/access` WITHOUT *sending* keys, but reads the stored
  `keys.ecc/kyber` the server returns, decrypts them (lib AES-GCM), and persists them
  base64 — needed to decrypt workspace mnemonics. 2FA prompted when required.
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

## Build / test / run

```sh
cargo build --release        # -> target/release/internxt
cargo test                   # crypto cross-check vs node (requires scripts/ref.js values; no network)
node scripts/ref.js          # regenerate reference crypto values (needs og/ fetched)

target/release/internxt login --email you@example.com
target/release/internxt upload-file -f ./file -i <folder-uuid>
target/release/internxt download-file -i <file-uuid> -d ./out --overwrite

# workspaces: list, switch into one (subsequent commands scope to it), switch back
target/release/internxt workspaces-list
target/release/internxt workspaces-use -i <workspace-uuid>
target/release/internxt workspaces-use --personal   # or: workspaces-unset
```

## Conventions

- Match the node CLI's observable behaviour (endpoints, payloads, file locations) so the
  two are interchangeable. When in doubt, read `og/` and mirror it.
- Keep transfers streaming — never read a whole file into memory.
- Endpoints/constants belong in `src/config.rs`, env-overridable.

## License

AGPL-3.0 (see `LICENSE`, `NOTICE`). Chosen because `@internxt/inxt-js` ships an AGPL-3.0
license file (its `package.json` says ISC — upstream conflict). `sdk`/`lib`/`cli` are MIT.

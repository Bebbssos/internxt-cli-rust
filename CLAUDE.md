# CLAUDE.md

Context for working on this repo. Read this first.

## What this is

An unofficial **Rust port of the Internxt CLI** (the official one is Node/TypeScript).
Goal: faster, lower memory, single static binary. Implements **login, upload-file,
download-file** with fully streaming transfers (handles 100GB+ files without loading
them into RAM), plus the drive-management commands (**logout, whoami, list,
create-folder, move/rename/trash/restore file+folder, trash-list, trash-clear,
delete-permanently**). Not affiliated with Internxt.

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

## Rust layout

| File | Responsibility |
|---|---|
| `src/main.rs` | clap CLI dispatch (all subcommands) |
| `src/config.rs` | API URLs + app constants (public; env-overridable), paths |
| `src/crypto.rs` | passToHash, CryptoJS AES-CBC, lib AES-GCM, GenerateFileKey, AES-256-CTR, hashes |
| `src/auth.rs` | login flow + credential save/read (AES-encrypted file) |
| `src/api.rs` | Drive REST client (`DRIVE_NEW_API_URL`) |
| `src/network.rs` | bridge/network client (`NETWORK_URL`), streaming PUT/GET |
| `src/commands.rs` | upload/download orchestration (streaming + multipart) |
| `src/drive_ops.rs` | logout, whoami, list, create/move/rename/trash/delete folder+file |
| `src/models.rs` | serde DTOs |

## Key facts / decisions

- **Crypto is byte-for-byte compatible with node.** Verified by tests in `src/crypto.rs`
  against reference values from `scripts/ref.js` (which runs the node algorithms using
  `og/node_modules/bip39`). Run `cargo test`. **If you touch crypto, re-run these.**
- **Login** uses `/auth/login/access` WITHOUT keys (skips PGP+Kyber keygen). Works for
  existing accounts. 2FA prompted when the account requires it.
- **Network auth**: bridge basic auth = `bridgeUser : sha256(userId).hex`.
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
```

## Conventions

- Match the node CLI's observable behaviour (endpoints, payloads, file locations) so the
  two are interchangeable. When in doubt, read `og/` and mirror it.
- Keep transfers streaming — never read a whole file into memory.
- Endpoints/constants belong in `src/config.rs`, env-overridable.

## License

AGPL-3.0 (see `LICENSE`, `NOTICE`). Chosen because `@internxt/inxt-js` ships an AGPL-3.0
license file (its `package.json` says ISC — upstream conflict). `sdk`/`lib`/`cli` are MIT.

# internxt-cli-rust

An unofficial Rust port of the [Internxt CLI](https://github.com/internxt/cli), focused on speed and low memory use. Implements the core flows: **login**, **upload**, **download** — with fully streaming transfers (handles large files without loading them into RAM).

> Not affiliated with or endorsed by Internxt.

## Status

| Feature | State |
|---|---|
| Login (email/password + 2FA) | ✅ |
| Upload (streaming, single-part + multipart) | ✅ |
| Download (streaming, decrypt-to-disk) | ✅ |
| Workspaces, thumbnails, token refresh, list/move/trash, WebDAV | ⛔ not yet |

Crypto is byte-for-byte compatible with the node CLI (verified by cross-check tests, see `cargo test`).

## Build

```sh
cargo build --release
# binary at target/release/internxt
```

## Usage

```sh
internxt login --email you@example.com          # prompts for password (+ 2FA if enabled)
internxt upload-file -f ./file.bin              # -i <folder-uuid> for a destination folder
internxt download-file -i <file-uuid> -d ./out  # --overwrite to replace
```

Credentials are stored AES-encrypted at `~/.internxt-cli/.inxtcli` (same location/format as the node CLI).

## Architecture

- `src/crypto.rs` — password hashing, CryptoJS-compatible AES, file-key derivation, AES-256-CTR.
- `src/auth.rs` — login flow + credential persistence.
- `src/api.rs` — Drive REST client.
- `src/network.rs` — bridge (network) client: start/PUT/finish + download links/shards.
- `src/commands.rs` — streaming upload/download orchestration.

### Streaming / memory

- Upload < 100MB: single presigned PUT, body streamed disk → encrypt → HTTP (~1MB RAM).
- Upload ≥ 100MB: multipart, 15MB parts, up to 10 concurrent PUTs (~150MB RAM cap regardless of file size).
- Download: shard response streamed → decrypt → disk.

## Configuration

API endpoints and app constants default to the public Internxt values (see `src/config.rs`) and can be overridden via environment variables of the same name (`DRIVE_NEW_API_URL`, `NETWORK_URL`, etc.).

## Credits

Protocol and crypto behaviour reverse-engineered from the official Internxt packages
(`@internxt/cli`, `@internxt/sdk`, `@internxt/lib` — MIT; `@internxt/inxt-js`). See `LICENSE`.

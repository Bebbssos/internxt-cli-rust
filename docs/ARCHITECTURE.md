# Architecture

How the project is laid out, and how to reuse the engine as a library. For usage,
see the [README](../README.md); for porting/contributor notes, see [CLAUDE.md](../CLAUDE.md).

## Two crates

A Cargo workspace with two crates. The binary `internxt` is built by the cli crate.

### `internxt-core` (lib `internxt_core`)

The Internxt Drive **engine**: *how Internxt works* (endpoints, payloads, encryption)
as a **protocol-agnostic library**. No terminal output, no clap, no indicatif, no
prompts, no browser. A CLI, a WebDAV/FUSE server, or a future GUI all build on it
without touching the API/crypto details.

| Module | Responsibility |
|---|---|
| `config` | API URLs + app constants (public; env-overridable) |
| `crypto` | passToHash, CryptoJS AES-CBC, lib AES-GCM, file-key derivation, AES-256-CTR, hashes, workspace-key decrypt (PGP/Kyber512/blake3) |
| `auth` | legacy login, SSO credential build, pure `refresh_credentials` (no fs) |
| `sso` | web SSO flow logic (build login URL, decode callback, build creds); local transport behind the `SsoCallbackServer` trait |
| `api` | Drive REST client; workspace-aware via `for_credentials` |
| `network` | bridge/network client, streaming PUT/GET |
| `transfer` | streaming transfer primitives + `ProgressSink` (see below) |
| `progress` | `ProgressSink` trait + `noop_sink` |
| `models` | serde DTOs; `Credentials` + helpers |

### `internxt-cli` (bin `internxt`)

The **front-end**: clap dispatch, the transfer UX (progress bars, `--json`),
drive-ops/workspaces wrappers, sync, and the WebDAV + FUSE + serve backends. Everything
human-facing lives here.

## Reusing the core

The core is a normal Rust crate — depend on it directly:

```toml
[dependencies]
internxt-core = { git = "https://github.com/Bebbssos/internxt-cli-rust" }
```

It deliberately leaves the UI/side-effect concerns to the caller (the **seams**):

- **byte progress** → the `ProgressSink` trait (`inc(bytes)`). Transfer primitives take
  `Option<Arc<dyn ProgressSink>>` (`None` discards). The cli wraps indicatif; a GUI would
  wrap its own widget.
- **2FA code** → `auth::login` takes an injected `on_need_2fa` closure (the cli prompts).
- **browser-open + URL** → `sso::login` takes an injected `on_login_url` closure.
- **refresh warnings** → `auth::refresh_credentials` takes an injected `on_warn` closure.
- **credential persistence** → entirely the caller's. `refresh_credentials(creds, on_warn)`
  is pure: it takes the current credentials and returns the (possibly-refreshed) ones plus a
  `changed` flag, doing no filesystem I/O. The cli owns *where and how* they are stored.
- **SSO callback transport** → the `SsoCallbackServer` trait. The cli's implementation is a
  temporary local axum HTTP server; another host could use a different mechanism.

### `fs` feature

Default on. Gates the native filesystem + runtime-bound helpers — the path-based
`upload_file_to_network`, multipart upload, and `create_folder_with_retry` (they pull in
`tokio::fs` / `spawn` / `time`). Build `--no-default-features` for just the reader/writer
surface: crypto, api, network, and the generic streaming `upload_stream_to_network<R:
AsyncRead>` / `download_file_to_writer<W: AsyncWrite>`, which take an in-memory
reader/writer instead of a path.

## Streaming / memory

Transfers never hold a whole file in RAM:

- Upload < 100MB: single presigned PUT, body streamed source → encrypt → HTTP (~1MB RAM).
- Upload ≥ 100MB: multipart, 15MB parts, up to 10 concurrent PUTs (~150MB RAM cap
  regardless of file size).
- Download: shard responses streamed → decrypt → sink. Ranged reads (`Range:` in WebDAV,
  seeks in FUSE) fetch only the covering shards.

## Configuration

API endpoints and app constants default to the public Internxt values (see
`crates/internxt-core/src/config.rs`) and can be overridden via environment variables of
the same name (`DRIVE_NEW_API_URL`, `NETWORK_URL`, etc.).

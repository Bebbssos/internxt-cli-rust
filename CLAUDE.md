# CLAUDE.md

Context for work here. Read first.

## What this is

Unofficial **Rust port of Internxt CLI** (official = Node/TypeScript). Goal: faster,
less memory, single static binary, fully streaming transfers. Not affiliated with Internxt.

Commands: login (legacy + SSO), file/folder transfers, drive management, thumbnails,
workspaces, one-way sync, multi-protocol **serve** (WebDAV/FUSE/SMB/NFS/SFTP) + `mount`
shortcut. Global `--json`. Full list: `internxt --help`.

Roadmap: [TODO.md](TODO.md). Library-reuse overview: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Porting source — `./og`

Original node sources live in `./og` (**git-ignored**; recreate with
`./scripts/fetch-og-sources.sh`). Whenever porting a command or chasing a behavioral
mismatch with the official CLI, read the matching og source and mirror it — don't
guess at endpoints, payloads, or crypto.

## Architecture

Cargo **workspace**, two crates:

- **`internxt-core`** — protocol-agnostic Drive engine (crypto, API, transfers). No
  terminal/clap/browser deps, so it works equally under the CLI, WebDAV/FUSE, or a
  future GUI.
- **`internxt-cli`** — front-end: clap dispatch, transfer UX, serve backends.

Core never touches stdio, prompts, or the filesystem for credentials — it exposes
progress, 2FA, browser-open, and refresh-warning hooks as injected closures/traits, and
the cli supplies them plus owns all credential persistence. This split is the thing to
preserve when adding features: engine logic in core, UX/IO in cli.

## Key facts / gotchas

- **Crypto is byte-for-byte compatible with node**, checked against reference test
  vectors. Touching crypto → run `cargo test` before considering it done.
- **SSO login can't carry the Kyber key** (the callback link/refresh never exposes it
  decrypted) — hybrid-Kyber workspaces need legacy (password) login instead.
- **Workspaces are a separate context**, not just a filter: their own bucket, network
  credentials, and mnemonic. Switching workspace changes where drive calls and
  transfers route.
- **Thumbnails are best-effort**, generated automatically after upload, silenceable via
  an env kill switch, and only visible through the folder-listing endpoint — the
  single-file metadata endpoint doesn't include them.
- **Transfers stay streaming end-to-end**, including ranged/partial downloads and
  chunked multipart uploads — RAM use stays bounded regardless of file size.
- **Credentials are stored encrypted**, same file location and format as the node CLI,
  so the two are interchangeable on the same machine.

## serve

One foreground process can run several protocol backends at once (WebDAV, FUSE, SMB,
NFS, SFTP), sharing credentials, a folder cache, and upload throttling. All backends
follow the same model: reads are streaming (forward reads stay cheap, backward seeks
become ranged fetches); writes are whole-file (Internxt has no partial update), so a
write materializes, then replaces the old Drive entry on completion. SMB/NFS/FUSE/SFTP
diverge on protocol-specific quirks (e.g. NFS has no close, so it flushes on idle) —
read the relevant backend module for those details rather than assuming parity.

## Build / test

```sh
cargo build --release          # default build; some backends are opt-in Cargo features
cargo test                     # crypto cross-check vs node (no network)
cargo clippy --workspace --all-targets
```

Feature flags and command flags: see `Cargo.toml` / `internxt <cmd> --help` — not
duplicated here.

## Conventions

- Match the node CLI's observable behaviour so the two stay interchangeable. When
  unsure, read `og/` and mirror it.
- Keep transfers streaming — never read a whole file into memory.
- Endpoints/constants live in `crates/internxt-core/src/config.rs`, env-overridable.

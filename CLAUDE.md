# CLAUDE.md

Context for work here. Read first.

## What this is

Unofficial **Rust port of Internxt CLI** (official = Node/TypeScript). Goal: faster,
less memory, single static binary, fully streaming transfers. Not affiliated with Internxt.

Commands: login (legacy + SSO), file/folder transfers, drive management, thumbnails,
workspaces, one-way sync, multi-protocol **serve** (WebDAV/FUSE/SMB/NFS/SFTP) + `mount`
shortcut. Global `--json`. Full list: `internxt --help`.

Roadmap: [TODO.md](TODO.md). Engine crate: [internxt-core](https://github.com/Bebbssos/internxt-core-rust).

## Porting source — `./og`

Original node sources live in `./og` (**git-ignored**; recreate with
`./scripts/fetch-og-sources.sh`). Whenever porting a command or chasing a behavioral
mismatch with the official CLI, read the matching og source and mirror it — don't
guess at endpoints, payloads, or crypto.

## Architecture

Two crates in **two repos**:

- **`internxt-core`** — protocol-agnostic Drive engine (crypto, API, transfers). No
  terminal/clap/browser deps, so it works equally under the CLI, WebDAV/FUSE, or a
  future GUI. **Lives in its own repo now**:
  [github.com/Bebbssos/internxt-core-rust](https://github.com/Bebbssos/internxt-core-rust),
  published to crates.io. This repo depends on it as a library (see `Cargo.toml`). To
  change engine logic you edit the core repo, cut a release, then bump the dep here.
- **`internxt-cli`** — this repo (single crate, binary `ixr`): clap dispatch,
  transfer UX, serve backends.

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
  transfers route. `IXR_WORKSPACE_ID` sets the workspace for one invocation only
  (mirrors og's docker `INXT_WORKSPACE_ID`, but scoped like `IXR_USER` — never
  persisted, never touches what `workspaces use` left active).
- **Thumbnails are best-effort**, generated automatically after upload, silenceable via
  an env kill switch, and only visible through the folder-listing endpoint — the
  single-file metadata endpoint doesn't include them.
- **Transfers stay streaming end-to-end**, including ranged/partial downloads and
  chunked multipart uploads — RAM use stays bounded regardless of file size.
- **Credentials are stored encrypted** at `~/.ixr/credentials` (own directory, not
  shared with the official CLI's `~/.internxt-cli`) — same crypto, different location.
  The file holds *every* logged-in account plus an active-account pointer
  (`auth::AccountsFile`), not a single account — `ixr` supports multiple
  accounts at once, unlike the official CLI. `IXR_USER` lets any command target a
  specific account for that invocation without switching the active one — it
  replaces the official CLI's `login`-only `INXT_USER`/`INXT_PASSWORD` (ixr's
  `login`/`login-legacy` flags have no env fallback; use `IXR_USER`/`IXR_PASSWORD`
  for the env-driven case, on any command).

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
- Endpoints/constants live in the `internxt-core` crate's `config` module (its own
  repo), env-overridable.

# TODO / Roadmap

Remaining work for the Rust port. For what's **already** implemented, see
[README.md](README.md).

> When an item is implemented, **remove it from this file** (don't tick it off) —
> what's done belongs in README.md / CLAUDE.md, not here.

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

## Missing commands

- `config` — show/set config
- `logs`
- `webdav-config`, `webdav start|stop|status`, `add-cert` — the daemon-style WebDAV
  management commands. The **server itself is ported** as `serve webdav` (foreground
  command, see README); these subcommands are intentionally deferred pending a decision
  on whether to add a background/daemon mode. Command names are left free for them.

## Feature gaps in already-ported commands

### Auth
- SSO login drops the kyber private key (universal link never carries it; the
  refresh endpoint returns it still-encrypted, no password to decrypt). ecc-only
  workspaces work; hybrid-Kyber workspaces need `login-legacy`. Matches og.
- Login with generated PGP/Kyber keys (`/auth/cli/login/access`). We use
  `/auth/login/access` WITHOUT keys; fine for existing accounts (the server
  returns the stored keys), but registration / key-update paths are unsupported.

### Upload
- ~~Thumbnail generation + upload (`ThumbnailService`).~~ **Done** — core `thumbnail`
  module (feature `thumbnails`, default-on) decodes/resizes/encodes a 300x300 PNG via
  the `image` crate and `POST /files/thumbnail`; wired into `upload-file` + `upload-folder`
  (best-effort, never fails the upload). Only image sources (jpg/png/webp/gif/tiff), as in og.
  PDF thumbnails and serve-backend PUT thumbnails still TODO.
- Retry with backoff on transient failures (`uploadFileWithRetry`, MAX_RETRIES/DELAYS_MS).
- Upload size limit check (node enforces a per-file limit; see CLI README "40GB").
- HMAC on upload (sdk now stores hmac on upload — see sdk commit; node inxt-js
  passes `hmac: undefined`, so we skip it. Revisit if server starts requiring it.)

### Download
- Range / resume support (`RangeOptions`, partial download with CTR offset IV math).
- Shared-link / token download (`getDownloadLinks(..., token)`).
- Multi-shard parallel download (we download shards sequentially).

### WebDAV
- `COPY` and `PROPPATCH` return `501` (og also stubs COPY; PROPPATCH unimplemented).
- `--timeout` is accepted but not wired to a request-timeout layer (og sets
  `server.requestTimeout`). Add a tower `TimeoutLayer` if needed.
- No local drive cache / path→uuid database (og uses sqlite); every resolution walks
  the folder tree. Fine for typical trees; could be slow for very large/deep folders.
- No thumbnail upload on PUT (shares the general thumbnail gap above).
- Range GET decrypts from the start and discards the prefix (CTR is continuous); a
  large offset still downloads+decrypts the skipped bytes. Proper CTR-offset seeking
  would avoid that (same math as the Download range item).
- HTTPS lives behind the `webdav-tls` feature (rustls + rcgen self-signed / custom cert).

### FUSE mount (`mount`, beyond og — no official equivalent)
- Random-access reads re-download from the start of the file and skip to the offset
  (per-handle stream only avoids this for *sequential* reads / small forward gaps). Same
  CTR-offset seeking fix as the WebDAV/Download range items would make random reads cheap.
- Writes buffer the whole file to a temp file and upload on `release` (Internxt has no
  partial update). `fsync` does **not** upload — a crash before `release` loses buffered
  writes. Could upload-on-fsync for durability at a re-upload cost.
- `setattr(size)` truncation only takes effect against an **open** write handle; a bare
  `truncate(2)` on a closed file updates local metadata only (no Drive rewrite).
- Read-modify-write of a large existing file downloads the whole file first (materialize),
  then re-uploads it whole. Fine for typical edits; heavy for huge files.
- No `--timeout`, no thumbnail upload (shares the general gaps). macOS/FreeBSD build the
  same way but are untested here (developed/verified on Linux + libfuse3).

### SMB/CIFS (`serve smb`, beyond og — no official equivalent, experimental, feature default-off)
- Shares the whole-file write + streaming/ranged read model with FUSE, so the same caveats
  apply (random reads not yet CTR-offset-cheap; RMW of a large file materializes then re-uploads;
  no partial update).
- Built on `smb-server`, pulled as a **git dependency** on a fork
  (`github.com/Bebbssos/rust-smb-server`) with two fixes: upstream 0.4.1 doesn't re-export the
  `ShareBackend`/`Handle` trait types (can't be implemented downstream unpatched), and QUERY_INFO
  returned a per-open volatile id (stale-handle on the Linux cifs client). Switch back to a
  crates.io release once the fork's PR lands upstream.
- Auth is a single username/password (or anonymous). Multi-user shares / per-user ACLs are
  possible (the crate supports them) but not wired to CLI flags.
- Default port 4445 (445 needs root/admin). No SMB-over-QUIC, no DFS, no change-notify beyond
  what the crate provides. Verified on Linux via `smbclient` (list / get / put / rename / del /
  mkdir / rmdir, 41MB multi-shard read + 512KB write round-trip md5-identical).

### Infrastructure / parity
- `.env` loading (node uses dotenv). We hardcode public defaults in `src/config.rs`,
  overridable via env vars. Decide if a `.env` file should be supported.
- SDK-style request retry layer (node `SdkManager` maxRetries: 3).
- Local drive cache (node uses better-sqlite3 / typeorm `internxt-cli-drive.db`).

## Beyond-og feature ideas (not in the official CLI)

- sync-down `--delete=trash` → OS trash (needs a cross-platform trash lib; currently errors).

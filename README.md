# internxt-cli-rust

An unofficial Rust port of the [Internxt CLI](https://github.com/internxt/cli), focused on speed and low memory use.

> Not affiliated with or endorsed by Internxt.

> This port was written mostly by [Claude Code](https://claude.com/claude-code), porting the behaviour of the official Node/TypeScript CLI.

## Status

Implemented: authentication (legacy + web-based SSO), streaming file transfers, recursive folder upload, one-way folder sync (`sync-up` / `sync-down`), workspaces (list/use/unset with workspace-scoped transfers), a foreground multi-protocol **`serve`** (a **WebDAV server** and, on Unix, a **FUSE mount** — run either or both at once), a top-level **`mount`** convenience command (FUSE), automatic token refresh near expiry, and the full set of drive-management commands (list, create/move/rename, trash + restore, permanent delete). Every command supports `--json` for scripting.

Built as a Cargo workspace: a reusable **`internxt-core`** engine crate (protocol-agnostic — auth, crypto, Drive REST, streaming transfers) and the **`internxt-cli`** front-end. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

Crypto is byte-for-byte compatible with the node CLI (verified by cross-check tests, see `cargo test`).

Not yet ported: thumbnails and the `config` command.

## Compatibility with `@internxt/cli`

This is intended to be a **mostly drop-in replacement** for the official [`@internxt/cli`](https://www.npmjs.com/package/@internxt/cli): for the implemented commands the names, aliases, flags, endpoints, payloads, credential file location/format (`~/.internxt-cli/.inxtcli`) and crypto all match, so the two are interchangeable for everyday `login` / `upload` / `download` / list / move / rename / trash workflows.

It is *not* a 100% replacement — anything in "Not yet ported" above is simply absent. The **WebDAV server** works differently by design (see the [serve](#serve-webdav--fuse) section): it runs in the foreground with options passed inline, rather than as a pm2-managed background service configured via `webdav-config`. Beyond that, the known **behavioural differences (breaking changes)** are:

- **`login` defaults to SSO.** Built with the default `sso` feature, `login` runs the web-browser callback flow (like the official CLI); `login-legacy` runs email + password + optional 2FA, and `login-sso` forces SSO. Build `--no-default-features` for a smaller binary where `login` falls back to legacy and `login-sso` errors. The SSO flow drops the kyber private key (hybrid-Kyber workspaces need `login-legacy`).
- **`--json` output schema differs.** We emit a simplified `{ "success": true, ... }` object per command rather than the exact oclif JSON envelope. Field names mostly match, but don't assume byte-identical structure.
- **No interactive prompting for missing flags.** Required arguments that are absent produce a clap usage error instead of an interactive prompt. The exceptions that still prompt: `login-legacy` (email/password/2FA) and `trash-clear` (confirmation, unless `--force`).
- **No thumbnail generation** on upload.
- **Plain-text table output** uses simple aligned columns rather than the official CLI's boxed tables. Use `--json` for stable machine-readable output.

## Build

```sh
cargo build --release
# binary at target/release/internxt (SSO + WebDAV over HTTP + FUSE enabled by default)

# add HTTPS support to the WebDAV server (pulls in a rustls TLS stack):
cargo build --release --features webdav-tls

# add the SMB / NFS / SFTP backends (all off by default):
cargo build --release --features smb          # SMB2/3 share
cargo build --release --features nfs          # NFSv3 export
cargo build --release --features sftp         # SFTP over SSH (pulls in russh)

# smaller binary without SSO login, WebDAV or FUSE (drops axum + open + fuser):
cargo build --release --no-default-features
```

Feature flags: `sso` (web-based login), `webdav` (WebDAV server, HTTP) and `fuse` (FUSE mount, Unix) are on by default; `webdav-tls` adds HTTPS. Disable any of them for a smaller binary. The `fuse` feature needs `libfuse3-dev` + `pkg-config` at build time on Unix (it is inert on Windows, so default builds still compile there).

## Commands

All commands accept the global `--json` flag, which prints a single JSON result object and suppresses progress output. IDs are Drive UUIDs. Where a destination/parent folder id is optional, leaving it empty targets your root folder.

| Command | Aliases | Arguments | Description |
|---|---|---|---|
| `login` | | `--host <HOST>`, `--port <PORT>` (SSO); or legacy flags when built without `sso` | Log in. SSO web-browser flow by default (`--host`/`--port` for cross-device); falls back to legacy when built `--no-default-features`. |
| `login-sso` | `login:sso` | `--host <HOST>`, `--port <PORT>` | Web-based SSO login (opens a browser, local callback server). Errors if built without the `sso` feature. |
| `login-legacy` | `login:legacy` | `-e, --email <EMAIL>`, `-p, --password <PASSWORD>`, `-w, --twofactor <CODE>` | Log in with email + password. Prompts for any missing value; 2FA prompted if the account requires it. |
| `logout` | | | Invalidate the session server-side and clear local credentials. |
| `whoami` | | | Show the currently logged-in user. |
| `usage` | `account`, `account-info` | | Show account usage: plan, used space (drive/backups), space limit and per-file upload limit. |
| `workspaces-list` | `workspaces:list` | `-e, --extended` | List the workspaces you belong to. |
| `workspaces-use` | `workspaces:use` | `-i, --id <WORKSPACE_ID>`, `-p, --personal` | Set the active workspace for subsequent commands (`--personal` switches back to your personal drive). |
| `workspaces-unset` | `workspaces:unset` | | Unset the active workspace (operate within your personal drive space). |
| `upload-file` | `upload:file` | `-f, --file <PATH>`, `-i, --destination <FOLDER_ID>`, `--no-upload-limit`, `--max-upload-size <SIZE>` | Upload a single file (streaming; single-part or multipart). |
| `upload-folder` | `upload:folder` | `-f, --folder <PATH>`, `-i, --destination <FOLDER_ID>`, `--no-upload-limit`, `--max-upload-size <SIZE>` | Recursively upload a folder tree (concurrent file uploads). |
| `download-file` | `download:file` | `-i, --id <FILE_ID>`, `-d, --directory <DIR>`, `-o, --overwrite` | Download + decrypt a file by id, streaming to disk. |
| `list` | | `-i, --id <FOLDER_ID>`, `-e, --extended` | List a folder's contents. `--extended` adds created date, modified date + size. |
| `create-folder` | `create:folder` | `-n, --name <NAME>`, `-i, --id <PARENT_ID>` | Create a folder. |
| `move-file` | `move:file` | `-i, --id <FILE_ID>`, `-d, --destination <FOLDER_ID>` | Move a file into a destination folder. |
| `move-folder` | `move:folder` | `-i, --id <FOLDER_ID>`, `-d, --destination <FOLDER_ID>` | Move a folder into a destination folder. |
| `rename-file` | `rename:file` | `-i, --id <FILE_ID>`, `-n, --name <NAME>` | Rename a file (name + extension split automatically). |
| `rename-folder` | `rename:folder` | `-i, --id <FOLDER_ID>`, `-n, --name <NAME>` | Rename a folder. |
| `trash-file` | `trash:file` | `-i, --id <FILE_ID>` | Move a file to the trash. |
| `trash-folder` | `trash:folder` | `-i, --id <FOLDER_ID>` | Move a folder to the trash. |
| `trash-list` | `trash:list` | `-e, --extended` | List the contents of the trash. |
| `trash-restore-file` | `trash:restore:file` | `-i, --id <FILE_ID>`, `-d, --destination <FOLDER_ID>` | Restore a trashed file into a destination folder. |
| `trash-restore-folder` | `trash:restore:folder` | `-i, --id <FOLDER_ID>`, `-d, --destination <FOLDER_ID>` | Restore a trashed folder into a destination folder. |
| `trash-clear` | `trash:clear` | `-f, --force` | Empty the trash permanently. Prompts unless `--force` (required in `--json` mode). |
| `delete-permanently-file` | `delete:permanently:file` | `-i, --id <FILE_ID>` | Permanently delete a file. Cannot be undone. |
| `delete-permanently-folder` | `delete:permanently:folder` | `-i, --id <FOLDER_ID>` | Permanently delete a folder. Cannot be undone. |
| `sync-up` | `sync:up` | `-l, --local <DIR>`, `-r, --remote <FOLDER_ID>`, `--delete[=trash\|permanent]`, `--dry-run`, `--no-upload-limit`, `--max-upload-size <SIZE>` | Make a remote folder match a local one (push): upload new/changed files, optionally trash/delete remote extras. |
| `sync-down` | `sync:down` | `-l, --local <DIR>`, `-r, --remote <FOLDER_ID>`, `--delete`, `--dry-run` | Make a local folder match a remote one (pull): download new/changed files, optionally remove local extras. |
| `serve <PROTOCOLS>` | | comma-list of `webdav` / `fuse` / `smb` / `nfs` / `sftp`; shared: `-i, --folder-uuid`, `--cache-ttl`/`--no-cache`, `-d, --delete-permanently`, `--spool`, `--spool-dir`, `--max-concurrent-uploads`, `--read-only`, `--no-upload-limit`, `--max-upload-size <SIZE>`; `--webdav-*`, `--fuse-*`, `--smb-*`, `--nfs-*`, `--sftp-*` prefixed per protocol | Run one or more Drive backends in the foreground until Ctrl-C, sharing creds/cache/upload-limit. `serve webdav`, `serve fuse`, `serve webdav,fuse`, `serve smb`, `serve nfs`, `serve sftp`. |
| `mount <MOUNTPOINT>` | | `-i, --folder-uuid`, `--read-only`, `-d, --delete-permanently`, `--spool-dir`, `--max-concurrent-uploads`, `--cache-ttl`/`--no-cache`, `--allow-other`, `--no-upload-limit`, `--max-upload-size <SIZE>` | Expose your Drive as a local read-write filesystem via FUSE (Unix only; Ctrl-C to unmount). Thin wrapper over `serve fuse`. |

### Account usage (`usage`)

`usage` (aliases `account`, `account-info`) prints your plan, used space (split
drive / backups), total space limit and the per-file upload limit:

```
Plan:               Free
Used:               3.89 TB / 10 TB (38.9%)
  Drive:            3.89 TB
  Backups:          0 B
Space limit:        10 TB
Upload file limit:  10 GB
```

Not an official-CLI command — it fans out the same drive-gateway endpoints the node
CLI uses internally (`/users/usage`, `/users/limit`, `/files/limits`) plus a
best-effort plan lookup on the payments API. The plan name reads `Tier (Type)` (e.g.
`Pro (Subscription)`), collapsing to one value when they agree. Legacy lifetime
accounts show `Free (Lifetime)` — the tier endpoint mislabels old plans as `free`,
but the `(Lifetime)` still signals it's a paid plan; the space limit is always
correct. If the payments API is unreachable the plan shows `unknown`. `--json` adds
raw `planLabel` / `subscriptionType` fields.

### Upload size limit

Uploads are validated against a per-file size cap before transferring, mirroring the
node CLI — except there is **no** hard-coded default: when your plan sets no cap,
uploads are unbounded. The cap is resolved in this order (first match wins):

1. `--no-upload-limit` — disable the check entirely.
2. `--max-upload-size <SIZE>` — a custom cap (`5GB`, `500M`, `1073741824`, … binary units).
3. `INTERNXT_MAX_UPLOAD_SIZE` env var — universal override for every upload command.
   A size string sets a cap; `off` / `none` / `unlimited` / `0` disables it.
4. Otherwise, your plan's `maxUploadFileSize` (from `/files/limits`; unlimited if unset).

These flags apply to `upload-file`, `upload-folder`, `sync-up`, and the `serve` /
`mount` backends. Over-limit files are rejected up front (folder/sync uploads skip the
offending file and continue; WebDAV `PUT` returns `413`; FUSE writes fail `EFBIG`).

### Sync

`sync-up` and `sync-down` do a single **one-way** reconcile pass then exit (not a daemon). The source side always wins — there is no bidirectional mode and no conflict resolution. Files are keyed by relative path; change detection compares size, then `modificationTime` (±2s tolerance). `--dry-run` prints the plan without transferring; `--json` emits a summary object with counts + per-action list. Downloaded files are stamped with the remote modification time so repeat runs are idempotent. `--delete` is opt-in and off by default; it prunes both extra files **and** extra folders (deleting the top-most extra folder cascades its whole subtree).

### serve (WebDAV + FUSE + SMB + NFS + SFTP)

`serve` runs one or more Drive backends in the **foreground** until Ctrl-C. Pass a comma-separated protocol list; `webdav`, `fuse` (Unix), `smb`, `nfs` and `sftp` are supported. Running several at once shares one set of credentials, one folder-listing cache and one global upload limit. Shared flags are bare (`--cache-ttl`, `--read-only`, …); protocol-specific flags are prefixed (`--webdav-*`, `--fuse-*`, `--smb-*`, `--nfs-*`, `--sftp-*`).

Unlike the official CLI — which runs WebDAV as a pm2-managed background service configured through a separate `webdav-config` command — this port runs it inline as a normal foreground command. `smb`, `nfs` and `sftp` are **experimental**, off by default, and must be built in (`--features smb` / `nfs` / `sftp`).

```sh
internxt serve webdav                                    # http://127.0.0.1:3005
internxt serve webdav --webdav-host 0.0.0.0 --webdav-port 8080   # accept LAN clients
internxt serve webdav --webdav-create-full-path          # auto-create missing parent folders on upload
internxt serve webdav --webdav-custom-auth --webdav-username alice --webdav-password secret
internxt serve webdav --webdav-https                     # HTTPS, self-signed cert (needs `webdav-tls`)
internxt serve webdav --webdav-https --webdav-cert cert.pem --webdav-key key.pem
internxt serve fuse --fuse-mountpoint ~/drive            # FUSE mount (Unix)
internxt serve smb --smb-password secret                 # SMB share (needs `--features smb`)
internxt serve nfs --nfs-host 0.0.0.0                    # NFSv3 export (needs `--features nfs`)
internxt serve sftp --sftp-password secret               # SFTP share (needs `--features sftp`)
internxt serve webdav,fuse --fuse-mountpoint ~/drive     # both at once, shared cache/creds
internxt serve webdav --read-only -i <folder-uuid>       # read-only, rooted at a subfolder
internxt serve webdav --spool --spool-dir /var/tmp/inxt  # spool each upload to disk first (see below)
internxt serve webdav --max-concurrent-uploads 2         # cap simultaneous upload transfers
internxt serve webdav --cache-ttl 15                     # cache folder listings 15s (0 / --no-cache to disable)
```

WebDAV supported methods: `OPTIONS`, `PROPFIND`, `GET`/`HEAD` (with `Range`), `PUT`, `MKCOL`, `DELETE`, `MOVE`, `LOCK`/`UNLOCK`. `COPY` and `PROPPATCH` return `501 Not Implemented` (as upstream). Transfers stream through the same encrypt/decrypt path as `upload-file`/`download-file`, so large files never load into RAM. `DELETE` trashes items by default (`--delete-permanently` to hard-delete). Paths are resolved by walking the folder tree, so it stays workspace-aware when a workspace is active.

#### Shared flags (any protocol)

| Flag | Default | Purpose |
|---|---|---|
| `-i, --folder-uuid <UUID>` | account/workspace root | Expose a subfolder as the root of every backend. |
| `-d, --delete-permanently` | off | Hard-delete instead of moving to trash. |
| `--read-only` | off | Reject all writes/mutations on every backend. |
| `-v, --verbose` | off | Log every per-operation request (`[OPEN]`, `[READ]`, …) across all backends. Without it, only errors/warnings (and upload/download failures) are printed. Also enables the WebDAV wire-level header dump (same as `INTERNXT_WEBDAV_DEBUG=1`). |
| `--spool` | off | Spool each upload body to a temp file, then upload from disk, instead of streaming the live client body straight to storage. Costs temp disk + a little latency; more robust for slow/bursty clients. (FUSE always spools; no-op there.) |
| `--spool-dir <DIR>` | system temp | Directory for spool temp files (created if missing). |
| `--max-concurrent-uploads <N>` | `0` (unlimited) | Cap how many upload transfers run at once, across all backends. `1` fully serializes. |
| `--no-upload-limit` / `--max-upload-size <SIZE>` | plan limit | Disable or override the per-file upload size cap (see [Upload size limit](#upload-size-limit)). WebDAV `PUT` over the cap returns `413`; FUSE writes past it fail with `EFBIG`. |
| `--cache-ttl <SECS>` | `5` | Cache folder listings this many seconds (also the FUSE kernel attr/entry TTL). `0` disables. Only folder listings are cached; file listings stay live, and folder changes invalidate. |
| `--no-cache` | off | Disable the folder-listing cache (same as `--cache-ttl 0`). |

#### WebDAV flags (`--webdav-*`)

| Flag | Default | Purpose |
|---|---|---|
| `--webdav-host <HOST>` | `127.0.0.1` | Address to bind (and advertise). `0.0.0.0` accepts LAN clients. |
| `--webdav-port <PORT>` | `3005` | Listen port. |
| `--webdav-https` | off | Serve over HTTPS (needs the `webdav-tls` feature). Self-signed unless a cert/key is given. |
| `--webdav-cert / --webdav-key <PEM>` | — | Your own TLS certificate + private key (both required together). |
| `--webdav-create-full-path` | off | Auto-create missing parent folders on `PUT` / `MKCOL`. |
| `--webdav-custom-auth` + `--webdav-username/--webdav-password` | off | Require HTTP Basic auth from clients. |
| `--webdav-timeout <MINS>` | `60` | Accepted for parity; not yet wired to a request-timeout layer. |

#### FUSE flags (`--fuse-*`)

| Flag | Default | Purpose |
|---|---|---|
| `--fuse-mountpoint <DIR>` | — | Local directory to mount onto (required when `fuse` is served). |
| `--fuse-allow-other` | off | Let other users (and root) access the mount (needs `user_allow_other` in `/etc/fuse.conf` on Linux). |

#### SMB flags (`--smb-*`)

**Experimental** and off by default — build with `--features smb`. Serves a Drive as a read-write SMB2/3 share on all platforms. Mount it from Windows (`net use`), Linux (`mount -t cifs`) or macOS.

| Flag | Default | Purpose |
|---|---|---|
| `--smb-host <HOST>` | `127.0.0.1` | Address to bind (and advertise). `0.0.0.0` accepts LAN clients. |
| `--smb-port <PORT>` | `4445` | Listen port. **The well-known SMB port 445 needs root/admin, so the default is an unprivileged port instead.** Bind `445` only if you can (e.g. via `setcap`/elevation) — Windows clients only speak `445`, so a non-default port needs a Linux/macOS client that can pass `-o port=`. |
| `--smb-share <NAME>` | `internxt` | Exported share name (`\\host\<name>`). |
| `--smb-username <USER>` | `internxt` | Username required from clients (with `--smb-password`). |
| `--smb-password <PASS>` | — | Password required from clients. Omit for an anonymous (guest) share — most clients, Windows especially, refuse anonymous, so a password is recommended. |

#### NFS flags (`--nfs-*`)

**Experimental** and off by default — build with `--features nfs`. Serves a Drive as a read-write NFSv3 export on all platforms. Mount it from Linux/macOS (`mount -t nfs`).

| Flag | Default | Purpose |
|---|---|---|
| `--nfs-host <HOST>` | `127.0.0.1` | Address to bind (and advertise). `0.0.0.0` accepts LAN clients. |
| `--nfs-port <PORT>` | `12049` | Listen port. **The well-known NFS port 2049 needs root/admin, so the default is an unprivileged port instead.** Mount with the port + mountport override, e.g. `sudo mount -t nfs -o nolock,vers=3,tcp,port=12049,mountport=12049 <host>:/ /mnt`. |

NFSv3 has no open/close for data, so a written file can't finalize on a `close` the way the other backends do. Each written file is buffered to a temp file and uploaded once writes have gone idle for ~2s (and the buffer is evicted after ~30s of quiet); a final flush runs on shutdown. Because of this, a freshly written file may take a moment to appear finalized on Drive. The Drive entry is created lazily **on that first flush, with content** — a file that is created but never written (e.g. `touch`) never persists. This is deliberate: free/legacy plans reject 0-byte files (`HTTP 402 "You can not have empty files"`), so NFS never POSTs an empty file.

#### SFTP flags (`--sftp-*`)

**Experimental** and off by default — build with `--features sftp` (pulls in `russh` for the SSH transport). Serves a Drive as a read-write SFTP share. Connect with `sftp`, `scp -s`, WinSCP, FileZilla, or an `sshfs` mount.

| Flag | Default | Purpose |
|---|---|---|
| `--sftp-host <HOST>` | `127.0.0.1` | Address to bind (and advertise). `0.0.0.0` accepts LAN clients. |
| `--sftp-port <PORT>` | `2022` | Listen port. **The well-known SSH port 22 needs root/admin, so the default is an unprivileged port instead** (connect with `sftp -P <port> <user>@<host>`). |
| `--sftp-username <USER>` | `internxt` | Username required from clients. |
| `--sftp-password <PASS>` | — | Password required from clients. Omit to accept any password (the username is still required). A password is recommended. Public-key auth is rejected so clients fall back to password. |
| `--sftp-host-key <PATH>` | `~/.internxt-cli/sftp_host_key` | SSH host private key (OpenSSH). Omit and a persistent key is generated once in the CLI data dir (mode `0600`) and reused on every later start, so the host fingerprint stays stable. Point this at your own key to override. |

#### Debug logging (WebDAV)

Set `INTERNXT_WEBDAV_DEBUG=1` (or pass `--verbose`) to dump each request line, all request/response headers, and the response status to stderr — useful for diagnosing client-specific behaviour (e.g. WinSCP). Note `--verbose` also turns on the bare `[METHOD] path` per-request trace for every backend; without it, all backends print only errors/warnings:

```sh
INTERNXT_WEBDAV_DEBUG=1 internxt serve webdav --webdav-host 0.0.0.0 --webdav-port 8099 2>&1 | tee webdav-debug.log
```

Notes / current limitations: HTTP by default (enable HTTPS with the `webdav-tls` feature); no local **on-disk** database cache (og uses sqlite) — folder listings are cached in-process for `--cache-ttl` seconds only; `--webdav-timeout` is accepted for parity but not yet wired. A background task refreshes the session token hourly, so a long-running server survives token expiry. The `webdav-config` / `webdav start|stop|status` names are left for a possible future daemon mode.

### mount (FUSE)

On Unix, `mount <MOUNTPOINT>` exposes your Drive as a local **read-write** filesystem — a thin wrapper over `serve fuse` where the shared flags use their bare names.

```sh
mkdir -p ~/drive && internxt mount ~/drive              # Ctrl-C to unmount
internxt mount ~/drive --read-only                      # browse/read only
internxt mount ~/drive -i <folder-uuid>                 # mount a subfolder as root
internxt mount ~/drive --allow-other                    # needs user_allow_other in /etc/fuse.conf
```

Needs `libfuse3-dev` + `pkg-config` at build time and a FUSE driver at runtime (fuse3 on Linux, macFUSE on macOS, `fusefs-libs3` on FreeBSD). Reads stream and decrypt lazily; writes buffer to a temp file and upload in full when the file is closed (Internxt has no partial-update API), replacing the old Drive entry. The `fuse` feature is on by default but inert on non-Unix targets, so Windows default builds still compile.

## Usage examples

```sh
internxt login                                  # SSO: opens a browser to authenticate
internxt login-legacy --email you@example.com   # email/password; prompts for password (+ 2FA if enabled)
internxt usage                                  # plan, used/total space, upload limit
internxt upload-file -f ./file.bin -i <folder-uuid>
internxt upload-file -f ./big.iso --max-upload-size 20GB       # override the per-file cap
internxt upload-file -f ./big.iso --no-upload-limit            # disable the cap
internxt upload-folder -f ./my-folder           # recursive; -i for a destination folder
internxt download-file -i <file-uuid> -d ./out --overwrite
internxt list -e                                # root folder, extended view
internxt list -i <folder-uuid> --json           # machine-readable output
internxt create-folder -n "Reports" -i <parent-uuid>
internxt trash-file -i <file-uuid>
internxt trash-clear --force
internxt sync-up   -l ./my-folder -r <folder-uuid> --dry-run   # preview a push
internxt sync-up   -l ./my-folder -r <folder-uuid> --delete    # push, trashing remote extras
internxt sync-down -l ./my-folder -r <folder-uuid>             # pull new/changed files
internxt serve webdav --webdav-host 0.0.0.0 --webdav-port 8080 # serve Drive over WebDAV (Ctrl-C to stop)
internxt mount ~/drive                                         # mount Drive as a local filesystem (Unix; Ctrl-C to unmount)
```

Credentials are stored AES-encrypted at `~/.internxt-cli/.inxtcli` (same location/format as the node CLI).

## Architecture

The project is a Cargo workspace: a reusable **`internxt-core`** engine crate
(protocol-agnostic — auth, crypto, Drive REST, streaming transfers) and the
**`internxt-cli`** front-end that builds the `internxt` binary. Transfers are fully
streaming — large files never load into RAM. See **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)**
for the crate split, how to reuse the core as a library, the `fs` feature, and the
streaming/memory model.

## Configuration

API endpoints and app constants default to the public Internxt values (see
`crates/internxt-core/src/config.rs`) and can be overridden via environment variables of the
same name (`DRIVE_NEW_API_URL`, `NETWORK_URL`, `PAYMENTS_API_URL`, etc.).

`INTERNXT_MAX_UPLOAD_SIZE` sets a universal per-file upload cap across all upload
commands (a size like `5GB`, or `off` / `none` / `unlimited` / `0` to disable) — see
[Upload size limit](#upload-size-limit).

## Credits

Protocol and crypto behaviour reverse-engineered from the official Internxt packages
(`@internxt/cli`, `@internxt/sdk`, `@internxt/lib` — MIT; `@internxt/inxt-js`). See `LICENSE`.

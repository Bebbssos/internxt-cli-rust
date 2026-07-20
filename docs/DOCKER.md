# Docker

A multi-arch image (`linux/amd64`, `linux/386`, `linux/arm64`, `linux/arm/v7`,
`linux/arm/v6`) built from the [`Dockerfile`](../Dockerfile) at the repo root —
see that file for how it's put together (cross-compiled with `cargo zigbuild`,
Alpine runtime).

```
ghcr.io/bebbssos/ixr:latest      # latest stable release
ghcr.io/bebbssos/ixr:<version>   # e.g. ghcr.io/bebbssos/ixr:0.1.0
```

Unlike the official Node CLI's image (a WebDAV-only entrypoint script), this
one just ships the `ixr` binary — the entrypoint is `ixr` itself, so any
subcommand works: `docker run ghcr.io/bebbssos/ixr <command>`.

**Feature set**: built with `webdav,webdav-tls,smb,nfs,sftp` — everything
except `sso` (no browser in a headless container; use `IXR_USER`/
`IXR_PASSWORD` below instead), `fuse` (a container-local FUSE mount can't be
seen from the host, and there's no musl cross story for `libfuse` — see
[docs/CLI.md § mount](CLI.md#mount) if you want a real mount, run `ixr mount`
on the host instead), `dotenv` and `self-update` (the image itself is the
update/config unit — `docker pull` and `-e`/`--env-file` cover both).

## Authentication

`serve`'s flags have no env fallback of their own, but credential resolution
is shared with every other command (see
[docs/CLI.md § Multiple accounts](CLI.md#multiple-accounts)): set `IXR_USER` +
`IXR_PASSWORD` and the container logs in on first start and reuses the stored
session afterwards, as long as `~/.ixr` (i.e. `/root/.ixr` in the container) is
a persistent volume — without one, every restart re-authenticates from
scratch.

| Variable | Required | Purpose |
|---|---|---|
| `IXR_USER` | yes | Account email to act as. |
| `IXR_PASSWORD` | yes (first start only) | Auto-login password. Only needed until a session is stored in the credentials volume. |
| `IXR_TWOFACTORCODE` | if 2FA enabled | A live 6-digit TOTP code — only useful when you start the container within that code's ~30s window. |
| `IXR_OTPTOKEN` | if 2FA enabled (preferred) | The TOTP *secret* instead of a code — `ixr` derives a fresh code itself, so it works on every (re)start, not just a 30s window. |
| `IXR_WORKSPACE_ID` | no | Route this container at a workspace instead of the personal Drive. Never persisted. |
| `IXR_NO_PERSIST` | no | Set to any value to skip writing the session to disk at all — every start re-authenticates from scratch. Mainly for one-shot/CI containers that shouldn't leave a session behind. |

Don't confuse these with `ixr`'s own `--features dotenv` `.env` loading — that
reads a `.env` file from the container's current directory (`/root`), which
this setup doesn't use. The `${...}` substitution in the compose file below is
Compose's own `.env` handling, resolved before the container ever starts.

## docker-compose: serving WebDAV, SMB and SFTP at once

One container, three protocols, sharing the credentials volume, folder cache
and upload throttle (see [docs/CLI.md § serve](CLI.md#serve)).

`.env` (next to `docker-compose.yml`, keep out of version control):

```sh
IXR_USER=me@example.com
IXR_PASSWORD=correct-horse-battery-staple
# Only one of these, if the account has 2FA:
# IXR_TWOFACTORCODE=123456
IXR_OTPTOKEN=JBSWY3DPEHPK3PXP

SMB_PASSWORD=change-me-smb
SFTP_PASSWORD=change-me-sftp
```

`docker-compose.yml`:

```yaml
services:
  ixr:
    image: ghcr.io/bebbssos/ixr:latest
    container_name: ixr
    restart: unless-stopped
    environment:
      IXR_USER: ${IXR_USER}
      IXR_PASSWORD: ${IXR_PASSWORD}
      # IXR_TWOFACTORCODE: ${IXR_TWOFACTORCODE}
      IXR_OTPTOKEN: ${IXR_OTPTOKEN}
      # IXR_WORKSPACE_ID: ${IXR_WORKSPACE_ID}
      # IXR_NO_PERSIST: "1"
    volumes:
      # Credentials + generated SFTP host key live here; without this
      # volume every restart re-authenticates and SFTP clients see a new
      # host key each time.
      - ixr-data:/root/.ixr
    ports:
      - "3005:3005"   # WebDAV  - mount with any WebDAV client at http://host:3005
      - "4445:4445"   # SMB     - \\host\internxt (or smb://host:4445/internxt)
      - "2022:2022"   # SFTP    - sftp -P 2022 internxt@host
      # NFS is bundled below (commented out) - see the note underneath.
      # - "12049:12049"
    command:
      - serve
      - webdav,smb,sftp
      - --webdav-host=0.0.0.0
      - --smb-host=0.0.0.0
      - --smb-password=${SMB_PASSWORD}
      - --sftp-host=0.0.0.0
      - --sftp-password=${SFTP_PASSWORD}
      - --verbose

volumes:
  ixr-data:
```

```sh
docker compose up -d
docker compose logs -f     # --verbose above logs every request across all three backends
```

**NFS**: add `nfs` to the protocol list and `--nfs-host=0.0.0.0` to `command`,
and expose its port — but NFSv3 clients (`mount -t nfs`) generally need the
same unprivileged port passed explicitly (`-o port=12049,mountport=12049`, see
[docs/CLI.md § serve](CLI.md#serve)) and some NFS clients are picky about
mounting from outside the container's network namespace at all. WebDAV/SMB/
SFTP are the better fit for a container; reach for `ixr mount` (FUSE, host
install) if you want a real local mount instead.

## One-shot: run a single command against a persistent session

Not every use of the image is a long-running `serve` — e.g. a cron job that
uploads a nightly backup and exits:

```sh
docker run --rm \
  -e IXR_USER=me@example.com \
  -e IXR_PASSWORD=correct-horse-battery-staple \
  -v ixr-data:/root/.ixr \
  -v "$PWD/backup":/data:ro \
  ghcr.io/bebbssos/ixr:latest \
  upload-folder -f /data --dest-path /Backups
```

Reusing the same `ixr-data` volume as the `serve` container above means this
only needs `IXR_PASSWORD` the very first time either one runs — after that,
both share the stored session. Add `IXR_NO_PERSIST=1` instead if you'd rather
this one-shot container never touch that volume's session file at all (every
run re-authenticates independently).

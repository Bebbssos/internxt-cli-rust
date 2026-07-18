# internxt-rust

An unofficial Rust rewrite of [Internxt Drive](https://internxt.com)'s tooling, built
as a Cargo workspace.

> Not affiliated with or endorsed by Internxt.

> Written mostly by [Claude Code](https://claude.com/claude-code), porting the
> behaviour of Internxt's official Node/TypeScript packages.

## Status

Early development. Most implemented functionality works, but expect frequent
breaking changes until things stabilize.

## What's in here

### `internxt-cli` (binary: `ixr`)

Internxt CLI ported fully to Rust — works on any account type, including Free,
unlike the official CLI which gates most functionality to the Ultimate plan.
Streaming transfers end to end (no whole-file buffering), and a `serve`
command that can expose your Drive over WebDAV, FUSE, SMB, NFS and SFTP —
several at once if you like — beyond just the official CLI's WebDAV-only
server.

Docs: [docs/CLI.md](docs/CLI.md)

### `internxt-core`

Rust library for interacting with Internxt Drive (auth, crypto, transfers).
Still very work in progress.

Docs: [docs/CORE.md](docs/CORE.md)

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for how the two crates fit together.

## License

[AGPL-3.0-or-later](LICENSE)

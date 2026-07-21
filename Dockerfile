# syntax=docker/dockerfile:1.7

# Multi-arch build for the `ixr` binary (Alpine runtime).
#
# Unlike the official Node CLI image (which wraps the CLI in a WebDAV-only
# entrypoint script), this image just ships the binary: run any subcommand you
# like, e.g. `docker run ixr serve webdav ...` or `... serve smb,sftp ...`.
#
# Cross-compilation, not emulation: the builder stage always runs on
# $BUILDPLATFORM (your native host arch) and cross-compiles every target with
# `cargo zigbuild` (zig's bundled clang + musl sysroots act as the linker/C
# compiler for each triple) instead of running a foreign-arch container under
# QEMU. Only the tiny final stage (an `apk add` and a `COPY`) runs as
# $TARGETPLATFORM, since Alpine's package manager is arch-native — that part is
# unavoidable but costs seconds, not a Rust/LLVM build under emulation.
#
# Build all platforms:
#   docker buildx build \
#     --platform linux/amd64,linux/386,linux/arm64,linux/arm/v7,linux/arm/v6 \
#     -t internxt-cli-rust:latest --push .

# Must satisfy the highest rust-version any locked dependency declares, not
# just our own MSRV: internxt-cli (crates/internxt-cli/Cargo.toml) declares 1.95
# for the smb-server fork, and the internxt-core dependency declares 1.88. Bump
# this together with those whenever Cargo.lock's transitive deps move.
ARG RUST_VERSION=1.96
ARG ZIG_VERSION=0.13.0
ARG ALPINE_VERSION=3.21

# ---------------------------------------------------------------------------
# builder: native $BUILDPLATFORM only, cross-compiles every target below
# ---------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS builder
ARG ZIG_VERSION

# clang/cmake/nasm/perl: build-time deps of C crates in the dependency tree
# (bindgen needs a *host* libclang; aws-lc-sys falls back to cmake+nasm for
# targets without prebuilt assembly). zig itself does the actual target-side
# compiling/linking.
RUN apt-get update && apt-get install -y --no-install-recommends \
      curl xz-utils clang cmake ninja-build nasm perl pkg-config git \
    && rm -rf /var/lib/apt/lists/*

# zig: one cross-toolchain that covers every target triple below, real
# cross-compilation (native codegen for the target), not QEMU.
RUN set -eux; \
    case "$(uname -m)" in \
      x86_64)  zigarch=x86_64 ;; \
      aarch64) zigarch=aarch64 ;; \
      *) echo "unsupported builder host arch: $(uname -m)" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${zigarch}-${ZIG_VERSION}.tar.xz" -o /tmp/zig.tar.xz; \
    mkdir -p /opt/zig; \
    tar -xJf /tmp/zig.tar.xz -C /opt/zig --strip-components=1; \
    rm /tmp/zig.tar.xz
ENV PATH="/opt/zig:${PATH}"

RUN cargo install cargo-zigbuild --locked

RUN rustup target add \
      x86_64-unknown-linux-musl \
      i686-unknown-linux-musl \
      aarch64-unknown-linux-musl \
      armv7-unknown-linux-musleabihf \
      arm-unknown-linux-musleabihf

WORKDIR /src
COPY . .

# fuse needs libfuse (no cross story worth the pain in a headless container,
# and a container-local FUSE mount can't be seen from the host anyway); sso
# needs a real browser to open (headless + legacy/env-var login covers
# Kyber-hybrid workspaces that SSO can't anyway, see CLAUDE.md); dotenv and
# self-update are pointless when the image itself is the update/config unit
# (docker pull / -e / --env-file replace both). webdav(-tls) + smb + nfs +
# sftp are all pure-Rust and cross-compile cleanly. Adjust to taste.
ARG CLI_FEATURES="webdav,webdav-tls,smb,nfs,sftp"

RUN set -eux; \
    mkdir -p /out; \
    for target in \
      x86_64-unknown-linux-musl \
      i686-unknown-linux-musl \
      aarch64-unknown-linux-musl \
      armv7-unknown-linux-musleabihf \
      arm-unknown-linux-musleabihf \
    ; do \
      cargo zigbuild --release --locked -p internxt-cli \
        --no-default-features --features "${CLI_FEATURES}" \
        --target "$target"; \
    done; \
    cp "target/x86_64-unknown-linux-musl/release/ixr"      /out/ixr-amd64; \
    cp "target/i686-unknown-linux-musl/release/ixr"        /out/ixr-386; \
    cp "target/aarch64-unknown-linux-musl/release/ixr"     /out/ixr-arm64; \
    cp "target/aarch64-unknown-linux-musl/release/ixr"     /out/ixr-arm64v8; \
    cp "target/armv7-unknown-linux-musleabihf/release/ixr" /out/ixr-armv7; \
    cp "target/arm-unknown-linux-musleabihf/release/ixr"   /out/ixr-armv6

# ---------------------------------------------------------------------------
# final: one per-platform image, binary picked by TARGETARCH/TARGETVARIANT
# ---------------------------------------------------------------------------
FROM alpine:${ALPINE_VERSION} AS final

# ca-certificates: rustls-platform-verifier (used for HTTPS API calls)
# validates against the system trust store on Linux.
RUN apk add --no-cache ca-certificates

# VERSION: set by the release workflow to the tag (e.g. `1.2.3`); left `dev`
# for local/manual builds. Purely metadata (OCI labels) — not baked into the
# binary, which gets its version from Cargo.toml at compile time.
ARG VERSION=dev
LABEL org.opencontainers.image.title="ixr" \
      org.opencontainers.image.description="Unofficial Rust port of the Internxt CLI" \
      org.opencontainers.image.source="https://github.com/Bebbssos/internxt-cli-rust" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.version="${VERSION}"

ARG TARGETARCH
ARG TARGETVARIANT
COPY --from=builder /out/ixr-${TARGETARCH}${TARGETVARIANT} /usr/local/bin/ixr

WORKDIR /root
VOLUME ["/root/.ixr"]

# webdav, smb, nfs, sftp default ports (see `ixr serve --help`)
EXPOSE 3005 4445 12049 2022

ENTRYPOINT ["ixr"]
CMD ["--help"]

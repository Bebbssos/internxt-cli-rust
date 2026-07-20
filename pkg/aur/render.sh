#!/usr/bin/env bash
# Fills in pkg/aur/PKGBUILD's placeholders from a just-published GitHub
# release. Run only after the gh-release job has uploaded that tag's assets
# (release.yml's `aur` job depends on `gh-release` for this reason).
set -euo pipefail

version="${1:?usage: render.sh <version, e.g. 1.2.3>}"
repo="Bebbssos/internxt-rust"
tag="v${version}"
dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

fetch_sha() {
  local target="$1"
  curl -fsSL "https://github.com/${repo}/releases/download/${tag}/ixr-${target}.tar.gz.sha256" | awk '{print $1}'
}

sha_x86_64=$(fetch_sha x86_64-unknown-linux-gnu)
sha_aarch64=$(fetch_sha aarch64-unknown-linux-gnu)

sed -i \
  -e "s/@PKGVER@/${version}/" \
  -e "s/@SHA256_X86_64@/${sha_x86_64}/" \
  -e "s/@SHA256_AARCH64@/${sha_aarch64}/" \
  "${dir}/PKGBUILD"

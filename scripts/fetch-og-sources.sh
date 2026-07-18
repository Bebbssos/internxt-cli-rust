#!/usr/bin/env bash
#
# Fetch the original Internxt node sources + the published CLI into ./og
# (the reference material this Rust port is based on).
#
# The Rust port is pinned to the upstream commits / versions recorded in TODO.md.
# This script checks out exactly those commits so `og/` matches what we ported from.
#
# Usage:  ./scripts/fetch-og-sources.sh
# Re-run safe: it skips repos already cloned (delete og/<name> to refresh).
set -euo pipefail

cd "$(dirname "$0")/.."
mkdir -p og
cd og

# repo  url  pinned-commit
clone_pin() {
  local name="$1" url="$2" commit="$3"
  if [ -d "$name/.git" ]; then
    echo "== $name already present, fetching =="
    git -C "$name" fetch --quiet origin
  else
    echo "== cloning $name =="
    git clone --quiet "$url" "$name"
  fi
  git -C "$name" checkout --quiet "$commit"
  echo "   $name -> $(git -C "$name" rev-parse --short HEAD)"
}

clone_pin cli     https://github.com/internxt/cli.git     d977ab5e8ad176f572bd821e0c759219eae9522d  # v1.6.7
clone_pin inxt-js https://github.com/internxt/inxt-js.git 855ed28c492ada9048d730d3de727f0d1732f5c2  # v3.3.5
clone_pin lib     https://github.com/internxt/lib.git     accd5890b22b0ab4719ef5f333545eb3eee4b5d2  # v1.5.1
clone_pin sdk     https://github.com/internxt/sdk.git     efc30f28b09bf491dc6afdcba10998190ca8afae  # v1.17.17

# Published CLI + its runtime deps (gives og/node_modules/@internxt/* at the
# versions the released CLI actually runs).
echo "== installing @internxt/cli@1.6.7 into og/node_modules =="
npm install --no-save --prefix . @internxt/cli@1.6.7

echo "Done. Reference sources are in ./og (git-ignored)."

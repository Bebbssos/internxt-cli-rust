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

clone_pin cli     https://github.com/internxt/cli.git     166fb5a77dab27aea3e9cdb1af0e1713d9dde04e  # v1.6.5
clone_pin inxt-js https://github.com/internxt/inxt-js.git a27cc91cde65700ebca088ebba3870d9bbf2a94f  # v3.3.1
clone_pin lib     https://github.com/internxt/lib.git     22eaae309ad17a8c39c03b742bca631feca0a8f9
clone_pin sdk     https://github.com/internxt/sdk.git     aa97c980562926b3425a290f8ca39ea5c1f45a15  # v1.17.9

# Published CLI + its runtime deps (gives og/node_modules/@internxt/* at the
# versions the released CLI actually runs: inxt-js@3.2.2, sdk@1.17.5, lib@1.4.2).
echo "== installing @internxt/cli@1.6.5 into og/node_modules =="
npm install --no-save --prefix . @internxt/cli@1.6.5

echo "Done. Reference sources are in ./og (git-ignored)."

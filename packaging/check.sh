#!/usr/bin/env bash
# Smoke test for the package manager templates: renders them with fake checksums and checks that
# the output parses. Uses ruby, jq and makepkg when installed, and skips each one that is missing.
# Usage: packaging/check.sh
set -euo pipefail

here=$(dirname "$0")
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for platform in linux-x86_64.tar.gz macos-aarch64.tar.gz windows-x86_64.zip; do
  echo "$(printf '%064d' 0)  clipbroad-v1.2.3-$platform"
done > "$tmp/SHA256SUMS"
"$here/render.sh" 1.2.3 "$tmp/SHA256SUMS" "$tmp/out"
cd "$tmp/out"

if grep -rn '@[A-Z0-9]*@' .; then
  echo "placeholders left after rendering" >&2
  exit 1
fi

skip() { echo "skip: $1 not installed"; }
if command -v ruby > /dev/null; then ruby -c homebrew-tap/Formula/clipbroad.rb; else skip ruby; fi
if command -v jq > /dev/null; then jq -e . scoop-bucket/bucket/clipbroad.json > /dev/null; else skip jq; fi
if command -v makepkg > /dev/null; then
  # The AUR shows the metadata from .SRCINFO, so it must match what makepkg reads from PKGBUILD.
  (cd aur && makepkg --printsrcinfo | diff -u .SRCINFO -)
else
  skip makepkg
fi
echo "packaging templates OK"

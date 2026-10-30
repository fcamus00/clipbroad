#!/usr/bin/env bash
# Smoke test for packaging/: renders the package manager templates with fake checksums and checks
# that the output parses (with ruby and jq, skipping each one that is missing). On Linux x86_64 it
# also runs install.sh against a fake local release.
# Usage: packaging/check.sh
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

for platform in macos-aarch64.tar.gz windows-x86_64.zip; do
  echo "$(printf '%064d' 0)  clipbroad-v1.2.3-$platform"
done > "$tmp/SHA256SUMS"
"$here/render.sh" 1.2.3 "$tmp/SHA256SUMS" "$tmp/out"

if grep -rn '@[A-Z0-9]*@' "$tmp/out"; then
  echo "placeholders left after rendering" >&2
  exit 1
fi

skip() { echo "skip: $1"; }
if command -v ruby > /dev/null; then
  ruby -c "$tmp/out/homebrew-tap/Formula/clipbroad.rb"
else
  skip "ruby not installed"
fi
if command -v jq > /dev/null; then
  jq -e . "$tmp/out/scoop-bucket/bucket/clipbroad.json" > /dev/null
else
  skip "jq not installed"
fi

if [ "$(uname -sm)" = "Linux x86_64" ]; then
  # A fake release: the archive layout of release.yml, with a script in place of the binary.
  release=$tmp/release name=clipbroad-v1.2.3-linux-x86_64
  mkdir -p "$release/$name"
  printf '#!/bin/sh\necho "clipbroad 1.2.3"\n' > "$release/$name/clipbroad"
  chmod +x "$release/$name/clipbroad"
  tar -czf "$release/$name.tar.gz" -C "$release" "$name"
  (cd "$release" && sha256sum "$name.tar.gz" > SHA256SUMS)

  install() {
    CLIPBROAD_DOWNLOAD_URL="file://$release" CLIPBROAD_INSTALL_DIR="$tmp/bin" sh "$here/install.sh"
  }
  install > /dev/null
  [ "$("$tmp/bin/clipbroad" --version)" = "clipbroad 1.2.3" ]
  # Running it again upgrades in place.
  install > /dev/null

  sed -i 's/^./f/' "$release/SHA256SUMS"
  if install > /dev/null 2>&1; then
    echo "install.sh accepted a bad checksum" >&2
    exit 1
  fi
  echo "install.sh OK"
else
  skip "install.sh needs Linux x86_64"
fi
echo "packaging OK"

#!/usr/bin/env bash
# Fills the Homebrew template for a release into <out-dir>/homebrew-tap.
# Usage: packaging/render.sh <version> <SHA256SUMS> <out-dir>
set -euo pipefail

version=$1 sums=$2 out=$3
here=$(dirname "$0")

# Stable versions only: pre-releases stay on GitHub.
if [[ ! $version =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "version must look like 1.2.3, got '$version'" >&2
  exit 1
fi

render() { # <template-dir> <platform>
  local name="clipbroad-v$version-$2" sha
  sha=$(awk -v name="$name" '$2 == name ".tar.gz" { print $1 }' "$sums")
  if [[ ! $sha =~ ^[0-9a-f]{64}$ ]]; then
    echo "no checksum for $name in $sums" >&2
    exit 1
  fi
  mkdir -p "$out"
  cp -r "$here/$1" "$out/"
  find "$out/$1" -type f -exec sed -i "s/@VERSION@/$version/g; s/@SHA256@/$sha/g" {} +
}

render homebrew-tap macos-aarch64

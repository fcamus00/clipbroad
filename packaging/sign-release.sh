#!/usr/bin/env bash
# Signs a published release, so that `clipbroad update` and the tray menu accept it. Run it by hand
# after the release workflow ends: the secret key and its password never reach GitHub.
# Usage: packaging/sign-release.sh <tag>
# Environment:
#   CLIPBROAD_SIGNING_KEY  the minisign secret key (default: ~/.config/clipbroad-release/minisign.key)
set -euo pipefail

tag=${1:?usage: packaging/sign-release.sh <tag>}
repo=fcamus00/clipbroad
here=$(cd "$(dirname "$0")" && pwd)
key=${CLIPBROAD_SIGNING_KEY:-$HOME/.config/clipbroad-release/minisign.key}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

gh release download "$tag" --repo "$repo" --pattern SHA256SUMS --dir "$tmp"
minisign -S -s "$key" -m "$tmp/SHA256SUMS" -t "clipbroad $tag"
# The key the binary trusts, so a signature with any other key fails here and not on every machine.
minisign -V -q -p "$here/minisign.pub" -m "$tmp/SHA256SUMS"
gh release upload "$tag" "$tmp/SHA256SUMS.minisig" --repo "$repo"
echo "Signed $tag"

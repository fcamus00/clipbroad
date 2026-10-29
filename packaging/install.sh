#!/bin/sh
# Installs or upgrades clipbroad on Linux x86_64 from a GitHub release. Each release attaches this
# script, so the latest one is at:
#   curl -fsSL https://github.com/fcamus00/clipbroad/releases/latest/download/install.sh | sh
# Environment:
#   CLIPBROAD_VERSION       tag to install, like v0.2.0 (default: the latest stable release)
#   CLIPBROAD_INSTALL_DIR   where the binary goes (default: ~/.local/bin)
#   CLIPBROAD_DOWNLOAD_URL  folder with SHA256SUMS and the archive; packaging/check.sh points it to
#                           a local file:// folder
set -eu

die() {
  echo "clipbroad install: $1" >&2
  exit 1
}

# Everything runs from main, so a download cut short by the network does not run half a script.
main() {
  [ "$(uname -sm)" = "Linux x86_64" ] ||
    die "only Linux x86_64 is supported. See https://github.com/fcamus00/clipbroad#install"
  command -v curl > /dev/null || die "curl is required"

  repo=https://github.com/fcamus00/clipbroad/releases
  if [ -n "${CLIPBROAD_VERSION:-}" ]; then
    base=$repo/download/$CLIPBROAD_VERSION
  else
    base=$repo/latest/download
  fi
  base=${CLIPBROAD_DOWNLOAD_URL:-$base}
  dir=${CLIPBROAD_INSTALL_DIR:-$HOME/.local/bin}

  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT

  curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS" || die "could not download $base/SHA256SUMS"
  line=$(grep 'linux-x86_64\.tar\.gz$' "$tmp/SHA256SUMS") || die "no Linux archive in SHA256SUMS"
  name=${line##* }
  curl -fsSL "$base/$name" -o "$tmp/$name" || die "could not download $base/$name"
  (cd "$tmp" && echo "$line" | sha256sum -c --quiet -) || die "checksum mismatch for $name"
  tar -xzf "$tmp/$name" -C "$tmp"

  # Rename over the old binary instead of writing into it: Linux refuses to write to a running
  # executable, and the daemon may be running.
  mkdir -p "$dir"
  cp "$tmp/${name%.tar.gz}/clipbroad" "$dir/.clipbroad.new"
  chmod 755 "$dir/.clipbroad.new"
  mv -f "$dir/.clipbroad.new" "$dir/clipbroad"

  echo "Installed $("$dir/clipbroad" --version) to $dir/clipbroad"
  case ":$PATH:" in
    *":$dir:"*) ;;
    *) echo "$dir is not in PATH; add it to your shell profile." ;;
  esac
  echo "Run 'clipbroad service install' to start the daemon at login, or to restart it after an upgrade."
}

main

#!/bin/sh
# Installs search from its GitHub release: the archive for this computer, checked
# against the release's checksums, into ~/.local/bin (or $SEARCH_INSTALL_DIR).
# Run it again to update.
#
#   curl -fsSL https://raw.githubusercontent.com/fschrhunt/search/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/fschrhunt/search/main/install.sh | sh -s -- --version v0.1.0
#
# $SEARCH_RELEASES replaces https://github.com/fschrhunt/search/releases, for testing.
set -eu

releases=${SEARCH_RELEASES:-https://github.com/fschrhunt/search/releases}
dir=${SEARCH_INSTALL_DIR:-$HOME/.local/bin}
version=${SEARCH_VERSION:-}

fail() { echo "search install: $*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Install the Search release for this computer into ~/.local/bin.
Run the installer again to upgrade; installed engines and settings are untouched.

Usage: sh install.sh [--version vX.Y.Z] [--dir DIR]
  --version TAG  choose a release (default: latest, or SEARCH_VERSION)
  --dir DIR      choose a folder (default: ~/.local/bin, or SEARCH_INSTALL_DIR)
  help           show this reference (--help is also accepted)

Download and inspect install.sh before running it. Archives are checked against
the release checksums; checksums do not independently authenticate the publisher.
EOF
}

while [ $# -gt 0 ]; do
  case $1 in
    --version) [ $# -ge 2 ] || fail "--version needs a version, like v0.1.0"; version=$2; shift 2 ;;
    --version=*) version=${1#--version=}; shift ;;
    --dir) [ $# -ge 2 ] || fail "--dir needs a folder"; dir=$2; shift 2 ;;
    help|-h|--help) usage; exit 0 ;;
    *) fail "unknown option $1; options: --version vX.Y.Z, --dir DIR" ;;
  esac
done

case $(uname -s) in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  *) fail "search runs on macOS and Linux; this is $(uname -s)" ;;
esac
case $(uname -m) in
  arm64|aarch64) arch=arm64 ;;
  x86_64|amd64) arch=amd64 ;;
  *) fail "search is built for arm64 and x86_64; this is $(uname -m)" ;;
esac

fetch() { # fetch URL FILE
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1" -o "$2"
  elif command -v wget >/dev/null 2>&1; then wget -q "$1" -O "$2"
  else fail "needs curl or wget"; fi
}

if [ -z "$version" ]; then
  # The latest release redirects to its tag; read the tag from where it lands.
  # The effective URL may or may not end in a slash (GitHub uses
  # `.../tag/vX.Y.Z`; a proxy may add `/`), so take the last non-empty segment.
  if command -v curl >/dev/null 2>&1; then
    landing=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "$releases/latest") || fail "could not reach $releases"
  else
    landing=$(wget -q --max-redirect=5 --server-response --spider "$releases/latest" 2>&1 | sed -n 's/^ *[Ll]ocation: *//p' | tail -1 | tr -d '\r')
  fi
  landing=$(echo "$landing" | sed 's:/*$::')
  version=${landing##*/}
fi
case $version in
  v[0-9]*.[0-9]*.[0-9]*) ;;
  [0-9]*.[0-9]*.[0-9]*) version=v$version ;;
  *) fail "could not tell the latest version from $releases (got \"$version\")" ;;
esac

if [ -L "$dir/search" ] || [ -e "$dir/.search-installed-by" ]; then
  fail "$dir/search belongs to a package manager; update it there, or choose another folder with --dir"
fi

archive="search_${version}_${os}_${arch}.tar.gz"
work=$(mktemp -d "${TMPDIR:-/tmp}/search-install.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM

echo "Installing search $version for $os/$arch"
fetch "$releases/download/$version/$archive" "$work/$archive" || fail "could not download $archive"
fetch "$releases/download/$version/checksums.txt" "$work/checksums.txt" || fail "could not download checksums.txt"

expected=$(grep " $archive\$" "$work/checksums.txt" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$work/$archive" | cut -d' ' -f1)
else actual=$(shasum -a 256 "$work/$archive" | cut -d' ' -f1); fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  fail "checksum mismatch for $archive; refusing to install"
fi

tar -xzf "$work/$archive" -C "$work" search
if [ ! -f "$work/search" ] || [ -L "$work/search" ]; then
  fail "$archive has no search binary"
fi
got=$("$work/search" version 2>/dev/null || true)
[ "$got" = "$version" ] || fail "$archive says it is \"$got\", not $version"

mkdir -p "$dir"
cp "$work/search" "$dir/.search.next"
chmod 755 "$dir/.search.next"
mv -f "$dir/.search.next" "$dir/search"
echo "Installed $dir/search"

case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "Add $dir to your PATH, for example in ~/.zshrc or ~/.bashrc:"
     echo "  export PATH=\"$dir:\$PATH\"" ;;
esac
echo "Next: search help, or search \"rust async\" (no server required)"

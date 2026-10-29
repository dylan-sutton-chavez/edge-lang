#!/usr/bin/env bash

set -e

BASE="${EDGE_INSTALL_BASE:-https://cdn.edgelang.dev/cli}"
INSTALL_DIR="${EDGE_INSTALL_DIR:-$HOME/.local/bin}"

# An optional vX.Y.Z reads the frozen copy of that release, no argument reads the latest.
version="${1:-}"
if [ -n "$version" ]; then
  printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || {
    echo "bad version: $version, expected vX.Y.Z" >&2
    exit 1
  }
  BASE="${BASE%/cli}/$version/cli"
fi

case "$(uname -s)" in
  Linux) os="unknown-linux-musl" ;;
  Darwin) os="apple-darwin" ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  x86_64|amd64) arch="x86_64" ;;
  aarch64|arm64) arch="aarch64" ;;
  *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
esac

target="${arch}-${os}"
archive="edge-${target}.tar.gz"

# the sha256 of a file in the hex the published digest holds
sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else openssl dgst -sha256 "$1" | sed 's/.*= //'
  fi
}

echo "downloading edge (${target})..."
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fsSL -o "$tmp/$archive" "${BASE}/${archive}"
# a pinned EDGE_INSTALL_SHA256 wins over the digest published beside the archive
want="${EDGE_INSTALL_SHA256:-$(curl -fsSL "${BASE}/${archive}.sha256" | cut -d' ' -f1)}"
got="$(sha256 "$tmp/$archive")"
if [ -z "$want" ] || [ "$got" != "$want" ]; then
  echo "the download of $archive does not match its sha256 ${want:-(none published)}, got $got" >&2
  exit 1
fi
mkdir -p "$INSTALL_DIR"
tar -xzf "$tmp/$archive" -C "$INSTALL_DIR" edge
chmod +x "$INSTALL_DIR/edge"
echo "installed $INSTALL_DIR/edge"

case "$(basename "${SHELL:-bash}")" in
  bash) rc="$HOME/.bashrc" ;;
  zsh) rc="$HOME/.zshrc" ;;
  *) rc="" ;;
esac

# Check the rc file, not the live PATH, an old shell keeps the dir after an uninstall.
rc_has_path() {
  grep -qs "$INSTALL_DIR" "$rc" 2>/dev/null && return 0
  # The default dir is often already on PATH via a $HOME-spelled rc/profile line.
  [ "$INSTALL_DIR" = "$HOME/.local/bin" ] && grep -qs '\$HOME/.local/bin' "$rc" 2>/dev/null
}

rc_changed=""
if [ -n "$rc" ] && ! rc_has_path; then
  printf '\nexport PATH="%s:$PATH"\n' "$INSTALL_DIR" >> "$rc"
  echo "added $INSTALL_DIR to PATH in $rc"
  rc_changed=1
fi

"$INSTALL_DIR/edge" --version

if [ -n "$rc_changed" ]; then
  echo "open a new terminal to pick up the new environment (or run 'exec \$SHELL')"
fi

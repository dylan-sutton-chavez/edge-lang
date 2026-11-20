#!/usr/bin/env bash

set -e

INSTALL_DIR="${INTERPRETER_INSTALL_DIR:-$HOME/.local/bin}"

# everything the CLI caches, interpreter-owned and refetchable, so it goes with the binary
CACHE_DIR="${XDG_CACHE_HOME:-$HOME/.cache}/interpreter"

# the binary
if [ -f "$INSTALL_DIR/interpreter" ]; then
  rm -f "$INSTALL_DIR/interpreter"
  echo "removed $INSTALL_DIR/interpreter"
else
  echo "no interpreter binary at $INSTALL_DIR/interpreter"
fi

# each PATH entry with a .interpreterbak left beside its rc file
for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
  [ -f "$rc" ] || continue
  if grep -qs "$INSTALL_DIR" "$rc"; then
    sed -i.interpreterbak "\|export PATH=\"$INSTALL_DIR:\$PATH\"|d" "$rc"
    echo "cleaned interpreter entries from $rc (backup at $rc.interpreterbak)"
  fi
done

# the cache which nothing else reads and every entry fetches again
if [ -d "$CACHE_DIR" ]; then
  rm -rf "$CACHE_DIR"
  echo "removed $CACHE_DIR"
fi

echo "interpreter removed"

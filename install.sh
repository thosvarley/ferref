#!/usr/bin/env bash
# Builds ferref in release mode, installs the binary onto PATH, and creates
# the library directory (ferref.db + pdfs/) it reads from. Safe to re-run:
# it overwrites the binary and only appends a PATH/FERREF_HOME line to your
# shell rc if one isn't already there.
set -euo pipefail

# cargo build and cp below are relative paths -- without this, running the
# script as `bash ~/Code/ferref/install.sh` from some other directory builds
# whatever crate happens to be under $PWD (or just errors) instead of ferref.
cd "$(dirname "$(readlink -f "$0")")"

DEFAULT_HOME="$HOME/.ferref"
BIN_DIR="$HOME/.local/bin"

if [ -t 0 ]; then
    read -rp "Library directory [$DEFAULT_HOME]: " FERREF_HOME_INPUT
else
    FERREF_HOME_INPUT=""
fi
LIBRARY_DIR="${FERREF_HOME_INPUT:-$DEFAULT_HOME}"
# `read` does not expand ~: answering the prompt with e.g. "~/refs" would
# otherwise be taken as a literal directory named "~", created under the
# repo (mkdir -p "~/refs"), and written into the rc file quoted -- where it
# never expands either, so every future invocation of ferref would treat
# the literal string "~/refs" as a path relative to wherever it's run from,
# exactly the per-directory accidental library Phase 14 exists to abolish.
LIBRARY_DIR="${LIBRARY_DIR/#\~/$HOME}"

echo "Building ferref (release)..."
cargo build --release --quiet

mkdir -p "$LIBRARY_DIR/pdfs"
mkdir -p "$BIN_DIR"
cp "target/release/ferref" "$BIN_DIR/ferref"
echo "Installed binary to $BIN_DIR/ferref"
echo "Library directory: $LIBRARY_DIR"

# Pick a shell rc to edit: whatever $SHELL says, falling back to .profile.
case "$(basename "${SHELL:-}")" in
    zsh) RC_FILE="$HOME/.zshrc" ;;
    bash) RC_FILE="$HOME/.bashrc" ;;
    *) RC_FILE="$HOME/.profile" ;;
esac
touch "$RC_FILE"

CHANGED=0
append_once() {
    local line="$1"
    grep -qxF "$line" "$RC_FILE" 2>/dev/null || { echo "$line" >>"$RC_FILE"; CHANGED=1; }
}

if [ "$LIBRARY_DIR" != "$DEFAULT_HOME" ]; then
    append_once "export FERREF_HOME=\"$LIBRARY_DIR\""
fi
case ":$PATH:" in
    *":$BIN_DIR:"*) ;;
    *) append_once "export PATH=\"$BIN_DIR:\$PATH\"" ;;
esac

if [ "$CHANGED" -eq 1 ]; then
    echo "Updated $RC_FILE -- run 'source $RC_FILE' or open a new terminal."
else
    echo "$BIN_DIR already on PATH, no shell rc changes needed."
fi

echo "Done. Try: ferref list"

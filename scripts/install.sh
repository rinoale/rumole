#!/usr/bin/env sh
set -eu

BIN_NAME="rumole"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

SCRIPT_DIR=$(CDPATH= cd "$(dirname "$0")" && pwd)
REPO_ROOT=$(CDPATH= cd "$SCRIPT_DIR/.." && pwd)
BIN_PATH="$REPO_ROOT/target/release/$BIN_NAME"
DEST_PATH="$INSTALL_DIR/$BIN_NAME"

if ! command -v cargo >/dev/null 2>&1; then
    echo "error: cargo was not found in PATH" >&2
    echo "install Rust with rustup, then rerun this script" >&2
    exit 1
fi

cd "$REPO_ROOT"
cargo build --release

if [ -d "$INSTALL_DIR" ]; then
    :
elif ! mkdir -p "$INSTALL_DIR" 2>/dev/null; then
    if ! command -v sudo >/dev/null 2>&1; then
        echo "error: could not create $INSTALL_DIR and sudo was not found" >&2
        exit 1
    fi

    sudo mkdir -p "$INSTALL_DIR"
fi

if [ -w "$INSTALL_DIR" ]; then
    install -m 755 "$BIN_PATH" "$DEST_PATH"
else
    if ! command -v sudo >/dev/null 2>&1; then
        echo "error: $INSTALL_DIR is not writable and sudo was not found" >&2
        exit 1
    fi

    sudo mkdir -p "$INSTALL_DIR"
    sudo install -m 755 "$BIN_PATH" "$DEST_PATH"
fi

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *)
        echo "warning: $INSTALL_DIR is not currently in PATH" >&2
        ;;
esac

echo "installed $BIN_NAME to $DEST_PATH"

#!/usr/bin/env bash
set -euo pipefail

echo "[gog_cli] Installing gog CLI..."

INSTALL_DIR="/usr/local/bin"

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  ARCH="amd64" ;;
    aarch64) ARCH="arm64" ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

OS=$(uname -s | tr '[:upper:]' '[:lower:]')

# Get latest version tag
LATEST=$(curl -fsSL "https://api.github.com/repos/ethereumdegen/gog/releases/latest" | grep '"tag_name"' | sed -E 's/.*"v([^"]+)".*/\1/')
if [ -z "$LATEST" ]; then
    echo "[gog_cli] Error: could not determine latest version"
    exit 1
fi

DOWNLOAD_URL="https://github.com/ethereumdegen/gog/releases/download/v${LATEST}/gog-${OS}-${ARCH}.tar.gz"
echo "[gog_cli] Downloading gog v${LATEST} from $DOWNLOAD_URL"

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/gog.tar.gz"
tar -xzf "$TMP_DIR/gog.tar.gz" -C "$TMP_DIR"

# Find the gog binary
GOG_BIN=$(find "$TMP_DIR" -name "gog" -type f | head -1)
if [ -z "$GOG_BIN" ]; then
    echo "[gog_cli] Error: gog binary not found in archive"
    exit 1
fi

chmod +x "$GOG_BIN"
mv "$GOG_BIN" "$INSTALL_DIR/gog"

echo "[gog_cli] Installed gog to $INSTALL_DIR/gog"
gog --version || true

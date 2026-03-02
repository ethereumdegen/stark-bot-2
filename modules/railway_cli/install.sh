#!/usr/bin/env bash
set -euo pipefail

echo "[railway_cli] Installing Railway CLI..."

INSTALL_DIR="/usr/local/bin"

# Detect architecture
ARCH=$(uname -m)
case "$ARCH" in
    x86_64)  ARCH="amd64" ;;
    aarch64) ARCH="arm64" ;;
    *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

OS=$(uname -s | tr '[:upper:]' '[:lower:]')

# Download latest Railway CLI
DOWNLOAD_URL="https://github.com/railwayapp/cli/releases/latest/download/railway-${OS}-${ARCH}.tar.gz"
echo "[railway_cli] Downloading from $DOWNLOAD_URL"

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/railway.tar.gz"
tar -xzf "$TMP_DIR/railway.tar.gz" -C "$TMP_DIR"

# Find the railway binary (may be in a subdirectory)
RAILWAY_BIN=$(find "$TMP_DIR" -name "railway" -type f | head -1)
if [ -z "$RAILWAY_BIN" ]; then
    echo "[railway_cli] Error: railway binary not found in archive"
    exit 1
fi

chmod +x "$RAILWAY_BIN"
mv "$RAILWAY_BIN" "$INSTALL_DIR/railway"

echo "[railway_cli] Installed railway to $INSTALL_DIR/railway"
railway version || true

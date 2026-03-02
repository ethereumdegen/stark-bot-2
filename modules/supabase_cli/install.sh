#!/usr/bin/env bash
set -euo pipefail

echo "[supabase_cli] Installing Supabase CLI..."

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
LATEST=$(curl -fsSL "https://api.github.com/repos/supabase/cli/releases/latest" | grep '"tag_name"' | sed -E 's/.*"v([^"]+)".*/\1/')
if [ -z "$LATEST" ]; then
    echo "[supabase_cli] Error: could not determine latest version"
    exit 1
fi

DOWNLOAD_URL="https://github.com/supabase/cli/releases/download/v${LATEST}/supabase_${OS}_${ARCH}.tar.gz"
echo "[supabase_cli] Downloading Supabase CLI v${LATEST} from $DOWNLOAD_URL"

TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

curl -fsSL "$DOWNLOAD_URL" -o "$TMP_DIR/supabase.tar.gz"
tar -xzf "$TMP_DIR/supabase.tar.gz" -C "$TMP_DIR"

# Find the supabase binary
SUPABASE_BIN=$(find "$TMP_DIR" -name "supabase" -type f | head -1)
if [ -z "$SUPABASE_BIN" ]; then
    echo "[supabase_cli] Error: supabase binary not found in archive"
    exit 1
fi

chmod +x "$SUPABASE_BIN"
mv "$SUPABASE_BIN" "$INSTALL_DIR/supabase"

echo "[supabase_cli] Installed supabase to $INSTALL_DIR/supabase"
supabase --version || true

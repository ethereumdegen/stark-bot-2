#!/bin/bash
set -e

# Extract version from Cargo.toml
VERSION=$(grep '^version' stark-backend/Cargo.toml | head -1 | sed 's/.*"\(.*\)".*/\1/')
echo "Detected starkbot version: $VERSION"

REPO="ghcr.io/starkbotai/starkbot-v2"

echo "Building Docker image (v2)..."
docker build \
  --build-arg STARKBOT_VERSION="$VERSION" \
  -t "$REPO:flash" \
  -t "$REPO:latest" \
  -t "$REPO:$VERSION" \
  .

echo "Pushing to registry..."
docker push "$REPO:flash"
docker push "$REPO:latest"
docker push "$REPO:$VERSION"

echo "Done! Pushed v2 version $VERSION to $REPO"

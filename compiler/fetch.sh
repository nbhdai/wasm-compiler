#!/usr/bin/env bash

set -euv -o pipefail

# --- Load Environment Variables ---
if [ -f ../.env ]; then
  set -a # Automatically export all variables
  source ../.env
  set +a # Stop automatically exporting
else
  echo "⚠️  .env file not found. Assuming CR_PAT is set in the environment."
fi

# Verify that CR_PAT is set
if [ -z "${CR_PAT:-}" ]; then
  echo "❌ Error: CR_PAT is not set. Create a .env file or export the variable."
  exit 1
fi

# --- Configuration ---
GH_OWNER="nbhdai"
IMAGE_NAME="rust-stable"
TAG="latest"
REGISTRY="ghcr.io"
IMAGE_REF="ghcr.io/${GH_OWNER}/${IMAGE_NAME}:${TAG}"

# --- Script ---
echo "🔐 Logging into GHCR..."
echo "${CR_PAT}" | podman login "${REGISTRY}" -u "${GH_OWNER}" --password-stdin

echo "--- Pulling ${IMAGE_REF} ---"
podman pull "${IMAGE_REF}"

echo "--- Tagging ${IMAGE_REF} as ${IMAGE_NAME} ---"
podman tag "${IMAGE_REF}" "${IMAGE_NAME}"

echo "✅ All images pulled and tagged successfully."
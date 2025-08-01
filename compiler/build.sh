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
IMAGE_REF="ghcr.io/${GH_OWNER}/${IMAGE_NAME}:${TAG}"

# --- Script ---
echo "🔐 Logging into GHCR..."
echo "${CR_PAT}" | podman login ghcr.io -u "${GH_OWNER}" --password-stdin

echo "🛠️ Building ${IMAGE_REF}..."
podman build \
        -t "${IMAGE_REF}" \
        --build-arg channel="stable" \
        base

echo "🚀 Pushing ${IMAGE_REF}..."
podman push "${IMAGE_REF}"

echo "✅ Successfully pushed ${IMAGE_REF} to GHCR."
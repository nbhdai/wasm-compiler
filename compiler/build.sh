#!/usr/bin/env bash
set -euv -o pipefail

podman build \
  -t "rust-wasm" \
  -t "nbhdai/rust-wasm" \
  --build-arg channel="stable" \
  base
#!/usr/bin/env bash

set -euv -o pipefail

podman build \
        -t "rust-stable" \
        -t "nbhdai/rust-stable" \
        --build-arg channel="stable" \
        base
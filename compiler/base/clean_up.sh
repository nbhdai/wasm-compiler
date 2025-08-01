#!/usr/bin/env bash

for wasm in $(find target/ -name '*wasm' -not -path '*/deps/*'); do
    rm "${wasm}"
done
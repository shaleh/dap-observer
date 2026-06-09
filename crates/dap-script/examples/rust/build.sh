#!/bin/sh
# Build the Rust sieve with debug info.
cd "$(dirname "$0")" || exit 1

RUSTC="$(command -v rustc)"
if [ -z "$RUSTC" ]; then
    echo "error: rustc not found on PATH. install Rust from https://rustup.rs" >&2
    exit 1
fi

"$RUSTC" -g sieve.rs -o sieve || exit 1

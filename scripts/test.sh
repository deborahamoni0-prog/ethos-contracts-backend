#!/usr/bin/env bash
set -e

echo "Running Ethos-Protocol tests..."
cargo test --manifest-path contracts/ttl_vault/Cargo.toml
echo "All tests passed."

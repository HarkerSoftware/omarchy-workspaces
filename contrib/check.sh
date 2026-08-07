#!/usr/bin/env bash
# Full local check: what CI runs. Keep this green before every commit.
set -euo pipefail
cd "$(dirname "$0")/.."

cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features

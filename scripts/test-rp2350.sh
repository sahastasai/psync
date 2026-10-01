#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo test --all-targets
cargo test --no-default-features
cargo build --release --target thumbv8m.main-none-eabihf --example rp2350
if [[ "${1:-}" == "--flash" ]]; then
    probe-rs download --chip RP235x target/thumbv8m.main-none-eabihf/release/examples/rp2350
fi

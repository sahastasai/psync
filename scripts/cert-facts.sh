#!/usr/bin/env bash
# Writes verify/facts.tsv: what the compiler found every call in the certified functions really
# does (files, services, panics, and hidden state: statics, thread-locals, cells and locks behind
# references), from `cargo psync check`, with paths relative to the repository. `psync-cert
# --facts verify/facts.tsv` adds these places to the certificates. CI regenerates the file (job
# `psync-check`) and fails if it changed without being committed.
#
#   scripts/cert-facts.sh            # needs rustup; builds psync-check with its pinned nightly
#
# PSYNC_FACTS_DIR (default target/cert-facts) holds the checked crates' builds, PSYNC_CHECK_TARGET
# (default psync-check/target, where its own `cargo test` builds) the driver.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
work=${PSYNC_FACTS_DIR:-$root/target/cert-facts}
toolchain=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$root/psync-check/rust-toolchain.toml")
export RUSTUP_TOOLCHAIN=$toolchain
mkdir -p "$work"
check_target=${PSYNC_CHECK_TARGET:-$root/psync-check/target}
(cd "$root/psync-check" && cargo build --quiet --target-dir "$check_target")
bin=$check_target/debug

# `cargo psync check` in a crate; prints the facts file it wrote.
facts() {
    local dir=$1 target=$2
    shift 2
    rm -rf "$target/psync"
    if ! (cd "$dir" && CARGO_TARGET_DIR=$target "$bin/cargo-psync" psync check "$@" >"$target.log" 2>&1); then
        cat "$target.log" >&2
        exit 1
    fi
    cat "$target/psync/facts.tsv"
}

{
    facts "$root" "$work/psync" --no-default-features --features std \
        --test graph --test sync --example graph_suite
    facts "$root/psync-check/tests/fixtures/hidden" "$work/fixture"
} | sed "s#^$root/##" |
    grep -E '^(tests/graph\.rs|tests/sync\.rs|psync-check/tests/fixtures/hidden/app/src/lib\.rs|examples/graph_suite/)' |
    LC_ALL=C sort -u >"$root/verify/facts.tsv"
echo "cert-facts: $(wc -l <"$root/verify/facts.tsv") call sites in verify/facts.tsv" >&2

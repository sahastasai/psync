#!/bin/sh
# Regenerate the certificates of every schedule in the tests, the fixture and graph_suite, then
# check them with Lean. Run from the repository root.
set -e
cargo run -q -p psync-graph --features cli --bin psync-cert -- tests/graph.rs tests/sync.rs psync-check/tests/fixtures/hidden/app/src/lib.rs examples/graph_suite/*.rs > verify/Certs.lean
cd verify && lake build PsyncVerify GeneratedCerts

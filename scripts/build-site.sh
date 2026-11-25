#!/usr/bin/env bash
# Builds https://psync.sahasta.com into book/book/: the book, and the Rustdoc of every crate
# (public API and internals) under /api/. Deploy with `cd book && npx wrangler@4 deploy`.
set -euo pipefail
cd "$(dirname "$0")/.."
mdbook_build() { if command -v mdbook >/dev/null; then mdbook build; else nix-shell -p mdbook --run 'mdbook build'; fi; }
(cd book && mdbook_build)

# The workspace crates: psync, psync-macros, psync-graph (internals included, for contributors).
cargo doc --no-deps --no-default-features --features std -p psync -p psync-macros -p psync-graph \
  --document-private-items
# psync-check links rustc's internals, so it builds with its pinned nightly.
rustup_run() { if command -v rustup >/dev/null; then "$@"; else nix-shell -p rustup --run "$*"; fi; }
(cd psync-check && rustup_run cargo doc --no-deps --document-private-items)

rm -rf book/book/api && mkdir -p book/book/api
cp -r target/doc/. book/book/api/
mkdir -p book/book/api/check && cp -r psync-check/target/doc/. book/book/api/check/
cat > book/book/api/index.html <<'HTML'
<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>psync API reference</title>
<style>body{font:16px/1.5 system-ui,sans-serif;max-width:44rem;margin:3rem auto;padding:0 1rem;color:#182c3c}
a{color:#006d77}li{margin:.4rem 0}@media(prefers-color-scheme:dark){body{background:#111;color:#ddd}a{color:#6cc}}</style>
<h1>psync API reference (Rustdoc)</h1>
<p>Generated from the source, internals included. Back to <a href="/">the book</a>.</p>
<ul>
<li><a href="psync/index.html">psync</a>: the attributes, the executors, runtime support for generated code</li>
<li><a href="psync_macros/index.html">psync-macros</a>: the attribute entry points</li>
<li><a href="psync_graph/index.html">psync-graph</a>: the analysis and code generation behind <code>#[psync::graph]</code></li>
<li><a href="check/psync_check/index.html">psync-check</a>: the effect model, summaries, link step, facts</li>
<li><a href="check/psync_driver/index.html">psync-driver</a>: the rustc driver (MIR summaries and checks)</li>
<li><a href="check/cargo_psync/index.html">cargo-psync</a>: the cargo subcommand</li>
</ul>
HTML
echo "site built in book/book (book + api/)"

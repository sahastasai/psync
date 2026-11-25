# API reference (Rustdoc)

The Rustdoc of every crate is published next to this book, generated from the source on each
deploy. It includes the internals: private modules and functions are documented too, because
contributors read them.

| Crate | What it holds |
|---|---|
| [psync](/api/psync/index.html) | `#[psync::graph]` and the hints, the executors (`Executor`, `MultiExecutor`), `yield_now`, and runtime support for generated code |
| [psync-macros](/api/psync_macros/index.html) | the attribute entry points |
| [psync-graph](/api/psync_graph/index.html) | the analysis and code generation: classification, access paths, effects, facts, hoisting, scheduling, loops, exits |
| [psync-check](/api/check/psync_check/index.html) | the effect model, summary files, the link step, facts for the macro |
| [psync-driver](/api/check/psync_driver/index.html) | the rustc driver: MIR summaries and the checks |
| [cargo-psync](/api/check/cargo_psync/index.html) | `cargo psync check`, `build`, `fix`, `summaries` |
| [Lean proofs](/api/lean/index.html) (doc-gen4) | every theorem of `verify/`, each with its statement in mathematical notation: [Schedule](/api/lean/Schedule.html), [World](/api/lean/World.html), [Cert](/api/lean/Cert.html), [Footprint](/api/lean/Footprint.html), [Certs](/api/lean/Certs.html), [Tutorial](/api/lean/Tutorial.html) |

The [index page](/api/index.html) links them all. To build the site yourself:
`scripts/build-site.sh`. It builds the book, runs `cargo doc --document-private-items` for the
workspace and for `psync-check` (with its pinned nightly), and runs doc-gen4 on the Lean proofs
(`verify/docbuild`, which keeps `verify/` itself free of dependencies).

In the proofs, every theorem's documentation begins with its statement in LaTeX, so the HTML shows
the mathematics first and the Lean code second. New theorems follow the same convention.

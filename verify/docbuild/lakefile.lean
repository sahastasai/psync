import Lake
open Lake DSL

/-- Builds the HTML documentation of psync's proofs with doc-gen4 (theorem statements in LaTeX
render as math). Kept apart from `verify/`, which builds with no dependencies and no downloads.
Run: `lake update && lake build PsyncVerify:docs GeneratedCerts:docs`; output in `.lake/build/doc`. -/
package docbuild

require psync_verify from ".."
require «doc-gen4» from git "https://github.com/leanprover/doc-gen4" @ "v4.22.0"

# Summary

[Introduction](./introduction.md)
[Getting started](./getting-started.md)

# Guide

- [Basic concepts](./guide/concepts.md)
- [Loops](./guide/loops.md)
- [Early exits and errors](./guide/exits.md)
- [Hints](./guide/hints.md)
- [Reading the report](./guide/explain.md)
- [Comparing builds](./guide/comparing.md)
- [Patterns and pitfalls](./guide/patterns.md)
- [The executor](./guide/executor.md)

# How psync works

- [The pipeline](./internals/overview.md)
- [Classifying statements](./internals/classification.md)
- [Reads and writes from tokens](./internals/access.md)
- [Exposing hidden awaits](./internals/hoisting.md)
- [The graph and the schedule](./internals/schedule.md)
- [Generated code](./internals/codegen.md)
- [Loop rewriting](./internals/loops.md)
- [Runtime support](./internals/runtime.md)
- [What is checked, what is assumed](./internals/soundness.md)

# Safety

- [Side effects: files, network and processes](./safety/effects.md)
- [Checking rewrites: `cargo psync check`](./safety/check.md)
- [Roadmap: beyond tokens](./safety/roadmap.md)

# Case studies

- [Sixteen real-world shapes](./case-studies/graph-suite.md)
- [kube-rs and DataFusion](./case-studies/kube-datafusion.md)

# Reference

- [Attributes and hints](./reference/attributes.md)
- [Environment variables and tools](./reference/tools.md)
- [Glossary](./reference/glossary.md)

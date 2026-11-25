# Compilers from zero

The rest of this book uses words like *token*, *syntax tree*, *MIR*, *dataflow*, *effect
summary* and *class-hierarchy analysis*. This part teaches them from the beginning. It
assumes you can read and write ordinary Rust, including `async fn`. It does not assume you
have taken a compilers course or read rustc's source.

Each chapter introduces one idea, shows it on a small program you can run, and then shows
where psync uses it. Each chapter ends with exercises, and their answers are behind a fold. By the end of the part
you will be able to read every crate in this repository and know why each one is shaped the
way it is.

## What a compiler does to your code

A compiler is a chain of translations. Each step takes the program in one form and produces a
form that makes the next question easier to answer. Here is the chain rustc runs, and where
psync plugs in:

```text
 source text      "let user = api.user(id).await?;"
     │  lexing                                             ── Tokens
     ▼
 tokens           let  user  =  api  .  user  ( id )  .  await  ?  ;
     │  parsing                                            ── Syntax trees
     ▼
 AST              Local { pat: user, init: Try(Await(MethodCall(..))) }
     │  macro expansion   ◄── #[psync::graph] runs HERE:   ── Procedural macros
     │                        tokens in, tokens out
     │  name resolution
     ▼
 HIR              desugared: `?` is a match, `.await` is a loop    ── Inside rustc
     │  type checking
     ▼
 THIR             every expression has a type; method calls are resolved
     │  building MIR
     ▼
 MIR              a control-flow graph of simple statements
     │  borrow checking, optimization
     │                    ◄── psync-driver reads MIR HERE  ── Effects and summaries
     ▼
 LLVM IR → machine code
```

psync works at two points on this chain:

- **`#[psync::graph]`** is a procedural macro. It runs during macro expansion, early on, before
  rustc has resolved a single name. It sees *tokens*: the words and symbols you wrote, with no
  types and no idea what `api.user` refers to. That is why it judges a call by its name, and why it
  is conservative.
- **`psync-driver`** (`cargo psync check` and `build`) is rustc itself, with an extra pass that runs
  after type checking. It reads *MIR*, where every call is resolved to the exact function it
  calls. That lets it check the macro's guesses, and feed facts back to it.

The proofs in `verify/` sit off to the side. They are not part of compilation. They show that
the *idea* behind the schedule is right, whatever the program.

## The chapters

| Chapter | You will learn | psync code it explains |
|---|---|---|
| [Tokens](./tokens.md) | what a lexer produces; token trees, spans | what `#[psync::graph]` receives |
| [Syntax trees](./syntax-trees.md) | parsing; `syn`; visitors | `classify.rs`, `access.rs` |
| [Procedural macros](./macros.md) | rewriting a tree; `quote!`; hygiene; errors | `psync-macros`, `rewrite.rs`, `schedule.rs` |
| [Inside rustc: HIR, THIR, MIR](./rustc.md) | desugaring; types; control-flow graphs; how `async` and `?` really compile | what `psync-driver` reads |
| [Dependencies and dataflow](./dataflow.md) | read/write sets; dependence; graphs, chains, levels; fixed points | `schedule.rs`, `loops.rs` |
| [Effects and summaries](./effects.md) | side effects; interprocedural analysis; soundness and precision | `effects.rs`, `analysis.rs` |
| [Dynamic dispatch and class hierarchies](./dispatch.md) | traits, `dyn`, monomorphization; closed-world resolution | the link step, `facts.rs` |
| [Proofs](./proofs.md) | what a proof assistant checks; reading Lean | `verify/Schedule.lean` |

## psync's vocabulary, in brief

The chapters use a few words from psync itself. Each is defined in full in
[Basic concepts](../guide/concepts.md), and all of them are in the [glossary](../reference/glossary.md):

- A **node** is a statement psync may move. A **barrier** is a statement that stays where it is
  (a lock, a sleep, the block's final value). Barriers cut a block into **regions**, and each
  region is scheduled on its own.
- An **edge** `a -> b` means `b` must wait for `a`. A **chain** is a run of nodes that each wait
  only for the one before. A **level** is a set of chains that run at the same time in one
  **join**.
- **Hints** are attributes you put on statements when tokens cannot show what you know:
  `#[psync::seq]` (keep this in place), `#[psync::fresh]`, `#[psync::independent]` and
  `#[psync::speculate]` ([Hints](../guide/hints.md)).
- **Compiler facts** are what `cargo psync build` hands the macro: for each call site in an
  annotated function, the effects the compiler found in the callee's body, in a file the macro
  reads ([Scheduling with compiler facts](../safety/facts.md)).

## Running the examples

The examples live in `psync-graph/examples/`, so `cargo test -p psync-graph` builds them and
they cannot drift from the code. From the repository root:

```sh
cargo run -p psync-graph --example tokens        # Tokens
cargo run -p psync-graph --example syntax_tree   # Syntax trees
cargo run -p psync-graph --example mini_psync    # Procedural macros
cargo run -p psync-graph --example dataflow      # Dependencies and dataflow
cargo run -p psync-graph --example summaries     # Effects and summaries; Dynamic dispatch
```

The rustc chapter uses the nightly toolchain that `psync-check` pins, because only nightly
rustc prints its internal forms:

```sh
rustup toolchain install nightly-2026-11-20 --component rustc-dev
rustc +nightly-2026-11-20 --edition 2024 --crate-type lib -Zunpretty=mir sample.rs
```

The proofs chapter needs Lean 4.22 (`elan` installs it).

## Three ideas to carry through

Three ideas come back in every chapter.

1. **Each form answers some questions and not others.** Tokens cannot tell you a type. MIR has
   forgotten your variable names in places. A good tool works at the form where its question is
   easy, and psync works at two forms for that reason.
2. **When an analysis does not know, it should assume the worst.** An analysis of "what may
   this code do?" is *sound* if it never leaves out something that can happen, and *precise* if it
   never includes something that cannot. psync is sound almost everywhere: a file write with an
   unknown path may be any file, and a child process may touch anything. Hints and compiler facts
   win back the precision this costs. There are exactly two places where psync assumes the
   *best* instead, both deliberate and both reported:
   - **The macro, on an awaited call it cannot classify by name** (`api.user(id)`): assumed to
     only read, so it may overlap other work. The report lists every such call as
     `unclassified`, `strict` turns the assumption off, and `cargo psync check` checks it
     against the callee's body.
   - **The driver, on a callee it cannot see through** (a function pointer, a `dyn` trait of
     another crate): it marks the summary *opaque*. With facts, the macro then keeps its own
     name-based guess as well as the compiler's effects. The guess is no worse than having no
     facts, but it is not "assume it writes everything".
3. **Facts are checked where they can be.** The macro's guesses are checked by the driver, the
   generated code is checked by the type and borrow checkers, and the scheduling argument is
   checked by Lean. What is left unchecked is listed in
   [What is checked, what is assumed](../internals/soundness.md).

# Rust → Charon → the MIR fragment of `verify/Footprint.lean`

```sh
# Charon: https://github.com/AeneasVerif/charon (built here with `nix build .#charon`)
cd scripts/charon/drain && charon cargo --preset=aeneas        # writes drain.llbc
python3 ../llbc2lean.py drain.llbc callers > ../../../verify/CharonDrain.lean
cd ../../../verify && lake build GeneratedCerts
```

`llbc2lean.py` translates every function of the crate into the fragment's `Stmt`, computes the
summaries as the least fixed point (as `psync-driver` does), and writes a certificate that runs
the calls of the named function in one level. Lean then decides, without trusting the script,
that the summaries pass the link check, that the certificate covers the footprints and that
`check` accepts it, and `mir_schedule_sound` applies. A crate whose calls share a place, or a
summary that forgets a write, is rejected by Lean.

Supported: integer locals and fields, `&mut` references to places (resolved as the borrow checker
names them), arithmetic, `if` with early returns, calls of the crate's functions with reference
arguments, recursion. Not supported (the script stops with an error): by-value arguments to the
crate's functions, loops, `dyn`, function pointers, calls of other body-less functions, and
unwinding.

## The driver fixture's certificates: `fixture2lean.py`

`fixture/` is `psync-check`'s fixture compiled as written: `as-written/` stands in for psync, with a
`#[psync::graph]` that only removes psync's statement attributes, so Charon sees the program in
order. `fixture2lean.py` reuses `llbc2lean.py`'s translation and ties psync's own certificates
`c28`, `c29` and `c32` (`stash_both`, `stash_same`, `stash_tail`) to the compiled statements of
their nodes, found by source line (`psync-cert --index`):

```sh
cd scripts/charon/fixture && charon cargo --preset=aeneas --start-from fixture::app::stash_both \
    --start-from fixture::app::stash_same --start-from fixture::app::stash_tail
cd ../../..
psync-cert --facts verify/facts.tsv --expect-unsafe psync-check/tests/fixtures/ --index index.json FILES... > verify/Certs.lean
psync-cert --index tokens-index.json FILES... > certs-tokens.lean      # the certificates without facts
python3 scripts/charon/fixture2lean.py scripts/charon/fixture/fixture.llbc index.json \
    verify/Certs.lean certs-tokens.lean c28 c29 c32 > verify/CharonFixture.lean
```

(`FILES` as in `scripts/certify.sh`.) Lean then decides `hindB` for each certificate as written:
`c28_sound` and `c32_sound` are `cert_schedule_sound` for the compiled code. For `c29` the
certificate psync writes with the facts names `a.json` in both nodes, so `hindB` holds and `check`
refutes the schedule (`c29_rejected` in `Certs.lean`); the certificate from the tokens alone is
refuted by `hindB` (`c29_tokens_refuted`), since both compiled calls write `a.json`.

What it assumes, beyond `llbc2lean.py`: a string constant is a place of its own (one per text),
`fs::write(p, d)` writes the file `*p.0` and reads `*p` and `*d`, `AsRef::as_ref` is the identity on
the reference, `Result::and`, `Try::branch` and `from_residual` move values, and drops do nothing.
Built with Charon 0.1.330 (`cargo build --release` in Charon's `charon/`, toolchain from its
`rust-toolchain`; on NixOS, `LD_LIBRARY_PATH` needs the toolchain's `lib` and nix-ld's libraries).
If `Certs.lean` is regenerated with other numbering, regenerate `CharonFixture.lean` too: it names
the certificates.

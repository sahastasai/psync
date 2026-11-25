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

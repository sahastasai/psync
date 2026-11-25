# tauri-bundler `prepare_tools`, built with psync (Phase II audit)

```sh
git clone https://github.com/tauri-apps/tauri && cd tauri && git checkout 548a369
for m in seq par hinted; do              # edit the path of the psync dependency in patch.py first
  git checkout -q . && python3 ../patch.py . $m
  cargo test --release -p tauri-bundler --lib --no-run   # copy the test binary to ../bin/bundler-$m
done
cd .. && BIN=$PWD/bin nix-shell -p toxiproxy --run ./run.sh
```

`patch.py` adds a benchmark test that runs `prepare_tools` on an empty cache and prints the median
time and a digest of the files it wrote. `run.sh` serves files of the real assets' sizes from
`mirror.py`, behind toxiproxy, through `TAURI_BUNDLER_TOOLS_GITHUB_MIRROR`, checks that all three
builds write the same files, and prints the medians. Results are in the book, chapter "The audit:
where Phase II helps".

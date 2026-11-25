# rustic, built with psync (Phase II audit)

```sh
git clone https://github.com/rustic-rs/rustic && cd rustic && git checkout 143d073
cargo build --release && cp target/release/rustic ../bin/rustic-seq
python3 ../patch.py .            # edit the path of the psync dependency first
cargo build --release && cp target/release/rustic ../bin/rustic-psync
cd .. && nix-shell -p toxiproxy restic-rest-server bc --run ./run.sh
```

`run.sh` makes a 20-snapshot repository behind a REST server, adds latency with toxiproxy, checks
that both binaries print the same output, and prints median wall times. Results are in the book,
chapter "The audit: where Phase II helps".

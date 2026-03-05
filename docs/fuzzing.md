# Fuzzing

The `fuzz/` directory contains [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) targets for network-facing parsers.

## Prerequisites

```bash
cargo install cargo-fuzz
```

Requires a nightly toolchain (libFuzzer needs compiler instrumentation):

```bash
rustup install nightly
```

## Targets

| Target | Crate | What it tests |
|--------|-------|---------------|
| `fuzz_bencode` | torrent-bencode | Bencode deserialization + round-trip |
| `fuzz_peer_wire` | torrent-wire | Wire message, extension handshake, holepunch parsing |
| `fuzz_tracker` | torrent-tracker | Compact peer lists, UDP tracker responses |
| `fuzz_metadata` | torrent-core | .torrent parsing (v1/v2/hybrid), magnet URI parsing |

## Running

Run a single target:

```bash
cd fuzz
cargo +nightly fuzz run fuzz_bencode
```

Run with a time limit (60 seconds):

```bash
cargo +nightly fuzz run fuzz_bencode -- -max_total_time=60
```

Run all targets in sequence:

```bash
for target in fuzz_bencode fuzz_peer_wire fuzz_tracker fuzz_metadata; do
    echo "=== $target ==="
    cargo +nightly fuzz run "$target" -- -max_total_time=60
done
```

## Seed Corpus

Each target has seed inputs in `fuzz/corpus/<target>/`. These provide starting points for the fuzzer — valid inputs that it can mutate to find edge cases.

To add seeds from your own `.torrent` files:

```bash
cp /path/to/file.torrent fuzz/corpus/fuzz_metadata/
```

## Reproducing Crashes

Crashes are saved in `fuzz/artifacts/<target>/`. Reproduce with:

```bash
cargo +nightly fuzz run fuzz_bencode fuzz/artifacts/fuzz_bencode/crash-<hash>
```

## Coverage

Generate a coverage report:

```bash
cargo +nightly fuzz coverage fuzz_bencode
```

#!/usr/bin/env bash
# Allocator bake-off for the runtime's `buffer` tier and region chunk source (TODOs.md D3).
# Builds one wasm32 module per allocator, a size build that exports only `cabi_realloc` and a
# workload build, then times the workloads in Node. Run inside `nix develop`.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p out
for allocator in talc talc_noip dlmalloc rlsf rlsf_full lol; do
  for build in size workloads; do
    features=$allocator
    [ "$build" = workloads ] && features="$allocator,workloads"
    cargo build -q --release --target wasm32-unknown-unknown --no-default-features --features "$features"
    cp target/wasm32-unknown-unknown/release/perry_allocator_bench.wasm "out/$allocator-$build.wasm"
  done
done
node run.mjs

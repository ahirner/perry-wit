#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

mkdir -p dist
cargo run --bin perry-wit -- examples/merge_docs.ts \
  --world merge-docs -o dist/perry_merge_docs.wasm
cargo run --bin perry-wit -- examples/merge_task.ts \
  --world task-runner -o dist/perry_merge_task.wasm

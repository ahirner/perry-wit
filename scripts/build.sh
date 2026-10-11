#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."

mkdir -p dist
cargo run --bin perry-wit -- examples/merge_docs/index.ts \
  --wit examples/merge_docs --world merge-docs -o dist/perry_merge_docs.wasm
cargo run --bin perry-wit -- examples/merge_task/index.ts \
  --wit examples/merge_task --world merge-task -o dist/perry_merge_task.wasm

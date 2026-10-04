#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT_DIR"

# Re-enter through the pinned toolchain when called outside its development shell.
if [[ -z "${WASI_P3_WIT_PATH:-}" ]]; then
  exec nix develop -c bash "$SCRIPT_DIR/build.sh"
fi

mkdir -p dist
cargo run --release --bin perry-wit -- examples/merge_docs.ts \
  --world command -o dist/perry_merge_docs.wasm
cargo run --release --bin perry-wit -- examples/merge_task.ts \
  --world task-runner -o dist/perry_merge_task.wasm

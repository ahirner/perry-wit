#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist"

cd "$ROOT_DIR"

if [[ ! -e "$ROOT_DIR/wit/deps" && -z "${WASI_WIT_PATH:-}" ]]; then
  echo "==> Restoring WIT dependencies from wkg.lock via wkg..."
  if command -v wkg >/dev/null 2>&1; then
    wkg wit fetch --config "$ROOT_DIR/wkg-config.toml"
  else
    nix run nixpkgs#wkg -- wit fetch --config "$ROOT_DIR/wkg-config.toml"
  fi
fi

echo "==> [1/2] Compiling guest-runtime (wasm32-unknown-unknown with imported memory)..."
cargo rustc --release --package guest-runtime --target wasm32-unknown-unknown -- \
  -C link-arg=--import-memory \
  -C link-arg=--global-base=1048576 \
  -C link-arg=--no-entry
export GUEST_RUNTIME_PATH="$ROOT_DIR/target/wasm32-unknown-unknown/release/guest_runtime.wasm"

echo "==> [2/3] Building WASIp2 CLI component via perry-wit pure-Rust pipeline..."
mkdir -p "$DIST_DIR"
cargo run --release --bin perry-wit -- examples/merge_docs.ts -o "$DIST_DIR/perry_merge_docs.stripped.wasm"

STRIP_SZ=$(stat -f%z "$DIST_DIR/perry_merge_docs.stripped.wasm" 2>/dev/null || stat -c%s "$DIST_DIR/perry_merge_docs.stripped.wasm")
echo "==> CLI component complete: ${STRIP_SZ} bytes -> $DIST_DIR/perry_merge_docs.stripped.wasm"

echo "==> [3/3] Building WASIp2 task component (merge_task.ts -> task-runner world)..."
cargo run --release --bin perry-wit -- examples/merge_task.ts \
  --wit wit \
  --world task-runner \
  -o "$DIST_DIR/perry_merge_task.wasm"

TASK_SZ=$(stat -f%z "$DIST_DIR/perry_merge_task.wasm" 2>/dev/null || stat -c%s "$DIST_DIR/perry_merge_task.wasm")
echo "==> Task component complete: ${TASK_SZ} bytes -> $DIST_DIR/perry_merge_task.wasm"


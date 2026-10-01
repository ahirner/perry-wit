#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist"

cd "$ROOT_DIR"

if [[ ! -d "$ROOT_DIR/wit/deps" ]]; then
  echo "==> Restoring WIT dependencies from wkg.lock via wkg..."
  if command -v wkg >/dev/null 2>&1; then
    wkg wit fetch --config "$ROOT_DIR/wkg-config.toml"
  else
    nix run nixpkgs#wkg -- wit fetch --config "$ROOT_DIR/wkg-config.toml"
  fi
fi

echo "==> [1/3] Compiling TypeScript source using Perry sub-crates (perry-parser, perry-hir, perry-codegen-wasm)..."
mkdir -p "$DIST_DIR"
cargo run --release --bin perry-wit -- examples/merge_docs.ts -o dist/merge_docs.core.wasm

echo "==> [2/3] Building WASIp2 component (crates/guest-runtime targeting wasm32-wasip2)..."
cargo build --release -p guest-runtime --target wasm32-wasip2

if command -v wasm-tools >/dev/null 2>&1; then
  run_wasm_tools() {
    wasm-tools "$@"
  }
else
  run_wasm_tools() {
    nix run nixpkgs#wasm-tools -- "$@"
  }
fi

echo "==> [3/3] Post-processing and stripping WASIp2 component..."
SRC="$ROOT_DIR/target/wasm32-wasip2/release/guest_runtime.wasm"
RAW_OUT="$DIST_DIR/perry_merge_docs.wasm"
STRIPPED_OUT="$DIST_DIR/perry_merge_docs.stripped.wasm"

cp "$SRC" "$RAW_OUT"
run_wasm_tools strip "$RAW_OUT" -o "$STRIPPED_OUT"
run_wasm_tools validate --features cm-async "$STRIPPED_OUT"

RAW_SZ=$(stat -f%z "$RAW_OUT" 2>/dev/null || stat -c%s "$RAW_OUT")
STRIP_SZ=$(stat -f%z "$STRIPPED_OUT" 2>/dev/null || stat -c%s "$STRIPPED_OUT")
echo "    perry_merge_docs: raw = ${RAW_SZ} bytes (${RAW_OUT}), stripped = ${STRIP_SZ} bytes (${STRIPPED_OUT})"

echo "==> Validating WASIp2 component..."
run_wasm_tools validate --features cm-async "$DIST_DIR/perry_merge_docs.stripped.wasm"

echo "==> Build complete. Artifacts ready in $DIST_DIR"

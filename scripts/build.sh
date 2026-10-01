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

echo "==> [1/4] Compiling TypeScript source using Perry compiler..."
mkdir -p "$DIST_DIR"
cargo run --release --bin perry-wit -- examples/merge_docs.ts -o "$DIST_DIR/merge_docs.core.wasm"

echo "==> [2/4] Compiling guest-runtime (wasm32-unknown-unknown with imported memory)..."
cargo rustc --release --package guest-runtime --target wasm32-unknown-unknown -- \
  -C link-arg=--import-memory \
  -C link-arg=--global-base=1048576 \
  -C link-arg=--no-entry

if command -v wasm-merge >/dev/null 2>&1; then
  run_wasm_merge() {
    wasm-merge "$@"
  }
else
  run_wasm_merge() {
    nix shell nixpkgs#binaryen -c wasm-merge "$@"
  }
fi

if command -v wasm-tools >/dev/null 2>&1; then
  run_wasm_tools() {
    wasm-tools "$@"
  }
else
  run_wasm_tools() {
    nix run nixpkgs#wasm-tools -- "$@"
  }
fi

echo "==> [3/4] Merging TypeScript core wasm and runtime into a single module..."
GUEST_RT="$ROOT_DIR/target/wasm32-unknown-unknown/release/guest_runtime.wasm"
MERGED_CORE="$DIST_DIR/perry_merged.core.wasm"
EMBEDDED_CORE="$DIST_DIR/perry_embedded.core.wasm"
RAW_OUT="$DIST_DIR/perry_merge_docs.wasm"
STRIPPED_OUT="$DIST_DIR/perry_merge_docs.stripped.wasm"

run_wasm_merge --all-features -n "$DIST_DIR/merge_docs.core.wasm" env "$GUEST_RT" rt -o "$MERGED_CORE"

echo "==> [4/4] Embedding WIT and creating WASIp2 component..."
run_wasm_tools component embed "$ROOT_DIR/wit" --world merge-docs "$MERGED_CORE" -o "$EMBEDDED_CORE"
run_wasm_tools component new "$EMBEDDED_CORE" -o "$RAW_OUT"
run_wasm_tools strip "$RAW_OUT" -o "$STRIPPED_OUT"

# Clean up intermediate core modules
rm -f "$MERGED_CORE" "$EMBEDDED_CORE"

RAW_SZ=$(stat -f%z "$RAW_OUT" 2>/dev/null || stat -c%s "$RAW_OUT")
STRIP_SZ=$(stat -f%z "$STRIPPED_OUT" 2>/dev/null || stat -c%s "$STRIPPED_OUT")
echo "    perry_merge_docs: raw = ${RAW_SZ} bytes (${RAW_OUT}), stripped = ${STRIP_SZ} bytes (${STRIPPED_OUT})"

echo "==> Validating WASIp2 component..."
run_wasm_tools validate --features cm-async "$STRIPPED_OUT"

echo "==> Build complete. Artifacts ready in $DIST_DIR"

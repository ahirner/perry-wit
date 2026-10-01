#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist"
COMPONENT="$DIST_DIR/perry_merge_docs.stripped.wasm"

cd "$ROOT_DIR"

if [[ ! -f "$COMPONENT" ]]; then
  echo "==> Component not found at $COMPONENT. Running build first..."
  "$SCRIPT_DIR/build.sh"
fi

if command -v wasmtime >/dev/null 2>&1; then
  run_wasmtime() {
    wasmtime "$@"
  }
else
  run_wasmtime() {
    nix shell github:NixOS/nixpkgs/nixpkgs-unstable#wasmtime --command wasmtime "$@"
  }
fi

SERVER_PID=""
cleanup() {
  if [[ -n "$SERVER_PID" ]]; then
    echo "==> Stopping background mock HTTP server (PID: $SERVER_PID)..."
    kill "$SERVER_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

# Check if port 8080 is reachable
if ! nc -z 127.0.0.1 8080 2>/dev/null; then
  echo "==> Verifying component fails with error when server is not running..."
  if run_wasmtime run -S http=y -S inherit-network=y "$COMPONENT" 2>/dev/null; then
    echo "==> ERROR: Component unexpectedly succeeded without server running!"
    exit 1
  else
    echo "==> Verified: Component exits with error when server is down as expected."
  fi

  echo "==> Starting mock HTTP server on 127.0.0.1:8080..."
  cargo build --bin mock_server --quiet
  cargo run --bin mock_server &
  SERVER_PID=$!
  # Wait for server to become responsive
  for i in {1..50}; do
    if nc -z 127.0.0.1 8080 2>/dev/null; then
      break
    fi
    sleep 0.1
  done
else
  echo "==> Mock HTTP server is already running on 127.0.0.1:8080."
fi

echo "==> Executing WASIp2 component with wasmtime (with -S http=y -S inherit-network=y)..."
OUTPUT=$(run_wasmtime run -S http=y -S inherit-network=y "$COMPONENT")

echo "$OUTPUT"

echo "==> Verifying output assertions..."
if echo "$OUTPUT" | grep -q "=== MERGED DOCUMENT (SPLATTED) ===" && \
   echo "$OUTPUT" | grep -q '"category": "wasm-preview2"' && \
   echo "$OUTPUT" | grep -q '"author": "WebAssembly Community Group"' && \
   echo "$OUTPUT" | grep -q '"version": 2' && \
   echo "$OUTPUT" | grep -q '"draft": false'; then
  echo "==> SUCCESS: End-to-end WASIp2 component execution verified!"
else
  echo "==> ERROR: Output did not contain expected merged document content."
  exit 1
fi

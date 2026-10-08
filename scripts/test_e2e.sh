#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR/.."
if [[ -z "${WASI_WIT_PATH:-}" ]]; then
  exec nix develop -c bash "$SCRIPT_DIR/test_e2e.sh"
fi

# HTTP fixtures bind temporary local ports and own their server lifetimes.
cargo test --test production_test --test waffle_wit_native_test --test waffle_http_handler_test

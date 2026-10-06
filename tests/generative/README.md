# Generated Semantic Checks

The generative test suite synthesizes well-typed TypeScript programs, compiles them to WASI 0.3 WebAssembly components, and checks differential semantic equivalence against Node.js across numerical boundary inputs.

- **Differential & Metamorphic**: Compares WASI 0.3 component execution in Wasmtime directly against Node.js across semantic-preserving program variants.
- **Automated Reduction**: Reduces failing programs to minimal reproducing TypeScript cases.
- **Resource-Bounded**: Enforces configurable fuel and memory limits in isolated worker processes.
- **Deterministic & Replayable**: Seeded generation with saved artifacts for replay against the current compiler.

## Quick Start

Run the smoke campaign inside the pinned Nix environment:

```sh
nix develop -c cargo test --test generative_test -- --nocapture
```

The smoke run evaluates regression trees and 8 synthesized expression trees (each expanded into 4 metamorphic forms) across 24 numeric boundary inputs, verifying each with `tsc --strict` prior to execution.

## API Levels

`PERRY_GENERATIVE_API_LEVEL` selects the supported language subset and host capabilities:

- **Level 0**: Core arithmetic, comparisons, conditionals, functions, arrays, records, and string operations.
- **Level 1**: Math builtins (`floor`, `ceil`, `trunc`, `abs`, `round`) and string search/casing.
- **Level 2** (default): `Date.getTime`, `Uint8Array`, `TextEncoder`, and JSON parsing/serialization.
- **Level 3**: Async timers via `node:timers/promises.setTimeout`.
- **Level 4**: Async filesystem I/O (`readFile`, `writeFile`).
- **Level 5**: Metadata and directory queries (`stat`, `readdir`, UTF-8 decoding).
- **Level 6**: Byte subviews and error recovery paths.

## Custom Campaigns

Configure campaign parameters using environment variables:

```sh
nix develop -c env \
  PERRY_GENERATIVE_API_LEVEL=5 \
  PERRY_GENERATIVE_FUEL=10000000 \
  PERRY_GENERATIVE_SEED=100 \
  PERRY_GENERATIVE_COUNT=200 \
  PERRY_GENERATIVE_DEPTH=4 \
  cargo test --test generative_test generated_programs_match_node -- --nocapture
```

| Variable | Default | Description |
|:---|:---:|:---|
| `PERRY_GENERATIVE_API_LEVEL` | `2` | API feature level (0–6). |
| `PERRY_GENERATIVE_COUNT` | `8` | Number of root expression trees to generate. |
| `PERRY_GENERATIVE_DEPTH` | `3` | Maximum AST recursion depth (max 6). |
| `PERRY_GENERATIVE_SEED` | `0` | SplitMix64 PRNG seed. |
| `PERRY_GENERATIVE_FUEL` | `1000000` | Wasmtime fuel budget per input. |
| `PERRY_GENERATIVE_SHRINK` | `100` | Maximum reduction attempts on failure. |
| `PERRY_GENERATIVE_REPLAY` | _unset_ | Saved TypeScript file (`.ts` or `.json`) to replay. |

## Failure Reduction & Replay

The runner prints the retained directory under `target/generative/run-<id>/` and saves failure diagnostics and any reduced reproducer.

Replay a saved case against the current compiler:

```sh
nix develop -c env \
  PERRY_GENERATIVE_REPLAY=target/generative/run-EXAMPLE/minimal.ts \
  cargo test --test generative_test generated_programs_match_node -- --nocapture
```

Keep the complete run directory intact when copying a reproducer, as replay references the accompanying metadata and inputs.

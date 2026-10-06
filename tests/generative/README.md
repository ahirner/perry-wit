# Generated Semantic Checks

The generative test suite synthesizes well-typed TypeScript programs, compiles them to WASI 0.3 WebAssembly components, and checks differential semantic equivalence against Node.js across numerical boundary inputs.

- **Differential**: Compares WASI 0.3 component execution in Wasmtime directly against Node.js across semantic-preserving program variants.
- **Reduction**: Reduces failing programs to minimal reproducing TypeScript cases.
- **Budgets**: Enforces configurable fuel and memory limits in isolated worker processes.
- **Replay**: Deterministic seeded generation and regression reproduction from saved artifacts.

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

## Replay

Replay reproduces program execution deterministically against the current compiler. It accepts saved candidate programs, regression trees, or reduced artifacts from prior campaign runs.

This mechanism encompasses failures whenever a synthesized program deviates from expected behavior, specifically across scenarios such as:
- **Semantic Mismatch**: Divergence between WASI 0.3 execution results in Wasmtime and Node.js oracle output.
- **Compilation Diagnostics**: Unexpected rejection of valid TypeScript syntax or internal compiler lowering failures.
- **Component Validation**: Canonical ABI layout mismatches or rejected WebAssembly component structures.
- **Execution Traps**: Guest panics, unhandled runtime exceptions, or exceeded memory limits.
- **Fuel Exhaustion**: Exceeding the allocated Wasmtime instruction fuel budget before reaching completion.
- **Process Timeouts**: Hanging asynchronous suspensions or unobserved dangling promises at instance boundaries.

When any of these failures occur during a campaign, the runner isolates the fault in a worker process, logs the failure diagnostics to `target/generative/run-<id>/`, and shrinks the program into a minimal reproducing case (`minimal.ts` and `minimal.json`).

Replay the reduced failure directly against the compiler:

```sh
nix develop -c env \
  PERRY_GENERATIVE_REPLAY=target/generative/run-EXAMPLE/minimal.ts \
  cargo test --test generative_test generated_programs_match_node -- --nocapture
```

Keep the complete run directory intact when copying a reproducer, as replay references the accompanying metadata and inputs.

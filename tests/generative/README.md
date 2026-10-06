# Generative Semantic Checks

**Generative Semantic Checks** provides deterministic differential fuzzing and metamorphic testing to verify compiler correctness and runtime soundness in **Perry-WIT** against Node.js.

Rather than relying solely on hand-written unit tests, the generative suite synthesizes well-typed TypeScript programs across expanding language features and host APIs, verifies them with `tsc --strict`, compiles them to WASI 0.3 WebAssembly components, and validates bit-exact execution results and ordered side-effect traces against Node.js across numerical boundary inputs.

- **Differential & Metamorphic**: Compares native WASI 0.3 component execution in Wasmtime directly against Node.js across identical expressions and semantic-preserving program variants (such as introducing fresh locals, conditional assignments, or single-iteration loops).
- **Automated Reduction**: Shrinks failing test cases via bounded structural reduction (delta debugging) to produce minimal reproducing TypeScript programs while preserving the exact failure category.
- **Isolated & Resource-Bounded**: Executes guest components inside Wasmtime with configurable fuel and memory limits in isolated worker processes, safely containing compiler panics, aborts, timeouts, or runaway loops.
- **Deterministic & Replayable**: Employs SplitMix64 pseudo-random generation with comprehensive artifact logging in `target/generative/run-*`, allowing any generated test program or reduced failure case to be replayed instantly.

For compiler architecture, memory layouts, and runtime lowering details, see [ARCHITECTURE.md](../../ARCHITECTURE.md). For formal capability verification, see the root [README.md](../../README.md) and the [capability catalog](../../catalog/capabilities.json).

## Quick Start

Run the default smoke campaign inside the pinned Nix environment:

```sh
nix develop -c cargo test --test generative_test -- --nocapture
```

The smoke campaign:
- Replays saved regression trees from `tests/generative/regressions.json`.
- Synthesizes 8 random expression trees, each transformed into 4 equivalent metamorphic forms.
- Evaluates 24 boundary numeric inputs per component instance (including signed zero `-0.0`, `NaN`, infinities, subnormals, maximum finite values, rounding ties, and `Date` clipping boundaries).
- Validates every generated source with `tsc --strict` prior to compilation and execution.

## How It Works

The generative test pipeline ensures semantic equivalence between TypeScript running on Node.js and ahead-of-time compiled WASI 0.3 components:

1. **Generation**: The typed grammar ([`model.rs`](model.rs)) deterministically generates abstract syntax trees using a fixed SplitMix64 PRNG seed.
2. **Type Checking**: Generated TypeScript source files are strictly checked using `tsc --strict`. Any rejection by TypeScript fails the test.
3. **Differential Execution**:
   - **Node.js Oracle**: Executes the TypeScript source using an isolated runner script ([`oracle.mjs`](oracle.mjs)), recording returned values and ordered side-effect traces.
   - **WASI 0.3 Component**: Perry-WIT compiles the exact same source to a WebAssembly component, which is instantiated and run inside Wasmtime.
4. **Observation Matching**:
   - **Bit-Exact Floats**: Observations preserve every IEEE-754 bit except specific NaN payload bits (preserving signed zero `-0.0` vs. `+0.0` and `±Infinity`).
   - **Ordered Trace**: Side-effects (such as method calls, mutations, or logged trace marks) must execute in the exact same sequence.
   - **Resource Disposal**: Completed guest calls must leave zero outstanding concurrent tasks or uncollected host resources.

## API Levels

`PERRY_GENERATIVE_API_LEVEL` selects progressively broader deterministic APIs and async effects:

- **Level 0 (Core Syntax & Text)**: Arithmetic, comparisons, short-circuit booleans, ternary conditionals, function calls, dense array indexing, record field projections, and string concatenation/casing/slicing.
- **Level 1 (Math & String Operations)**: Adds `Math.floor`, `ceil`, `trunc`, `abs`, and `round`, along with string `indexOf`, `charAt`, and uppercase conversion.
- **Level 2 (Default — Dates, Bytes & JSON)**: Adds `Date.getTime`, `Uint8Array` construction and indexing, `TextEncoder` with byte subviews, and JSON `stringify`, `parse`, and field reads.
- **Level 3 (Timers & Asynchronous Control)**: Adds direct, stored, and repeated `await` forms of `node:timers/promises.setTimeout` (generates 6 metamorphic forms per tree).
- **Level 4 (Filesystem I/O)**: Adds asynchronous `node:fs/promises.readFile` and `writeFile` with read-back verification in isolated temporary directories (generates 8 forms per tree).
- **Level 5 (Retained Stats & File Queries)**: Adds `stat`, `readdir`, and binary `readFile` with UTF-8 decoding. Tests generic JSON parsing via temporary array storage (generates 10 forms per tree).
- **Level 6 (Byte Subviews & Error Recovery)**: Adds binary `writeFile` from byte subviews, detached read-backs, truncation via empty subviews, and repeated awaits on rejected reads followed by recovery (generates 12 forms per tree).

## Running Campaigns

### Custom Campaigns

Configure campaign parameters using environment variables:

```sh
# Run an extended campaign across advanced filesystem APIs
nix develop -c env \
  PERRY_GENERATIVE_API_LEVEL=5 \
  PERRY_GENERATIVE_FUEL=10000000 \
  PERRY_GENERATIVE_SEED=100 \
  PERRY_GENERATIVE_COUNT=200 \
  PERRY_GENERATIVE_DEPTH=4 \
  cargo test --test generative_test generated_programs_match_node -- --nocapture
```

### Configuration Reference

| Environment Variable | Default | Description |
|:---|:---:|:---|
| `PERRY_GENERATIVE_API_LEVEL` | `2` | API feature level (0–6). Higher levels enable timers and filesystem operations. |
| `PERRY_GENERATIVE_COUNT` | `8` | Number of root expression trees to generate before variant expansion. |
| `PERRY_GENERATIVE_DEPTH` | `3` | Maximum AST recursion depth (bounded at 6). |
| `PERRY_GENERATIVE_SEED` | `0` | SplitMix64 PRNG seed for deterministic generation. |
| `PERRY_GENERATIVE_FUEL` | `1000000` | Wasmtime fuel budget per input (1–100,000,000). Fuel exhaustion is tracked distinctly. |
| `PERRY_GENERATIVE_SHRINK` | `100` | Maximum structural reduction attempts on failure. |
| `PERRY_GENERATIVE_REPLAY` | _unset_ | File path to a saved `minimal.json` failure artifact to replay. |

## Failure Reduction & Replay

When a failure occurs (compilation error, fuel exhaustion, runtime trap, or output mismatch):

1. **Process Containment**: A dedicated worker process isolates compiler panics, aborts, and timeouts. The compiler executable is snapshotted before the campaign begins to prevent concurrent rebuild interference.
2. **Structural Reduction**: The runner applies typed AST transformations to shrink the failing program, validating that each reduced candidate still reproduces the exact same failure category.
3. **Artifact Logging**: Each campaign creates a directory in `target/generative/run-<id>/` containing:
   - Generated TypeScript sources (`case-*.ts`) and WIT definitions
   - Inputs, oracle logs, tool versions, and Git revisions
   - For failing runs: original tree, reduced tree (`minimal.json`), and failure diagnostics
4. **Deterministic Replay**: Replay any saved failure tree against the current compiler:

```sh
# Replay a minimal failing case
nix develop -c env \
  PERRY_GENERATIVE_REPLAY=target/generative/run-EXAMPLE/minimal.json \
  cargo test --test generative_test generated_programs_match_node -- --nocapture
```

## Sustained Campaigns & Metrics

To tally results across sustained fuzzing runs, create a goal manifest JSON file beside the `target/generative/run-*` directories:

`startedUnix` uses Unix seconds, matching campaign reports. This example starts at 2026-10-06 00:00 UTC.

```json
{
  "startedUnix": 1791244800,
  "continueUntilLocal": "2026-10-06T20:00:00"
}
```

Then run the tally script:

```sh
nix develop -c node scripts/tally_generative.mjs target/generative/goal-manifest.json
```

The tally report:
- Includes only completed matching programs and total input executions.
- Deduplicates distinct TypeScript source programs using SHA-256 hashes.
- Excludes diagnostic replay runs.
- Accurately tracks in-progress runs by counting only verified prefixes.

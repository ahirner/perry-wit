# Generated semantic checks

Run the default smoke campaign inside the pinned toolchain:

```sh
nix develop -c cargo test --test generative_test -- --nocapture
```

The campaign runs the saved regression trees plus eight generated expression trees, each in four equivalent forms, and runs 24 numeric inputs per component instance, including signed zero, NaN, infinities, subnormals, maximum finite values, rounding ties, and the Date clipping boundary.
`tsc --strict` checks every generated source before execution.
Node and the production resolved-WIT compiler receive the same TypeScript source.
Wasmtime validates and executes the resulting components with fuel and memory limits.
A separate process contains compiler panics, aborts, and timeouts. Each campaign snapshots its worker executable so a concurrent rebuild cannot change the compiler halfway through a run.
The development profile optimizes the upstream `perry-hir` dependency: its unoptimized frontend overflowed the normal Rust test-thread stack on a saved nested Date/byte/JSON case. The regression runs at the normal stack limit.
Missing tools, oracle errors, and rejected generated programs fail the test.

The grammar covers arithmetic, comparisons, short-circuit booleans, conditionals, function calls, dense arrays, record fields, string concatenation/casing/slicing, and ordered side effects.
`PERRY_GENERATIVE_API_LEVEL` selects progressively broader deterministic APIs:

- `0`: the original language and text operations.
- `1`: also `Math.floor/ceil/trunc/abs/round`, string `indexOf`, `charAt`, and uppercase conversion.
- `2` (default): also `Date.getTime`, `Uint8Array` conversion/indexing, `TextEncoder` plus byte subviews, and JSON serialization/parse/field reads.
- `3`: all level-two expressions plus direct and stored/repeated awaits of `node:timers/promises.setTimeout`, for six forms per tree.
- `4`: also pending `node:fs/promises.readFile` and `writeFile`/read-back forms, for eight forms per tree.

Every added expression participates in typed structural reduction.
Timer variants run against the real Node timer API and the production WASI P3 clock host; only values and ordered source effects are compared, not wall-clock timing.
Filesystem variants use separate fresh temporary directories for Node and the guest, initialized with the same UTF-8/NUL fixture, so one implementation cannot supply the other's output.
Each completed guest call must leave no outstanding concurrent tasks or host resources.
Metamorphic variants introduce a fresh local, conditional assignment, or a one-iteration loop.
Each variant must preserve Node's original observations and match the component's result and side-effect trace.
Numeric observations preserve every IEEE-754 bit except NaN payloads, including signed zero and infinities.
Text literals stay within the BMP because Perry-WIT deliberately counts Unicode scalars while Node counts UTF-16 code units.
Unsupported operators and methods are outside this grammar; generated tests do not add capability claims to the conformance catalog.

For a larger reproducible campaign:

```sh
nix develop -c env PERRY_GENERATIVE_SEED=100 PERRY_GENERATIVE_COUNT=200 PERRY_GENERATIVE_DEPTH=4 cargo test --test generative_test generated_programs_match_node -- --nocapture
```

Seeds use a fixed SplitMix64 mapping.
Count is the number of generated trees, before the four, six, or eight variants selected by the API level; the saved regression trees always run too, and depth is bounded at six.
The default shrink budget is 100 attempts, configurable with `PERRY_GENERATIVE_SHRINK`.
Each run prints its retained directory under `target/generative/`, containing sources, WIT, oracle, inputs, tool versions, grammar version/API level, Git revision and source patch, start time, elapsed time, and progress.
A semantic failure also saves the original and reduced trees and TypeScript, plus failure details.
Reduction preserves the failure category and checks the final reduced program again.
Replay a saved tree against the current compiler:

```sh
nix develop -c env PERRY_GENERATIVE_REPLAY=target/generative/run-EXAMPLE/minimal.json cargo test --test generative_test generated_programs_match_node -- --nocapture
```

This is deterministic generative differential testing, not coverage-guided fuzzing.
The design follows `../nix-wit/tests/equivalence`: typed generation, metamorphic observations, bounded structural reduction, and saved replay artifacts.
[Fuzzilli](https://github.com/googleprojectzero/fuzzilli) provides a related model of separating valid-program generation, execution, and minimization.

# Generated semantic checks

Run the default smoke campaign inside the pinned toolchain:

```sh
nix develop -c cargo test --test generative_test -- --nocapture
```

The campaign generates eight expression trees, each in four equivalent forms, and runs nine numeric inputs per component instance.
`tsc --strict` checks every generated source before execution.
Node and the production resolved-WIT compiler receive the same TypeScript source.
Wasmtime validates and executes the resulting components with fuel and memory limits.
A separate process contains compiler panics, aborts, and timeouts.
Missing tools, oracle errors, and rejected generated programs fail the test.

The grammar covers arithmetic, comparisons, short-circuit booleans, conditionals, function calls, dense arrays, record fields, string concatenation/casing/slicing, and ordered side effects.
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
Count is the number of original trees, before the four variants; depth is bounded at six.
The default shrink budget is 100 attempts, configurable with `PERRY_GENERATIVE_SHRINK`.
Each run prints its retained directory under `target/generative/`, containing sources, WIT, oracle, inputs, tool versions, and progress.
A semantic failure also saves the original and reduced trees and TypeScript, plus failure details.
Reduction preserves the failure category and checks the final reduced program again.
Replay a saved tree with the same test binary path used by the campaign:

```sh
nix develop -c env PERRY_GENERATIVE_REPLAY=target/generative/run-EXAMPLE/minimal.json cargo test --test generative_test generated_programs_match_node -- --nocapture
```

This is deterministic generative differential testing, not coverage-guided fuzzing.
The design follows `../nix-wit/tests/equivalence`: typed generation, metamorphic observations, bounded structural reduction, and saved replay artifacts.
[Fuzzilli](https://github.com/googleprojectzero/fuzzilli) provides a related model of separating valid-program generation, execution, and minimization.

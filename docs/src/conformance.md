# Conformance

`perry-conformance` tests Perry-WIT differentially against Node.js.
A contract names one capability, such as `ecma.string.operations`, and supplies directed cases plus a property-based generator (`proptest`) of TypeScript programs and inputs.
Executable cases and their metamorphic variants run as compiled components in a Wasmtime host with WASI 0.3, fuel metering and memory limits, and under Node.js, except aggregate cases with exact integers beyond 2^53, which use an expected-value model.
The runner checks that each variant matches its oracle and that transformations preserve return values, evaluation traces and standard streams. Rejection cases and their variants must report the required diagnostic.

## Expected Outcomes

Each case declares what must happen:

| Outcome | Meaning |
| :--- | :--- |
| `EquivalentExecution` | The component produces the same observable result as Node.js. |
| `CompilerRejection` | The compiler rejects the program with a diagnostic, for constructs outside the subset. |
| `InjectedFaultDetection` | The harness detects a deliberately broken export adapter: a wrong value, a dropped effect or a leaked host resource. These cases test the harness itself. |
| `BoundedGrowth` | Fuel for an input of size 4n is at most 6 times the fuel for size n. This admits O(n log n) work and rejects quadratic growth (16 times). |

## Expected-Pass List

`passing.txt` in `perry-conformance` lists the contracts that must pass.
A listed contract that fails is a regression, and an unlisted contract that passes fails the run until it is added, so the list only grows.
`deferred.txt` records contracts outside the supported domain with their exclusion rationale.

```sh
cargo run -p perry-conformance -- check
cargo run -p perry-conformance -- check --select node.fs
cargo run -p perry-conformance -- list
cargo run -p perry-conformance -- doc
```

`--select` filters by contract ID prefix. `doc` prints the catalog table below.

## Catalog

{{#include conformance-catalog.md}}

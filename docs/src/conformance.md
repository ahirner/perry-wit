# Conformance

`perry-conformance` tests Perry-WIT differentially against Node.js.
A contract names one capability, such as `ecma.string.operations`, and supplies directed cases plus a property-based generator (`proptest`) of TypeScript programs and inputs.
Executable cases and each of their metamorphic variants run independently as compiled components in a Wasmtime host with WASI 0.3, fuel metering and memory limits. Each variant also runs under Node.js, except exact-integer aggregate cases beyond 2^53, which use an explicit expected-value model.
The runner checks both that each component agrees with its own oracle and that transformations preserve return values, evaluation traces and standard streams. Rejection cases must retain the required diagnostic across transformations.
Numeric programs use branch, loop and exception forms; aggregate and collection cases also reorder declarations that affect specialization or iterator state. Array inference uses seven forms: annotated storage, alias chains, branches, loops, extracted values, helper-return chains and indexed appends. Cases use at most seven writes and four aliases, with three calls per form. Conflicting writes must remain rejected when moved through temporaries or control flow.

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

# Conformance

`perry-conformance` tests Perry-WIT differentially against Node.js.
A contract names one capability, such as `ecma.string.operations`, and supplies directed cases plus a property-based generator (`proptest`) of TypeScript programs and inputs.
Every case runs twice: under Node.js, and as a compiled component in a Wasmtime host with WASI 0.3, fuel metering and memory limits.
The runner compares return values, standard streams and exceptions across each seed case and its metamorphic variants (such as reordered independent declarations).

## Expected Outcomes

Each case declares what must happen:

| Outcome | Meaning |
| :--- | :--- |
| `EquivalentExecution` | The component produces the same observable result as Node.js. |
| `CompilerRejection` | The compiler rejects the program with a diagnostic, for constructs outside the subset. |
| `InjectedFaultDetection` | The harness detects a deliberately broken export adapter: a wrong value, a dropped effect or a leaked host resource. These cases test the harness itself. |
| `BoundedGrowth` | Fuel for an input of size 4n is at most 6 times the fuel for size n. This admits O(n log n) work and rejects quadratic growth (16 times). |

## Expected-Pass List

`crates/conformance/passing.txt` lists the contracts that must pass.
A listed contract that fails is a regression, and an unlisted contract that passes fails the run until it is added, so the list only grows.
`crates/conformance/deferred.txt` lists contracts outside the supported domain, each with its reason; they are not run.

```sh
cargo run -p perry-conformance -- check
cargo run -p perry-conformance -- check --select node.fs
cargo run -p perry-conformance -- list
cargo run -p perry-conformance -- doc
```

`--select` filters by contract ID prefix; `doc` prints the catalog table below.

## Catalog

{{#include conformance-catalog.md}}

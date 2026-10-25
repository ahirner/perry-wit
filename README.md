# perry-wit

**Perry-WIT** compiles a static subset of TypeScript ahead-of-time into WebAssembly components targeting **WASI 0.3 (Preview 3)**.

Rather than embedding a JavaScript engine or in-Wasm interpreter (such as QuickJS or SpiderMonkey), Perry-WIT lowers TypeScript through
[Perry](https://www.perryts.com) HIR and typed SSA MIR built on [pliron](https://github.com/pliron-org/pliron) directly to native WebAssembly instructions, linking only the `#![no_std]` runtime and standard-library functions a program reaches.
An integer `add` export compiles to 100 bytes, and a string `trim` export stays under 6 KiB.

- **Dual-mode authoring**: Write CLI commands with top-level `await`, export typed functions or HTTP handlers, or do both in one module, whose top-level code then initializes state once before any export runs.
- **Node-compatible**: Modules without WIT imports also run under Node.js, and property-based differential contracts (`perry-conformance`) compare their behavior in Node.js and Wasmtime.
- **WIT-refined numbers**: Specialize `u8`–`s64` parameters and in-range arithmetic to native `i32` and `i64` instructions via forward range and backward bitwidth analysis while preserving IEEE 754 `f64` semantics for unconstrained `number` values.
- **Tiered memory reclamation**: Persist module-level state across calls while reclaiming temporaries through SROA, bounded activation frames, and per-call task regions or precise safe-point collection.
- **Callback-driven concurrency**: Lower `async` functions to stackless state machines so `await`, streams, and Promise combinators suspend directly on WASI 0.3 callbacks.
- **Toolchain-free embedding**: Compile from in-memory buffers via `perry_wit::api` or the standalone WASI 0.2 compiler component (`perry-wit.wasm`) without an external Rust or LLVM toolchain.

[Homepage](https://ahirner.github.io/perry-wit/) · [Handbook](https://ahirner.github.io/perry-wit/docs/) · [Playground](https://ahirner.github.io/perry-wit/play/) · [Rust API](https://ahirner.github.io/perry-wit/rust/perry_wit/api/index.html) · [Architecture](ARCHITECTURE.md)

## Quickstart

A WIT world declares the exports. `kebab-case` WIT names map to `camelCase` TypeScript functions:

```wit check=quickstart
package example:filter@0.1.0;

world auth-filter {
  export verify-token: func(header: string) -> result<string, string>;
}
```

```ts check=quickstart
export function verifyToken(header: string): string {
  if (!header.startsWith("Bearer ")) {
    throw new Error("missing bearer prefix");
  }
  return header.slice(7).trim();
}
```

```sh
perry-wit gen-types --wit wit --world auth-filter --entry src/index.ts -o .perry/types
perry-wit src/index.ts --wit wit --world auth-filter -o dist/auth.wasm
wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'verify-token("Bearer tok_live_98a")' dist/auth.wasm
```

The handbook's Authoring chapter covers commands, HTTP handlers and the project template.

## Development

Enter the pinned development shell with `direnv allow` or `nix develop`.
It provides Rust, Node.js, TypeScript, Wasmtime and `wasm-tools`, sets `WASI_WIT_PATH` to the WASI 0.3 packages, and shares runtime builds through `PERRY_RUNTIME_TARGET_DIR`.

```sh
cargo fetch --manifest-path runtime/Cargo.toml --locked    # once: runtime builds run offline
cargo test --workspace                                     # compiler tests; edited runtime modules rebuild
cargo test -p perry-mir                                    # one compiler crate, without runtime modules or Wasmtime
cargo test --test integration stream                       # integration tests matching a name
cargo test --manifest-path runtime/Cargo.toml --workspace  # runtime unit tests
cargo run -p perry-conformance -- check --select node.fs   # conformance contracts with an ID prefix
cargo test --test benchmark -- --ignored                   # sizes, fuel and memory against tests/benchmark.json
nix flake check   # rustfmt (both workspaces), WASI WIT, CLI, Wasm compiler, site, SDK sync, template and example runs
```

Benchmark sizes exclude custom sections such as debug names. Initialization fuel includes explicit data copying, with Wasmtime's platform-specific copy-on-write initialization disabled. When a change moves benchmark numbers on purpose, run the benchmark with `PERRY_EXPECT_BENCH=1` and commit the rewritten `tests/benchmark.json` together with the change.
CI additionally runs the collectible runtime tests (`--all-features`), Clippy for both workspaces and all conformance contracts.

### Flake outputs

```sh
nix build .#perry-wit           # compiler CLI
nix build .#site                # homepage, handbook and playground
nix build .#template-component  # starter template component
nix build .#example-merge-task  # reference component
```

### Upstream benchmarks

The benchmark app reads Perry's original TypeScript entry files from the same Git revision as `perry-hir` in `Cargo.lock`. It compiles each entry as a WASI command and compares it with Node, using Perry's timing contracts and stdout verifier. No sibling checkout or separate Perry flake input is required.

```sh
nix run .#perry-bench -- --list
nix run .#perry-bench -- bench_fibonacci suite/05_fibonacci --runs 3
nix run .#perry-bench -- --all
```

Every `.ts` file under upstream `benchmarks/` gets a report row, including specialized directories, helper fixtures, compilation failures, crashes and timeouts. Node and Wasmtime are attempted independently. Entries without upstream timing/output contracts use process wall time and remain unchecked. Unchecked outputs and zero timings have no speedup claims.

`summary.md`, `results.json`, logs and components go under `artifacts/perry-benchmarks/` (`--out` selects a new directory). Each engine gets three fresh processes, alternating order and stopping after its first failure. Node runs the original TypeScript, preserving relative imports; process wall times include type stripping. Internal timings preserve upstream warmups. Wasmtime's compilation cache is disabled. Peak RSS measures the whole process, including runtime/JIT memory. `--timeout` bounds each subprocess to 60 seconds. Fuel is omitted because the Wasmtime CLI does not report consumed fuel.

To benchmark an already-built compiler inside `nix develop`:

```sh
python3 scripts/perry-bench.py suite/05_fibonacci --compiler ./target/debug/perry-wit
```

The development shell supplies the locked source. `--source` selects another checkout; `--compile-only` probes compilation. CI runs the full catalog after Linux Nix builds for non-draft PRs, including `ready_for_review`. This reporting step is non-blocking: failures remain in the job-summary table and linked `perry-bench` artifact. Local untracked sources require `nix run path:.#perry-bench` or `nix develop path:.`.

## Contributing

Human and AI contributors are welcome: “Act always so as to increase the number of choices” — [Heinz von Foerster](https://de.wikipedia.org/wiki/Heinz_von_Foerster).

Register every new capability contract, input domain and boundary diagnostic in `perry-conformance`.

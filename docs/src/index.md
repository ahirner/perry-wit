# Overview

Perry-WIT compiles a static subset of TypeScript ahead of time into WebAssembly components for WASI 0.3.
The WIT world is the contract: it fixes the exported functions, the host capabilities a program may import, and the integer types of its numbers.

- **Direct WebAssembly codegen.** TypeScript lowers through typed SSA to native WebAssembly instructions. A component exporting a string function is about 3 KB.
- **Memory stays flat across calls.** Each call's temporaries are freed when it returns. The test suite runs 1,000 calls with a nested record argument within two 64 KiB pages.
- **WIT integer types refine `number`.** A `u8` parameter is a `number` known to be an integer in `0..=255`. In-range arithmetic compiles to `i32` or `i64` instructions.
- **Callback-driven WASI 0.3 async.** `await` suspends through WASI 0.3 callbacks, and the host resumes the waiting task directly.
- **Differential conformance against Node.js.** Each conformance contract runs the same program under Node.js and Wasmtime and compares observable results.

## Where to Start

- To write and build a component, read [Authoring](authoring.md).
- To compile TypeScript inside your own Rust host or a browser, read [Embedding](embedding.md).
- To check language rules and WIT integer types, read [Semantics](semantics.md).
- To look up supported globals, methods and modules, read [Standard Library](stdlib.md).
- To see how far Node.js equivalence is tested, read [Conformance](conformance.md).
- To change the compiler, read [Architecture](architecture.md).

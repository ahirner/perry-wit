# Authoring

Perry-WIT components can be authored as typed library components exporting custom WIT interfaces, standalone WASI 0.3 CLI commands, or stateless HTTP service handlers.

## Library Components

When targeting an application WIT world, the WIT file defines the component boundary: imported host capabilities, exported function names, parameter types, return types, and `async` suspension effects.

Kebab-case WIT export names map to `camelCase` TypeScript function exports. For example, given `wit/task.wit`:

```wit
package example:pipeline@0.1.0;

world task-runner {
  export run-task: async func(input: string) -> result<string, string>;
}
```

The corresponding TypeScript entry point exports `runTask`:

```ts
export async function runTask(input: string): Promise<string> {
  const normalized = input.trim();
  if (normalized.length === 0) {
    throw new Error("empty task payload");
  }
  return "processed:" + normalized;
}
```

Compile and invoke the component under Wasmtime P3:

```sh
perry-wit src/index.ts --wit wit --world task-runner -o dist/task.wasm
wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'run-task("hello")' dist/task.wasm
```

- **Export Mapping**: WIT `kebab-case` names map to TypeScript `camelCase` exports. Helper functions that are not exported in WIT remain private to the guest module.
- **Asynchronous Exports**: Exporting an `async func` in WIT binds to an `async function` returning `Promise<T>`, lowering to the stackless WASI 0.3 callback ABI.
- **Type Marshalling**: Records, dense arrays, `option<T>`, and discriminated variants cross component boundaries directly. 64-bit integers (`u64`, `s64`) map to `bigint`. Outbound `list<u8>` parameters accept `Uint8Array` views or UTF-8 strings.
- **Result Marshalling**: WIT `result<T, E>` exports accept direct return values (where thrown exceptions map to `err(E)`), or explicit `{ ok: true, value }` / `{ ok: false, error }` discriminated objects.
- **Core Module Output**: Passing `--core-only` emits the linked core WebAssembly module prior to Component Model wrapping.

## Command Components

A TypeScript file containing top-level statements compiles directly against the standard `wasi:cli/command@0.3.0` world (exporting `wasi:cli/run@0.3.0`). The compiler synthesizes the command runner export around top-level statements, including top-level `await`:

```ts
import { readFile } from "node:fs/promises";

const payload = await readFile("input.json", "utf8");
console.log("loaded bytes:", payload.length);
```

Compile and execute the CLI command:

```sh
perry-wit cli.ts --world command -o cli.wasm
wasmtime run -S p3=y -W component-model-async=y --dir . cli.wasm
```

- **Standard Streams**: `console.log` and `console.error` write line-by-line through asynchronous WASI 0.3 output streams.
- **Process Lifecycle**: Reading `process.env` and `process.argv` queries `wasi:cli/environment@0.3.0`. Setting `process.exitCode` or calling `process.exit(code)` controls the command exit status.
- **Uncaught Exceptions**: Unhandled rejections or exceptions format diagnostic messages to `stderr` and exit with a non-zero status code.

## HTTP Handlers

Components targeting `wasi:http/service@0.3.0` export `wasi:http/handler@0.3.0`, receiving incoming requests and returning structured HTTP responses:

```ts
const encoder = new TextEncoder();

export function handle(request: {
  method: string;
  pathWithQuery: string;
}): {
  status: number;
  headers: [string, Uint8Array][];
  body: Uint8Array;
} {
  const body = encoder.encode(`handled ${request.method} ${request.pathWithQuery}\n`);
  return {
    status: 200,
    headers: [["content-type", encoder.encode("text/plain; charset=utf-8")]],
    body,
  };
}
```

Generate TypeScript declarations for the HTTP world, compile, and serve via Wasmtime:

```sh
perry-wit gen-types --wit wit/http-handler --world handler -o handler-types --no-tsconfig
perry-wit handler.ts --wit wit/http-handler --world handler -o handler.wasm
wasmtime serve -S p3=y -W component-model-async=y handler.wasm
```

- **Per-Request Task Isolation**: Each inbound HTTP request executes inside an isolated guest task.
- **Fault Containment**: An uncaught exception during a request logs the diagnostic and returns an HTTP `500 Internal Server Error` response, keeping the component instance healthy for subsequent requests.
- **Streaming Bodies**: Request and response payloads transfer through native WASI 0.3 byte streams without intermediate UTF-16 transcoding.

## Type Generation

`perry-wit gen-types` inspects a WIT directory and target world to emit ambient TypeScript `.d.ts` declarations for all imported interfaces and exported entry-point contracts:

```sh
perry-wit gen-types --wit wit --world task-runner --entry src/index.ts -o .perry/types
tsc --noEmit -p .perry/types
```

Running `tsc --noEmit` against the generated `tsconfig.json` verifies that your TypeScript module exports match the exact parameter types, return types, and `Promise` signatures declared in WIT before invoking the WebAssembly compiler.

## Project Scaffolding

To initialize a standalone component repository outside the compiler workspace, use the bundled Nix flake template:

```sh
mkdir my-component && cd my-component
nix flake init -t github:ahirner/perry-wit
```

The template configures a reproducible development shell (`nix develop .#sdk`) that automatically regenerates `.perry/types` from `wit/` whenever your WIT contracts change, plus a `nix build` target that compiles and validates the final `.wasm` component.

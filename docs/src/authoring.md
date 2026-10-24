# Authoring

A component is a TypeScript entry module plus a WIT world.
Perry-WIT builds typed components that export WIT functions, CLI commands that run top-level code, and HTTP handlers.

## Typed Components

The WIT world declares the exports.
Each `kebab-case` WIT export binds to the matching `camelCase` TypeScript export; remaining module functions stay internal.
The starter template contains this pair:

```wit check=task
{{#include ../../template/wit/world.wit}}
```

```ts check=task
{{#include ../../template/src/index.ts}}
```

```sh
perry-wit src/index.ts --wit wit --world task -o dist/task.wasm
wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'run-task("hello")' dist/task.wasm
```

- An `async func` in WIT binds to an `async function` returning a `Promise`.
- A `result<T, E>` export returns `T` directly, and a thrown exception becomes `err`. Returning `{ ok: true, value }` or `{ ok: false, error }` also works.
- `--core-only` emits the standalone core WebAssembly module. `--keep-names` preserves function names in the custom `name` section for guest profiling.
- WASI 0.3 packages resolve from `WASI_WIT_PATH`. Packages in `wit/deps` take precedence over them.

## Generated Types

`perry-wit gen-types` writes `.d.ts` declarations for a world's imports and exports, plus a `tsconfig.json` that checks the entry module against them:

```sh
perry-wit gen-types --wit wit --world task --entry src/index.ts -o .perry/types
tsc --noEmit -p .perry/types
```

WIT integer types appear as `number` and `bigint`. See [Semantics](semantics.md) for how the compiler uses them.

## Commands

A world that includes `wasi:cli/command@0.3.0` turns the module's top-level statements, including top-level `await`, into the `wasi:cli/run` export:

```wit check=command
package example:cli@0.1.0;

world app {
  include wasi:cli/command@0.3.0;
}
```

```ts check=command
import { readFile } from "node:fs/promises";

const text = await readFile("input.json", "utf8");
const items: number[] = JSON.parse(text);
console.log("sum:", items.reduce((a, b) => a + b, 0));
if (items.length === 0) process.exitCode = 1;
```

```sh
perry-wit src/main.ts --wit wit --world app -o app.wasm
wasmtime run -S p3=y -W component-model-async=y --dir . app.wasm
```

`console` writes to WASI standard output and error, `process.env` reads the WASI environment, and `process.exitCode` or `process.exit(code)` sets the exit status.

If the module also exports functions, its top-level statements initialize the module once instead, and `wasi:cli/run` and every export wait for that initialization.
Top-level statements that start asynchronous work without awaiting it are then a compile error.

## HTTP Handlers

A module implementing the `perry:http-handler` world receives a request record and returns a response record.
The compiler wraps it in an adapter, so the component exports `wasi:http/handler@0.3.0`.
The world ships in `wit/http-handler`:

```wit check=handler
{{#include ../../wit/http-handler/world.wit}}
```

```ts check=handler
import type { Request, Response } from "./handler-types/world";

const encoder = new TextEncoder();

export function handle(request: Request): Response {
  const path = request.pathWithQuery ?? "/";
  return {
    status: 200,
    headers: [["content-type", encoder.encode("text/plain")]],
    body: encoder.encode(`${request.method.tag} ${path}\n`),
  };
}
```

```sh
perry-wit gen-types --wit wit/http-handler --world handler -o handler-types --no-tsconfig
perry-wit handler.ts --wit wit/http-handler --world handler -o handler.wasm
wasmtime serve -S p3=y -W component-model-async=y handler.wasm
```

The adapter reads the whole request body before calling `handle`.
An uncaught exception becomes a `500` response, and the instance keeps serving later requests.

## New Projects

The flake template creates a standalone project. Its development shell generates `.perry/types` on entry, and `nix build` type-checks and compiles the component:

```sh
mkdir my-component && cd my-component
nix flake init -t github:ahirner/perry-wit
nix develop
nix build
```

After editing `wit/*.wit` inside the shell, run `perry-wit gen-types --no-tsconfig --wit wit --entry src/index.ts` again before `tsc --noEmit -p .perry/types`.

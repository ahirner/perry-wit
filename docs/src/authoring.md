# Authoring

A component is built from a TypeScript entry module, with its WIT contract either inferred from exported signatures or read from `wit/`.

## Typed Components

Named exported functions (and local `export { ... } from` re-exports) form the component interface; remaining module functions stay internal. The starter template contains:

```ts check=task
{{#include ../../template/src/index.ts}}
```

```sh
perry-wit src/index.ts -o dist/task.wasm
wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'run-task("hello")' dist/task.wasm
```

TypeScript `camelCase` export names map to `kebab-case` WIT functions. Exported parameters require type annotations; return types can be annotated or inferred for scalar expressions:

| TypeScript | Generated WIT |
| :--- | :--- |
| `number`, `bigint` | `f64`, `s64` |
| `u8`..`u64`, `s8`..`s64`, `f32`, `f64` (`perry:types`) | Matching WIT numeric type |
| `boolean`, `string`, `void` | `bool`, `string`, no result |
| `T[]`, `Uint8Array`, `[A, B, ...]`, object types | `list<T>`, `list<u8>`, `tuple<A, B, ...>`, `record` |
| `T \| null` | `option<T>` |
| `T \| undefined`, optional property `field?: T` | `variant` (`undefined`, `value(T)`) |
| `T \| null \| undefined` | `variant` (`null`, `undefined`, `value(T)`) |
| `"a" \| "b"` | `enum` |
| `{ tag: "a", val?: T } \| ...` | `variant` |
| `{ ok: true, value?: T } \| { ok: false, error?: E }` | `result<T, E>` |
| `async` function, `ReadableStream<Uint8Array>` | `async func`, `stream<u8>` |

## Contract Files

When `./wit` exists (or `--wit <dir>` is passed), `perry-wit` compiles against that WIT package instead of inferring a contract. `--world <name>` selects the world when the package defines more than one.

Pass `--gen-wit` to write the inferred contract to `<wit-dir>/world.wit` (`package local:<world>`, defaulting to `wit/world.wit` and world `app`) and compile against it:

```sh
perry-wit src/index.ts --gen-wit -o dist/task.wasm
```

In an authored WIT world:
- A `result<T, E>` export accepts either a `{ ok: true, value } | { ok: false, error }` return value or a function that returns `T` directly and throws `E` on failure.
- Standard WASI 0.3 interfaces resolve from the embedded packages; packages in `<wit-dir>/deps` take precedence over them or add custom host interfaces.
- `--core-only` emits the world-bound core WebAssembly module. `--keep-names` preserves function names in the custom `name` section for guest profiling.

## Generated Types

`perry-wit gen-types` writes `.d.ts` declarations and a `tsconfig.json` into `.perry/types` (or `-o <dir>`), plus a root `tsconfig.json` when none exists:

```sh
perry-wit gen-types
tsc --noEmit -p .perry/types
```

Without a WIT directory, `gen-types` writes `runtime.d.ts` (the supported Node.js, Web, `Temporal` and `perry:types` declarations). When `./wit` (or `--wit <dir>`) exists, it also writes `world.d.ts`, `imports.d.ts` and an `implementation-check.ts` that checks `--entry` (default `src/index.ts`) against `ComponentImplementation`. Outbound `list<u8>` parameters and results in `WitInput<T>` accept both `Uint8Array` and UTF-8 `string` values.

## Commands

Top-level statements become the `wasi:cli/run` export, including top-level `await`:

```ts check=command
import { readFile } from "node:fs/promises";

const text = await readFile("input.json", "utf8");
const items: number[] = JSON.parse(text);
console.log("sum:", items.reduce((a, b) => a + b, 0));
if (items.length === 0) process.exitCode = 1;
```

```sh
perry-wit src/main.ts -o app.wasm
wasmtime run -S p3=y -W component-model-async=y --dir . app.wasm
```

`console` writes to WASI standard output and error, `process.env` reads the WASI environment, and `process.exitCode` or `process.exit(code)` sets the exit status.

If the module also exports functions, its top-level statements initialize state once instead, and `wasi:cli/run` and every export wait for that initialization. Top-level statements that start asynchronous work must `await` it.

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

Run `perry-wit src/index.ts --gen-wit` to materialize `wit/world.wit` when you want to edit the contract, and re-run `perry-wit gen-types` after editing `wit/*.wit`.

# Authoring

A component starts with a TypeScript entry module. Perry-WIT derives its contract from public signatures or reads an authored WIT world.

## Typed Components

Named exported functions form the component interface. Local helpers stay internal, and supported local re-exports expose functions from other modules. The starter template contains:

```ts check=task
{{#include ../../template/src/index.ts}}
```

```sh
perry-wit src/index.ts -o dist/task.wasm
wasmtime run -C cache=n -S p3=y -W component-model-async=y --invoke 'run-task("hello")' dist/task.wasm
```

TypeScript `camelCase` names map to WIT `kebab-case`. Exported parameters need concrete type annotations. Return annotations support structured signatures; simple scalar returns can be inferred.

| TypeScript | Generated WIT |
| --- | --- |
| `number`, `bigint` | `f64`, `s64` |
| Integer and float aliases from `perry:types` | The corresponding numeric type |
| `boolean`, `string`, return `void` | `bool`, `string`, no result |
| Arrays, `Uint8Array`, tuples, records | Lists, `list<u8>`, tuples, records |
| `T \| null` | `option<T>` |
| `T \| undefined`, optional fields | Presence variants |
| `T \| null \| undefined` | Presence variants with distinct null and undefined cases |
| String-literal unions, supported tagged unions | Enums, variants, results |
| Async functions, `ReadableStream<Uint8Array>` | Async functions, `stream<u8>` |

Bare `bigint` retains its internal arithmetic semantics and is checked against `s64` when crossing the boundary. Explicit numeric aliases or authored WIT select other numeric domains. An inferred contract preserves declared result unions; thrown exceptions alone do not define an error channel.

## Contract Files

Compilation reads `./wit` when it exists. `--wit` and `--world` select an authored input contract. An explicitly requested missing directory or a malformed existing contract produces an error.

Generate a contract for editing while compiling:

```sh
perry-wit src/index.ts --gen-wit --wit interfaces --world app -o dist/task.wasm
```

This writes `interfaces/world.wit` with package identity `local:app`, using the generated contract for that compilation. Generation defaults to directory `wit` and world `app`. Existing input WIT is ignored during generation; unrelated files and dependency packages are retained.

After editing the generated contract, compile against it:

```sh
perry-wit src/index.ts --wit interfaces --world app -o dist/task.wasm
```

Each authored WIT export binds to its corresponding TypeScript export. A `result<T, E>` export accepts explicit tagged results or completion-style implementations that return `T` and throw `E`. Builtin host requirements resolve from embedded WASI definitions; custom host interfaces use authored WIT.

`--core-only` emits the selected world's core module. `--keep-names` preserves guest function names for profiling. `-o` controls the Wasm output independently of WIT generation.

## Generated Types

`gen-types` emits builtin runtime declarations and a TypeScript configuration even before an entry file exists:

```sh
perry-wit gen-types
tsc --noEmit
```

When the selected WIT directory exists, it also emits contract declarations and an implementation check:

```sh
perry-wit gen-types --wit interfaces --world app --entry src/index.ts
tsc --noEmit -p .perry/types
```

Generated declarations preserve numeric distinctions through aliases. Export declarations describe exact contracts; `ComponentImplementation` also accepts completion-style implementations and supported outbound byte conveniences.

## Commands

Top-level scripts receive a `wasi:cli/run` entrypoint, including scripts with top-level `await`:

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

In modules that also export functions, top-level statements initialize state once before the command or any function runs. Such initialization must await asynchronous work that it starts.

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

The template starts with TypeScript only. Generate WIT when you need an editable interface, then run `gen-types` again after contract edits.

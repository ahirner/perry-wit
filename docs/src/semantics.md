# Semantics

Perry-WIT accepts a statically typed subset of TypeScript.
Names, object layouts and host capabilities are resolved at compile time, and a construct outside the subset is a compile error with a source location.

## Objects

- Interfaces, type aliases and object literals compile to closed layouts with fixed field offsets. Types with the same fields share one layout.
- Adding properties at run time and mutating prototypes are rejected. `delete` works on optional properties and on `Record<K, V>` maps.
- Unions of literals, records and WIT variants narrow through `typeof`, equality and discriminant properties.

## Arrays

- Arrays (`T[]`) are dense and homogeneous.
- Reading an index outside the array, or a fractional index, yields `undefined` when the element type admits it.
- Writing at `length` appends. Writing at any other index outside `0..length` throws `RangeError`.

## Strings

- Strings are UTF-8 and index by Unicode scalar value, not UTF-16 code unit. Indices agree with Node.js for text in the Basic Multilingual Plane. Characters outside it count as one scalar value.
- `charCodeAt`, `String.fromCharCode` and lone surrogate escapes (`\uD800` to `\uDFFF`) are compile errors. Use `codePointAt` and `String.fromCodePoint`.
- Text arriving through the Canonical ABI, `JSON.parse` or `TextDecoder` is validated as UTF-8.

## Exceptions

- `Error`, `TypeError`, `RangeError`, `ReferenceError` and `SyntaxError` keep `name`, `message` and `cause` through `try`, `catch` and `finally`.
- An export declared with a WIT `result<T, E>` turns an uncaught exception into `err`. Without a `result`, an uncaught exception traps, and a trap does not run pending `finally` blocks.

## Async

- An `async` function runs until its first suspension. Awaiting an already settled promise continues without suspending.
- There is no microtask queue in the guest. Waiting tasks resume from WASI 0.3 callbacks, so the order of continuations for different host events follows the host, not Node.js. Observers of the same promise resume in registration order.
- `Promise.race` leaves the losing tasks running. They must settle before the export returns, or the call traps.
- `for await` over a byte stream releases the reader lock and cancels the stream when the loop exits early. Releasing a reader lock rejects pending reads but keeps buffered chunks for the next reader.

## Standard Library

{{#include stdlib-inventory.md}}

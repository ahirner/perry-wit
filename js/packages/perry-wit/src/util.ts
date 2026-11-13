/** Shared encoder; `TextEncoder` holds no state between calls. */
const encoder = new TextEncoder();

/** Encode text as UTF-8, passing bytes through unchanged. */
export function toBytes(contents: string | Uint8Array): Uint8Array {
  return typeof contents === "string" ? encoder.encode(contents) : contents;
}

/** Iterate the key-value pairs of a `Map` or a plain object alike. */
export function entries<V>(map: Readonly<Record<string, V>> | ReadonlyMap<string, V>): Iterable<[string, V]> {
  return map instanceof Map ? map : Object.entries(map);
}

/**
 * Narrow bytes to an `ArrayBuffer`-backed array.
 * Only for bytes known to own a whole, non-shared buffer,
 * so callers can pass them to `WebAssembly.compile`, `Blob` and similar APIs.
 */
export function owned(bytes: Uint8Array): Uint8Array<ArrayBuffer> {
  return bytes as Uint8Array<ArrayBuffer>;
}

/** Whether a value is bytes starting with the WebAssembly magic number `\0asm`. */
export function hasWasmMagic(value: unknown): value is Uint8Array {
  return value instanceof Uint8Array && value[0] === 0x00 && value[1] === 0x61 && value[2] === 0x73 && value[3] === 0x6d;
}

/** Convert a `kebab-case` name, as WIT spells it, to `camelCase`. */
export function camelCase(name: string): string {
  return name.replace(/-([a-z])/g, (_, letter: string) => letter.toUpperCase());
}

/** `true` when `A` and `B` are mutually assignable, for compile-time checks with `satisfies`. */
export type Same<A, B> = [A] extends [B] ? ([B] extends [A] ? true : false) : false;

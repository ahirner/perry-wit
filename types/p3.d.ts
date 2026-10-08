/// <reference path="./http-handler.d.ts" />
/** Capability imports accepted by the production WASI 0.3 compiler.
 * Standard TypeScript library declarations also describe Math.random(),
 * performance.now(), Date.now(), crypto.randomUUID(), and crypto.getRandomValues().
 * This compiler currently accepts only Uint8Array destinations for random filling.
 * It returns the same view, throws numeric 1 for other supported value kinds or 2
 * for a view over 65536 bytes, and makes no host call for an empty view.
 * Random UUIDs are lowercase v4 strings. Clock reads use milliseconds; Date.now()
 * returns whole signed UTC epoch milliseconds. Date supports new Date(epochMs)
 * and ISO string construction, getTime(), toISOString(), getUTCFullYear(),
 * getUTCMonth(), getUTCDate(), getUTCDay(), and setUTCDate(). Offset-free ISO
 * date-times use UTC, the guest's supported local time zone. Invalid dates
 * return NaN from getTime() and throw numeric 1 from toISOString(). Dates retain
 * guest-local identity in helpers, objects, and Promises. Omitted, copy, dynamic,
 * and multi-argument constructors, other calendar methods, and valueOf() are
 * diagnosed. Component boundaries carry UTC ISO strings.
 * Temporal supports the typed immutable subset declared below.
 * WIT stream<u8> and HTTP bodies expose ReadableStream<Uint8Array>. Readers
 * support getReader(), getReader({mode:"byob"}), read(), cancel(), and releaseLock().
 * BYOB read accepts Uint8Array with an optional literal {min:number}; it transfers
 * the entire backing ArrayBuffer and detaches every old view. Reuse the returned
 * value.buffer. Short EOF returns a view; cancellation of a pending read returns
 * undefined. Other BYOB view types and dynamic reader options are diagnosed.
 * Unconstrained any coercion, callbacks, and detached work
 * are deferred. Standard library declarations do not imply compiler support.
 * JSON.parse accepts strict UTF-8 strings and rejects unpaired surrogate escapes.
 * JSON.stringify supports primitives, plain records, JSON value trees, dense typed arrays, string
 * arrays, and Dates; undefined root/field/element behavior matches ECMAScript.
 * Syntax/surrogate/depth-or-cycle failures throw numeric 1/2/3. Unsupported value
 * kinds throw 12. Runtime delete, revivers, replacers, indentation, custom prototypes, sparse array
 * literals, and byte-view/Stats/Promise serialization are unsupported. Parsed
 * graphs retain guest identity across helpers, collection, and stored Promises;
 * component boundaries carry JSON strings. */
/** Standard BYOB minimum-fill option, missing from older TypeScript DOM libraries. */
interface ReadableStreamBYOBReader {
  read<T extends ArrayBufferView>(view: T, options: { min: number }): Promise<ReadableStreamReadResult<T>>;
}

declare namespace NodeJS {
  interface ProcessEnv {
    readonly [key: string]: string | undefined;
  }
  interface WritableStream { write(chunk: Uint8Array | string): boolean; }
  interface Process {
    readonly stdout: WritableStream;
    readonly stderr: WritableStream;
    /** Cached read-only environment. Missing keys return undefined.
     * Enumeration and JSON return independent snapshots. All mutation, including
     * through aliases or casts, is rejected by the WAFFLE/P3 compiler. */
    readonly env: ProcessEnv;
    /** Cached read-only host arguments, with no synthetic Node prefixes. */
    readonly argv: readonly string[];
    /** Cached initial host directory, or / if absent. A supplied empty string is retained. */
    cwd(): string;
    exitCode: number | undefined;
    exit(code?: number): never;
  }
}
declare var process: NodeJS.Process;

/** ISO-calendar Temporal subset. Parsing/range/unsupported-annotation failures
 * throw numeric 1/2/3. Bracket annotations and formatting options are unsupported.
 * Values retain guest identity; component boundaries carry ISO strings. */
declare namespace Temporal {
  class Instant {
    private constructor();
    static from(text: string): Instant;
    static fromEpochMilliseconds(milliseconds: number): Instant;
    readonly epochMilliseconds: number;
    toString(): string;
  }
  /** Calendar fields without a time zone. from() ignores numeric offsets and rejects Z. */
  class PlainDateTime {
    private constructor();
    static from(text: string): PlainDateTime;
    readonly year: number;
    readonly month: number;
    readonly day: number;
    readonly dayOfWeek: number;
    readonly hour: number;
    readonly minute: number;
    readonly second: number;
    readonly millisecond: number;
    readonly microsecond: number;
    readonly nanosecond: number;
    add(duration: { days: number }): PlainDateTime;
    toString(): string;
  }
  /** ISO calendar date. from() ignores time and numeric offsets, and rejects Z. */
  class PlainDate {
    private constructor();
    static from(text: string): PlainDate;
    readonly year: number;
    readonly month: number;
    readonly day: number;
    readonly dayOfWeek: number;
    add(duration: { days: number }): PlainDate;
    toString(): string;
  }
}



declare module "node:timers/promises" {
  export function setTimeout<T = void>(milliseconds?: number, value?: T, options?: {signal?: AbortSignal}): Promise<T>;
}

declare module "node:stream" {
  export class Writable {
    static toWeb(stream: NodeJS.WritableStream): WritableStream<Uint8Array>;
  }
}

declare module "fs" {
  export interface Stats {
    readonly size: number;
    readonly mtimeMs: number;
    isFile(): boolean;
    isDirectory(): boolean;
  }
}
declare module "node:fs" { export type { Stats } from "fs"; }

declare module "fs/promises" {
  import type { Stats } from "fs";
  /** Materialize the file in guest memory; errors use the numeric WASI filesystem ordinal. */
  export function readFile(path: string, encoding: "utf8" | "utf-8"): Promise<string>;
  export function readFile(path: string, encoding?: null): Promise<Uint8Array>;
  export function readFile(path: string, options: string | { encoding?: string | null; flag?: "r"; signal?: AbortSignal }): Promise<string | Uint8Array>;
  export function writeFile(path: string, data: string | Uint8Array, options?: string | { encoding?: string | null; flag?: "w"; signal?: AbortSignal } | null): Promise<void>;
  export function stat(path: string, options?: { bigint?: false; throwIfNoEntry?: true } | null): Promise<Stats>;
  export function mkdir(path: string): Promise<void>;
  export function unlink(path: string): Promise<void>;
  export function rmdir(path: string): Promise<void>;
  export function readdir(path: string, options?: string | { encoding?: string | null; recursive?: false; withFileTypes?: false } | null): Promise<string[]>;
  const fs: { readFile: typeof readFile; writeFile: typeof writeFile; stat: typeof stat; mkdir: typeof mkdir; unlink: typeof unlink; rmdir: typeof rmdir; readdir: typeof readdir };
  export default fs;
}

declare module "node:fs/promises" {
  export { readFile, writeFile, stat, mkdir, unlink, rmdir, readdir, default } from "fs/promises";
}

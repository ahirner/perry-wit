/** Capability imports accepted by the production WASI 0.3 compiler.
 * Standard TypeScript library declarations also describe Math.random(),
 * performance.now(), Date.now(), crypto.randomUUID(), and crypto.getRandomValues().
 * This compiler currently accepts only Uint8Array destinations for random filling.
 * It returns the same view, throws numeric 1 for other supported value kinds or 2
 * for a view over 65536 bytes, and makes no host call for an empty view.
 * Random UUIDs are lowercase v4 strings. Clock reads use milliseconds; Date.now()
 * returns whole signed UTC epoch milliseconds. Date supports new Date(epochMs)
 * with a statically known number, getTime(), and toISOString(). Invalid dates
 * return NaN from getTime() and throw numeric 1 from toISOString(). Dates retain
 * guest-local identity in helpers, objects, and Promises. Omitted, copy, nonnumeric,
 * and multi-argument constructors, calendar getters, valueOf(), setters, and
 * string parsing are diagnosed. Component boundaries carry UTC ISO strings.
 * Temporal supports the typed immutable subset declared below.
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
declare namespace NodeJS {
  interface ProcessEnv {
    readonly [key: string]: string | undefined;
  }
  interface Process {
    /** Cached read-only environment. Missing keys return undefined.
     * Enumeration and JSON return independent snapshots. All mutation, including
     * through aliases or casts, is rejected by the WAFFLE/P3 compiler. */
    readonly env: ProcessEnv;
    /** Cached read-only host arguments, with no synthetic Node prefixes. */
    readonly argv: readonly string[];
    /** Cached initial host directory, or / if absent. A supplied empty string is retained. */
    cwd(): string;
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
    readonly hour: number;
    readonly minute: number;
    readonly second: number;
    readonly millisecond: number;
    readonly microsecond: number;
    readonly nanosecond: number;
    add(duration: { days: number }): PlainDateTime;
    toString(): string;
  }
}

declare module "perry:clocks" {
  /** Wait in milliseconds. May be stored and awaited repeatedly within one invocation.
   * Invalid or overflowing durations trap before host I/O. */
  export function waitFor(milliseconds: number): Promise<void>;
}

declare module "node:timers/promises" {
  /** Millisecond wait with Node delay clamping/truncation. Optional result values
   * and AbortSignal options are not yet supported. */
  export function setTimeout(milliseconds?: number): Promise<void>;
}

declare module "perry:http-handler/types" {
  export type Method =
    | { tag: "get" | "head" | "post" | "put" | "delete" | "connect" | "options" | "trace" | "patch" }
    | { tag: "other"; val: string };
  export type Scheme = { tag: "http" | "https" } | { tag: "other"; val: string };
  export interface Request {
    method: Method;
    scheme: Scheme | null | undefined;
    authority: string | null | undefined;
    pathWithQuery: string | null | undefined;
    headers: [string, Uint8Array][];
    body: Uint8Array;
  }
  /** Buffered response for compile_http_handler. Body caps are compiler options.
   * Status is 200–599; 204/205/304 require an empty body. Headers preserve duplicates
   * and bytes. Typed owned tasks and combinators may remain pending within handle. */
  export interface Response {
    status: number;
    headers: [string, Uint8Array][];
    body: Uint8Array;
  }
}

declare module "perry:http" {
  /** Buffered response with no remaining native HTTP resources. Headers preserve
   * duplicates and exact bytes. Indices must be integers in [0, headerCount).
   * Response metadata is immutable; body and header views remain mutable bytes. */
  export interface HttpResponse {
    readonly status: number;
    readonly body: Uint8Array;
    readonly headerCount: number;
    headerName(index: number): string;
    headerValue(index: number): Uint8Array;
  }
  /** Bounded GET, supporting stored tasks in resolved WIT components. Scheme is exactly http or https; path
   * includes any query. Request headers are string-valued. The response limit
   * must be an integer in [0, 4294967295]. Numeric failures: 8 body overflow,
   * 12 invalid metadata/limit/index, 100 + WASI HTTP error discriminant, or
   * 200 + WASI header error discriminant. Non-2xx status remains a response.
   * Native producer completion is checked independently of body EOF. */
  export function get(
    scheme: "http" | "https",
    authority: string,
    path: string,
    headers: { [name: string]: string },
    maxResponseBytes: number,
  ): Promise<HttpResponse>;
}

declare module "perry:random" {
  /** A number in [0, 1) from the high 53 bits of a WASI random word. */
  export function randomNumber(): number;
}

declare module "perry:stdio" {
  /** Write the visible bytes, then await the independent P3 output completion.
   * Tasks remain owned by the invocation. Numeric rejection codes: 1 I/O, 2 invalid
   * byte sequence, 3 broken pipe. Already written bytes are not rolled back. */
  export function writeStdout(bytes: Uint8Array): Promise<void>;
  /** Same transfer and completion contract as writeStdout, directed to stderr. */
  export function writeStderr(bytes: Uint8Array): Promise<void>;
}

declare module "fs" {
  type Utf8Encoding = `${"u" | "U"}${"t" | "T"}${"f" | "F"}${"" | "-"}8`;
  type BinaryEncoding = `${"b" | "B"}${"i" | "I"}${"n" | "N"}${"a" | "A"}${"r" | "R"}${"y" | "Y"}`;
  /** Materialize exact bytes from a preopen-confined path and await producer completion.
   * Encoding strings may be selected at runtime. */
  export function readFileSync(
    path: string,
    options?: BinaryEncoding | null,
  ): Uint8Array;
  /** Strict UTF-8, preserving BOMs. Malformed text throws filesystem error 9.
   * Reads use memory proportional to the file size; paths/flags follow readFileSync above. */
  export function readFileSync(
    path: string,
    options: Utf8Encoding,
  ): string;
  /** Runtime encoding strings and reusable option objects return a tagged string-or-byte value.
   * Plain objects support aliases, helper calls, and declared-field mutation. Getters,
   * spreads, computed literal keys, methods, and custom prototypes are diagnosed.
   * The compiler can specialize inline literals more precisely than this declaration.
   * Narrow with typeof before using string-only or byte-only operations.
   * Unsupported labels throw filesystem error 12 before opening a file. */
  export function readFileSync(
    path: string,
    options: string | { encoding?: string | null; flag?: "r" },
  ): string | Uint8Array;
  interface WriteOptions {
    /** Case-insensitive utf8 or utf-8; binary is also accepted for byte data. */
    encoding?: string | null;
    flag?: "w";
  }
  /** Overwrite a preopen-confined path with exact UTF-8 or visible bytes.
   * Plain option objects may be reused and mutated. Unsupported options throw before I/O.
   * May suspend until transfers and the independent P3 completion settle.
   * Failures throw the one-based WASI 0.3 filesystem error ordinal. */
  export function writeFileSync(
    path: string,
    data: string | Uint8Array,
    options?: string | WriteOptions | null,
  ): void;
  /** Metadata with identity and fields retained through collection and Promises.
   * Component boundaries use a stats record: size, mtime-ms, and a stats-kind enum. */
  export interface Stats {
    readonly size: number;
    readonly mtimeMs: number;
    isFile(): boolean;
    isDirectory(): boolean;
  }
  /** Follow confined symlinks and report size, modification milliseconds, and type.
   * Options are plain data objects. Missing entries throw; bigint is unsupported. */
  export function statSync(path: string, options?: { bigint?: false; throwIfNoEntry?: true } | null): Stats;
  /** Return false for filesystem errors, including paths outside available preopens. */
  export function existsSync(path: string): boolean;
  /** Create one directory. Recursive creation, permissions, and other options are unsupported. */
  export function mkdirSync(path: string, options?: undefined): void;
  export function unlinkSync(path: string, options?: undefined): void;
  export function rmdirSync(path: string, options?: undefined): void;
  /** Materialize UTF-8 entry names, excluding dot entries, then await producer completion.
   * Plain option objects and runtime UTF-8 labels are supported. Ordering is host-defined.
   * String arrays survive retained Promises and cross component boundaries as list<string>. */
  export function readdirSync(path: string, options?: string | { encoding?: string | null; recursive?: false; withFileTypes?: false } | null): string[];
  const fs: {
    writeFileSync: typeof writeFileSync;
    readFileSync: typeof readFileSync;
    statSync: typeof statSync;
    existsSync: typeof existsSync;
    mkdirSync: typeof mkdirSync;
    unlinkSync: typeof unlinkSync;
    rmdirSync: typeof rmdirSync;
    readdirSync: typeof readdirSync;
  };
  export default fs;
}

declare module "node:fs" {
  export { writeFileSync, readFileSync, statSync, existsSync, mkdirSync, unlinkSync, rmdirSync, readdirSync, Stats, default } from "fs";
}

declare module "fs/promises" {
  import type { Stats } from "fs";
  /** Materialize the file in guest memory; errors use the numeric WASI filesystem ordinal. */
  export function readFile(path: string, encoding: "utf8" | "utf-8"): Promise<string>;
  export function readFile(path: string, encoding?: null): Promise<Uint8Array>;
  export function readFile(path: string, options: string | { encoding?: string | null; flag?: "r" }): Promise<string | Uint8Array>;
  export function writeFile(path: string, data: string | Uint8Array, options?: string | { encoding?: string | null; flag?: "w" } | null): Promise<void>;
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

/** Opaque readable end of a native byte stream, owned by the entry invocation. */
interface ByteStream {
  readonly __perryByteStream: unique symbol;
}

/** Read up to 8192 bytes. Zero means EOF, which does not imply producer success.
 * Must be immediately awaited. The next read replaces the current chunk. */
declare function readChunk(input: ByteStream): Promise<number>;

/** Fill a byte view and return the number of bytes written, leaving the rest unchanged.
 * A nonempty view returning zero means EOF, which does not imply producer success.
 * An empty view returns zero without consuming input. Invalidates the current chunk.
 * Must be immediately awaited; the destination and backing storage remain live. */
declare function readInto(input: ByteStream, destination: Uint8Array): Promise<number>;

/** Read a byte from the current chunk. Noninteger or out-of-range indices trap. */
declare function byteAt(index: number): number;

/** Capability imports accepted by the WAFFLE/P3 compiler API. */
declare module "perry:clocks" {
  /** Wait in milliseconds. May be stored and awaited repeatedly within one invocation.
   * Invalid or overflowing durations trap before host I/O. */
  export function waitFor(milliseconds: number): Promise<void>;
}

declare module "perry:random" {
  /** A number in [0, 1) from the high 53 bits of a WASI random word. */
  export function randomNumber(): number;
}

declare module "perry:stdio" {
  /** Write the visible bytes, then await the independent P3 output completion.
   * Must be immediately awaited. Numeric rejection codes: 1 I/O, 2 invalid
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
   * Plain objects support aliases, helper calls, mutation, and deletion. Getters,
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

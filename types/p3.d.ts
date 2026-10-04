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

/** Capability imports accepted by the WAFFLE/P3 compiler API. */
declare module "perry:clocks" {
  /** Wait in milliseconds. Invalid or overflowing durations trap before host I/O. */
  export function waitFor(milliseconds: number): Promise<void>;
}

declare module "perry:random" {
  /** A number in [0, 1) from the high 53 bits of a WASI random word. */
  export function randomNumber(): number;
}

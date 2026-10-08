// Supported runtime capabilities; Web APIs use the standard TypeScript libraries.
declare namespace NodeJS {
  interface ProcessEnv {
    readonly [key: string]: string | undefined;
  }
  interface WritableStream { write(chunk: Uint8Array | string): boolean; }
  interface Process {
    readonly stdout: WritableStream;
    readonly stderr: WritableStream;
    readonly env: ProcessEnv;
    readonly argv: readonly string[];
    cwd(): string;
    exitCode: number | undefined;
    exit(code?: number): never;
  }
}
declare var process: NodeJS.Process;

declare namespace Temporal {
  class Instant {
    private constructor();
    static from(text: string): Instant;
    static fromEpochMilliseconds(milliseconds: number): Instant;
    readonly epochMilliseconds: number;
    toString(): string;
  }
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

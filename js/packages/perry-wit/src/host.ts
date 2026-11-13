import * as cli from "@bytecodealliance/preview2-shim/cli";
import * as clocks from "@bytecodealliance/preview2-shim/clocks";
import * as filesystem from "@bytecodealliance/preview2-shim/filesystem";
import * as io from "@bytecodealliance/preview2-shim/io";
import * as random from "@bytecodealliance/preview2-shim/random";
import type { ImportObject } from "../bindings/compiler.js";
import type { LoadOptions } from "./types.js";
import { camelCase } from "./util.js";

const dispose = Symbol.dispose ?? Symbol.for("dispose");

type OutputStream = ReturnType<typeof cli.stderr.getStderr>;
type Pollable = ReturnType<OutputStream["subscribe"]>;

/** Build a synchronous in-memory WASI `output-stream` resource backed by `write`. */
function sinkStream(write: (bytes: Uint8Array) => void): OutputStream {
  const pollable = Object.assign(Object.create(io.poll.Pollable.prototype) as Pollable, {
    ready: () => true,
    block: () => {},
    [dispose]() {},
  });
  return Object.assign(Object.create(io.streams.OutputStream.prototype) as OutputStream, {
    checkWrite: () => 1_048_576n,
    write: (contents: Uint8Array) => {
      write(contents);
    },
    blockingWriteAndFlush: (contents: Uint8Array) => {
      write(contents);
    },
    flush: () => {},
    blockingFlush: () => {},
    subscribe: () => pollable,
    [dispose]() {},
  });
}

/**
 * WASI 0.2 interfaces for the compiler, keyed by `wasi:package/interface` with or without a version.
 * The compiler works on in-memory inputs only, so it sees no directories, arguments or environment variables.
 */
export function wasiImports(options: Pick<LoadOptions, "stderr" | "stdout"> = {}): ImportObject {
  const stderr = options.stderr ? sinkStream(options.stderr) : undefined;
  const stdout = options.stdout ? sinkStream(options.stdout) : undefined;
  const packages: Record<string, Record<string, unknown>> = {
    cli: {
      ...cli,
      ...(stderr ? { stderr: { getStderr: () => stderr } } : {}),
      ...(stdout ? { stdout: { getStdout: () => stdout } } : {}),
      environment: { getArguments: () => [], getEnvironment: () => [], initialCwd: () => undefined },
    },
    clocks,
    io,
    random,
    filesystem: { ...filesystem, preopens: { getDirectories: () => [] } },
  };
  const imports = new Proxy({}, {
    get(_, specifier) {
      if (typeof specifier !== "string") return undefined;
      const [, name, member] = /^wasi:([\w-]+)\/([\w-]+)(?:@0\.2\.\d+)?$/.exec(specifier) ?? [];
      const value = name && member ? packages[name]?.[camelCase(member)] : undefined;
      if (!value) throw new Error(`perry-wit: the compiler requested unsupported import ${specifier}`);
      return value;
    },
  });
  return imports as ImportObject;
}

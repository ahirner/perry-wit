import * as cli from "@bytecodealliance/preview2-shim/cli";
import * as clocks from "@bytecodealliance/preview2-shim/clocks";
import * as filesystem from "@bytecodealliance/preview2-shim/filesystem";
import * as io from "@bytecodealliance/preview2-shim/io";
import * as random from "@bytecodealliance/preview2-shim/random";
import type { ImportObject } from "../bindings/compiler.js";
import { camelCase } from "./util.js";

/**
 * WASI 0.2 interfaces for the compiler, keyed by `wasi:package/interface` with or without a version.
 * The compiler works on in-memory inputs only, so it sees no directories, arguments or environment variables.
 */
export function wasiImports(): ImportObject {
  const packages: Record<string, Record<string, unknown>> = {
    cli: {
      ...cli,
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

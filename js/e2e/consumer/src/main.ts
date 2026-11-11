// Uses perry-wit the way a downstream project would, through its published entry point only.
import assert from "node:assert/strict";
import {
  compile,
  compileCore,
  Compiler,
  CompileError,
  generateTypes,
  type CompiledComponent,
  type CompiledModule,
  type Diagnostic,
} from "perry-wit";

const wit = `
package example:greeter@0.1.0;

world greeter {
  export greet: func(name: string) -> string;
  export double: func(n: f64) -> f64;
}
`;

const source = `
import { twice } from "./math.js";

export function greet(name: string): string {
  return \`Hello, \${name}!\`;
}

export function double(n: number): number {
  return twice(n);
}
`;

const modules = { "math.ts": "export function twice(n: number): number { return n * 2; }" };

/** Whether bytes start with the header of a WebAssembly component rather than a core module. */
const isComponent = (bytes: Uint8Array) => bytes[0] === 0 && bytes[4] === 0x0d && bytes[6] === 0x01;

const compiled: CompiledComponent = await compile({ source, modules, wit });
assert.ok(isComponent(compiled.component), "compile returns a component");
assert.ok(WebAssembly.validate(compiled.core), "compile returns a valid core module");

const coreOnly: CompiledModule = await compile({ source, modules, wit, coreOnly: true });
// @ts-expect-error A core-only result has no component.
assert.equal(coreOnly.component, undefined);
const { instance: { exports } } = await WebAssembly.instantiate(coreOnly.core);
assert.equal((exports.double as (n: number) => number)(21), 42, "the core module runs");

const raw = await compileCore({ source: "export function seven(): number { return 7; }" });
const instance = await WebAssembly.instantiate(raw);
assert.equal((instance.instance.exports.seven as () => number)(), 7, "compileCore output runs");

const declarations = await generateTypes({ wit });
assert.deepEqual(declarations.map((file) => file.path).sort(), ["imports.d.ts", "runtime.d.ts", "world.d.ts"]);

const error = await compile({ source: { path: "src/bad.ts", contents: "export function greet(name) { return name; }" }, wit }).catch(
  (error: unknown) => error,
);
assert.ok(error instanceof CompileError, "invalid programs reject with a CompileError");
const [diagnostic]: readonly Diagnostic[] = error.diagnostics;
assert.equal(diagnostic?.span?.file, "src/bad.ts");

const compiler = await Compiler.load();
assert.ok(isComponent(compiler.compile({ source, modules, wit }).component), "a loaded compiler runs synchronously");

await assert.rejects(import("perry-wit/dist/compiler.js" as string), { code: "ERR_PACKAGE_PATH_NOT_EXPORTED" });

console.log("perry-wit e2e: compile, coreOnly, compileCore, generateTypes, CompileError and Compiler.load work");

import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { Compiler, compile, compileCore, CompileError, formatDiagnostics, generateTypes } from "perry-wit";

const source = "export function run(n: number): number { return n * 2; }";
const wit = "package test:double; world app { export run: func(n: f64) -> f64; }";
const isComponent = (bytes: Uint8Array) => bytes[4] === 0x0d && bytes[6] === 0x01;

test("compiles a program into a component", async () => {
  const { component, core, allocationReport } = await compile({ source, wit });
  assert.ok(isComponent(component));
  assert.ok(WebAssembly.validate(core));
  assert.ok(allocationReport.length > 0);
});

test("keepNames retains the name section", async () => {
  const stripped = await compile({ source, wit });
  const named = await compile({ source, wit, keepNames: true });
  assert.ok(named.component.byteLength > stripped.component.byteLength);
});

test("coreOnly returns the world-bound core module", async () => {
  const compiled = await compile({ source, wit, coreOnly: true });
  assert.equal("component" in compiled, false);
  const { exports } = new WebAssembly.Instance(new WebAssembly.Module(compiled.core));
  assert.equal((exports.run as (n: number) => number)(21), 42);
});

test("compileCore exports raw TypeScript functions", async () => {
  const core = await compileCore({ source: `${source} export function seven(): number { return 7; }` });
  const { exports } = new WebAssembly.Instance(new WebAssembly.Module(core));
  assert.equal((exports.seven as () => number)(), 7);
});

test("resolves supplied modules and WIT dependencies", async () => {
  const { component } = await compile({
    source: { path: "src/main.ts", contents: "import { twice } from './math.js'; export function run(n: number): number { return twice(n); }" },
    modules: new Map([["src/math.ts", "export function twice(n: number): number { return n * 2; }"]]),
    wit: [
      "package test:bundle; world app { use test:types/numbers.{number}; export run: func(n: number) -> number; }",
      { path: "extra.wit", contents: "package test:bundle; interface additional {}" },
    ],
    witDependencies: ["package test:types; interface numbers { type number = f64; }"],
  });
  assert.ok(isComponent(component));
});

test("reports diagnostics as a CompileError", async () => {
  const error = await compile({ source: { path: "src/main.ts", contents: "export function run(n) { return n; }" }, wit }).then(
    () => assert.fail("expected a CompileError"),
    (error: unknown) => error,
  );
  assert.ok(error instanceof CompileError);
  assert.equal(error.name, "CompileError");
  const [first] = error.diagnostics;
  assert.ok(first);
  assert.equal(error.primary, first);
  assert.equal(first.kind, "type-check");
  assert.equal(first.code, "missing-type-annotation");
  assert.equal(first.span?.file, "src/main.ts");
  assert.equal(typeof first.span?.startByte, "number");
  assert.match(error.message, /^src\/main\.ts:1:\d+: error\[missing-type-annotation\]:/);

  const mutable = [first];
  const snapshotError = new CompileError(mutable);
  mutable.shift();
  assert.equal(snapshotError.diagnostics.length, 1);
  assert.equal(snapshotError.primary, snapshotError.diagnostics[0]);
  assert.equal(snapshotError.primary, first);
  assert.ok(Object.isFrozen(snapshotError.diagnostics));
  assert.equal(
    formatDiagnostics([
      {
        kind: "type-check",
        code: "missing-required-field",
        message: "Missing value",
        span: { file: "src/é.ts", startLine: 2, startColumn: 3, endLine: 2, endColumn: 8, startByte: 10, endByte: 15 },
        notes: [{ message: "Record fields are mandatory" }],
        help: "Add value: null",
      },
      { kind: "syntax", code: "parse-error", message: "Unexpected token", notes: [] },
    ]),
    "src/é.ts:2:3: error[missing-required-field]: Missing value\n  Record fields are mandatory\n  help: Add value: null\nerror[parse-error]: Unexpected token",
  );
  assert.equal(
    formatDiagnostics([
      {
        kind: "contract",
        code: "contract-mismatch",
        message: "type mismatch",
        span: { file: "src/main.ts", startLine: 4, startColumn: 1, endLine: 4, endColumn: 12, startByte: 20, endByte: 31 },
        notes: [
          { message: "while binding WIT export run" },
          {
            message: "expected here",
            span: { file: "src/types.ts", startLine: 1, startColumn: 10, endLine: 1, endColumn: 16, startByte: 9, endByte: 15 },
          },
        ],
        help: "update signature",
      },
    ]),
    "src/main.ts:4:1: error[contract-mismatch]: type mismatch\n  while binding WIT export run\n  src/types.ts:1:10: expected here\n  help: update signature",
  );
});

test("generates declarations for a world", async () => {
  const files = await generateTypes({ wit });
  assert.deepEqual(files.map((file) => file.path), ["world.d.ts", "imports.d.ts", "runtime.d.ts"]);
  assert.match(files[0]!.contents, /run/);
});

test("a loaded compiler runs synchronously", async () => {
  const compiler = await Compiler.load();
  assert.ok(isComponent(compiler.compile({ source, wit }).component));
  assert.throws(() => compiler.compile({ source, wit: "package test:bad; world app { export missing: func(); }" }), CompileError);
});

test("the shared compiler is replaced after any error but a CompileError", async (t) => {
  let loads = 0;
  const load = Compiler.load;
  t.mock.method(Compiler, "load", () => (loads++, load.call(Compiler)));
  // A stand-in for a host exception thrown during a call, such as the RangeError of an exhausted call stack.
  const exhausted = {
    get source(): string {
      throw new RangeError("Maximum call stack size exceeded");
    },
  };

  await compileCore({ source });
  const before = loads;
  await assert.rejects(compileCore({ source: "export function run(n) { return n; }" }), CompileError);
  await compileCore({ source });
  assert.equal(loads, before, "a CompileError keeps the shared compiler");

  await assert.rejects(compileCore(exhausted), RangeError);
  await compileCore({ source });
  assert.equal(loads, before + 1, "any other error loads a fresh compiler");
});

test("the documented example compiles a component", async () => {
  const directory = await mkdtemp(join(tmpdir(), "perry-wit-example-"));
  try {
    const example = fileURLToPath(new URL("../examples/compile.ts", import.meta.url));
    const { stdout } = await promisify(execFile)(process.execPath, [example], { cwd: directory });
    assert.match(stdout, /greeter\.wasm: \d+ bytes/);
    assert.ok(isComponent(new Uint8Array(await readFile(join(directory, "greeter.wasm")))));
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});

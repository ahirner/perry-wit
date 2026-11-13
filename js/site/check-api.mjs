import assert from 'node:assert/strict';
import { readFile, mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { Compiler, CompileError } from 'perry-wit';
import { coreSlices } from './.generated/compiler-slices.js';

const defaultOutput = fileURLToPath(new URL('./dist', import.meta.url));
const compilerBytes = await readFile(resolve(process.env.SITE_OUTPUT || defaultOutput, 'downloads/perry-wit.wasm'));
const compiler = await Compiler.load({
  loadCoreModule: name => {
    const slice = coreSlices[name];
    if (!slice) throw new Error(`Missing compiler slice for ${name}`);
    return WebAssembly.compile(compilerBytes.subarray(slice.start, slice.end));
  },
});

function diagnosticsMatch(pattern) {
  return error => error instanceof CompileError
    && error.diagnostics.length > 0
    && error.diagnostics.every(diagnostic => diagnostic.code && diagnostic.severity && diagnostic.message)
    && pattern.test(error.message);
}

const encode = text => new TextEncoder().encode(text);
const source = 'export function run(n: number): number { return n * 2; }';
const contract = 'package test:parity; world app { export run: func(n: f64) -> f64; }';
const compiled = compiler.compile({ source, wit: contract });
const coreOnly = compiler.compile({ source, wit: contract, coreOnly: true });
assert.deepEqual(coreOnly.core, compiled.core);
assert.equal(coreOnly.allocationReport, compiled.allocationReport);
assert.throws(
  () => compiler.compile({ source, wit: 'package test:missing; world app { export missing: func(); }', coreOnly: true }),
  diagnosticsMatch(/missing/),
);

for (const contents of [new Uint8Array([255]), 'export function {']) {
  assert.throws(() => compiler.compile({ source: contents, wit: contract }), diagnosticsMatch(/UTF-8|Parsing/));
}
assert.throws(() => compiler.compileCore({ source: "export { run } from './missing';" }), diagnosticsMatch(/Cannot resolve/));

const helperSource = '// é🙂\r\nexport function helper(value) { return value; }';
assert.throws(
  () => compiler.compileCore({
    source: { path: 'src/main.ts', contents: "import { helper } from './helper'; export function run():number{return helper(1);}" },
    modules: { 'src/helper.ts': helperSource },
  }),
  error => {
    assert(error instanceof CompileError);
    const [diagnostic] = error.diagnostics;
    assert.equal(diagnostic.code, 'missing-type-annotation');
    assert.equal(diagnostic.severity, 'error');
    assert.equal(diagnostic.span.file, 'src/helper.ts');
    assert.equal(diagnostic.span.startLine, 2);
    const start = encode(helperSource.slice(0, helperSource.indexOf('value'))).length;
    assert(diagnostic.span.startByte <= start && diagnostic.span.endByte >= start + 5);
    assert.equal('sanitized' in diagnostic, false);
    assert(diagnostic.help);
    return true;
  },
);

assert.throws(
  () => compiler.compile({
    source: { path: 'src/main.ts', contents: "import { twice } from './math.js'; export function run(n: number): number { return twice(n); }" },
    modules: { 'src/math.ts': 'export function twice(n: number): number { return n * 2; }' },
    wit: 'package test:bundle; world app { use test:types/numbers.{number}; export run: func(n: number) -> number; }',
  }),
  diagnosticsMatch(/test:types/),
);
assert.throws(() => compiler.generateTypes({ wit: new Uint8Array([255]) }), diagnosticsMatch(/UTF-8/));
assert.equal(compiler.generateTypes({ wit: 'package test:http; world app { import wasi:http/client@0.3.0; }' }).length, 3);

const parityCases = [{ name: 'floating point', source, contract, compiled, sdk: compiler.generateTypes({ wit: contract }) }];
for (const [domain, result, args, expected] of [
  ['s8', 's8', [127, 1], -128],
  ['u8', 'u8', [255, 1], 0],
  ['s16', 's16', [32767, 1], -32768],
  ['u16', 'u16', [65535, 1], 0],
  ['s32', 's32', [2147483647, 1], -2147483648],
  ['u32', 'u32', [4294967295, 1], 0],
  ['s64', 's64', [9223372036854775807n, 1n], -9223372036854775808n],
  ['u64', 'u64', [18446744073709551615n, 1n], 0n],
  ['u64', 'u64', [9007199254740993n, 2n], 9007199254740995n],
  ['u8', 'u32', [255, 255], 510],
]) {
  const imports = domain.endsWith('64') ? "import type { s64, u64 } from 'perry:types'; " : '';
  const type = domain.endsWith('64') ? domain : 'number';
  const source = `${imports}function add(a: ${type}, b: ${type}): ${type} { return a + b; } export function run(a: ${type}, b: ${type}): ${type} { return add(a, b); }`;
  const contract = `package test:integer; world app { export run: func(a: ${domain}, b: ${domain}) -> ${result}; }`;
  const compiled = compiler.compile({ source, wit: contract });
  const core = compiler.compile({ source, wit: contract, coreOnly: true });
  assert.deepEqual(core.core, compiled.core);
  assert.equal('component' in core, false);
  const module = new WebAssembly.Module(compiled.core);
  assert.deepEqual(WebAssembly.Module.imports(module), [], `${domain}: no runtime imports`);
  assert.deepEqual(WebAssembly.Module.exports(module), [{ name: 'run', kind: 'function' }], `${domain}: no memory, allocator or cleanup exports`);
  assert.equal(new WebAssembly.Instance(module).exports.run(...args), expected, `${domain} → ${result}`);
  if (type === 'number') {
    const raw = new WebAssembly.Instance(new WebAssembly.Module(compiler.compileCore({ source })));
    assert.equal(raw.exports.run(...args), args[0] + args[1], 'Raw compilation retains unconstrained number semantics');
  }
  parityCases.push({ name: `${domain} → ${result}`, source, contract, compiled, sdk: compiler.generateTypes({ wit: contract }) });
}
console.log('Wasm library: embedded slice loader, invalid UTF-8, Unicode/CRLF spans, built-in WASI WIT, all integer widths, exact 64-bit arithmetic, and raw/core output');

if (process.env.PERRY_NATIVE_COMPILER && process.env.PERRY_COMPILER_WASM) {
  const directory = await mkdtemp(join(tmpdir(), 'perry-parity-'));
  try {
    await mkdir(join(directory, 'wit'));
    const native = resolve(process.env.PERRY_NATIVE_COMPILER);
    const wasm = resolve(process.env.PERRY_COMPILER_WASM);
    for (const fixture of parityCases) {
      await writeFile(join(directory, 'input.ts'), fixture.source);
      await writeFile(join(directory, 'wit/world.wit'), fixture.contract);
      for (const [name, command, prefix] of [
        ['native', native, []],
        ['wasm', process.env.PERRY_WASMTIME || 'wasmtime', ['run', '--dir', '.', wasm]],
      ]) {
        const run = args => execFileSync(command, [...prefix, ...args], { cwd: directory, stdio: 'pipe' });
        run(['input.ts', '--wit', 'wit', '-o', `${name}.wasm`]);
        assert.deepEqual(new Uint8Array(await readFile(join(directory, `${name}.wasm`))), fixture.compiled.component, `${fixture.name}: ${name} component`);
        run(['input.ts', '--wit', 'wit', '--core-only', '-o', `${name}.core.wasm`]);
        assert.deepEqual(new Uint8Array(await readFile(join(directory, `${name}.core.wasm`))), fixture.compiled.core, `${fixture.name}: ${name} core`);
        run(['gen-types', '--wit', 'wit', '--out', `${name}-sdk`, '--no-tsconfig']);
        for (const file of fixture.sdk) assert.equal(await readFile(join(directory, `${name}-sdk`, file.path), 'utf8'), file.contents);
      }
    }
    console.log('Native CLI / Wasm CLI / Wasm library: identical component, core and SDK artifacts');
  } finally { await rm(directory, { recursive: true, force: true }); }
}

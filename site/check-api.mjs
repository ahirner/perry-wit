import assert from 'node:assert/strict';
import { readFile, mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { resolve, join } from 'node:path';
import { execFileSync } from 'node:child_process';
import { imports } from './.generated/compiler-manifest.js';
import { compilerHost, instantiateCompiler } from './play/compiler-host.js';
const host = compilerHost(imports);
const preopens = imports.find(name => name.split('@')[0] === 'wasi:filesystem/preopens');
assert.deepEqual(host[preopens].getDirectories(), []);
const compiler = await instantiateCompiler(await readFile(resolve(process.env.SITE_OUTPUT || 'site/dist', 'downloads/perry-wit.wasm')));
const api = compiler.api;
const encode = text => new TextEncoder().encode(text);
const source = 'export function run(n: number): number { return n * 2; }';
const contract = 'package test:parity; world app { export run: func(n: f64) -> f64; }';
const program = { source: { contents: encode(source) }, modules: [] };
const wit = { application: { tag: 'sources', val: [{ contents: encode(contract) }] }, dependencies: [] };
const compiled = api.compile(program, wit, { coreOnly: false });
assert(compiled.component.bytes.length > 0 && compiled.component.stripped.length > 0 && compiled.allocationReport.length > 0);
assert(api.compileCore(program).length > 0);
const coreOnly = api.compile(program, wit, { coreOnly: true });
assert.deepEqual(coreOnly.core, compiled.core);
assert.equal(coreOnly.component, undefined);
assert.equal(coreOnly.allocationReport, compiled.allocationReport);
assert.throws(() => api.compile(program, { application: { tag: 'sources', val: [{ contents: encode('package test:missing; world app { export missing: func(); }') }] }, dependencies: [] }, { coreOnly: true }), /missing/);
const raw = api.compileCore({ source: { contents: encode(source + ' export function extra(): number { return 7; }') }, modules: [] });
assert(WebAssembly.Module.exports(new WebAssembly.Module(raw)).some(entry => entry.name === 'extra'));
const declaration = await readFile(new URL('./.generated/compiler/interfaces/perry-compiler-api.d.ts', import.meta.url), 'utf8');
assert.match(declaration, /interface Module\s*\{\s*name: string[,;]/);
assert.match(declaration, /component\?: ComponentOutput/);
assert.match(declaration, /allocationReport: string/);
const sdk = api.generateSdk(wit);
assert.deepEqual(sdk.map(file => file.name), ['world.d.ts', 'imports.d.ts', 'runtime.d.ts']);
for (const bytes of [new Uint8Array([255]), encode('export function {')]) {
  assert.throws(() => api.compile({ source: { contents: bytes }, modules: [] }, wit, { coreOnly: false }), /UTF-8|Parsing/);
}
assert.throws(() => api.compileCore({ source: { contents: encode("export { run } from './missing';") }, modules: [] }), /Cannot resolve/);
const bundled = { source: { name: 'src/main.ts', contents: encode("import { twice } from './math.js'; export function run(n: number): number { return twice(n); }") },
  modules: [{ name: 'src/math.ts', contents: encode('export function twice(n: number): number { return n * 2; }') }] };
const grouped = { application: { tag: 'sources', val: [
  { contents: encode('package test:bundle; world app { use test:types/numbers.{number}; export run: func(n: number) -> number; }') },
  { contents: encode('package test:bundle; interface additional {}') },
] }, dependencies: [{ tag: 'sources', val: [{ contents: encode('package test:types; interface numbers { type number = f64; }') }] }] };
assert(api.compile(bundled, grouped, { coreOnly: false }).component.stripped.length > 0);
assert.equal(api.generateSdk(grouped).length, 3);
assert.throws(() => api.compile(bundled, { ...grouped, dependencies: [] }, { coreOnly: false }), /test:types/);
assert.throws(() => api.generateSdk({ application: { tag: 'sources', val: [{ contents: new Uint8Array([255]) }] }, dependencies: [] }), /UTF-8/);
assert.equal(api.generateSdk({ application: { tag: 'sources', val: [{ contents: encode('package test:http; world app { import wasi:http/client@0.3.0; }') }] }, dependencies: [] }).length, 3);
console.log('Wasm library: anonymous buffers, supplied modules, grouped/embedded WIT, core output, SDK and invalid inputs; standard WASI imports, no filesystem grants');

if (process.env.PERRY_NATIVE_COMPILER && process.env.PERRY_COMPILER_WASM) {
  const directory = await mkdtemp(join(tmpdir(), 'perry-parity-'));
  try {
    await mkdir(join(directory, 'wit'));
    await writeFile(join(directory, 'input.ts'), source);
    await writeFile(join(directory, 'wit/world.wit'), contract);
    const native = resolve(process.env.PERRY_NATIVE_COMPILER);
    const wasm = resolve(process.env.PERRY_COMPILER_WASM);
    for (const [name, command, prefix] of [
      ['native', native, []],
      ['wasm', process.env.PERRY_WASMTIME || 'wasmtime', ['run', '--dir', '.', wasm]],
    ]) {
      const run = args => execFileSync(command, [...prefix, ...args], { cwd: directory, stdio: 'pipe' });
      run(['input.ts', '--wit', 'wit', '-o', `${name}.wasm`]);
      assert.deepEqual(new Uint8Array(await readFile(join(directory, `${name}.wasm`))), compiled.component.stripped);
      run(['input.ts', '--wit', 'wit', '--core-only', '-o', `${name}.core.wasm`]);
      assert.deepEqual(new Uint8Array(await readFile(join(directory, `${name}.core.wasm`))), compiled.core);
      run(['gen-types', '--wit', 'wit', '--out', `${name}-sdk`, '--no-tsconfig']);
      for (const file of sdk) assert.deepEqual(new Uint8Array(await readFile(join(directory, `${name}-sdk`, file.name))), file.contents);
    }
    console.log('Native CLI / Wasm CLI / Wasm library: identical component, core and SDK artifacts');
  } finally { await rm(directory, { recursive: true, force: true }); }
}

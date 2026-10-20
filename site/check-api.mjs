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
function diagnosticsMatch(pattern) {
  return error => Array.isArray(error.payload) && error.payload.length > 0
    && error.payload.every(diagnostic => diagnostic.code && diagnostic.severity && diagnostic.message)
    && pattern.test(error.payload.map(diagnostic => [diagnostic.message, ...diagnostic.notes].join('\n')).join('\n'));
}
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
assert.throws(() => api.compile(program, { application: { tag: 'sources', val: [{ contents: encode('package test:missing; world app { export missing: func(); }') }] }, dependencies: [] }, { coreOnly: true }), diagnosticsMatch(/missing/));
const raw = api.compileCore({ source: { contents: encode(source + ' export function extra(): number { return 7; }') }, modules: [] });
assert(WebAssembly.Module.exports(new WebAssembly.Module(raw)).some(entry => entry.name === 'extra'));
const declaration = await readFile(new URL('./.generated/compiler/interfaces/perry-compiler-api.d.ts', import.meta.url), 'utf8');
assert.match(declaration, /interface Module\s*\{\s*name: string[,;]/);
assert.match(declaration, /component\?: ComponentOutput/);
assert.match(declaration, /allocationReport: string/);
const sdk = api.generateSdk(wit);
assert.deepEqual(sdk.map(file => file.name), ['world.d.ts', 'imports.d.ts', 'runtime.d.ts']);
for (const bytes of [new Uint8Array([255]), encode('export function {')]) {
  assert.throws(() => api.compile({ source: { contents: bytes }, modules: [] }, wit, { coreOnly: false }), diagnosticsMatch(/UTF-8|Parsing/));
}
assert.throws(() => api.compileCore({ source: { contents: encode("export { run } from './missing';") }, modules: [] }), diagnosticsMatch(/Cannot resolve/));
const helperSource = '// é🙂\r\nexport function helper(value) { return value; }';
assert.throws(() => api.compileCore({source:{name:'src/main.ts',contents:encode("import { helper } from './helper'; export function run():number{return helper(1);}")},modules:[{name:'src/helper.ts',contents:encode(helperSource)}]}), error => {
  const diagnostic = error.payload[0];
  assert.equal(diagnostic.code, 'missing-type-annotation');
  assert.equal(diagnostic.severity, 'error');
  assert.equal(diagnostic.span.file, 'src/helper.ts');
  assert.equal(diagnostic.span.startLine, 2);
  const start = BigInt(encode(helperSource.slice(0, helperSource.indexOf('value'))).length);
  assert(diagnostic.span.startByte <= start && diagnostic.span.endByte >= start + 5n);
  assert.equal('sanitized' in diagnostic, false);
  assert(diagnostic.help);
  return true;
});
const bundled = { source: { name: 'src/main.ts', contents: encode("import { twice } from './math.js'; export function run(n: number): number { return twice(n); }") },
  modules: [{ name: 'src/math.ts', contents: encode('export function twice(n: number): number { return n * 2; }') }] };
const grouped = { application: { tag: 'sources', val: [
  { contents: encode('package test:bundle; world app { use test:types/numbers.{number}; export run: func(n: number) -> number; }') },
  { contents: encode('package test:bundle; interface additional {}') },
] }, dependencies: [{ tag: 'sources', val: [{ contents: encode('package test:types; interface numbers { type number = f64; }') }] }] };
assert(api.compile(bundled, grouped, { coreOnly: false }).component.stripped.length > 0);
assert.equal(api.generateSdk(grouped).length, 3);
assert.throws(() => api.compile(bundled, { ...grouped, dependencies: [] }, { coreOnly: false }), diagnosticsMatch(/test:types/));
assert.throws(() => api.generateSdk({ application: { tag: 'sources', val: [{ contents: new Uint8Array([255]) }] }, dependencies: [] }), diagnosticsMatch(/UTF-8/));
assert.equal(api.generateSdk({ application: { tag: 'sources', val: [{ contents: encode('package test:http; world app { import wasi:http/client@0.3.0; }') }] }, dependencies: [] }).length, 3);
const parityCases = [{ name: 'floating point', source, contract, compiled, sdk }];
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
  const program = { source: { contents: encode(source) }, modules: [] };
  const wit = { application: { tag: 'sources', val: [{ contents: encode(contract) }] }, dependencies: [] };
  const compiled = api.compile(program, wit, { coreOnly: false });
  const core = api.compile(program, wit, { coreOnly: true });
  assert.deepEqual(core.core, compiled.core);
  assert.equal(core.component, undefined);
  const module = new WebAssembly.Module(compiled.core);
  assert.deepEqual(WebAssembly.Module.imports(module), [], `${domain}: no runtime imports`);
  assert.deepEqual(WebAssembly.Module.exports(module), [{ name: 'run', kind: 'function' }], `${domain}: no memory, allocator or cleanup exports`);
  assert.equal(new WebAssembly.Instance(module).exports.run(...args), expected, `${domain} → ${result}`);
  if (type === 'number') {
    const raw = new WebAssembly.Instance(new WebAssembly.Module(api.compileCore(program)));
    assert.equal(raw.exports.run(...args), args[0] + args[1], 'Raw compilation retains unconstrained number semantics');
  }
  parityCases.push({ name: `${domain} → ${result}`, source, contract, compiled, sdk: api.generateSdk(wit) });
}
console.log('Wasm library: anonymous buffers, supplied modules, grouped/embedded WIT, all integer widths, exact 64-bit arithmetic, raw/core output, SDK and invalid inputs; no filesystem grants');

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
        assert.deepEqual(new Uint8Array(await readFile(join(directory, `${name}.wasm`))), fixture.compiled.component.stripped, `${fixture.name}: ${name} component`);
        run(['input.ts', '--wit', 'wit', '--core-only', '-o', `${name}.core.wasm`]);
        assert.deepEqual(new Uint8Array(await readFile(join(directory, `${name}.core.wasm`))), fixture.compiled.core, `${fixture.name}: ${name} core`);
        run(['gen-types', '--wit', 'wit', '--out', `${name}-sdk`, '--no-tsconfig']);
        for (const file of fixture.sdk) assert.deepEqual(new Uint8Array(await readFile(join(directory, `${name}-sdk`, file.name))), file.contents);
      }
    }
    console.log('Native CLI / Wasm CLI / Wasm library: identical component, core and SDK artifacts');
  } finally { await rm(directory, { recursive: true, force: true }); }
}

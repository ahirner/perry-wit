import { formatDiagnostics } from './diagnostics.js';
import { instantiateCompiler } from './compiler-host.js';
import * as compilerCli from '@bytecodealliance/preview2-shim/cli';
import { $init as bindgenReady, generate } from '@bytecodealliance/jco-transpile/component';
import { $init as toolsReady, tools } from '@perry/wasm-tools';
import * as cli from '@bytecodealliance/preview3-shim/cli';
import * as clocks from '@bytecodealliance/preview3-shim/clocks';
import * as http from '@bytecodealliance/preview3-shim/http';
import * as random from '@bytecodealliance/preview3-shim/random';

const camel = name => name.replace(/-([a-z])/g, (_, letter) => letter.toUpperCase());

function applicationImports(names) {
  const groups = { 'wasi:cli': cli, 'wasi:clocks': clocks, 'wasi:http': http, 'wasi:random': random };
  return Object.fromEntries(names.map(name => {
    const [identity, version] = name.split('@');
    const [group, member] = identity.split('/');
    const value = !version || version === '0.3.0' ? groups[group]?.[camel(member)] : undefined;
    if (!value) throw new Error(`The browser cannot provide import ${name}. Download the component to run it in a host that supports this interface.`);
    return [name, value];
  }));
}

async function instantiateComponent(generated) {
  const files = new Map(generated.files);
  const imports = applicationImports(generated.imports);
  const moduleUrl = URL.createObjectURL(new Blob([files.get('application.js')], { type: 'text/javascript' }));
  try {
    const { instantiate } = await import(moduleUrl);
    return await instantiate(path => WebAssembly.compile(files.get(path)), imports);
  } finally { URL.revokeObjectURL(moduleUrl); }
}

function callableExports(generated, exports) {
  const functions = new Map();
  const seen = new Set();
  for (const [name, type] of generated.exports) {
    const value = exports[name];
    if (seen.has(value)) continue;
    seen.add(value);
    if (type === 'function') functions.set(name, value);
    else for (const [member, fn] of Object.entries(value)) {
      if (typeof fn === 'function' && !/^class\s/.test(Function.prototype.toString.call(fn))) functions.set(`${name}#${member}`, fn);
    }
  }
  return functions;
}

self.onmessage = async ({ data }) => {
  const send = (kind, result) => postMessage({ kind, id: data.id, ...result });
  const progress = message => send('progress', { message });
  let log = '';
  let phase = 'load';
  const decoder = new TextDecoder();
  const output = { write(bytes) { log += decoder.decode(bytes, { stream: true }); }, flush() {}, blockingFlush() {} };
  cli._setStdout(output);
  cli._setStderr(output);
  compilerCli._setStdout(output);
  compilerCli._setStderr(output);
  try {
    const response = await fetch(new URL(`../../downloads/perry-wit.wasm${new URL(import.meta.url).search}`, import.meta.url));
    if (!response.ok) throw new Error(`Could not load compiler: HTTP ${response.status}`);
    const compiler = await instantiateCompiler(new Uint8Array(await response.arrayBuffer()));
    if (data.action === 'generate-sdk') {
      phase = 'generate-sdk';
      send('sdk', { files: compiler.api.generateSdk(data.wit, data.options?.world) });
      return;
    }
    progress('Compiling…');
    phase = 'compile';
    const started = performance.now();
    const api = compiler.api;
    const compiled = api.compile(data.program, data.wit, data.options);
    const bytes = compiled.component.stripped;
    send('compiled', { bytes, coreSize: compiled.core.length, allocationReport: compiled.allocationReport, elapsed: performance.now() - started });
    progress('Inspecting component…');
    phase = 'inspect';
    await Promise.all([toolsReady, bindgenReady]);
    const generated = await generate(bytes, { name: 'application', instantiation: { tag: 'async' }, noTypescript: true, noNodejsCompat: true, base64Cutoff: 0 });
    send('inspection', { wat: tools.print(bytes), interface: tools.componentWit(bytes) });
    const functions = callableExports(generated, await instantiateComponent(generated));
    send('exports', { names: [...functions.keys()] });
    let result = 'Compiled successfully. Download the component or select Compile & run.';
    let needsExport = false;
    if (data.run) {
      const fn = functions.get(data.exportName) ?? (functions.size === 1 ? functions.values().next().value : undefined);
      if (fn) {
        progress('Running…');
        phase = 'run';
        const value = await fn(...data.args);
        let display = value;
        if (typeof value === 'string') {
          try { const json = JSON.parse(value); if (json && typeof json === 'object') display = json; } catch {}
        }
        result = value === undefined ? '(no return value)' : JSON.stringify(display, (_, value) => typeof value === 'bigint' ? `${value}n` : value, 2);
      } else {
        needsExport = functions.size > 0;
        result = needsExport ? 'Compilation succeeded. Choose a function and provide its arguments, then run again.' : 'This component has no callable exports.';
      }
    }
    send('complete', { result, needsExport, log: log + decoder.decode() });
  } catch (error) {
    const payload = error?.payload;
    send('error', { phase, log, diagnostics: Array.isArray(payload) ? payload : [], message: Array.isArray(payload) ? formatDiagnostics(payload) : typeof payload === 'string' ? payload : payload ? JSON.stringify(payload, null, 2) : error?.message || String(error) });
  }
};

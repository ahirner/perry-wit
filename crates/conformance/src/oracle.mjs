import { createServer } from 'node:http';
import { readFileSync, writeFileSync, mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const testCase = JSON.parse(readFileSync(process.argv[5], 'utf8'));
let clockQueue = [];
if (testCase.kind === 'lifecycle') {
  const { default: timers } = await import('node:timers/promises');
  const { registerHooks, syncBuiltinESMExports } = await import('node:module');
  timers.setTimeout = (_milliseconds, value, options = {}) => new Promise((resolve, reject) => {
    const signal = options.signal;
    const abort = () => {
      clockQueue = clockQueue.filter(item => item !== release);
      reject(new DOMException('Aborted', 'AbortError'));
    };
    const release = () => {
      signal?.removeEventListener('abort', abort);
      resolve(value);
    };
    if (signal?.aborted) {
      abort();
      return;
    }
    signal?.addEventListener('abort', abort, { once: true });
    clockQueue.push(release);
  });
  syncBuiltinESMExports();
  globalThis.__conformanceRelease = () => {
    const release = clockQueue.shift();
    if (!release) throw new Error('no pending clock');
    release();
  };
  registerHooks({
    resolve(specifier, context, nextResolve) {
      if (specifier === 'test:generated/control') {
        return {
          url: 'data:text/javascript,export function release(){globalThis.__conformanceRelease()}',
          shortCircuit: true,
        };
      }
      return nextResolve(specifier, context);
    },
  });
}

const guest = await import(pathToFileURL(process.argv[2]));
const inputs = JSON.parse(readFileSync(process.argv[3], 'utf8')).map(value =>
  value === 'NaN' ? NaN : Buffer.from(value, 'hex').readDoubleBE(0));
const numberBytes = new DataView(new ArrayBuffer(8));
function number(value) {
  if (typeof value !== 'number') throw new TypeError('numeric observation required');
  if (Number.isNaN(value)) return 'NaN';
  numberBytes.setFloat64(0, value, false);
  return numberBytes.getBigUint64(0, false).toString(16).padStart(16, '0');
}

const directory = mkdtempSync(join(tmpdir(), 'perry-oracle-'));
const observations = [];
let server;
let requests = 0;
if (testCase.kind === 'fetch') {
  server = createServer((request, response) => {
    if (request.method !== 'GET' || request.url !== '/contract' ||
        request.headers['x-contract'] !== 'bounded') {
      throw new Error('Fetch request differs from model');
    }
    requests++;
    const bytes = Buffer.from(testCase.input.bytes);
    response.writeHead(testCase.input.status, {
      'Content-Length': bytes.length + (testCase.input.truncated ? 1 : 0),
      'Connection': 'close',
    });
    response.end(bytes);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
}

try {
  writeFileSync(join(directory, 'fixture.txt'), JSON.parse(process.argv[4]));
  process.chdir(directory);
  const selected = ['stream', 'fetch', 'handler'].includes(testCase.kind) ? [0, 1, 2] : inputs;
  for (const valueInput of selected) {
    let input = valueInput;
    if (testCase.kind === 'stream') {
      const bytes = new Uint8Array(testCase.input.bytes);
      let position = 0;
      input = new ReadableStream({
        type: "bytes",
        pull(controller) {
          if (position === bytes.length) {
            controller.close();
            controller.byobRequest?.respond(0);
            return;
          }
          const end = Math.min(bytes.length, position + 8192);
          controller.enqueue(bytes.slice(position, end));
          position = end;
        },
      });
    }
    if (testCase.kind === 'fetch') {
      input = `http://127.0.0.1:${server.address().port}/contract`;
    }
    let result;
    if (testCase.kind === 'handler') {
      const bytes = new Uint8Array(testCase.input.bytes);
      const headers = [
        ['x-value', new TextEncoder().encode('first')],
        ['x-value', new TextEncoder().encode('second')],
      ];
      const response = guest.handle({
        method: { tag: 'put' },
        scheme: { tag: 'https' },
        authority: 'example.test',
        pathWithQuery: '/contract?q=1',
        headers,
        body: bytes,
      });
      if (JSON.stringify(response.headers) !== JSON.stringify(headers)) {
        throw new Error('native handler lost duplicate headers');
      }
      result = { value: response.status, trace: Array.from(response.body) };
    } else {
      result = await guest.run(input);
    }
    observations.push({ value: number(result.value), trace: result.trace.map(number) });
    if (clockQueue.length !== 0) throw new Error('clock operations leaked');
  }
} finally {
  if (server) {
    server.closeAllConnections();
    await new Promise(resolve => server.close(resolve));
  }
  process.chdir(tmpdir());
  rmSync(directory, { recursive: true, force: true });
}
if (server && requests !== 3) throw new Error('missing Fetch requests');
writeFileSync(process.argv[6], JSON.stringify(observations));

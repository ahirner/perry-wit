import { createServer } from 'node:http';
import { readFileSync, writeFileSync, mkdtempSync, rmSync, readdirSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { createInterface } from 'node:readline';
import timers from 'node:timers/promises';
import { registerHooks, syncBuiltinESMExports } from 'node:module';

const realSetTimeout = timers.setTimeout;
let clockQueue = [];
let lifecycleActive = false;

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

function setLifecycleMode(enabled) {
  if (enabled === lifecycleActive) return;
  lifecycleActive = enabled;
  if (enabled) {
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
  } else {
    timers.setTimeout = realSetTimeout;
  }
  syncBuiltinESMExports();
}

const numberBytes = new DataView(new ArrayBuffer(8));
function number(value) {
  if (typeof value !== 'number') throw new TypeError('numeric observation required');
  if (Number.isNaN(value)) return 'NaN';
  numberBytes.setFloat64(0, value, false);
  return numberBytes.getBigUint64(0, false).toString(16).padStart(16, '0');
}

function decodeInputs(rawInputs) {
  return rawInputs.map(value =>
    value === 'NaN' ? NaN : Buffer.from(value, 'hex').readDoubleBE(0));
}

let importCounter = 0;

async function executeCase(casePath, rawInputs, fixtureText, testCase, directory) {
  clockQueue = [];
  setLifecycleMode(testCase.kind === 'lifecycle');
  const guest = await import(`${pathToFileURL(casePath).href}?v=${++importCounter}`);
  const inputs = decodeInputs(rawInputs);
  const observations = [];
  const payloadBytes = testCase.payloadPath
    ? readFileSync(testCase.payloadPath)
    : (testCase.input?.bytes ? Buffer.from(testCase.input.bytes) : null);
  const status = testCase.status ?? testCase.input?.status ?? 200;
  const truncated = testCase.truncated ?? testCase.input?.truncated ?? false;
  let server;
  let requests = 0;
  if (testCase.kind === 'fetch') {
    server = createServer((request, response) => {
      if (request.method !== 'GET' || request.url !== '/contract' ||
          request.headers['x-contract'] !== 'bounded') {
        throw new Error('Fetch request differs from model');
      }
      requests++;
      response.writeHead(status, {
        'Content-Length': payloadBytes.length + (truncated ? 1 : 0),
        'Connection': 'close',
      });
      response.end(payloadBytes);
    });
    await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  }

  try {
    for (const entry of readdirSync(directory)) {
      if (entry !== 'fixture.txt') {
        rmSync(join(directory, entry), { recursive: true, force: true });
      }
    }
    writeFileSync(join(directory, 'fixture.txt'), fixtureText);
    process.chdir(directory);
    const selected = ['stream', 'fetch', 'handler'].includes(testCase.kind) ? [0, 1, 2] : inputs;
    for (const valueInput of selected) {
      let input = valueInput;
      if (testCase.kind === 'stream') {
        const bytes = new Uint8Array(payloadBytes.buffer, payloadBytes.byteOffset, payloadBytes.byteLength);
        let position = 0;
        input = new ReadableStream({
          type: 'bytes',
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
        const bytes = new Uint8Array(payloadBytes.buffer, payloadBytes.byteOffset, payloadBytes.byteLength);
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
        if (Array.isArray(result)) {
          result = { value: result[0], trace: result.slice(1) };
        } else {
          const { value, trace, ...extras } = result;
          const lifted = Object.values(extras).map(v => (v == null ? -66.0 : v + 0.0));
          result = { value, trace: [...trace, ...lifted] };
        }
      }
      observations.push({ value: number(result.value), trace: result.trace.map(number) });
      if (clockQueue.length !== 0) throw new Error('clock operations leaked');
    }
  } finally {
    if (server) {
      server.closeAllConnections();
      await new Promise(resolve => server.close(resolve));
    }
    setLifecycleMode(false);
  }
  if (server && requests !== 3) throw new Error('missing Fetch requests');
  return observations;
}

if (process.argv.length > 2) {
  const testCase = JSON.parse(readFileSync(process.argv[5], 'utf8'));
  const rawInputs = JSON.parse(readFileSync(process.argv[3], 'utf8'));
  const fixtureText = JSON.parse(process.argv[4]);
  const directory = mkdtempSync(join(tmpdir(), 'perry-oracle-'));
  try {
    const observations = await executeCase(process.argv[2], rawInputs, fixtureText, testCase, directory);
    writeFileSync(process.argv[6], JSON.stringify(observations));
  } finally {
    process.chdir(tmpdir());
    rmSync(directory, { recursive: true, force: true });
  }
} else {
  const directory = mkdtempSync(join(tmpdir(), 'perry-oracle-'));
  const rawStdoutWrite = process.stdout.write.bind(process.stdout);
  const rawStderrWrite = process.stderr.write.bind(process.stderr);
  const rl = createInterface({ input: process.stdin, crlfDelay: Infinity });
  for await (const line of rl) {
    if (!line.trim()) continue;
    const req = JSON.parse(line);
    const stdoutChunks = [];
    const stderrChunks = [];
    const toBuffer = (chunk, encoding) => {
      if (chunk instanceof Uint8Array) return Buffer.from(chunk);
      return Buffer.from(String(chunk), typeof encoding === 'string' ? encoding : 'utf8');
    };
    process.stdout.write = (chunk, encoding, cb) => {
      stdoutChunks.push(toBuffer(chunk, encoding));
      if (typeof encoding === 'function') encoding();
      else if (typeof cb === 'function') cb();
      return true;
    };
    process.stderr.write = (chunk, encoding, cb) => {
      stderrChunks.push(toBuffer(chunk, encoding));
      if (typeof encoding === 'function') encoding();
      else if (typeof cb === 'function') cb();
      return true;
    };
    let response;
    try {
      const observations = await executeCase(req.casePath, req.inputs, req.fixture, req.testCase, directory);
      const hasStdout = stdoutChunks.length > 0;
      const hasStderr = stderrChunks.length > 0;
      if (hasStdout) writeFileSync(req.stdoutPath, Buffer.concat(stdoutChunks));
      if (hasStderr) writeFileSync(req.stderrPath, Buffer.concat(stderrChunks));
      response = {
        ok: true,
        observations,
        hasStdout,
        hasStderr,
      };
    } catch (error) {
      const errText = error && error.stack ? String(error.stack) : String(error);
      stderrChunks.push(Buffer.from(errText + '\n', 'utf8'));
      response = {
        ok: false,
        error: Buffer.concat(stderrChunks).toString('utf8'),
      };
    } finally {
      process.stdout.write = rawStdoutWrite;
      process.stderr.write = rawStderrWrite;
    }
    rawStdoutWrite(JSON.stringify(response) + '\n');
  }
  process.chdir(tmpdir());
  rmSync(directory, { recursive: true, force: true });
}

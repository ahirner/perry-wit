async function readBounded(response: Response, limit: number): Promise<Uint8Array> {
  const body = response.body;
  if (!(limit >= 0 && limit <= 4294967295 && Math.trunc(limit) === limit)) {
    if (body !== null) await body.cancel();
    throw 12;
  }
  if (body === null) return new Uint8Array(0);
  const reader = body.getReader({ mode: 'byob' });
  let buffer = new Uint8Array(limit);
  let length = 0;
  try {
    while (length < limit) {
      const result = await reader.read(buffer.subarray(length));
      const bytes = result.value;
      if (bytes === undefined) throw 12;
      length += bytes.length;
      // A BYOB read transfers the backing buffer; recover its returned owner.
      buffer = new Uint8Array(bytes.buffer);
      if (result.done) return buffer.subarray(0, length);
    }
    const end = await reader.read(new Uint8Array(1));
    const extra = end.value;
    if (extra !== undefined && extra.length > 0) throw 8;
    if (!end.done) throw 12;
    return buffer.subarray(0, length);
  } finally {
    await reader.cancel();
    reader.releaseLock();
  }
}

type Outcome = { ok: true } | { ok: false };

async function read(path: string): Promise<string> {
  const response = await fetch('http://127.0.0.1:8080' + path, { headers: { accept: 'application/json' } });
  if (response.status !== 200) {
    const body = response.body;
    if (body !== null) await body.cancel();
    throw 12;
  }
  return new TextDecoder('utf-8', { fatal: true }).decode(await readBounded(response, 65536));
}

export async function runRun(): Promise<Outcome> {
  const first = JSON.parse(await read('/doc1.json'));
  const second = JSON.parse(await read('/doc2.json'));
  const output = JSON.stringify({ first, second });
  if (typeof output !== "string") return { ok: false };
  console.log(output);
  return { ok: true };
}

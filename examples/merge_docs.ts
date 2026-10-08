async function readDocument(url: string): Promise<string> {
  const response = await fetch(url, { headers: { accept: 'application/json' } });
  const body = response.body;
  if (body === null) throw 12;
  const reader = body.getReader({ mode: 'byob' });
  try {
    if (response.status !== 200) throw 12;
    // Wait for EOF or one byte beyond the 64 KiB limit.
    const result = await reader.read(new Uint8Array(65537), { min: 65537 });
    const bytes = result.value;
    if (bytes === undefined) throw 12;
    if (bytes.length > 65536) throw 8;
    return new TextDecoder('utf-8', { fatal: true }).decode(bytes);
  } finally {
    try {
      await reader.cancel();
    } finally {
      reader.releaseLock();
    }
  }
}

type Outcome = { ok: true } | { ok: false };

export async function runRun(): Promise<Outcome> {
  const first = JSON.parse(await readDocument('http://127.0.0.1:8080/doc1.json'));
  const second = JSON.parse(await readDocument('http://127.0.0.1:8080/doc2.json'));
  const output = JSON.stringify({ first, second });
  if (typeof output !== "string") return { ok: false };
  console.log(output);
  return { ok: true };
}

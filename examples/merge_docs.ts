import { get } from 'perry:http';

type Outcome = { ok: true } | { ok: false };

async function read(path: string): Promise<string> {
  const response = await get('http', '127.0.0.1:8080', path, { accept: 'application/json' }, 65536);
  if (response.status !== 200) { throw 12; }
  return new TextDecoder('utf-8', { fatal: true }).decode(response.body);
}

export async function runRun(): Promise<Outcome> {
  const first = JSON.parse(await read('/doc1.json'));
  const second = JSON.parse(await read('/doc2.json'));
  const output = JSON.stringify({ first, second });
  if (typeof output !== "string") return { ok: false };
  console.log(output);
  return { ok: true };
}

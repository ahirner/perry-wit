function body(response: Response): ReadableStream<Uint8Array> {
  const stream = response.body;
  if (stream === null) throw 90;
  return stream;
}

async function first(stream: ReadableStream<Uint8Array>): Promise<number> {
  for await (const chunk of stream) {
    const byte = chunk[0];
    if (byte === undefined) throw 89;
    return byte;
  }
  return 0;
}

export async function run(base: string): Promise<number> {
  const stream = body(await fetch(base + '/complete'));
  let total = 0;
  for await (const chunk of stream) {
    if (!stream.locked || chunk[0] !== 120 || chunk[chunk.length - 1] !== 120) throw 91;
    total += chunk.length;
    await 0;
    continue;
  }
  if (stream.locked) throw 92;

  const stopped = body(await fetch(base + '/complete'));
  for await (const chunk of stopped) {
    if (chunk[0] !== 120) throw 93;
    break;
  }
  if (stopped.locked) throw 94;
  for await (const unexpected of stopped) throw 95;

  const returned = body(await fetch(base + '/complete'));
  if (await first(returned) !== 120 || returned.locked) throw 96;

  const thrown = body(await fetch(base + '/truncated'));
  let caught = 0;
  try {
    for await (const chunk of thrown) throw 42;
  } catch (error) { caught = error; }
  if (caught !== 42 || thrown.locked) throw 97;

  const failed = body(await fetch(base + '/truncated'));
  let rejected = false;
  try {
    for await (const chunk of failed) {}
  } catch { rejected = true; }
  if (!rejected || failed.locked) throw 98;

  let latest = new Uint8Array(0);
  for await (latest of body(new Response('abc'))) {
    for await (const nested of body(new Response('de'))) {
      if (nested.length !== 2) throw 99;
    }
  }
  if (latest.length !== 3) throw 100;
  return total;
}

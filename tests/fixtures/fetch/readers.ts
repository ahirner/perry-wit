async function size(stream: ReadableStream<Uint8Array>): Promise<number> {
  const reader = stream.getReader();
  let total = 0;
  while (true) {
    const chunk = await reader.read();
    if (chunk.done) break;
    const bytes = chunk.value;
    if (bytes === undefined) throw 90;
    total += bytes.length;
  }
  reader.releaseLock();
  return total;
}

export async function run(base: string): Promise<number> {
  const response = await fetch(base + '/gated');
  const body = response.body;
  if (body === null) throw 91;
  const reader = body.getReader();
  let rejected = 0;
  try { body.getReader(); } catch { rejected++; }
  try { await body.cancel(); } catch { rejected++; }
  try { await response.text(); } catch { rejected++; }
  if (response.bodyUsed) throw 92;
  const first = reader.read();
  const second = reader.read();
  reader.releaseLock();
  try { await first; } catch (original) {
    try { await first; } catch (repeated) {
      if (repeated !== original || !(repeated instanceof TypeError)) throw new Error('Read rejection identity changed');
    }
    rejected++;
  }
  try { await second; } catch { rejected++; }
  if (rejected !== 5 || body.locked) throw 93;
  const resumed = size(body);
  const release = await fetch(base + '/release');
  await release.text();
  const count = await resumed;
  if (count !== 5 || !response.bodyUsed) throw 94;
  try { await response.bytes(); } catch { rejected++; }
  const next = body.getReader();
  const end = await next.read();
  next.releaseLock();
  if (!end.done || end.value !== undefined || rejected !== 6) throw 95;
  const buffered = new Response('abc');
  const cached = buffered.body;
  if (cached === null) throw 96;
  await buffered.text();
  if (!cached.locked) throw 97;
  let locked = false;
  try { cached.getReader(); } catch { locked = true; }
  if (!locked) throw 97;
  const cancelled = await fetch(base + '/stall');
  const cancellable = cancelled.body;
  if (cancellable === null) throw 98;
  const pendingReader = cancellable.getReader();
  const pending = pendingReader.read();
  await pendingReader.cancel();
  const stopped = await pending;
  pendingReader.releaseLock();
  if (!stopped.done || !cancelled.bodyUsed) throw 99;
  const controller = new AbortController();
  const interrupted = await fetch(base + '/stall', {signal: controller.signal});
  const interruptible = interrupted.body;
  if (interruptible === null) throw 100;
  const abortReader = interruptible.getReader();
  const waiting = abortReader.read();
  controller.abort();
  let aborted = false;
  try { await waiting; } catch { aborted = true; }
  abortReader.releaseLock();
  if (!aborted) throw 101;
  return count;
}

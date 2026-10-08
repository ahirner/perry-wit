async function size(stream: ReadableStream<Uint8Array>): Promise<number> {
  const reader = stream.getReader({mode:"byob"});
  let buffer=new Uint8Array(16);
  let total = 0;
  while (true) {
    const chunk = await reader.read(buffer,{min:16});
    const bytes = chunk.value;
    if (bytes === undefined) throw 90;
    total += bytes.length;
    buffer=new Uint8Array(bytes.buffer);
    if(chunk.done)break;
  }
  reader.releaseLock();
  return total;
}

export async function run(base: string): Promise<number> {
  const response = await fetch(base + '/gated');
  const body = response.body;
  if (body === null) throw 91;
  const reader = body.getReader({mode:"byob"});
  let rejected = 0;
  try { body.getReader(); } catch { rejected++; }
  try { await body.cancel(); } catch { rejected++; }
  try { await response.text(); } catch { rejected++; }
  if (response.bodyUsed) throw 92;
  const first = reader.read(new Uint8Array(8),{min:8});
  const second = reader.read(new Uint8Array(8));
  reader.releaseLock();
  try { await first; } catch { rejected++; }
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
  const pendingReader = cancellable.getReader({mode:"byob"});
  const pending = pendingReader.read(new Uint8Array(8));
  await pendingReader.cancel();
  const stopped = await pending;
  pendingReader.releaseLock();
  if (!stopped.done || stopped.value!==undefined || !cancelled.bodyUsed) throw 99;
  const controller = new AbortController();
  const interrupted = await fetch(base + '/stall', {signal: controller.signal});
  const interruptible = interrupted.body;
  if (interruptible === null) throw 100;
  const abortReader = interruptible.getReader({mode:"byob"});
  const waiting = abortReader.read(new Uint8Array(8));
  controller.abort();
  let aborted = false;
  try { await waiting; } catch { aborted = true; }
  abortReader.releaseLock();
  if (!aborted) throw 101;
  const local=new Response(new Uint8Array([3,7,11,19,23]));
  const localBody=local.body;if(localBody===null)throw 102;
  const localReader=localBody.getReader({mode:"byob"});
  const a=await localReader.read(new Uint8Array(2));
  const b=await localReader.read(new Uint8Array(8),{min:3});
  const firstBytes=a.value;const lastBytes=b.value;
  if(firstBytes===undefined||lastBytes===undefined)throw 103;
  if(a.done||b.done||firstBytes.length!==2||lastBytes.length!==3||firstBytes[1]!==7||lastBytes[2]!==23)throw 104;
  const endLocal=await localReader.read(new Uint8Array(8));
  if(!endLocal.done)throw 105;
  localReader.releaseLock();
  const shortBody=new Response(new Uint8Array([1,2,3,4,5])).body;
  if(shortBody===null)throw 106;
  const shortReader=shortBody.getReader({mode:"byob"});
  await shortReader.read(new Uint8Array(2));
  let shortRejected=false;try{await shortReader.read(new Uint8Array(8),{min:8});}catch{shortRejected=true;}
  if(!shortRejected)throw 107;
  shortReader.releaseLock();return count;
}

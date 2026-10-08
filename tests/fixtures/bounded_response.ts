async function readBounded(response: Response, limit: number): Promise<Uint8Array> {
  const body = response.body;
  if (!(limit >= 0 && limit <= 4294967295 && Math.trunc(limit) === limit)) {
    if (body !== null) await body.cancel();
    throw 12;
  }
  if (body === null) return new Uint8Array(0);
  const reader = body.getReader();
  const bytes = new Uint8Array(limit);
  let length = 0;
  try {
    let result = await reader.read();
    while (!result.done) {
      const chunk = result.value;
      if (chunk === undefined) throw 12;
      if (chunk.length > limit - length) throw 8;
      bytes.set(chunk, length);
      length += chunk.length;
      result = await reader.read();
    }
    return bytes.subarray(0, length);
  } finally {
    await reader.cancel();
    reader.releaseLock();
  }
}

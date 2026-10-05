export async function run(base: string): Promise<string> {
  const headers = new AbortController();
  const input = new Request(base + "/pending", { signal: headers.signal });
  const pending = fetch(input);
  const sibling = fetch(base + "/pending", { signal: headers.signal });
  const gate = await fetch(base + "/gate");
  await gate.text();
  const unrelated = fetch(base + "/ok");
  headers.abort();
  let rejected = 0;
  try { await pending; } catch { rejected++; }
  try { await pending; } catch { rejected++; }
  try { await sibling; } catch { rejected++; }
  try { await fetch(base + "/never", { signal: headers.signal }); } catch { rejected++; }
  const response = await unrelated;
  if (await response.text() !== "ok") throw 1;

  const bodyOwner = new AbortController();
  const streaming = await fetch(base + "/body", { signal: bodyOwner.signal });
  const body = streaming.text();
  const checkpoint = await fetch(base + "/body-gate");
  await checkpoint.text();
  bodyOwner.abort();
  try { await body; } catch { rejected++; }
  try { await body; } catch { rejected++; }

  const idle = new AbortController();
  const unused = await fetch(base + "/body", { signal: idle.signal });
  idle.abort();
  if (unused.status !== 200) throw 2;
  if (rejected !== 6) throw rejected;
  return "cancelled";
}

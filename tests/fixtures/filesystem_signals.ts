import { readFile, writeFile } from "node:fs/promises";

async function writeRejected(writing: Promise<void>): Promise<boolean> {
  try { await writing; return false; } catch { return true; }
}

export async function run(path: string): Promise<number> {
  const controller = new AbortController();
  const options: { signal?: AbortSignal } = { signal: controller.signal };
  const reading = readFile(path, options);
  const writing = writeFile(path + ".output", new Uint8Array(65536), options);
  const rejectedWrite = writeRejected(writing);
  controller.abort();
  let rejected = 0;
  try { await reading; } catch { rejected++; }
  try { await reading; } catch { rejected++; }
  if (await rejectedWrite) rejected++;
  try { await readFile(path, options); } catch { rejected++; }
  try { await writeFile(path + ".never", "never", options); } catch { rejected++; }
  if (rejected !== 5) throw rejected;
  console.log("cancelled");
  return 42;
}

import { readFile, writeFile } from "node:fs/promises";

export async function run(path: string): Promise<number> {
  const controller = new AbortController();
  const options: { signal?: AbortSignal } = { signal: controller.signal };
  const reading = readFile(path, options);
  const writing = writeFile(path + ".output", new Uint8Array(65536), options);
  controller.abort();
  let rejected = 0;
  try { await reading; } catch { rejected++; }
  try { await reading; } catch { rejected++; }
  try { await writing; } catch { rejected++; }
  try { await readFile(path, options); } catch { rejected++; }
  try { await writeFile(path + ".never", "never", options); } catch { rejected++; }
  if (rejected !== 5) throw rejected;
  console.log("cancelled");
  return 42;
}

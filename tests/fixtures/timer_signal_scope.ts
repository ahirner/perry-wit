import { setTimeout } from "node:timers/promises";
import { readFile } from "node:fs/promises";

export async function run(path: string): Promise<string> {
  const controller = new AbortController();
  await setTimeout(1, undefined, { signal: controller.signal });
  controller.abort();
  const first = (await readFile(path, "utf8"));
  let rejected = false;
  try { await setTimeout(1, undefined, { signal: controller.signal }); }
  catch { rejected = true; }
  if (!rejected) throw 1;
  return first + (await readFile(path, "utf8"));
}

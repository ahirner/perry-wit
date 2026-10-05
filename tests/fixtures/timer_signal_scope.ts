import { setTimeout } from "node:timers/promises";
import { readFileSync } from "node:fs";

export async function run(path: string): Promise<string> {
  const controller = new AbortController();
  await setTimeout(1, undefined, { signal: controller.signal });
  controller.abort();
  const first = readFileSync(path, "utf8");
  let rejected = false;
  try { await setTimeout(1, undefined, { signal: controller.signal }); }
  catch { rejected = true; }
  if (!rejected) throw 1;
  return first + readFileSync(path, "utf8");
}

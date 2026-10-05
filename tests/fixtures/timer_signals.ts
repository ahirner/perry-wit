import { setTimeout } from "node:timers/promises";

export async function run(): Promise<number> {
  const controller = new AbortController();
  const options: { signal?: AbortSignal } = { signal: controller.signal };
  const pending = setTimeout(60_000, 1, options);
  const sibling = setTimeout(60_000, 2, options);
  const unrelated = setTimeout(1, 42);
  const winner = await Promise.race([pending, unrelated]);
  if (winner !== 42) throw 1;
  controller.abort();
  const outcomes = await Promise.allSettled([pending, sibling]);
  if (outcomes[0].status !== "rejected" || outcomes[1].status !== "rejected") throw 2;
  let rejected = 0;
  try { await pending; } catch { rejected++; }
  try { await setTimeout(10, undefined, options); } catch { rejected++; }
  if (rejected !== 2) throw 3;
  const empty: { signal?: AbortSignal } = {};
  await setTimeout(1, undefined, empty);
  await setTimeout(1, null, { signal: undefined });
  return await unrelated;
}

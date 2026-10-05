import { setTimeout as delay } from "node:timers/promises";
import * as timers from "node:timers/promises";

let order = 0;
function duration(): number { order = order * 10 + 1; return 1; }
function result(value: string): string { order = order * 10 + 2; return value; }

async function text(value: string): Promise<string> {
  return await delay(undefined, value + " value");
}

export async function run(input: string): Promise<string> {
  const bytes = new Uint8Array([0, 41, 0]);
  const pendingBytes = delay(10, bytes.subarray(1, 2));
  const pendingRecord = delay(10, { label: input + " record" });
  for (let i = 0; i < 2000; i++) {
    const temporary = input.toLowerCase().split(".").join("-");
  }
  const values = await Promise.all([delay(1, 42), text(input), delay(1, true)]);
  if (values[0] !== 42 || !values[2]) return "invalid scalar results";
  const view = await pendingBytes;
  view[0] = 42;
  if (bytes[1] !== 42 || (await pendingBytes) !== view) return "lost byte identity";
  const record = await pendingRecord;
  record.label = record.label + " retained";
  if ((await pendingRecord) !== record) return "lost record identity";
  const tree = JSON.parse('{"name":"retained"}');
  const savedTree = await delay(1, tree);
  if (typeof savedTree !== "object" || typeof savedTree.name !== "string" || savedTree.name !== "retained") return "JSON tree";
  await delay(1, undefined);
  if (await delay(1, null) !== null) return "null result";
  order = 0;
  const ordered = await timers.setTimeout(duration(), result(input));
  if (order !== 12 || ordered !== input) return "invalid argument order";
  return values[1] + "|" + record.label;
}

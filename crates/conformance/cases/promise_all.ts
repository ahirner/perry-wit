import { setTimeout } from 'node:timers/promises';

async function task(value: number): Promise<number> {
  await setTimeout(3 - value);
  return value;
}

const first = task(1);
const second = task(2);
const values: number[] = await Promise.all([first, second]);
const output = JSON.stringify(values);
if (typeof output === 'string') console.log(output);
const observedAgain = await first;
if (observedAgain === 1) console.log('REPEATED_OBSERVATION_OK');

const empty: Promise<number>[] = [];
console.log("empty", (await Promise.all(empty)).length);
console.log("typeof", typeof values[0], typeof output, typeof values[3], typeof JSON.parse("[1]"));

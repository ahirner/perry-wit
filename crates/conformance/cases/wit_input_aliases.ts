import type { WitInput } from "./handler-types/world";

interface Parts { items: Uint8Array[] }
interface Inputs { items: WitInput<Uint8Array>[] }

class Holder {
  items: Uint8Array[];

  constructor(items: Uint8Array[]) {
    this.items = items;
  }

  size(): number {
    return this.items[0].length;
  }
}

function append(items: WitInput<Uint8Array>[], text: string): void {
  items.push(text);
}

function firstIs(items: WitInput<Uint8Array>[], seed: Uint8Array): boolean {
  return items[0] === seed;
}

function keep<T>(items: T[]): T[] { return items; }

function check(): void {
  const seed = new Uint8Array(3);
  const parts: Parts = { items: [seed] };
  const inputs: Inputs = parts;
  console.log(parts.items === inputs.items, firstIs(inputs.items, seed));
  inputs.items[0] = "é";
  const astral = "🍏";
  append(inputs.items, astral);
  console.log(typeof parts.items[0], parts.items[0].length, parts.items[1].length === astral.length);
  let calls = 0;
  try {
    parts.items[0].fill(++calls, 0, 0);
    console.log(false);
  } catch (e) {
    console.log(e instanceof TypeError, calls);
  }
  parts.items[0] = seed;
  parts.items[0][0] = 42;
  console.log(firstIs(inputs.items, seed), inputs.items.length, parts.items[0][0]);
  const strings = keep<string>(["été"]);
  const numbers = keep<number>([7]);
  const writable: WitInput<Uint8Array>[] = strings;
  writable.push(seed);
  console.log(writable === strings, strings[0].length, typeof strings[1], strings[1].length, numbers[0] + 1);
  const narrow: Uint8Array[] = [seed];
  const holder: Holder = new Holder(narrow);
  const view: Inputs = holder;
  view.items[0] = "é";
  console.log(holder.size(), typeof narrow[0]);
}

check();

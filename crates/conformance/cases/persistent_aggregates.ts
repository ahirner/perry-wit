class Counter {
  value: number;
  constructor(value: number) { this.value = value; }
}

let current: Counter = new Counter(0);
const counters = new Map<number, Counter>();

export function writeNext(key: number): number {
  current = new Counter(key + 1);
  counters.set(key, current);
  return current.value;
}

export function overwrite(value: number): number {
  current.value = value;
  return current.value;
}

export function readKey(key: number): number {
  const saved = counters.get(key);
  if (saved === undefined) return -1;
  return saved.value;
}

export function run(x: number): { value: number; trace: number[] } {
  const last = current.value;
  const max = -1 >>> 0;
  const wide = writeNext(max);
  overwrite(x);
  const before = readKey(max);
  writeNext(0);
  return { value: last, trace: [wide, before, readKey(max), readKey(0)] };
}

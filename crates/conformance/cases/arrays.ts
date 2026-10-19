// Array search, growth, copying combinators, iterable conversion and keyed collections.

interface Item {
  n: number;
}
const values: number[] = [1];
function start(): number {
  values.push(2);
  return 0;
}
const a: number[] = [1, 2];
const rest: number[] = [3, 4];
function later(): number {
  rest[0] = 8;
  return a.length;
}
function fail(): number {
  throw new Error("argument");
}
function failText(): string {
  throw new Error("argument");
}

// `includes` is SameValueZero, `indexOf` strict equality; both clamp ECMAScript start positions.
const item: Item = { n: 1 };
const other: Item = { n: 1 };
const objects: Item[] = [item];
const words: string[] = ["a", "b"];
const pair: number[] = [1, 4];
const special: number[] = [NaN, 0];
const optional: (number | undefined)[] = [undefined, NaN, 1];
const mixed: (string | number)[] = ["x", NaN];
console.log("includes-start", values.includes(2, start()));
console.log("identity", objects.includes(item), objects.includes(other), objects.indexOf(other));
console.log("strings", words.includes("b"), words.indexOf("b", -1));
console.log("positions", pair.includes(1, NaN), pair.includes(1, Infinity), pair.includes(1, -Infinity), pair.indexOf(4, -2.8), pair.indexOf(1, 0.8), pair.includes(1, -1));
console.log("special", special.includes(NaN), special.indexOf(NaN), special.includes(-0));
console.log("optional", optional.includes(NaN), optional.includes(undefined), optional.indexOf(NaN));
console.log("mixed", mixed.includes(NaN), mixed.indexOf(NaN));

// `push` evaluates every argument and spread before the receiver grows.
const pushed: number[] = [];
console.log("push-args", pushed.push(1, pushed.length), pushed[1], pushed.push());
const holder: { items: string[] } = { items: [] };
try {
  holder.items.push("first", failText());
} catch (e) {}
console.log("push-failed", holder.items.length);
console.log("push-spread", a.push(0, ...rest, later()), a[3], a[4], a[5]);
console.log("push-self", a.push(...a), a[9]);
try {
  a.push(7, ...rest, fail());
} catch {}
console.log("push-throw", a.length);
const pushedWords: string[] = ["é"];
pushedWords.push(..."🙂,x".split(","));
console.log("push-strings", pushedWords.length, pushedWords[1]);

function reduceAndForEach(items: number[]): number {
  let sum = 0;
  items.forEach((x, idx) => {
    sum += x + idx;
  });
  const prod = items.reduce((acc, x) => acc * x, 1);
  const bare = items.reduce((acc, x) => acc + x);
  return sum + prod + bare;
}
const labels = new Map<string, { label: string }>();
labels.set("k", { label: "found" });
console.log("optional-chain", labels.get("k")?.label ?? null, labels.get("missing")?.label ?? null);
const nums: number[] = [2, 3, 4];
const backwards: number[] = [1, 2, 3];
console.log("reduce", reduceAndForEach(nums), backwards.reduceRight((acc, n, i, array) => {
  if (i === 2) { array.pop(); array.pop(); }
  return acc * 10 + n;
}, 0));
console.log("at", nums.at(-1), nums.at(0), nums.at(5), nums.at(NaN), nums.at(-Infinity));
console.log("find-last", nums.findLast((x) => x < 4), nums.findLastIndex((x) => x < 4), nums.findLast((x) => x > 9), nums.findLastIndex((x) => x > 9));
const shrink: number[] = [1, 2, 3];
let shrinkCalls = 0;
shrink.findLast((_v, _i, arr) => {
  shrinkCalls++;
  arr.pop();
  arr.pop();
  return false;
});
console.log("find-last-shrink", shrinkCalls);
console.log("copies", nums.toReversed().join(","), nums.join(","), nums.slice(1, -1).join(","), nums.slice(NaN).join(","), nums.slice(0, NaN).length, [1, 2, 3, 4].slice(-1.5).join(","), [1, 2, 3].reverse().join(","));
const nested: number[][] = [[1, 2], [], [3]];
console.log("flat", nested.flat().join(","));

interface Scored {
  id: string;
  label: string;
  score: number;
}
const items: Scored[] = [
  { id: "i1", label: "alpha", score: 10 },
  { id: "i2", label: "beta", score: 20 },
  { id: "i3", label: "gamma", score: 30 },
];
const byLabel = new Map(items.map((c) => [c.label, c]));
console.log("map-from-entries", byLabel.size, byLabel.get("beta")?.score);
function fromDefault(value: number, index: number, offset = 5): number {
  return value + index + offset;
}
console.log("array-from", Array.from({ length: 4 }, (_v, i) => i * 2).join(","), Array.from(new Set(["x", "y", "x"]), (s) => s.toUpperCase()).join(","), Array.from([1], fromDefault).join(","));
const fromArray: number[] = [1, 2];
const fromSet = new Set([1, 2, 3]);
console.log("array-from-live", Array.from(fromArray, (value, index, source?: number[]) => {
  if (index === 0) fromArray.push(3);
  return value * 10 + index + (source === undefined ? 0 : 1000);
}).join(","), Array.from(fromSet.values(), (value, index) => {
  if (index === 0) { fromSet.delete(2); fromSet.add(4); }
  return value * 10 + index;
}).join(","));
const fromShrinking: number[] = [1, 2, 3];
console.log("array-from-shrink", Array.from(fromShrinking, (value) => {
  fromShrinking.pop();
  return value;
}).join(","));
const [first, second, ...tail] = ["head", "second", "t1", "t2", "t3"];
const [only, missing = "def", ...emptyRest] = ["only"];
console.log("rest-destructuring", first, second, tail.join(":"), only, missing, emptyRest.length);
const series = [10, 20, 30, 20, 10];
console.log("last-index", series.lastIndexOf(20), series.lastIndexOf(99), series.lastIndexOf(20, 2));
console.log("last-index-bounds", series.lastIndexOf(10, undefined), series.lastIndexOf(10, NaN), series.lastIndexOf(20, -2), series.lastIndexOf(20, -Infinity), series.lastIndexOf(20, Infinity));
console.log("with", series.with(1, 99).join(","), series.join(","), series.with(-1, 77).join(","));
console.log("math-spread", Math.max(...series), Math.min(...series), Math.max(1, 5, 3, 9, 2));
let visited = 0;
outer: for (let i = 0; i < 5; i++) {
  for (let j = 0; j < 5; j++) {
    if (i === 2 && j === 1) break outer;
    visited += 1;
  }
}
console.log("labeled-break", visited);
const sA = new Set(["a", "b", "c"]);
const sB = new Set(["b", "d"]);
console.log("set-algebra", [...sA.difference(sB)].join(","), [...sA.intersection(sB)].join(","), [...sA.union(sB)].join(","), [...sA.symmetricDifference(sB)].join(","));
console.log("set-relations", new Set(["a", "b"]).isSubsetOf(sA), sB.isSubsetOf(sA), sA.isSupersetOf(new Set(["a"])), sA.isDisjointFrom(new Set(["z"])));
const grouped = Map.groupBy(items, (entry) => (entry.score >= 20 ? "high" : "low"));
console.log("group-by", grouped.get("low")?.length, grouped.get("high")?.length);
const original = { a: 1, nested: [2, 3] };
const cloned = structuredClone(original);
original.a = 99;
original.nested.push(4);
console.log("structured-clone", cloned.a, cloned.nested.join(","));

interface Point { x: number; y: number }
function makePoint(i: number): Point {
  return { y: i * 2, x: i };
}
function recordArrayLoop(x: number): number {
  const items: Point[] = [];
  for (let i = 0; i < x; i++) {
  items.push(makePoint(i));
  }
  let sum = 0;
  for (let k = 0; k < items.length; k++) {
  const point = items[k];
  point.x = point.x + 1;
  sum = (sum + point.x + point.y) % 1000003;
  }
  const pair: number[] = [sum, x];
  pair[1] = pair[0];
  pair[2] = 5;
  return pair[1] + pair[2];
}
for (const x of [0, 3, 100, 5000]) console.log("record-array-loop", x, recordArrayLoop(x));

function inferredOptionalElements(x: number): number {
  const bytes = new Uint8Array([7]);
  const a = [bytes[x], 11];
  const b = [13, bytes[x]];
  if (a[0] === undefined && b[1] === undefined) { return a[1] + b[0]; }
  return a[0] + b[1];
}
for (const x of [0, 1]) console.log("inferred-optional", x, inferredOptionalElements(x));

const gapValues: number[] = [1, 4];
const [gapFirst, , gapMissing = 7] = gapValues;
const [tupleText, tupleCount] = ["ok", 3] as [string, number];
console.log("destructuring-gaps", gapFirst, gapMissing, tupleText, tupleCount);

interface Parent<T> { value: T }
interface Child extends Parent<number> { extra: number }
function nestedCollections() {
    if (false) { const skipped = new Map<number, boolean>(); }
    return new Map<string, Set<number>>([['key', new Set<number>([3])]]);
}
function nestedConstructorInference(n: number): number {
    const shapes = new Map<string, {value:number}>();
    shapes.set("a", {value:n});
    const inferred = new Set([3, 4]);
    const inferredMap = new Map([[1, 2], [3, 4]]);
    const values = nestedCollections();
    const pair: Child = {value: n, extra: values.size};
    return pair.value + pair.extra + inferred.size + inferredMap.size;
}
for (const n of [0, 7]) console.log("nested-constructors", nestedConstructorInference(n));

const input = JSON.parse('{"name":"漢🙂","values":[1,2,3],"active":true}');
const output = JSON.stringify(input);
if (typeof output !== "string") throw 1;

interface Item {
  n: number;
  opt?: number;
}
interface Loose {
  n: number;
  extra: unknown;
}

const globalItem: Item = { n: 1 };
function mutateGlobal(): void {
  globalItem.n = 2;
}
function perry_json_parse_user(text: string): number {
  return text.length;
}
function perry_json_copy_0(n: number): number {
  return n + 7;
}

function checkJsonAndPresence(): string {
  const localOrig: Item = { n: 1 };
  const localText = JSON.stringify(localOrig);
  localOrig.n = 2;
  const localParsed: Item = JSON.parse(localText);

  const globalText = JSON.stringify(globalItem);
  mutateGlobal();
  const globalParsed: Item = JSON.parse(globalText);

  const disjointOrig: Item = { n: 1 };
  const disjointOther: Item = { n: 10 };
  const disjointText = JSON.stringify(disjointOrig);
  disjointOther.n = 99;
  const disjointParsed: Item = JSON.parse(disjointText);

  const absentOpt: Item = { n: 5 };
  const explicitUndef: Item = { n: 5, opt: undefined };
  const presentOpt: Item = { n: 5, opt: 8 };
  const absentIn = "opt" in absentOpt ? 1 : 0;
  const explicitIn = "opt" in explicitUndef ? 1 : 0;
  const absentKeys = Object.keys(absentOpt).length;
  const explicitKeys = Object.keys(explicitUndef).length;

  const trickyFloat = 14257184145526768;
  const floatFormatted = JSON.stringify(trickyFloat) === "14257184145526768" ? 1 : 0;
  const loose: Loose = JSON.parse('{"n":3}');

  return `${JSON.stringify(loose)}:${localParsed.n}:${globalParsed.n}:${disjointParsed.n + disjointOther.n}:${absentIn}:${explicitIn}:${absentKeys}:${explicitKeys}:${JSON.stringify(explicitUndef)}:${perry_json_parse_user(localText)}:${perry_json_copy_0(3)}:${floatFormatted}:${presentOpt.opt ?? 0}`;
}

function clearMap(m: Map<string, number>): void {
  m.clear();
}

function checkCollections(): string {
  const setA = new Set<number>([1, 2]);
  const setB = setA;
  setA.delete(1);
  const setAliasHas = setB.has(1) ? 1 : 0;

  let setSameKeyVal = 0;
  const setPairs = new Set<number>([5, 6]);
  setPairs.forEach((value, key) => {
    if (value === key) setSameKeyVal++;
  });

  const setIsArray = Array.isArray(new Set<number>([1])) ? 1 : 0;
  const markerSet: Set<string> = new Set(["__perry_set_type_args", "s"]);

  const mapA = new Map<string, number>();
  mapA.set("x", 1);
  const mapB = mapA;
  mapA.delete("x");
  const mapAliasHas = mapB.has("x") ? 1 : 0;

  const mapToClear = new Map<string, number>();
  mapToClear.set("x", 1);
  clearMap(mapToClear);

  const nanMap = new Map<number, number>();
  nanMap.set(NaN, 3);
  nanMap.set(NaN, 4);
  const nanScore = nanMap.size * 10 + (nanMap.get(NaN) ?? 0);

  const dictA: Record<string, number> = { a: 1, b: 2 };
  const dictB = dictA;
  delete dictA["b"];
  dictA["c"] = 3;
  const dictAliasHas = "b" in dictB ? 1 : 0;
  const rebuilt = Object.fromEntries(Object.entries(dictB));

  return `${setAliasHas}:${setSameKeyVal}:${setIsArray}:${markerSet.size}:${mapAliasHas}:${mapToClear.size}:${nanScore}:${dictAliasHas}:${rebuilt["c"] ?? 0}`;
}

interface Entry {
  key: string;
  value: number;
  tags: string[];
}

interface Boxed<T> {
  value: T;
}
interface Page<T> extends Boxed<T[]> {
  total: number;
  nextCursor?: string | null;
}
type EntryPreview = Pick<Entry, "key" | "value">;
type PartialEntry = Partial<Entry>;

function checkArraysAndNumerics(miss: boolean): string {
  const entries: Entry[] = [
    { key: "food.main", value: 20, tags: ["m"] },
    { key: "extra", value: 5, tags: ["e", "x"] },
    { key: "food", value: 10, tags: ["f"] },
  ];
  const missIdx = entries.findIndex((e: Entry) => e.key === "missing");
  if (missIdx !== -1) {
    entries[missIdx] = { key: "missing", value: 99, tags: [] };
  } else {
    entries.push({ key: "food.salad", value: 30, tags: ["s"] });
  }
  const hitIdx = entries.findIndex((e: Entry) => e.key === "food");
  const sorted = entries.toSorted((a: Entry, b: Entry) => a.key.localeCompare(b.key));
  const nums: number[] = [30, 20, 10, 5];
  const sortedNums = nums.toSorted((a: number, b: number) => a - b);
  const tags: string[] = ["food.main", "food", "food.salad", "extra", "food.dessert"];
  tags.sort((a: string, b: string) => (a < b ? -1 : a > b ? 1 : 0));

  const flatCount = sorted.flatMap((e: Entry) => e.tags).length;
  const sum = sortedNums.reduce((acc: number, n: number) => acc + n, 0);
  const rsum = sortedNums.reduceRight((acc: number, n: number) => acc + n, 0);
  let keySum = 0;
  for (const k of [7, 9].keys()) keySum += k;
  const joinedIdx = miss ? -1 : entries.length;
  const joinedCheck = joinedIdx < 0 ? -1 : joinedIdx;

  const page: Page<EntryPreview> = {
    value: [{ key: "root", value: 12 }],
    total: 1,
    nextCursor: null,
  };
  const extra: PartialEntry | null = { key: "fallback" };
  const genericScore = page.total + (page.value[0]?.value ?? 0) + (extra?.key?.length ?? 0);

  const radixWrap = parseInt("10", 4294967298);
  const parsedSum =
    parseInt("  -42px") +
    parseInt("0x10") +
    parseInt("101", 2) +
    Number.parseInt("ff", 16) +
    parseFloat("  3.5e1abc") +
    Number.parseFloat("-0.25") +
    Number("  0x20  ");
  const nanChecks =
    Number.isNaN(parseInt("10", 37)) &&
    Number.isNaN(parseInt("   ")) &&
    Number.isNaN(Number("12px")) &&
    Number.isSafeInteger(Number.MAX_SAFE_INTEGER) &&
    !Number.isSafeInteger(Number.MAX_SAFE_INTEGER + 1) &&
    Number.isFinite(123.45) &&
    !Number.isFinite(Infinity)
      ? 1
      : 0;

  return `${entries[0].value}:${entries.length}:${hitIdx}:${sorted[0].key}:${nums[0]}:${sortedNums[0]}:${tags[0]}:${flatCount}:${sum}:${rsum}:${keySum}:${joinedCheck}:${genericScore}:${radixWrap}:${parsedSum}:${nanChecks}`;
}

console.log(
  `${output}|${checkJsonAndPresence()}|${checkCollections()}|${checkArraysAndNumerics(true)}`,
);

// Checked tuples are arrays and keep numeric element order.
interface Rule {
  filter: Array<[string, string]>;
}
const rules: Rule[] = JSON.parse('[{"filter":[["MenuRowName","Lunch"],["showInMenu","true"]]}]');
const wide: [number, number, number, number, number, number, number, number, number, number, number] =
  JSON.parse("[0,1,2,3,4,5,6,7,8,9,10]");
const numbered: { _0: number; _1: number } = { _0: 1, _1: 2 };
console.log("tuples", rules[0].filter[0][1], JSON.stringify(rules), JSON.stringify(wide), JSON.stringify(numbered));

// Scalar equality never coerces between JSON types.
function matches(text: string, expected: string | null | undefined): boolean {
  return JSON.parse(text).value === expected;
}
const scalars = JSON.parse('[1,"1",true,false,0,{},[]]');
console.log("scalar-equality", matches('{"value":"yes"}', "yes"), matches('{"value":1}', "1"), matches('{"value":null}', null), matches("{}", undefined), matches("{}", null), matches('{"value":null}', undefined));
console.log("scalar-values", scalars[0] === 1, scalars[1] === 1, scalars[2] === true, scalars[3] === 0, scalars[4] === false, scalars[5] === true, scalars[6] === 0, "1" === scalars[1]);

// Quoted closed-record field names survive checked conversion.
interface Days {
  "0"?: number[];
  "a-b": number;
  "": string;
  "漢": number;
}
const days: Days = JSON.parse('{"0":[7],"a-b":11,"":"yes","漢":13}');
const daysCopy: Days = JSON.parse(JSON.stringify(days));
console.log("quoted-fields", daysCopy["0"]?.[0], daysCopy["a-b"], daysCopy[""], daysCopy["漢"]);

// Spread and rest keep undeclared extras of checked records.
interface OrderRow {
  Order: number;
}
const orderRows: OrderRow[] = JSON.parse('[{"Order":1,"Extra":"keep","Components":[{"x":1}]}]');
// @ts-expect-error `Components` is an undeclared extra of the parsed value.
const { Components, ...orderRest } = orderRows[0];
console.log("extras", JSON.stringify({ ...orderRows[0], Components: undefined }), JSON.stringify(orderRest), JSON.stringify({ ...orderRows[0], Order: 2, Components: undefined }));

// Copied `undefined` stays present; omitted and JSON-missing fields stay absent.
interface Point {
  x?: number;
  y?: number;
}
const omittedSource: Point = {};
const copiedAbsent: Point = { x: omittedSource.x };
const heapSource: Point = {};
const heapKeys = Object.keys(heapSource).length;
const heapCopy: Point = { x: heapSource.x };
const assignedPoint: Point = { x: 1 };
assignedPoint.x = omittedSource.x;
const jsonMissing: Point = JSON.parse("{}");
const explicitPoint: Point = { x: undefined };
const pointValues = Object.values(explicitPoint);
const pointEntries = Object.entries(explicitPoint);
console.log("copied-presence", "x" in copiedAbsent, copiedAbsent.x, Object.keys(copiedAbsent).length, heapKeys, Object.keys(heapCopy).length, Object.keys(assignedPoint).length, "x" in jsonMissing);
console.log("values-entries", pointValues.length, pointValues[0], pointEntries.length, pointEntries[0][0], pointEntries[0][1]);

// Nested collections, fresh iterators, Set spread and keys that resemble internal metadata.
const nestedSets = new Map<string, Set<number>>([["key", new Set<number>([3, 4])]]);
const entryPairs: [string, number][] = [["value", 3]];
const widenedMap = new Map<string, number | undefined>(entryPairs);
const widenedDict: Record<string, number | undefined> = Object.fromEntries(entryPairs);
widenedMap.set("empty", undefined);
widenedDict.empty = undefined;
console.log("widened-entries", widenedMap.get("value"), widenedMap.has("empty"), widenedDict.value, "empty" in widenedDict);
const identitySet = new Set<number>([1]);
const spreadSet = [...new Set([4, 5])];
const nulSet = new Set(["\u0000perry_set_type_args", "__perry_set_type_args", "s"]);
console.log("nested-set", nestedSets.get("key")?.has(4), nestedSets.get("key")?.size);
console.log("iterator-identity", (identitySet.values() as unknown) !== (identitySet.values() as unknown), spreadSet.join(","));
console.log("metadata-keys", nulSet.size, nulSet.has("\u0000perry_set_type_args"), nulSet.has("__perry_set_type_args"));

// Escapes, nested reads, malformed input and recovery after failures.
function identity(value: number): number {
  return value;
}
for (const x of [0, 1, -0.5, 2.5, -2.5, 7, NaN]) {
  const text = JSON.stringify({ text: 'a"\n\u0001é', value: x, list: [1, x, -0], flag: true });
  const parsed = JSON.parse(text);
  let malformed = "parsed";
  try {
    JSON.parse("{" + x);
  } catch (e) {
    malformed = "rejected";
  }
  const value = JSON.parse('{"n":7}');
  const before: number = value.n;
  try {
    new Date(NaN).toISOString();
  } catch (e) {}
  console.log("escapes", text, identity(parsed.text.length), parsed.list[1], JSON.parse([JSON.stringify({ text: "漢" })][0]).text, malformed, before + identity(value.n));
}

interface Ordered { z?: number; a?: number; m?: number }
const firstOrder: Ordered = { z: 1, a: 2, m: 3 };
const secondOrder: Ordered = { m: 3, a: 2, z: 1 };
console.log("literal-order", JSON.stringify(firstOrder), JSON.stringify(secondOrder));
delete firstOrder.a;
firstOrder.a = 4;
console.log("reinsert-order", JSON.stringify(firstOrder));
const copiedOrder: Ordered = JSON.parse(JSON.stringify(firstOrder));
console.log("copy-order", JSON.stringify(copiedOrder));
const undefinedOrder: Ordered = { z: undefined, a: 1, m: 2 };
const normalizedOrder: Ordered = JSON.parse(JSON.stringify(undefinedOrder));
normalizedOrder.z = 3;
console.log("normalized-order", JSON.stringify(normalizedOrder));
const parsedOrder: Ordered = JSON.parse('{"extra":1,"m":2,"z":3,"middle":4,"m":5,"a":6}');
parsedOrder.z = undefined;
parsedOrder.z = 7;
console.log("parsed-order", JSON.stringify(parsedOrder));
console.log("spread-order", JSON.stringify({ before: 0, ...parsedOrder, a: 8, after: 9 }));
const { m: removedOrder, ...restOrder } = parsedOrder;
console.log("rest-order", removedOrder, JSON.stringify(restOrder));
console.log("numeric-order", JSON.stringify({ z: 1, "10": 10, a: 2, "2": 2, "01": 3 }));
const largeOrder = { z: 1, y: 2, x: 3, w: 4, v: 5, u: 6, t: 7, s: 8, r: 9 };
console.log("large-order", JSON.stringify(largeOrder));
interface LargeOrder { z?: number; y?: number; x?: number; w?: number; v?: number; u?: number; t?: number; s?: number; r?: number }
const emptyOrder: LargeOrder = JSON.parse('{}');
console.log("empty-order", JSON.stringify(emptyOrder));
emptyOrder.r = 9;
emptyOrder.z = 1;
console.log("grown-order", JSON.stringify(emptyOrder));
const largeCopy: LargeOrder = JSON.parse(JSON.stringify(largeOrder));
console.log("large-copy-order", JSON.stringify(largeCopy));
class OrderedClass {
  z: number = 3;
  a: number = 1;
  m: number = 2;
}
console.log("class-order", JSON.stringify(new OrderedClass()));


const whitespace = "\t\n\u000b\f\r \u00a0\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2007\u2008\u2009\u200a\u2028\u2029\u202f\u205f\u3000\ufeff";
for (const text of [`${whitespace}42${whitespace}`, "\u180e42", "\u200b42", ".5", "1.", "01", "1e+", "-0", "2.4703282292062328e-324", "1e9999999999999", "Infinitytail", "\uFEFF 42 \u2029", "\u008542"]) {
  console.log("decimal-token", text, parseFloat(text), Number(text), text.trim(), text.trimStart(), text.trimEnd());
}

const replacementInput = {a:"alpha",b:2,nested:{a:"beta",b:3}};
console.log("replacer", JSON.stringify(replacementInput, (key,value) => key === "b" ? undefined : value));
console.log("property-list", JSON.stringify(replacementInput, ["nested","a","a"]));

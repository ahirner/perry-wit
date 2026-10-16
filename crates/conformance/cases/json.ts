const input = JSON.parse('{"name":"漢🙂","values":[1,2,3],"active":true}');
const output = JSON.stringify(input);
if (typeof output !== "string") throw 1;

interface Item {
  n: number;
  opt?: number;
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

  return `${localParsed.n}:${globalParsed.n}:${disjointParsed.n + disjointOther.n}:${absentIn}:${explicitIn}:${absentKeys}:${explicitKeys}:${JSON.stringify(explicitUndef)}:${perry_json_parse_user(localText)}:${perry_json_copy_0(3)}:${floatFormatted}:${presentOpt.opt ?? 0}`;
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


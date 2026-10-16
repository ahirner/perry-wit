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

  const setGrow = new Set<number>([1]);
  let setGrowCount = 0;
  setGrow.forEach((value) => {
    setGrowCount++;
    if (value === 1) setGrow.add(2);
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

  const mapGrow = new Map<string, number>();
  mapGrow.set("a", 1);
  let mapGrowCount = 0;
  mapGrow.forEach((_value, key) => {
    mapGrowCount++;
    if (key === "a") mapGrow.set("b", 2);
  });

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

  const iterSet = new Set<number>([10, 20]);
  const setValues = iterSet.values();
  let setConsumed1 = 0;
  let setConsumed2 = 0;
  for (const v of setValues) setConsumed1 += v;
  for (const v of setValues) setConsumed2 += v;

  const iterMap = new Map<string, number>([
    ["a", 1],
    ["b", 2],
  ]);
  const mapKeys = iterMap.keys();
  let mapConsumed1 = "";
  let mapConsumed2 = "";
  for (const k of mapKeys) mapConsumed1 += k;
  for (const k of mapKeys) mapConsumed2 += k;

  const resumeSet = new Set<number>([1, 2, 3]);
  const resumeIt = resumeSet.values();
  let resume1 = 0;
  let resume2 = 0;
  for (const v of resumeIt) {
    resume1 += v;
    break;
  }
  for (const v of resumeIt) resume2 += v;

  const exhaustedSet = new Set<number>([1]);
  const exhaustedIt = exhaustedSet.values();
  let ex1 = 0;
  let ex2 = 0;
  for (const v of exhaustedIt) ex1 += v;
  exhaustedSet.add(2);
  for (const v of exhaustedIt) ex2 += v;

  const compactActive = new Set<number>();
  for (let i = 1; i <= 10; i++) compactActive.add(i);
  let activeSum = 0;
  for (const v of compactActive.values()) {
    activeSum += v;
    if (v <= 8) compactActive.delete(v);
  }
  let futureSum = 0;
  for (const v of compactActive.values()) futureSum += v;

  return `${setAliasHas}:${setSameKeyVal}:${setGrowCount}:${setIsArray}:${markerSet.size}:${mapAliasHas}:${mapToClear.size}:${mapGrowCount}:${nanScore}:${dictAliasHas}:${rebuilt["c"] ?? 0}:${setConsumed1}:${setConsumed2}:${mapConsumed1}:${mapConsumed2.length}:${resume1}:${resume2}:${ex1}:${ex2}:${activeSum}:${futureSum}`;
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

interface InnerNum {
  x: number;
}
interface OuterNum {
  inner: InnerNum;
}
interface ArrNum {
  xs: number[];
}

function ctxHelper(a: number, b: number): number {
  const s = a + b;
  return s > 200 ? s : -s;
}
function ctx01(): number { return ctxHelper(1, 1); }
function ctx02(): number { return ctxHelper(2, 1); }
function ctx03(): number { return ctxHelper(3, 1); }
function ctx04(): number { return ctxHelper(4, 1); }
function ctx05(): number { return ctxHelper(5, 1); }
function ctx06(): number { return ctxHelper(6, 1); }
function ctx07(): number { return ctxHelper(7, 1); }
function ctx08(): number { return ctxHelper(8, 1); }
function ctx09(): number { return ctxHelper(9, 1); }
function ctx10(): number { return ctxHelper(10, 1); }
function ctx11(): number { return ctxHelper(11, 1); }
function ctx12(): number { return ctxHelper(12, 1); }
function ctx13(): number { return ctxHelper(13, 1); }
function ctx14(): number { return ctxHelper(14, 1); }
function ctx15(): number { return ctxHelper(15, 1); }
function ctx16(): number { return ctxHelper(16, 1); }
function ctx17(): number { return ctxHelper(17, 1); }
function ctx18(): number { return ctxHelper(18, 1); }
function ctx19(): number { return ctxHelper(19, 1); }
function ctx20(): number { return ctxHelper(20, 1); }
function ctx21(): number { return ctxHelper(21, 1); }
function ctx22(): number { return ctxHelper(22, 1); }
function ctx23(): number { return ctxHelper(23, 1); }
function ctx24(): number { return ctxHelper(24, 1); }
function ctx25(): number { return ctxHelper(25, 1); }
function ctx26(): number { return ctxHelper(26, 1); }
function ctx27(): number { return ctxHelper(27, 1); }
function ctx28(): number { return ctxHelper(28, 1); }
function ctx29(): number { return ctxHelper(29, 1); }
function ctx30(): number { return ctxHelper(30, 1); }
function ctx31(): number { return ctxHelper(31, 1); }
function ctx32(): number { return ctxHelper(32, 1); }
function ctx33(): number { return ctxHelper(33, 1); }
function ctx34(): number { return ctxHelper(34, 1); }
function ctx35(a: number, b: number): number { return ctxHelper(a, b) % 256; }

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

  const nestedNum: OuterNum = { inner: { x: parseInt("42") } };
  const recArrNum: ArrNum = { xs: [parseInt("7"), parseInt("8")] };
  const arrRecNum: InnerNum[] = [{ x: parseInt("9") }];
  const pushedNum: number[] = [];
  pushedNum.push(parseInt("11"));
  const indexedNum: number[] = [0];
  indexedNum[0] = parseInt("12");
  const mixedNum: number[] = [1, parseInt("2")];
  const optNum: number | undefined = miss ? parseInt("13") : undefined;
  const numAggrJson = `${nestedNum.inner.x}:${recArrNum.xs[0] + recArrNum.xs[1]}:${arrRecNum[0].x}:${pushedNum[0]}:${indexedNum[0]}:${mixedNum[0] + mixedNum[1]}:${JSON.stringify(nestedNum)}:${JSON.stringify(recArrNum)}:${JSON.stringify(arrRecNum)}:${JSON.stringify(pushedNum)}:${JSON.stringify(indexedNum)}:${JSON.stringify(mixedNum)}:${JSON.stringify(optNum)}`;

  const ctxNoise =
    ctx01() + ctx02() + ctx03() + ctx04() + ctx05() + ctx06() + ctx07() + ctx08() +
    ctx09() + ctx10() + ctx11() + ctx12() + ctx13() + ctx14() + ctx15() + ctx16() +
    ctx17() + ctx18() + ctx19() + ctx20() + ctx21() + ctx22() + ctx23() + ctx24() +
    ctx25() + ctx26() + ctx27() + ctx28() + ctx29() + ctx30() + ctx31() + ctx32() +
    ctx33() + ctx34();
  const ctxCheck = `${ctx35(250, 10)}:${ctxNoise}`;

  return `${entries[0].value}:${entries.length}:${hitIdx}:${sorted[0].key}:${nums[0]}:${sortedNums[0]}:${tags[0]}:${flatCount}:${sum}:${rsum}:${keySum}:${joinedCheck}:${genericScore}:${radixWrap}:${parsedSum}:${nanChecks}:${numAggrJson}:${ctxCheck}`;
}

console.log(
  `${output}|${checkJsonAndPresence()}|${checkCollections()}|${checkArraysAndNumerics(true)}`,
);


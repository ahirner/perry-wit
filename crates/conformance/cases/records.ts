// Records, narrowing, generics, spread/destructuring, parameters and classes.

interface Row {
  name: string;
}
interface Native {
  rows: Array<{ row: Row | null }>;
}
interface Boundary {
  rows: Array<{ row: Row | null | undefined }>;
}
function widen(value: Native): Boundary {
  return value;
}
// Widening a closed aggregate keeps aliasing and `null` distinct from `undefined`.
const native: Native = { rows: [{ row: null }, { row: { name: "before" } }] };
const boundary = widen(native);
const nullBefore = boundary.rows[0].row === null;
const undefinedBefore = boundary.rows[0].row === undefined;
boundary.rows[1].row!.name = "changed";
const aliasName = native.rows[1].row!.name;
native.rows[0].row = { name: "new" };
const reverseAlias = boundary.rows[0].row!.name;
boundary.rows[2] = { row: null };
for (let i = 0; i < 10000; i++) {
  const scratch: Native = { rows: [{ row: { name: "temporary" } }] };
}
console.log("widening", nullBefore, undefinedBefore, aliasName, reverseAlias, native.rows.length, native.rows[2].row === null);

type Choice = { tag: "text"; val: string } | { tag: "number"; val: number };
function size(value: Choice): number {
  try {
    if (value.tag === "text") return value.val.length;
    return value.val + 1;
  } finally {
    const ignored = 0;
  }
}
function presentRecord(value: { n: number } | null | undefined): number {
  return value ? value.n : 10;
}
interface Options {
  query?: { search?: string };
}
function label(options: Options): string {
  return options.query?.search?.trim().toLowerCase() ?? "";
}
function checked(options: Options, visits: number[]): Options {
  visits.push(1);
  return options;
}
const visits: number[] = [];
const chained = checked({ query: { search: " OK " } }, visits).query?.search?.trim().toLowerCase() ?? "";
console.log("narrowing", size({ tag: "text", val: "abc" }), size({ tag: "number", val: 4 }), presentRecord({ n: 7 }), presentRecord(null), presentRecord(undefined));
console.log("optional-chain", JSON.stringify(label({})), JSON.stringify(label({ query: {} })), chained, visits.length);

type A = { tag: "a"; val: number };
type B = { tag: "b"; val: string };
type C = { tag: "c"; val: boolean };
type ABC = A | B | C;
type OnlyAB = Exclude<ABC, C>;
type JustA = Extract<ABC, { tag: "a" }>;
type DefiniteString = NonNullable<string | null | undefined>;
function checkVariant(x: ABC): number {
  if (x.tag === "a" && x.val > 5) return x.val;
  if (x.tag !== "b" || x.val.length === 0) return 0;
  return x.val.length;
}
let blank = "";
let filledText = "primary";
const s1: DefiniteString = blank || "fallback";
const s2: DefiniteString = filledText || "fallback";
const s3: DefiniteString = filledText && "right";
const s4: DefiniteString = blank && "right";
const ab: OnlyAB = { tag: "a", val: 9 };
const ja: JustA = { tag: "a", val: 3 };
console.log("utility-types", s1, s2, s3, JSON.stringify(s4), checkVariant(ab), ja.val, checkVariant({ tag: "b", val: "hi" }));

interface Item {
  n: number;
}
interface Pair<Item, Other = Item> {
  a: Item;
  b: Other;
}
interface Page<T> {
  items: T[];
  next: string | null;
}
interface Base<T> {
  value: T;
}
interface Entry<T = string> extends Base<T> {}
type Box<T> = { item: T };
interface Link<T> {
  value: T;
  next: Link<T> | null;
}
function firstLength(page: Page<Entry>): number {
  const items = page.items;
  return items[0].value.length;
}
function unbox(box: Box<number>): number {
  return box.item;
}
const generic: Pair<string, Item> = { a: "x", b: { n: 17 } };
const defaults: Pair<string> = { a: "a", b: "bc" };
const link: Link<number> = { value: 3, next: null };
console.log("generics", generic.b.n, defaults.b.length, firstLength({ items: [{ value: "hello" }], next: null }), unbox({ item: 2 }), link.value);

interface Product {
  id: number;
  name: string;
  plu: string;
  price?: number;
}
type SelectedProduct = Required<Partial<Pick<Product, "id" | "price">>>;
type PriceIndex<T = number> = Record<string, T>;
type MaybeSelected = SelectedProduct | null | undefined;
function selectedPrice(value: MaybeSelected): number {
  return value == null ? -1 : value.id + value.price;
}
function indexedPrice(values: PriceIndex): number {
  return Object.values(values).reduce((sum, price) => sum + price, 0);
}
console.log("resolved-views", selectedPrice({ id: 7, price: 13 }), selectedPrice(null), selectedPrice(undefined), indexedPrice({ first: 2, second: 3 }));
interface Triple {
  a: number;
  b: number;
  c?: number;
}
function sumTriple({ a, b: second, c = 10 }: Triple): number {
  return a + second + c;
}
const base: Product = { id: 1, name: "Soup", plu: "100", price: 500 };
const updated: Product = { ...base, plu: "200" };
const { id, name: renamed, price = 0, ...productRest } = updated;
console.log("spread", JSON.stringify(updated), base.plu, id, renamed, price, productRest.plu);
const triples: Triple[] = [{ a: 1, b: 2 }, { a: 3, b: 4, c: 5 }];
let total = 0;
for (const { a, b: beta, c = 10 } of triples) total += a + beta + c;
console.log("destructuring", total, triples.map(({ a, b, c = 1 }) => a * 10 + b + c).join(","), sumTriple({ a: 2, b: 3 }), sumTriple({ a: 2, b: 3, c: 4 }));
const withP: { p?: number } = { p: 1 };
const explicitUndef: { p?: number } = { p: undefined };
const omitted: { p?: number } = {};
const overUndef = { ...withP, ...explicitUndef };
const overOmitted = { ...withP, ...omitted };
const copyOmitted = { ...omitted };
const partial: { p?: number; q: number } = { q: 1 };
const { q, ...restOmitted } = partial;
const nullableA: { p: number | null } = { p: 1 };
const nullableB: { p: number | null } = { p: null };
console.log("presence", overUndef.p, "p" in overUndef, overOmitted.p, "p" in overOmitted, "p" in omitted, "p" in copyOmitted, Object.keys(copyOmitted).length);
console.log("rest-presence", q, "p" in restOmitted, Object.keys(restOmitted).length, { ...nullableA, ...nullableB }.p);

interface ProjectionSource { item?: { value: number; extra: string }; extra: number }
interface ProjectionTarget { item?: { value: number } }
function projectFields(value: ProjectionSource): ProjectionTarget { return value; }
const absentProjection: ProjectionSource = { extra: 1 };
const fullProjection: ProjectionSource = { item: { value: 2, extra: "nested" }, extra: 3 };
const projectedAbsent = projectFields(absentProjection);
console.log("record-projection", "item" in projectedAbsent, projectedAbsent.item?.value, projectFields(fullProjection).item?.value);

function formatTag(tagLabel = "item", count = 1, suffix: string | null = null): string {
  return `${tagLabel}:${count}:${suffix ?? "none"}`;
}
function sumWithBase(start = 10, ...rest: number[]): number {
  return rest.reduce((acc, v) => acc + v, start);
}
function failNumber(): number {
  throw new Error("default evaluated");
}
let calls = 0;
const sparse: Array<number | undefined> = [undefined, 7];
const filled = sparse.map((v = ++calls, i = v + 10) => v + i);
const present: Array<number | undefined> = [1];
const unthrown = present.map((v = failNumber()) => v);
const fixedArgs: [string, number] = ["spread", 5];
console.log("defaults", formatTag(), formatTag(undefined, 5), formatTag("box", undefined, "kg"), formatTag("box", 2, null), sumWithBase(), sumWithBase(5, 1, 2, 3), formatTag(...fixedArgs), sumWithBase(undefined, ...[1, 2, 3]));
console.log("callback-defaults", calls, filled.join(","), unthrown[0]);

function stringifyUnknown(v: unknown): string {
  return JSON.stringify(v);
}
function extractId<T extends { id: string }>(item: T): string {
  return item.id;
}
type Kind = "a" | "b";
function assertNever(x: never): never {
  throw new Error("unexpected: " + String(x));
}
function classifyKind(k: Kind): number {
  if (k === "a") return 1;
  if (k === "b") return 2;
  const exhaustive: never = k;
  return assertNever(exhaustive);
}
const isBig = (x: { score: number }): boolean => x.score >= 20;
const scores = [{ score: 10 }, { score: 20 }, { score: 30 }];
console.log("functions", stringifyUnknown(JSON.parse('{"k":7}')), extractId({ id: "i1" }), classifyKind("a"), classifyKind("b"), scores.filter(isBig).length, isBig(scores[0]));

class Counter {
  static step = 2;
  static scaled(n: number): number {
    return n * Counter.step;
  }
  static from(n: number): Counter {
    return new Counter(n);
  }
  count: number;
  label = "ctr";
  constructor(initial = 0) {
    this.count = initial;
  }
  get value(): number {
    return this.count;
  }
  set value(v: number) {
    this.count = v;
  }
  inc(by = 1): number {
    this.count += by;
    return this.count;
  }
  describe(): string {
    return `${this.label}:${this.count}`;
  }
}
class Holder<T> {
  item: T;
  constructor(item: T) {
    this.item = item;
  }
  unwrap(): T {
    return this.item;
  }
}
const counter = new Counter(5);
const before = `${counter.value}:${counter.inc(3)}:${counter.inc()}`;
counter.value = 12;
const incremented = ++counter.value;
const fromStatic = Counter.from(10);
fromStatic.inc(5);
const boxed = new Holder<string>("hello");
console.log("classes", before, incremented, counter.describe(), Counter.scaled(4), fromStatic.describe(), boxed.unwrap());
console.log("instanceof", counter instanceof Counter, counter instanceof Holder, boxed instanceof Holder, boxed instanceof Counter);

interface UpdateCell { value: number }
function updateReceiver(cell: UpdateCell, visits: number[]): UpdateCell { visits.push(1); return cell; }
function recordUpdates(x: number): number {
  const cell: UpdateCell = { value: x };
  const visits: number[] = [];
  const first = updateReceiver(cell, visits).value++;
  const second = --updateReceiver(cell, visits).value;
  return first * 100 + second * 10 + cell.value + visits.length * 1000;
}
console.log("record-updates", recordUpdates(3));

interface OptionalFields { label?: string; value?: number | null; next?: OptionalFields | null; }
function readOptionalFields(value: OptionalFields): number {
  let result = 0;
  if (value.label !== undefined) result += value.label.length;
  if (value.value === undefined) result += 10;
  else if (value.value === null) result += 20;
  else result += value.value;
  const next = value.next;
  if (next !== undefined && next !== null) result += readOptionalFields(next);
  return result;
}
function optionalClosedFields(n: number): number {
  const empty: OptionalFields = {};
  const nullable: OptionalFields = { value: null, next: { label: 'abc', value: n } };
  const parsed: OptionalFields = JSON.parse('{"value":null,"next":{"label":"é"}}');
  if (JSON.stringify(empty) !== '{}') return -1;
  if (JSON.stringify(parsed) !== '{"value":null,"next":{"label":"é"}}') return -2;
  empty.label = 'x';
  empty.value = 5;
  empty.label = undefined;
  empty.value = undefined;
  return readOptionalFields(empty) + readOptionalFields(nullable) + readOptionalFields(parsed);
}
console.log("optional-fields", optionalClosedFields(7));

function traceLiteral(value: number): number {
  console.log("literal-field", value);
  if (value < 0) throw new Error("negative");
  return value;
}
const orderedPoint: { x: number; y: number } = { y: traceLiteral(1), x: traceLiteral(2) };
console.log("literal-result", orderedPoint.x - orderedPoint.y);
function tracePoint(value: number): { x: number; y: number } {
  return { y: traceLiteral(value), x: traceLiteral(value + 1) };
}
const { x: discardedPoint, ...orderedRest } = {
  ...tracePoint(3), y: traceLiteral(5), x: traceLiteral(6), extra: traceLiteral(7)
};
console.log("construction-order", discardedPoint, JSON.stringify(orderedRest));
try {
  const failedPoint = { ...tracePoint(8), extra: traceLiteral(-1), ...tracePoint(100) };
  console.log("unreachable-point", failedPoint.x);
} catch {
  console.log("construction-stopped");
}

function indexById<T extends {id:string}>(rows:T[]):Map<string,T> {
  const result = new Map<string,T>();
  for(const row of rows) {
    if(result.has(row.id)) throw new Error("duplicate");
    result.set(row.id,row);
  }
  return result;
}
function defaultedValue<T>(value:T):T {
  const read = (fallback:T = [value].map((item:T):T => item)[0]):T => fallback;
  return read();
}
const indexedRow = {id:"a",count:defaultedValue(7)};
const genericIndex = indexById([indexedRow]);
indexedRow.count = 9;
console.log("generic-collection", indexById([{id:"a",name:defaultedValue("payload")}]).get("a")?.name,
  genericIndex.get("a") === indexedRow, genericIndex.get("a")?.count);
const emptyIndexedRows:{id:string,name:string}[] = [];
console.log("generic-empty", indexById(emptyIndexedRows).size);
try { indexById([indexedRow,indexedRow]); } catch(e) { console.log("generic-duplicate", e instanceof Error); }

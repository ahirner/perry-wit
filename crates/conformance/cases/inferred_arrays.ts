const items: any[] = [];
const alias = items;
const secondAlias: any[] = alias;
for (let i = 0; i < 8; i++) {
  items.push({
    id: i,
    name: "item_" + i,
    value: i * 3.14159,
    tags: ["tag_" + (i % 10), "tag_" + (i % 5)],
    nested: { x: i, y: i * 2 }
  });
  if (i % 2 === 0) {
    alias[i] = { id: i, name: "even", value: i, tags: ["a", "b"], nested: { x: i, y: i } };
  } else {
    secondAlias[i] = { id: i, name: "odd", value: i, tags: ["c", "d"], nested: { x: i, y: i } };
  }
}
const blob = JSON.stringify(items);
console.log(blob);
console.log(JSON.stringify(JSON.parse(blob)));

function count(n: number): number {
  const words = [];
  words.push("seed");
  const alias = words;
  for (let i = 0; i < n; i++) alias.push("first", "second");
  words[0] = "replacement";
  console.log(JSON.stringify(alias));
  return words.length;
}
console.log(count(3), count(1));

let label: "seed" | "next" = items.length > 0 ? "seed" : "next";
const labels: any[] = [];
labels.push(label);
labels.push("tail");
console.log(JSON.stringify(labels), labels[0].length);

class Counter {
  count(): number {
    const numbers: any[] = [];
    let i = 0;
    do { numbers.push(i); i++; } while (i < 2);
    while (i < 4) { numbers[i] = i; i++; }
    switch (i) {
      case 4: numbers.push(4); break;
      default: numbers.push(5);
    }
    try { numbers.push(6); throw 1; } catch { numbers.push(7); } finally { numbers.push(8); }
    console.log(JSON.stringify(numbers));
    console.log(numbers[20] === undefined, numbers[20] + 1);
    return numbers.length;
  }
}
console.log(new Counter().count());

function pushedRecords(n: number) {
  const records: any[] = [];
  const alias = records;
  const chain = alias;
  for (let i = 0; i < n; i++) {
    if (i % 2 === 0) chain.push({ x: i, label: "a" });
    else alias.push({ label: "b", x: i });
  }
  if (n > 0) records[0].x = 42;
  return chain;
}
for (const n of [0, 3]) console.log("inferred-alias", JSON.stringify(pushedRecords(n)));

function pushedNumbers(flag: boolean): string {
  const first = [];
  first.push(1);
  const second: Array<any> = [];
  let alias = first;
  const retained = alias;
  alias = second;
  const selected = flag ? first : second;
  selected.push(2, 3);
  alias.push(...[4, 5]);
  return retained.join(",") + "/" + second.join(",");
}
console.log("inferred-reassignment", pushedNumbers(false), pushedNumbers(true));

function pushedVariants(flag: boolean): string {
  const values: any[] = [];
  if (flag) values.push({ tag: "number", val: 7 });
  else values.push({ tag: "text", val: "seven" });
  const value = values[0];
  return value.tag === "number" ? String(value.val + 1) : value.val.toUpperCase();
}
console.log("inferred-variants", pushedVariants(true), pushedVariants(false));

function pushedOptional(flag: boolean): string {
  const values: any[] = [];
  if (flag) values.push(7);
  else values.push(undefined);
  return values[0] === undefined ? "missing" : String(values[0]);
}
function pushedScalarUnion(flag: boolean): string {
  const values: any[] = [];
  values.push(flag ? 7 : "seven");
  const value = values[0];
  return typeof value + ":" + String(value);
}
console.log("inferred-unions", pushedOptional(true), pushedOptional(false), pushedScalarUnion(true), pushedScalarUnion(false));
